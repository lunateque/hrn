//! The keymap: named actions bound to chords.
//!
//! Built-in defaults live in the embedded `data/keybindings.toml`. User remap layers merge on top
//! in strict precedence order:
//! `embedded < default-profile <default_data_dir>/keybindings.toml < active-profile <active_data_dir>/keybindings.toml`.
//! Each layer overrides action by action (`"id" = "chord"`, `"id" = ["a", "b"]`, `"id" = []` or
//! `"id" = ""` to unbind). `/hotkeys` renders the merged active map.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The compiled-in defaults.
const DEFAULTS: &str = include_str!("../data/keybindings.toml");

/// Every built-in `app.*` action identifier supported by the TUI keymap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Action {
    HistorySearch,
    EditorNewline,
    EditorExternal,
    LocalOpen,
    MessageFollowUp,
    MessageDequeue,
    ThinkingToggle,
    ThinkingCycle,
    ModelCycleForward,
    ModelCycleBackward,
    ModelSelect,
    ModelSelectTemporary,
    PlanToggle,
    ModeCycle,
    ToolsExpand,
    ToolsToggleVisibility,
    LinksHint,
    DisplayRedraw,
    Retry,
    ClipboardCopyPrompt,
    ClipboardPasteImage,
    ClipboardPasteTextRaw,
    Hotkeys,
    SessionPin,
    SessionUnpin,
    SessionDelete,
    BtwInject,
    ApprovalApprove,
    ApprovalDeny,
    AgentsHub,
    TreeNextBranch,
    TreePreviousBranch,
}

impl Action {
    /// Every declared action in canonical order.
    pub(crate) const ALL: &'static [Self] = &[
        Self::HistorySearch,
        Self::EditorNewline,
        Self::EditorExternal,
        Self::LocalOpen,
        Self::MessageFollowUp,
        Self::MessageDequeue,
        Self::ThinkingToggle,
        Self::ThinkingCycle,
        Self::ModelCycleForward,
        Self::ModelCycleBackward,
        Self::ModelSelect,
        Self::ModelSelectTemporary,
        Self::PlanToggle,
        Self::ModeCycle,
        Self::ToolsExpand,
        Self::ToolsToggleVisibility,
        Self::LinksHint,
        Self::DisplayRedraw,
        Self::Retry,
        Self::ClipboardCopyPrompt,
        Self::ClipboardPasteImage,
        Self::ClipboardPasteTextRaw,
        Self::Hotkeys,
        Self::SessionPin,
        Self::SessionUnpin,
        Self::SessionDelete,
        Self::BtwInject,
        Self::ApprovalApprove,
        Self::ApprovalDeny,
        Self::AgentsHub,
        Self::TreeNextBranch,
        Self::TreePreviousBranch,
    ];

    /// Actions dispatched globally from the main prompt loop (excludes overlay-scoped bindings
    /// such as approval dock, session picker, `/btw`, and editor-level paste/follow-up chords).
    pub(crate) const GLOBAL: &'static [Self] = &[
        Self::ThinkingCycle,
        Self::ToolsExpand,
        Self::ToolsToggleVisibility,
        Self::LinksHint,
        Self::ModelCycleForward,
        Self::ModelCycleBackward,
        Self::ModeCycle,
        Self::DisplayRedraw,
        Self::HistorySearch,
        Self::EditorExternal,
        Self::LocalOpen,
        Self::ThinkingToggle,
        Self::ModelSelect,
        Self::ModelSelectTemporary,
        Self::AgentsHub,
        Self::PlanToggle,
        Self::Retry,
        Self::ClipboardCopyPrompt,
        Self::Hotkeys,
        Self::TreeNextBranch,
        Self::TreePreviousBranch,
    ];

    /// Canonical dotted action identifier matching `data/keybindings.toml`.
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::HistorySearch => "app.history.search",
            Self::EditorNewline => "app.editor.newline",
            Self::EditorExternal => "app.editor.external",
            Self::LocalOpen => "app.local.open",
            Self::MessageFollowUp => "app.message.followUp",
            Self::MessageDequeue => "app.message.dequeue",
            Self::ThinkingToggle => "app.thinking.toggle",
            Self::ThinkingCycle => "app.thinking.cycle",
            Self::ModelCycleForward => "app.model.cycleForward",
            Self::ModelCycleBackward => "app.model.cycleBackward",
            Self::ModelSelect => "app.model.select",
            Self::ModelSelectTemporary => "app.model.selectTemporary",
            Self::PlanToggle => "app.plan.toggle",
            Self::ModeCycle => "app.mode.cycle",
            Self::ToolsExpand => "app.tools.expand",
            Self::ToolsToggleVisibility => "app.tools.toggleVisibility",
            Self::LinksHint => "app.links.hint",
            Self::DisplayRedraw => "app.display.redraw",
            Self::Retry => "app.retry",
            Self::ClipboardCopyPrompt => "app.clipboard.copyPrompt",
            Self::ClipboardPasteImage => "app.clipboard.pasteImage",
            Self::ClipboardPasteTextRaw => "app.clipboard.pasteTextRaw",
            Self::Hotkeys => "app.hotkeys",
            Self::SessionPin => "app.session.pin",
            Self::SessionUnpin => "app.session.unpin",
            Self::SessionDelete => "app.session.delete",
            Self::BtwInject => "app.btw.inject",
            Self::ApprovalApprove => "app.approval.approve",
            Self::ApprovalDeny => "app.approval.deny",
            Self::AgentsHub => "app.agents.hub",
            Self::TreeNextBranch => "app.tree.nextBranch",
            Self::TreePreviousBranch => "app.tree.previousBranch",
        }
    }

    /// Resolve a dotted action identifier to its typed [`Action`], if known.
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|action| action.id() == id)
    }
}

/// One entry of the embedded defaults: an action, its chords, and what it does.
#[derive(Deserialize)]
struct Spec {
    id: String,
    chords: Vec<String>,
    help: String,
}

#[derive(Deserialize)]
struct Defaults {
    action: Vec<Spec>,
}

/// One bound action: its chords (normalized) and display text.
#[derive(Clone, Default)]
struct Binding {
    keys: Vec<KeyEvent>,
    display: String,
}

/// The merged map, in the defaults' display order.
#[derive(Clone, Default)]
pub(crate) struct Keymap {
    order: Vec<String>,
    help: BTreeMap<String, String>,
    bindings: BTreeMap<String, Binding>,
}

/// Bring a chord and a key event to one comparable form:
/// - Uppercase ASCII letters normalize to lowercase + `SHIFT` so `Ctrl+Shift+O` stays distinct
///   from `Ctrl+O` and `Alt+Shift+P` stays distinct from `Alt+P`.
/// - `Shift+Tab` and `BackTab` normalize to `BackTab` with no `SHIFT` modifier.
pub(crate) fn normalizes(key: KeyEvent) -> KeyEvent {
    let mut mods = key.modifiers;
    let code = match key.code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => {
            mods |= KeyModifiers::SHIFT;
            KeyCode::Char(c.to_ascii_lowercase())
        }
        KeyCode::Tab if mods.contains(KeyModifiers::SHIFT) => {
            mods -= KeyModifiers::SHIFT;
            KeyCode::BackTab
        }
        KeyCode::BackTab => {
            mods -= KeyModifiers::SHIFT;
            KeyCode::BackTab
        }
        other => other,
    };
    KeyEvent::new(code, mods)
}

/// Parse `Ctrl+Shift+P`, `Alt+Up`, `Shift+Enter`, `F5`; case-insensitive.
fn parse_chord(chord: &str) -> Result<KeyEvent, String> {
    let mut mods = KeyModifiers::NONE;
    let mut key: Option<KeyCode> = None;
    for part in chord.split('+') {
        let name = part.trim();
        let low = name.to_ascii_lowercase();
        match low.as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" | "opt" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "enter" | "return" => key = Some(KeyCode::Enter),
            "tab" => key = Some(KeyCode::Tab),
            "backtab" => key = Some(KeyCode::BackTab),
            "esc" | "escape" => key = Some(KeyCode::Esc),
            "space" => key = Some(KeyCode::Char(' ')),
            "backspace" => key = Some(KeyCode::Backspace),
            "delete" | "del" => key = Some(KeyCode::Delete),
            "up" => key = Some(KeyCode::Up),
            "down" => key = Some(KeyCode::Down),
            "left" => key = Some(KeyCode::Left),
            "right" => key = Some(KeyCode::Right),
            "home" => key = Some(KeyCode::Home),
            "end" => key = Some(KeyCode::End),
            "pageup" | "pgup" => key = Some(KeyCode::PageUp),
            "pagedown" | "pgdn" => key = Some(KeyCode::PageDown),
            other if other.chars().count() == 1 => {
                let c = other
                    .chars()
                    .next()
                    .ok_or_else(|| format!("empty chord `{chord}`"))?;
                key = Some(KeyCode::Char(c));
            }
            other if other.starts_with('f') && other.len() >= 2 => {
                let num = other
                    .get(1..)
                    .and_then(|s| s.parse::<u8>().ok())
                    .filter(|n| (1..=24).contains(n))
                    .ok_or_else(|| format!("unrecognized chord `{chord}`"))?;
                key = Some(KeyCode::F(num));
            }
            _ => return Err(format!("unrecognized chord `{chord}`")),
        }
    }
    key.map_or_else(
        || Err(format!("chord `{chord}` names no key")),
        |code| Ok(normalizes(KeyEvent::new(code, mods))),
    )
}

impl Keymap {
    /// Merge embedded defaults with an ordered slice of on-disk `keybindings.toml` files
    /// (`default_profile < active_profile`). Missing files are silently skipped; unreadable or
    /// malformed files emit diagnostics while keeping earlier layers active.
    pub(crate) fn load_files(paths: &[PathBuf]) -> (Self, Vec<String>) {
        let mut map = match Self::embedded() {
            Ok(map) => map,
            Err(problem) => return (Self::default(), vec![problem]),
        };
        let mut problems = Vec::new();
        for path in paths {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    let label = display_label(path);
                    map.apply_layer(&label, &text, &mut problems);
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => problems.push(format!("{}: {err}", path.display())),
            }
        }
        (map, problems)
    }

    fn apply_layer(&mut self, label: &str, text: &str, problems: &mut Vec<String>) {
        match toml::from_str::<BTreeMap<String, Chords>>(text) {
            Ok(remaps) => {
                for (action, chords) in remaps {
                    if !self.help.contains_key(&action) {
                        problems.push(format!("{label}: unknown action `{action}`"));
                        continue;
                    }
                    match binding_for(&chords) {
                        Ok(binding) => {
                            self.bindings.insert(action, binding);
                        }
                        Err(problem) => problems.push(format!("{label}: {problem}")),
                    }
                }
            }
            Err(err) => problems.push(format!("{label}: {err}")),
        }
    }

    pub(crate) fn embedded() -> Result<Self, String> {
        let parsed: Defaults = toml::from_str(DEFAULTS)
            .map_err(|err| format!("the built-in keybindings are malformed: {err}"))?;
        let mut map = Self::default();
        for spec in parsed.action {
            if Action::from_id(&spec.id).is_none() {
                return Err(format!("unknown built-in action `{}`", spec.id));
            }
            let keys = parse_chords(&spec.id, &spec.chords)?;
            let display = if spec.chords.is_empty() {
                "(unbound)".to_owned()
            } else {
                spec.chords.join(" · ")
            };
            map.order.push(spec.id.clone());
            map.help.insert(spec.id.clone(), spec.help);
            map.bindings.insert(spec.id, Binding { keys, display });
        }
        Ok(map)
    }

    /// The first global [`Action`] claimed by `key`, ignoring overlay-scoped bindings.
    pub(crate) fn global_action(&self, key: KeyEvent) -> Option<Action> {
        Action::GLOBAL
            .iter()
            .copied()
            .find(|action| self.is(action.id(), key))
    }

    /// The first action id `key` fires across all actions, if any bound action claims it.
    pub(crate) fn action(&self, key: KeyEvent) -> Option<&str> {
        self.order
            .iter()
            .find(|action| self.is(action, key))
            .map(String::as_str)
    }

    /// Whether `key` fires `action`.
    pub(crate) fn is(&self, action: &str, key: KeyEvent) -> bool {
        let want = normalizes(key);
        self.bindings
            .get(action)
            .is_some_and(|b| b.keys.contains(&want))
    }

    /// The chord list shown for `action` in `/hotkeys`.
    pub(crate) fn chords(&self, action: &str) -> String {
        self.bindings
            .get(action)
            .map_or_else(String::new, |b| b.display.clone())
    }

    /// `(id, chords, help)` in display order.
    pub(crate) fn rows(&self) -> Vec<(&str, String, String)> {
        self.order
            .iter()
            .map(|id| {
                (
                    id.as_str(),
                    self.chords(id),
                    self.help.get(id).cloned().unwrap_or_default(),
                )
            })
            .collect()
    }
}

fn display_label(path: &Path) -> String {
    path.display().to_string()
}

/// One remapped action's binding, or the problem that rejects the whole line.
fn binding_for(chords: &Chords) -> Result<Binding, String> {
    let list = chords.list();
    if list.is_empty() {
        return Ok(Binding {
            keys: Vec::new(),
            display: "(unbound)".to_owned(),
        });
    }
    let mut keys = Vec::new();
    let mut display = Vec::new();
    for chord in list {
        keys.push(parse_chord(chord)?);
        display.push(chord.to_owned());
    }
    Ok(Binding {
        keys,
        display: display.join(" · "),
    })
}

/// Parse every chord of a built-in entry; one bad chord fails the whole embedded map.
fn parse_chords(action: &str, chords: &[String]) -> Result<Vec<KeyEvent>, String> {
    chords
        .iter()
        .map(|chord| parse_chord(chord).map_err(|err| format!("`{action}`: {err}")))
        .collect()
}

/// The user file's value shape: one chord (empty string unbinds) or a list (empty list unbinds).
#[derive(Deserialize)]
#[serde(untagged)]
enum Chords {
    One(String),
    Many(Vec<String>),
}

impl Chords {
    fn list(&self) -> Vec<&str> {
        match self {
            Self::One(chord) if chord.trim().is_empty() => Vec::new(),
            Self::One(chord) => vec![chord.as_str()],
            Self::Many(list) => list
                .iter()
                .map(String::as_str)
                .filter(|s| !s.trim().is_empty())
                .collect(),
        }
    }
}
