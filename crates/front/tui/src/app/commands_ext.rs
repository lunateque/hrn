//! Wave 11c slash command handlers: `/wt` (`/worktree`), `/memory`, `/init`, `/trace`, `/dump`.

use super::App;
use crate::git;
use crate::trace_view;
use crate::transcript::Level;
use std::fmt::Write as _;
use std::path::Path;

impl App {
    /// `/wt [branch]` (alias `/worktree`): create a worktree carrying uncommitted changes and
    /// relocate the live session. Refuses while streaming (also enforced by `streaming_guard`).
    pub(super) fn handle_wt(&mut self, branch: Option<&str>) {
        if self.is_busy() {
            self.notice(
                Level::Warn,
                "cannot run /wt while a turn is running — wait for it to finish or press Esc to interrupt",
            );
            return;
        }
        match self.backend.relocate_worktree(branch) {
            Ok(outcome) => {
                self.cwd.clone_from(&outcome.new_cwd);
                self.cwd_label = git::display_path(&self.cwd);
                self.refresh_git();
                if self.branch.is_none() {
                    self.branch = Some(outcome.branch.clone());
                }
                let mut msg = format!(
                    "Relocated session to worktree {}\nBranch: {}\nCarried changes: {} tracked, {} untracked\nPrevious checkout: {}",
                    outcome.new_cwd.display(),
                    outcome.branch,
                    outcome.carried_tracked,
                    outcome.carried_untracked,
                    outcome.previous_checkout,
                );
                for warn in &outcome.warnings {
                    let _ = write!(msg, "\nWarning: {warn}");
                }
                self.notice(Level::Info, msg);
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// `/memory [view|stats|search|enqueue|clear] [arg]`.
    pub(super) fn handle_memory(&mut self, arg: Option<&str>) {
        match self.backend.memory_command(arg) {
            Ok(text) => self.notice(Level::Info, text),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// `/init`: submit the bundled `AGENTS.md` initialization/refresh task prompt.
    pub(super) fn handle_init(&mut self) {
        if self.is_busy() {
            self.notice(
                Level::Warn,
                "cannot run /init while a turn is running — wait for it to finish or press Esc to interrupt",
            );
            return;
        }
        let prompt = self.backend.init_prompt();
        self.send_prompt(&prompt, Some("/init".to_owned()));
    }

    /// `/trace [export <file>]`: show per-turn span table or export spans to JSON/Markdown.
    pub(super) fn handle_trace(&mut self, arg: Option<&str>) {
        let records = match self.backend.traces() {
            Ok(r) => r,
            Err(err) => {
                self.notice(Level::Error, err.to_string());
                return;
            }
        };
        let trimmed = arg.map(str::trim).unwrap_or_default();
        if trimmed.is_empty() {
            let table = trace_view::format_trace_table(&records);
            self.open_view("trace", &table);
            return;
        }
        if let Some(rest) = trimmed
            .strip_prefix("export")
            .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
        {
            match trace_view::export_traces(&records, &self.cwd, rest.trim()) {
                Ok(path) => self.notice(
                    Level::Info,
                    format!(
                        "exported {} trace span(s) to {}",
                        records.len(),
                        path.display()
                    ),
                ),
                Err(err) => self.notice(Level::Error, err),
            }
            return;
        }
        self.notice(
            Level::Warn,
            format!("unknown /trace argument `{trimmed}` · use `/trace` or `/trace export <file>`"),
        );
    }

    /// `/dump [file]`: dump plain-text transcript to OS clipboard or write it to `<file>`.
    pub(super) fn handle_dump(&mut self, arg: Option<&str>) {
        let text = match self.backend.dump_transcript() {
            Ok(text) => text,
            Err(err) => {
                self.notice(Level::Error, err.to_string());
                return;
            }
        };
        let lines = text.lines().count();
        let chars = text.chars().count();
        if let Some(raw_path) = arg.map(str::trim).filter(|s| !s.is_empty()) {
            let clean = raw_path.trim_matches('"').trim_matches('\'');
            if clean.is_empty() || clean.contains('\0') {
                self.notice(Level::Error, "invalid output file path for /dump");
                return;
            }
            let p = Path::new(clean);
            let target = if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.cwd.join(p)
            };
            if let Some(parent) = target.parent()
                && !parent.as_os_str().is_empty()
                && let Err(err) = std::fs::create_dir_all(parent)
            {
                self.notice(
                    Level::Error,
                    format!("create `{}`: {err}", parent.display()),
                );
                return;
            }
            match std::fs::write(&target, &text) {
                Ok(()) => self.notice(
                    Level::Info,
                    format!(
                        "dumped plain-text transcript ({lines} lines, {chars} chars) to {}",
                        target.display()
                    ),
                ),
                Err(err) => {
                    self.notice(Level::Error, format!("write `{}`: {err}", target.display()));
                }
            }
            return;
        }
        if self.io.raw_clipboard.write_text(&text) {
            self.notice(
                Level::Info,
                format!("copied plain-text transcript to clipboard ({lines} lines, {chars} chars)"),
            );
        } else {
            self.notice(
                Level::Error,
                "clipboard unavailable · pass a file path: /dump <file>",
            );
        }
    }

    /// `/review [staged|base|<focus>]`: submit a structured P0-P3 code-review prompt.
    pub(super) fn handle_review(&mut self, arg: Option<&str>) {
        if self.is_busy() {
            self.notice(
                Level::Warn,
                "cannot run /review while a turn is running — wait for it to finish or press Esc to interrupt",
            );
            return;
        }
        let prompt = crate::slash::review_prompt(arg);
        let shown = match arg.map(str::trim).filter(|s| !s.is_empty()) {
            Some(target) => format!("/review {target}"),
            None => "/review".to_owned(),
        };
        self.send_prompt(&prompt, Some(shown));
    }

    /// `/vibe [on|off|status]`: toggle low-friction `auto-edit` coding mode (never escalates to yolo).
    pub(super) fn handle_vibe(&mut self, arg: Option<&str>) {
        use hrn_kernel::PermissionMode;
        let trimmed = arg.map(str::trim).unwrap_or_default();
        if trimmed.eq_ignore_ascii_case("status") {
            let state = if self.settings.mode == PermissionMode::AutoEdit {
                "on (auto-edit)"
            } else {
                "off"
            };
            self.notice(
                Level::Info,
                format!(
                    "vibe: {state} · current mode: {}",
                    self.settings.mode.as_str()
                ),
            );
            return;
        }
        let enable = if trimmed.is_empty() {
            self.settings.mode != PermissionMode::AutoEdit
        } else if trimmed.eq_ignore_ascii_case("on") {
            true
        } else if trimmed.eq_ignore_ascii_case("off") {
            false
        } else {
            self.notice(
                Level::Warn,
                format!("unknown /vibe argument `{trimmed}` · use `/vibe [on|off|status]`"),
            );
            return;
        };
        self.settings.mode = if enable {
            PermissionMode::AutoEdit
        } else {
            PermissionMode::Default
        };
        self.backend.apply_settings(&self.settings);
        self.settle_approvals();
        let label = if enable {
            "vibe → on (auto-edit: file edits auto, shell still asks)"
        } else {
            "vibe → off (default mode)"
        };
        self.flash(label);
    }

    /// `/agy [status|[--model M] [--effort E] [--mode MODE] [--worktree] <prompt>]`.
    pub(super) fn handle_agy(&mut self, arg: Option<&str>) {
        match self.backend.agy_command(arg) {
            Ok(text) if !text.trim().is_empty() => self.notice(Level::Info, text),
            Ok(_) => {}
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }
}
