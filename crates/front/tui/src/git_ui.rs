//! Fullscreen interactive git workbench shared by `/git [revision]` inside `hrn chat` and the
//! standalone `hrn git [revision]` CLI subcommand.

use crate::git_ui_ops::{
    GitFileEntry, branch_label, commit_staged, diff_for_entry, list_revision_files, list_status,
    stage_path, staged_diff, unstage_path, validate_revision,
};
use crate::inline::{Frame, Inline};
use crate::term;
use crate::theme;
use crate::vim::CursorShape;
use crate::wrap;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[path = "git_ui_draw.rs"]
mod draw;

/// Optional borrowed callback producing a commit message from the currently staged diff.
pub type CommitMsgGenerator<'a> = &'a mut (dyn FnMut(&str) -> Result<String, String> + 'a);

/// Optional owned callback producing a commit message from the currently staged diff.
pub type BoxCommitMsgGenerator = Box<dyn FnMut(&str) -> Result<String, String>>;

/// Outcome of handling one key event in [`GitWorkbench`].
#[derive(Debug, PartialEq, Eq)]
pub enum GitUiAction {
    /// Remain inside the git workbench.
    Stay,
    /// Exit the git workbench (returning to chat or ending `hrn git`).
    Close,
    /// A git commit succeeded with the given summary line.
    Committed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Files,
    Diff,
    Commit,
}

/// Interactive git status, split diff, stage/unstage, and commit composer state.
pub struct GitWorkbench {
    repo_dir: PathBuf,
    revision: Option<String>,
    branch: String,
    entries: Vec<GitFileEntry>,
    selected: usize,
    diff_text: String,
    diff_scroll: usize,
    focus: Focus,
    commit_msg: String,
    commit_cursor: usize,
    status_msg: Option<String>,
    status_is_err: bool,
}

impl GitWorkbench {
    /// Open the workbench for `repo_dir`, optionally scoped to a read-only `revision`.
    ///
    /// # Errors
    /// Returns an error if `revision` is unsafe or git status/diff-tree fails.
    pub fn open(repo_dir: PathBuf, revision: Option<String>) -> Result<Self, String> {
        if let Some(ref rev) = revision {
            validate_revision(rev)?;
        }
        let branch = branch_label(&repo_dir);
        let mut wb = Self {
            repo_dir,
            revision,
            branch,
            entries: Vec::new(),
            selected: 0,
            diff_text: String::new(),
            diff_scroll: 0,
            focus: Focus::Files,
            commit_msg: String::new(),
            commit_cursor: 0,
            status_msg: None,
            status_is_err: false,
        };
        wb.refresh()?;
        Ok(wb)
    }

    /// Repository root directory backing this workbench.
    #[must_use]
    pub fn repo_dir(&self) -> &Path {
        &self.repo_dir
    }

    /// Current sidebar entries.
    #[must_use]
    pub fn entries(&self) -> &[GitFileEntry] {
        &self.entries
    }

    /// Currently highlighted entry, if any.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&GitFileEntry> {
        self.entries.get(self.selected)
    }

    /// Reload status (or revision file list) and refresh the right-hand diff pane.
    ///
    /// # Errors
    /// Returns an error if invoking `git` fails.
    pub fn refresh(&mut self) -> Result<(), String> {
        let prev_path = self.selected_entry().map(|e| e.path.clone());
        self.branch = branch_label(&self.repo_dir);
        self.entries = match &self.revision {
            Some(rev) => list_revision_files(&self.repo_dir, rev)?,
            None => list_status(&self.repo_dir)?,
        };
        if let Some(prev) = prev_path
            && let Some(pos) = self.entries.iter().position(|e| e.path == prev)
        {
            self.selected = pos;
        } else if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
        self.reload_diff();
        Ok(())
    }

    /// Toggle staging for the currently selected file (`Space`).
    ///
    /// Stages unstaged/untracked changes; if the entry is purely staged, unstages it.
    ///
    /// # Errors
    /// Returns an error in revision mode or when `git add` / `git restore --staged` fails.
    pub fn toggle_stage(&mut self) -> Result<(), String> {
        if self.revision.is_some() {
            return Err("staging is disabled while inspecting a revision".to_owned());
        }
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(());
        };
        if entry.is_staged() && !entry.is_unstaged() {
            unstage_path(&self.repo_dir, &entry.path)?;
            self.set_info(format!("unstaged {}", wrap::sanitize(&entry.path)));
        } else {
            stage_path(&self.repo_dir, &entry.path)?;
            self.set_info(format!("staged {}", wrap::sanitize(&entry.path)));
        }
        self.refresh()
    }

    /// Explicitly stage the currently selected file (`s`).
    ///
    /// # Errors
    /// Returns an error in revision mode or when `git add` fails.
    pub fn stage_selected(&mut self) -> Result<(), String> {
        if self.revision.is_some() {
            return Err("staging is disabled while inspecting a revision".to_owned());
        }
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(());
        };
        stage_path(&self.repo_dir, &entry.path)?;
        self.set_info(format!("staged {}", wrap::sanitize(&entry.path)));
        self.refresh()
    }

    /// Explicitly unstage the currently selected file (`u`).
    ///
    /// # Errors
    /// Returns an error in revision mode or when `git restore --staged` fails.
    pub fn unstage_selected(&mut self) -> Result<(), String> {
        if self.revision.is_some() {
            return Err("unstaging is disabled while inspecting a revision".to_owned());
        }
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(());
        };
        unstage_path(&self.repo_dir, &entry.path)?;
        self.set_info(format!("unstaged {}", wrap::sanitize(&entry.path)));
        self.refresh()
    }

    /// Process a keyboard event, optionally using `msg_gen` to draft commit messages (`g`).
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        msg_gen: Option<CommitMsgGenerator<'_>>,
    ) -> GitUiAction {
        if self.focus == Focus::Commit {
            return self.handle_commit_key(key, msg_gen);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => GitUiAction::Close,
            KeyCode::Char('c') if ctrl => GitUiAction::Close,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Files => Focus::Diff,
                    Focus::Diff | Focus::Commit => Focus::Files,
                };
                GitUiAction::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::Diff {
                    self.diff_scroll = self.diff_scroll.saturating_add(1);
                } else if self.selected + 1 < self.entries.len() {
                    self.selected += 1;
                    self.reload_diff();
                }
                GitUiAction::Stay
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::Diff {
                    self.diff_scroll = self.diff_scroll.saturating_sub(1);
                } else if self.selected > 0 {
                    self.selected -= 1;
                    self.reload_diff();
                }
                GitUiAction::Stay
            }
            KeyCode::Char('J') | KeyCode::PageDown => {
                self.diff_scroll = self.diff_scroll.saturating_add(10);
                GitUiAction::Stay
            }
            KeyCode::Char('K') | KeyCode::PageUp => {
                self.diff_scroll = self.diff_scroll.saturating_sub(10);
                GitUiAction::Stay
            }
            KeyCode::Char(' ') => {
                if let Err(err) = self.toggle_stage() {
                    self.set_err(err);
                }
                GitUiAction::Stay
            }
            KeyCode::Char('s') => {
                if let Err(err) = self.stage_selected() {
                    self.set_err(err);
                }
                GitUiAction::Stay
            }
            KeyCode::Char('u') => {
                if let Err(err) = self.unstage_selected() {
                    self.set_err(err);
                }
                GitUiAction::Stay
            }
            KeyCode::Char('r') => {
                if let Err(err) = self.refresh() {
                    self.set_err(err);
                } else {
                    self.set_info("refreshed".to_owned());
                }
                GitUiAction::Stay
            }
            KeyCode::Char('c') => {
                self.open_composer(false, msg_gen);
                GitUiAction::Stay
            }
            KeyCode::Char('g') => {
                self.open_composer(true, msg_gen);
                GitUiAction::Stay
            }
            _ => GitUiAction::Stay,
        }
    }

    fn open_composer(&mut self, generate: bool, msg_gen: Option<CommitMsgGenerator<'_>>) {
        if self.revision.is_some() {
            self.set_err("committing is disabled in revision view".to_owned());
            return;
        }
        if !self.entries.iter().any(GitFileEntry::is_staged) {
            self.set_err("nothing staged · press Space to stage files first".to_owned());
            return;
        }
        self.focus = Focus::Commit;
        if generate {
            self.generate_message(msg_gen);
        } else {
            self.set_info(
                "type commit message · Enter commit · Ctrl+G generate · Esc cancel".to_owned(),
            );
        }
    }

    fn generate_message(&mut self, msg_gen: Option<CommitMsgGenerator<'_>>) {
        let Some(generator) = msg_gen else {
            self.set_err("commit message generator is unavailable in this mode".to_owned());
            return;
        };
        match staged_diff(&self.repo_dir) {
            Ok(diff) if diff.trim().is_empty() => {
                self.set_err("staged diff is empty".to_owned());
            }
            Ok(diff) => match generator(&diff) {
                Ok(msg) => {
                    let single = msg.trim().lines().next().unwrap_or("").trim().to_owned();
                    self.commit_cursor = single.len();
                    self.commit_msg = single;
                    self.set_info("generated commit message · edit or press Enter".to_owned());
                }
                Err(err) => self.set_err(format!("message generation failed: {err}")),
            },
            Err(err) => self.set_err(err),
        }
    }

    fn handle_commit_key(
        &mut self,
        key: KeyEvent,
        msg_gen: Option<CommitMsgGenerator<'_>>,
    ) -> GitUiAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.focus = Focus::Files;
                GitUiAction::Stay
            }
            KeyCode::Char('g' | 'G') if ctrl || alt => {
                self.generate_message(msg_gen);
                GitUiAction::Stay
            }
            KeyCode::Enter | KeyCode::Char('s' | 'S') if key.code == KeyCode::Enter || ctrl => {
                match commit_staged(&self.repo_dir, &self.commit_msg) {
                    Ok(summary) => {
                        self.commit_msg.clear();
                        self.commit_cursor = 0;
                        self.focus = Focus::Files;
                        self.set_info(summary.clone());
                        let _ = self.refresh();
                        GitUiAction::Committed(summary)
                    }
                    Err(err) => {
                        self.set_err(err);
                        GitUiAction::Stay
                    }
                }
            }
            KeyCode::Left => {
                self.commit_cursor = prev_boundary(&self.commit_msg, self.commit_cursor);
                GitUiAction::Stay
            }
            KeyCode::Right => {
                self.commit_cursor = next_boundary(&self.commit_msg, self.commit_cursor);
                GitUiAction::Stay
            }
            KeyCode::Home => {
                self.commit_cursor = 0;
                GitUiAction::Stay
            }
            KeyCode::End => {
                self.commit_cursor = self.commit_msg.len();
                GitUiAction::Stay
            }
            KeyCode::Backspace => {
                if self.commit_cursor > 0 {
                    let prev = prev_boundary(&self.commit_msg, self.commit_cursor);
                    self.commit_msg.replace_range(prev..self.commit_cursor, "");
                    self.commit_cursor = prev;
                }
                GitUiAction::Stay
            }
            KeyCode::Char('u' | 'U') if ctrl => {
                self.commit_msg.clear();
                self.commit_cursor = 0;
                GitUiAction::Stay
            }
            KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => {
                self.commit_msg.insert(self.commit_cursor, c);
                self.commit_cursor += c.len_utf8();
                GitUiAction::Stay
            }
            _ => GitUiAction::Stay,
        }
    }

    fn reload_diff(&mut self) {
        self.diff_scroll = 0;
        self.diff_text = match self.selected_entry() {
            Some(entry) => diff_for_entry(&self.repo_dir, entry, self.revision.as_deref()),
            None => String::new(),
        };
    }

    fn set_info(&mut self, msg: String) {
        self.status_msg = Some(msg);
        self.status_is_err = false;
    }

    fn set_err(&mut self, msg: String) {
        self.status_msg = Some(msg);
        self.status_is_err = true;
    }
}

/// Run the standalone interactive git workbench (`hrn git [revision]`) on the current terminal.
///
/// # Errors
/// Returns an `io::Error` if opening the repository, terminal raw mode, drawing, or restoring
/// the terminal fails.
pub fn run_git_workbench(
    repo_dir: PathBuf,
    revision: Option<String>,
    mut msg_gen: Option<BoxCommitMsgGenerator>,
) -> io::Result<()> {
    let mut wb = GitWorkbench::open(repo_dir, revision).map_err(io::Error::other)?;
    term::enter(false)?;
    let mut screen = Inline::new(io::stdout(), theme::truecolor());
    let run_res = standalone_loop(&mut screen, &mut wb, &mut msg_gen);
    let cleared = screen.finish();
    let restored = term::leave();
    run_res.and(cleared).and(restored)
}

fn standalone_loop(
    screen: &mut Inline<io::Stdout>,
    wb: &mut GitWorkbench,
    msg_gen: &mut Option<BoxCommitMsgGenerator>,
) -> io::Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            let (width, height) = term::size()?;
            let rect = Rect::new(0, 0, width, height);
            let mut live = Buffer::empty(rect);
            let cursor = wb.draw(rect, &mut live);
            let _ = screen.paint(&Frame {
                reset: false,
                scrollback: &[],
                live: &live,
                cursor,
                cursor_shape: CursorShape::Default,
            })?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let gen_ref = msg_gen
                    .as_mut()
                    .map(|f| f.as_mut() as CommitMsgGenerator<'_>);
                match wb.handle_key(key, gen_ref) {
                    GitUiAction::Close => return Ok(()),
                    GitUiAction::Stay | GitUiAction::Committed(_) => dirty = true,
                }
            }
            Event::Resize(..) => dirty = true,
            _ => {}
        }
    }
}

fn prev_boundary(s: &str, idx: usize) -> usize {
    let c = idx.min(s.len());
    s.get(..c)
        .and_then(|h| h.char_indices().next_back().map(|(i, _)| i))
        .unwrap_or(0)
}

fn next_boundary(s: &str, idx: usize) -> usize {
    let c = idx.min(s.len());
    s.get(c..)
        .and_then(|t| t.chars().next().map(|ch| c + ch.len_utf8()))
        .unwrap_or(s.len())
}
