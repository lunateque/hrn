//! The `ask` forms queued behind the one on screen, and the timeout that submits an untouched one.

use super::App;
use crate::ask::AskPanel;
use crate::port::AskPrompt;
use hrn_kernel::Submission;
use std::time::Instant;

impl App {
    pub(super) fn on_ask(&mut self, prompt: AskPrompt) {
        if self.quit {
            return;
        }
        self.ask_queue.push_back(prompt);
        self.advance_ask();
    }

    /// Open the next queued form when none is showing. A form with nothing to answer is
    /// submitted as it stands.
    pub(super) fn advance_ask(&mut self) {
        while self.active_ask.is_none() {
            let Some(next) = self.ask_queue.pop_front() else {
                break;
            };
            let form = next.request.form.clone();
            if let Some(panel) = AskPanel::new(form, self.ask_timeout, Instant::now()) {
                self.active_ask = Some((panel, next));
                self.announce_input_needed();
            } else {
                let values = next.request.form.values(&next.request.form.defaults());
                next.answer(Some(Submission {
                    values,
                    timed_out: false,
                }));
            }
        }
    }

    /// The open form shows a countdown that needs a repaint every second.
    pub(super) fn active_ask_counts_down(&self) -> bool {
        self.active_ask
            .as_ref()
            .is_some_and(|(panel, _)| panel.deadline().is_some())
    }

    /// The open form's deadline passed: submit what it holds.
    pub(super) fn expire_ask(&mut self, now: Instant) -> bool {
        let expired = self
            .active_ask
            .as_ref()
            .and_then(|(panel, _)| panel.deadline())
            .is_some_and(|at| now >= at);
        if !expired {
            return false;
        }
        if let Some((panel, prompt)) = self.active_ask.take() {
            prompt.answer(Some(panel.timed_out()));
        }
        self.advance_ask();
        true
    }
}
