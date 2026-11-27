//! Action handlers for Wave 11b `app.*` keybindings: `app.local.open`, `app.links.hint`,
//! `app.tools.toggleVisibility`, `app.clipboard.pasteTextRaw`, `app.tree.nextBranch`,
//! `app.tree.previousBranch`, and live profile-merged keymap reload.

use super::App;
use crate::keymap::{Action, Keymap};
use crate::links::{self, LinkAction, LinkHintsOverlay};
use crate::local_picker::{self, LocalPicker, LocalPickerAction};
use crate::transcript::Level;
use crate::vim::{ClipboardIo, SystemClipboard};
use crossterm::event::KeyEvent;
use hrn_kernel::{DagNode, PermissionMode};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub(super) type LocalOpener = Box<dyn FnMut(&Path) -> Result<(), String>>;
pub(super) type LinkOpener = Box<dyn FnMut(&str) -> Result<(), String>>;

/// Injectable external I/O hooks (overridden in tests for deterministic headless runs).
pub(super) struct IoHooks {
    pub(super) local_opener: LocalOpener,
    pub(super) link_opener: LinkOpener,
    pub(super) raw_clipboard: Box<dyn ClipboardIo>,
}

impl Default for IoHooks {
    fn default() -> Self {
        Self {
            local_opener: Box::new(crate::exted::open_file),
            link_opener: Box::new(crate::links::open_url),
            raw_clipboard: Box::new(SystemClipboard),
        }
    }
}

impl App {
    /// Dispatch a globally-bound [`Action`] from the merged active keymap.
    pub(super) fn dispatch_global_action(&mut self, action: Action) {
        self.vim.cancel_pending();
        match action {
            Action::ThinkingCycle => self.cycle_thinking(),
            Action::ToolsExpand => {
                self.verbose = !self.verbose;
                self.flow.replay = true;
                let state = if self.verbose {
                    "expanded"
                } else {
                    "collapsed"
                };
                self.flash(format!("details {state}"));
            }
            Action::ToolsToggleVisibility => self.toggle_tools_visibility(),
            Action::LinksHint => self.open_link_hints(),
            Action::ModelCycleForward => self.cycle_model(true),
            Action::ModelCycleBackward => self.cycle_model(false),
            Action::ModeCycle => self.cycle_mode(),
            Action::DisplayRedraw => self.flow.replay = true,
            Action::HistorySearch => {
                let entries = self.editor.history().to_vec();
                self.history = Some(crate::history::HistorySearch::new(entries));
            }
            Action::EditorExternal => self.open_external_editor(),
            Action::LocalOpen => self.open_local_artifact(),
            Action::ThinkingToggle => {
                self.show_thinking = !self.show_thinking;
                self.transcript.set_thinking_visible(self.show_thinking);
                self.flow.replay = true;
                self.flash(if self.show_thinking {
                    "thinking shown"
                } else {
                    "thinking hidden"
                });
            }
            Action::ModelSelect => self.open_roles_hub(),
            Action::ModelSelectTemporary => self.open_temporary_picker(None),
            Action::AgentsHub => self.open_agent_hub(),
            Action::PlanToggle => {
                if self.settings.mode == PermissionMode::Plan {
                    self.exit_plan_mode();
                } else {
                    self.enter_plan_mode();
                }
            }
            Action::Retry => self.retry_last_turn(),
            Action::ClipboardCopyPrompt => self.copy_last_answer(),
            Action::Hotkeys => self.open_hotkeys(),
            Action::TreeNextBranch => self.switch_sibling_branch(true),
            Action::TreePreviousBranch => self.switch_sibling_branch(false),
            // Scoped/editor actions are handled in their dedicated overlays or editor_key.
            Action::MessageFollowUp
            | Action::MessageDequeue
            | Action::ClipboardPasteImage
            | Action::ClipboardPasteTextRaw
            | Action::SessionPin
            | Action::SessionUnpin
            | Action::SessionDelete
            | Action::BtwInject
            | Action::ApprovalApprove
            | Action::ApprovalDeny => {}
        }
    }

    /// `Ctrl+Shift+O` (`app.tools.toggleVisibility`): show or hide tool call cards in the transcript.
    pub(super) fn toggle_tools_visibility(&mut self) {
        self.show_tools = !self.show_tools;
        self.transcript.set_tools_visible(self.show_tools);
        self.flow.replay = true;
        self.flash(if self.show_tools {
            "tools shown"
        } else {
            "tools hidden"
        });
    }

    /// `Ctrl+Shift+V` / `Alt+Shift+V` (`app.clipboard.pasteTextRaw`): paste plain text directly
    /// from the clipboard without image capture or path-to-image interception, respecting Vim mode.
    pub(super) fn paste_clipboard_text_raw(&mut self) {
        if let Some(text) = self.io.raw_clipboard.read_text() {
            self.vim.paste_text_raw(&mut self.editor, &text);
        } else {
            self.flash("clipboard has no text");
        }
    }

    /// `Alt+G` (`app.local.open`): open a session `local://` artifact in `$VISUAL`/`$EDITOR`
    /// directly when there is 1 artifact, open an inline picker when there are multiple, or show
    /// a notice when there are none.
    pub(super) fn open_local_artifact(&mut self) {
        let Some(dir) = self.backend.local_artifacts_dir() else {
            self.notice(Level::Info, "no local:// artifacts in this session yet");
            return;
        };
        let artifacts = local_picker::list_artifacts(&dir);
        match artifacts.as_slice() {
            [] => self.notice(Level::Info, "no local:// artifacts in this session yet"),
            [single] => self.open_validated_local_file(&dir, &single.rel),
            _ => self.local_picker = Some(LocalPicker::new(dir, artifacts)),
        }
    }

    pub(super) fn local_picker_key(&mut self, key: KeyEvent) -> bool {
        let Some(picker) = &mut self.local_picker else {
            return false;
        };
        match picker.key(key) {
            LocalPickerAction::Stay => {}
            LocalPickerAction::Close => self.local_picker = None,
            LocalPickerAction::Open(art) => {
                let root = picker.root().to_path_buf();
                self.local_picker = None;
                self.open_validated_local_file(&root, &art.rel);
            }
        }
        true
    }

    fn open_validated_local_file(&mut self, root: &Path, rel: &str) {
        match local_picker::resolve_artifact(root, rel) {
            Ok(path) => match (self.io.local_opener)(&path) {
                Ok(()) => {
                    self.flow.replay = true;
                    self.flash(format!("opened local://{rel}"));
                }
                Err(err) => {
                    self.flow.replay = true;
                    self.notice(Level::Error, err);
                }
            },
            Err(err) => self.notice(Level::Error, err),
        }
    }

    /// `Alt+O` (`app.links.hint`): open an overlay listing hinted links in the transcript.
    pub(super) fn open_link_hints(&mut self) {
        let found = links::extract_transcript_links(&self.transcript);
        if found.is_empty() {
            self.flash("no links in transcript");
            return;
        }
        self.link_hints = Some(LinkHintsOverlay::new(found));
    }

    pub(super) fn link_hints_key(&mut self, key: KeyEvent) -> bool {
        let Some(overlay) = &mut self.link_hints else {
            return false;
        };
        match overlay.key(key) {
            LinkAction::Stay => {}
            LinkAction::Close => self.link_hints = None,
            LinkAction::Copy(url) => {
                self.link_hints = None;
                if self.io.raw_clipboard.write_text(&url) {
                    self.flash(format!("copied {url}"));
                } else {
                    self.notice(Level::Warn, "clipboard unavailable");
                }
            }
            LinkAction::Open(url) => {
                self.link_hints = None;
                if let Some(rel) = url.strip_prefix("local://") {
                    if let Some(dir) = self.backend.local_artifacts_dir() {
                        self.open_validated_local_file(&dir, rel);
                    } else {
                        self.notice(Level::Warn, "no local:// artifacts in this session");
                    }
                    return true;
                }
                let copied = self.io.raw_clipboard.write_text(&url);
                match (self.io.link_opener)(&url) {
                    Ok(()) => self.flash(format!("opened {url}")),
                    Err(err) if copied => self.flash(format!("copied {url} ({err})")),
                    Err(err) => self.notice(Level::Error, err),
                }
            }
        }
        true
    }

    /// `app.tree.nextBranch` / `app.tree.previousBranch`: cycle between sibling branch leaves of
    /// the current session DAG without opening the full `/tree` overlay.
    pub(super) fn switch_sibling_branch(&mut self, forward: bool) {
        if self.is_busy() {
            self.notice(
                Level::Warn,
                "the agent is working | press esc to interrupt first",
            );
            return;
        }
        let Ok(Some((head, nodes))) = self.backend.current_tree() else {
            self.flash("no session tree yet");
            return;
        };
        let leaves = branch_leaves(&nodes);
        if leaves.len() <= 1 {
            self.flash("no sibling branches in this session");
            return;
        }
        let cur_idx = current_leaf_index(&nodes, head.as_deref(), &leaves).unwrap_or(0);
        let next_idx = if forward {
            (cur_idx + 1) % leaves.len()
        } else {
            (cur_idx + leaves.len() - 1) % leaves.len()
        };
        let Some(target) = leaves.get(next_idx) else {
            return;
        };
        match self.backend.checkout_optional(Some(target.as_str()), false) {
            Ok((meta, branch)) => {
                self.show_session(&meta, &branch);
                self.backend.apply_settings(&self.settings);
                self.flash(format!(
                    "switched to branch {}/{} ({})",
                    next_idx + 1,
                    leaves.len(),
                    target
                ));
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// Reload merged profile keybindings from `self.appearance.keymap_paths`, surfacing any
    /// unknown-action or invalid-chord diagnostics as warnings.
    pub(super) fn reload_keymap(&mut self, announce: bool) {
        let (next, problems) = Keymap::load_files(&self.appearance.keymap_paths);
        self.keymap = next;
        let has_problems = !problems.is_empty();
        for problem in problems {
            self.notice(Level::Warn, problem);
        }
        if announce && !has_problems {
            self.flash("keybindings reloaded");
        }
    }
}

/// Ordered branch leaves (nodes with no children) in deterministic DAG order.
fn branch_leaves(nodes: &[DagNode]) -> Vec<String> {
    let parents: HashSet<&str> = nodes
        .iter()
        .filter_map(|n| n.parent_id.as_deref())
        .collect();
    nodes
        .iter()
        .filter(|n| !parents.contains(n.id.as_str()))
        .map(|n| n.id.clone())
        .collect()
}

fn current_leaf_index(nodes: &[DagNode], head: Option<&str>, leaves: &[String]) -> Option<usize> {
    let head = head?;
    if let Some(pos) = leaves.iter().position(|id| id == head) {
        return Some(pos);
    }
    let parent_of: HashMap<&str, Option<&str>> = nodes
        .iter()
        .map(|n| (n.id.as_str(), n.parent_id.as_deref()))
        .collect();
    leaves.iter().position(|leaf| {
        let mut walk = Some(leaf.as_str());
        let mut guard = 0usize;
        while let Some(id) = walk {
            if id == head {
                return true;
            }
            guard += 1;
            if guard > nodes.len() {
                break;
            }
            walk = parent_of.get(id).copied().flatten();
        }
        false
    })
}
