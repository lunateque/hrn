//! `/queue [message]`, FIFO follow-up queue management (`Alt+Up` dequeue), and the mid-turn
//! slash command streaming guard.

use super::App;
use crate::slash::Command;
use crate::transcript::Level;
use std::fmt::Write as _;

impl App {
    /// Return a streaming-guard error message when `command` mutates session state and must not
    /// run while a turn or compaction is active. Read-only builtins return `None`.
    pub(super) fn streaming_guard(&self, command: &Command) -> Option<String> {
        if !self.is_busy() {
            return None;
        }
        let blocked = match command {
            Command::New
            | Command::Sessions
            | Command::Tree
            | Command::Branch
            | Command::Fork(_)
            | Command::Wt(_)
            | Command::Shake
            | Command::Login(_)
            | Command::Logout(_)
            | Command::Setup
            | Command::Init
            | Command::Rename(_)
            | Command::Delete(_)
            | Command::Commit(_)
            | Command::Review(_)
            | Command::Retry => true,
            Command::Memory(Some(arg)) => {
                let sub = arg
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                matches!(sub.as_str(), "clear" | "enqueue")
            }
            Command::Todo(Some(arg)) => arg.trim().eq_ignore_ascii_case("edit"),
            _ => false,
        };
        blocked.then(|| {
            format!(
                "cannot run /{} while a turn is running — wait for it to finish or press Esc to interrupt",
                command.name()
            )
        })
    }

    /// `/queue [message]`: bare lists queued prompts; with `<message>` enqueues while busy or
    /// submits immediately while idle.
    pub(super) fn handle_queue(&mut self, arg: Option<&str>) {
        let Some(msg) = arg.map(str::trim).filter(|s| !s.is_empty()) else {
            self.list_queue();
            return;
        };
        if self.is_busy() {
            self.enqueue_prompt(msg);
        } else {
            self.send_prompt(msg, None);
        }
    }

    /// Push `text` onto the back of the FIFO follow-up queue and render a queued card.
    pub(super) fn enqueue_prompt(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let item = trimmed.to_owned();
        self.followups.push_back(item.clone());
        self.transcript.follow_up(&item);
        let pos = self.followups.len();
        self.flash(format!(
            "queued #{pos} after this turn · alt+up takes it back"
        ));
    }

    /// Bare `/queue`: display numbered queued prompts or `(queue is empty)`.
    pub(super) fn list_queue(&mut self) {
        if self.followups.is_empty() {
            self.notice(Level::Info, "(queue is empty)");
            return;
        }
        let mut out = format!("Queued prompts ({}):\n", self.followups.len());
        for (idx, item) in self.followups.iter().enumerate() {
            let _ = writeln!(out, "{}. {}", idx + 1, crate::toolfmt::one_line(item));
        }
        out.push_str("Press Alt+Up to dequeue the newest item back into the editor.");
        self.notice(Level::Info, out);
    }

    /// Queue the current editor draft as a follow-up (`Ctrl+Enter` while busy).
    pub(super) fn queue_follow_up(&mut self) {
        let text = self.editor.take();
        if text.trim().is_empty() {
            return;
        }
        self.enqueue_prompt(&text);
    }

    /// `Alt+Up`: pop the newest queued follow-up back into the editor so the user can edit or cancel it.
    pub(super) fn dequeue_follow_up(&mut self) -> bool {
        match self.followups.pop_back() {
            Some(text) => {
                self.transcript.drop_follow_up(&text);
                let current = self.editor.take();
                let merged = if current.trim().is_empty() {
                    text
                } else {
                    format!("{text}\n{current}")
                };
                self.editor.set(&merged);
                true
            }
            None => false,
        }
    }

    /// Drain the front item of the FIFO queue after a turn or compaction finishes.
    /// Returns `true` when an item was dispatched.
    pub(super) fn drain_next_queued(&mut self) -> bool {
        let Some(next) = self.followups.pop_front() else {
            return false;
        };
        self.transcript.settle_follow_up(&next);
        if let Some(Ok(cmd @ (Command::Compact(_) | Command::Shake))) = crate::slash::parse(&next) {
            self.run_command(cmd);
        } else {
            self.send_prompt(&next, None);
        }
        true
    }
}
