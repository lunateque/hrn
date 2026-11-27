//! `/commit` modal and `/git [revision]` workbench handlers on [`App`].

use super::App;
use crate::commit_modal::{CommitModal, CommitModalAction};
use crate::git_ui::{GitUiAction, GitWorkbench};
use crate::git_ui_ops::{find_repo_root, validate_revision};
use crate::transcript::Level;
use crossterm::event::KeyEvent;

impl App {
    pub(super) fn handle_commit(&mut self, arg: Option<&str>) {
        if let Some(extra) = arg.map(str::trim).filter(|s| !s.is_empty()) {
            self.notice(
                Level::Warn,
                format!(
                    "unexpected `/commit` argument `{extra}` · use `/commit` to review and confirm"
                ),
            );
            return;
        }
        match self.backend.prepare_commit_preview() {
            Ok(preview) => {
                self.commit_modal = Some(CommitModal::new(preview));
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn handle_git(&mut self, rev: Option<&str>) {
        let rev = rev
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(ref r) = rev
            && let Err(err) = validate_revision(r)
        {
            self.notice(Level::Error, err);
            return;
        }
        let repo_dir = match find_repo_root(&self.cwd) {
            Ok(root) => root,
            Err(err) => {
                self.notice(Level::Error, err);
                return;
            }
        };
        match GitWorkbench::open(repo_dir, rev) {
            Ok(wb) => self.git_ui = Some(wb),
            Err(err) => self.notice(Level::Error, err),
        }
    }

    pub(super) fn commit_modal_key(&mut self, key: KeyEvent) -> bool {
        let Some(modal) = &mut self.commit_modal else {
            return false;
        };
        match modal.key(key) {
            CommitModalAction::Stay => {}
            CommitModalAction::Cancel => {
                self.commit_modal = None;
                self.flash("commit cancelled");
            }
            CommitModalAction::Confirm { message, changelog } => {
                self.commit_modal = None;
                match self.backend.execute_commit(&message, changelog) {
                    Ok(summary) => self.notice(Level::Info, summary),
                    Err(err) => self.notice(Level::Error, err.to_string()),
                }
            }
        }
        true
    }

    pub(super) fn git_ui_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut wb) = self.git_ui.take() else {
            return false;
        };
        let action = {
            let backend = &mut self.backend;
            let mut msg_gen = |staged: &str| {
                backend
                    .generate_commit_message_for_staged(staged)
                    .map_err(|e| e.to_string())
            };
            wb.handle_key(key, Some(&mut msg_gen))
        };
        match action {
            GitUiAction::Stay => {
                self.git_ui = Some(wb);
            }
            GitUiAction::Close => {
                self.flash("closed git workbench");
            }
            GitUiAction::Committed(summary) => {
                self.git_ui = Some(wb);
                self.flash(summary);
            }
        }
        true
    }
}
