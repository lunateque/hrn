//! `/compact [instructions]` (including busy abort-and-resume) and `/shake` handlers on [`App`].

use super::{App, Task};
use crate::transcript::Level;

/// Pending `/compact [instructions]` queued behind an interrupted in-flight turn.
pub(super) struct PendingCompact {
    pub(super) instructions: Option<String>,
    pub(super) resume_prompt: Option<String>,
}

impl App {
    /// `/compact [instructions]`: when idle starts compaction immediately; when a turn is running
    /// interrupts it, compacts once it settles, and resumes the prompt when `auto_continue = true`.
    pub(super) fn handle_compact(&mut self, instructions: Option<&str>) {
        match self.task {
            Task::Idle => self.start_compact(instructions, None),
            Task::Compact => self.notice(Level::Warn, "compaction is already running"),
            Task::Turn { .. } => {
                let resume_prompt = if self.backend.compaction_auto_continue() {
                    self.last_prompt.clone()
                } else {
                    None
                };
                self.pending_compact = Some(PendingCompact {
                    instructions: instructions.map(str::to_owned),
                    resume_prompt,
                });
                self.interrupt();
                self.flash("interrupting turn to compact…");
            }
        }
    }

    /// Launch the background compaction task and remember an optional prompt to resume afterwards.
    pub(super) fn start_compact(
        &mut self,
        instructions: Option<&str>,
        resume_prompt: Option<String>,
    ) {
        match self.backend.compact(instructions) {
            Ok(true) => {
                self.resume_after_compact = resume_prompt;
                self.start_task(Task::Compact);
                self.flash("compacting…");
            }
            Ok(false) => {
                self.resume_after_compact = None;
                self.notice(Level::Info, "nothing to compact yet");
            }
            Err(err) => {
                self.resume_after_compact = None;
                self.notice(Level::Error, err.to_string());
            }
        }
    }

    /// `/shake`: aggressively elide old tool outputs to `local://` without an LLM call.
    pub(super) fn handle_shake(&mut self) {
        if self.is_busy() {
            self.notice(
                Level::Warn,
                "the agent is working · press esc to interrupt first",
            );
            return;
        }
        match self.backend.shake() {
            Ok(true) => {
                self.start_task(Task::Compact);
                self.flash("shaking old tool outputs…");
            }
            Ok(false) => self.notice(Level::Info, "nothing to compact yet"),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }
}
