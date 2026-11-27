//! Background work: the indicator in the status band, and the follow-up turn that delivers
//! results once the session is idle.

use super::{App, Task};
use crate::port::AgentHubSnapshot;
use crate::transcript::Level;

/// What the UI knows about the session's background work.
#[derive(Default)]
pub(super) struct Background {
    /// Ids of the agents, jobs and services still running.
    pub(super) running: Vec<String>,
    /// Live Agent Hub snapshot (roster + activity log) for status band, jump block, and `Alt+A`.
    pub(super) hub: AgentHubSnapshot,
    /// The user interrupted the last turn: results wait for their next prompt instead of
    /// restarting the model on their own.
    pub(super) held: bool,
}

impl App {
    /// Start a turn from the results waiting in the session inbox, when nothing stands in the
    /// way: no task runs, no overlay or question is open, and the user did not just interrupt.
    /// A busy session picks them up at its next turn boundary instead.
    pub(super) fn deliver_inbox(&mut self) {
        if !matches!(self.task, Task::Idle)
            || self.quit
            || self.background.held
            || self.picker.is_some()
            || self.tree.is_some()
            || self.login.is_some()
            || self.agent_hub.is_some()
            || self.agents_hub.is_some()
            || self.focused_agent.is_some()
        {
            return;
        }
        match self.backend.start_inbox_turn() {
            Ok(Some(summary)) => {
                self.notice(Level::Info, summary);
                self.start_task(Task::Turn {
                    awaiting_start: true,
                });
            }
            Ok(None) => {}
            Err(err) => {
                self.background.held = true;
                self.notice(Level::Error, format!("background results: {err}"));
            }
        }
    }
}
