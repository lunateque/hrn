//! Running a session until it is settled: its own turn, then whatever steering and background
//! results arrive, until nothing is left in flight. Used by `hrn exec` and by background agents.

use crate::control::SessionControl;
use crate::runtime::{AgentRuntime, TurnIo, TurnResult};
use hrn_kernel::{CancelFlag, HrnError, Inbox, Result, format_inbox};
use std::pin::Pin;
use std::task::Poll;

/// Wait until `inbox` changed after `seen`, or `cancel` trips. `true` means cancelled.
pub(crate) async fn wait_change(inbox: &Inbox, seen: u64, cancel: &CancelFlag) -> bool {
    let mut changed = inbox.changed(seen);
    let mut cancelled = cancel.cancelled();
    std::future::poll_fn(|cx| {
        if Pin::new(&mut cancelled).poll(cx).is_ready() {
            Poll::Ready(true)
        } else if Pin::new(&mut changed).poll(cx).is_ready() {
            Poll::Ready(false)
        } else {
            Poll::Pending
        }
    })
    .await
}

fn absorb(total: &mut TurnResult, next: TurnResult) {
    total.steps += next.steps;
    if let Some(usage) = next.usage {
        total
            .usage
            .get_or_insert_with(Default::default)
            .accumulate(usage);
    }
    if !next.final_text.trim().is_empty() {
        total.final_text = next.final_text;
    }
    total.head_node_id = next.head_node_id;
    total.model = next.model;
}

impl AgentRuntime {
    /// Run `prompt`, then keep going while there is more to do for `session_id`: steering that
    /// missed the last step and results delivered by its background agents become follow-up
    /// turns, and the call waits while agents it started are still running. It returns once the
    /// session is idle with nothing in flight.
    ///
    /// # Errors
    /// The failure of any turn; [`HrnError::Cancelled`] also when `io.cancel` trips while waiting.
    pub async fn run_turn_settled(
        &self,
        session_id: &str,
        prompt: &str,
        control: &SessionControl,
        io: TurnIo,
    ) -> Result<TurnResult> {
        let inbox = self.agents.inbox();
        let mut total = self
            .run_turn(session_id, prompt, control, io.clone())
            .await?;
        loop {
            // The version is read first, so anything delivered after this point wakes the wait.
            let seen = inbox.version();
            // `pending` before `take`: a delivery lowers it only together with queueing its item.
            let pending = inbox.pending(session_id);
            let mut next = control.drain_steers();
            // A goal-continuation turn the last turn queued (goal mode).
            if let Some(follow) = control.pop_follow_up() {
                next.push(follow);
            }
            let items = inbox.take(session_id);
            if !items.is_empty() {
                next.push(format_inbox(&items));
            }
            if !next.is_empty() {
                let done = self
                    .run_turn(session_id, &next.join("\n\n"), control, io.clone())
                    .await?;
                absorb(&mut total, done);
            } else if pending == 0 {
                return Ok(total);
            } else if wait_change(inbox, seen, &io.cancel).await {
                return Err(HrnError::Cancelled);
            }
        }
    }
}
