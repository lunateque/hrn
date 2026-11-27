//! Live per-session control handle: mid-turn `steer`, `followUp` queue, cancellation and the
//! live permission mode.

use crate::runtime::HandoffDraft;
use hrn_kernel::{CancelFlag, HrnError, PermissionMode, Result};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct ControlState {
    steers: VecDeque<String>,
    follow_ups: VecDeque<String>,
    advisor_notes: VecDeque<String>,
    advisor_override: Option<(bool, Option<String>)>,
    active: Option<CancelFlag>,
    permission_mode: Option<PermissionMode>,
    pending_handoff: Option<HandoffDraft>,
}

/// Shared handle used by a front-end to steer, queue follow-ups for, or interrupt a session.
///
/// Clones share state. At most one turn may run per control handle: `begin_turn`
/// refuses a second concurrent turn, which would otherwise race on the DAG head.
#[derive(Clone, Default)]
pub struct SessionControl {
    state: Arc<Mutex<ControlState>>,
    /// The structured result of this session's hidden `yield` tool (subagent runs only); read
    /// by the task executor as the agent's final output.
    yield_data: Arc<Mutex<Option<String>>>,
}

/// Marks a turn as running; clears the active flag when dropped (also on early return or panic).
pub(crate) struct TurnGuard {
    control: SessionControl,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.control.lock().active = None;
    }
}

impl SessionControl {
    fn lock(&self) -> MutexGuard<'_, ControlState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queue a mid-turn steering note injected between the current step and the next LLM call.
    pub fn push_steer(&self, text: String) {
        self.lock().steers.push_back(text);
    }

    /// Queue a follow-up prompt to run automatically when the current turn finishes.
    pub fn push_follow_up(&self, text: String) {
        self.lock().follow_ups.push_back(text);
    }

    /// Pop the next queued follow-up prompt, if any.
    pub fn pop_follow_up(&self) -> Option<String> {
        self.lock().follow_ups.pop_front()
    }

    /// The per-run `yield` result slot handed to every tool call of this session.
    #[must_use]
    pub fn yield_data(&self) -> &Arc<Mutex<Option<String>>> {
        &self.yield_data
    }

    /// Cancel the currently running turn if one is active.
    pub fn interrupt(&self) {
        if let Some(flag) = self.lock().active.as_ref() {
            flag.cancel();
        }
    }

    /// Whether a turn is currently running on this handle.
    pub fn is_running(&self) -> bool {
        self.lock().active.is_some()
    }

    /// Take every queued steer that has not reached the model yet. A front-end calls this after a
    /// turn ended, because a steer sent after the final step would otherwise linger until the next
    /// turn's first step.
    pub fn drain_steers(&self) -> Vec<String> {
        self.lock().steers.drain(..).collect()
    }

    /// Override the session's permission mode. A running turn applies it from its next tool call
    /// on (and persists it), so tightening the mode mid-turn takes effect immediately.
    pub fn set_permission_mode(&self, mode: PermissionMode) {
        self.lock().permission_mode = Some(mode);
    }

    /// The live permission mode override, if one was set.
    pub fn permission_mode(&self) -> Option<PermissionMode> {
        self.lock().permission_mode
    }

    /// Enable or disable the passive advisor (and optionally override its model) for this session.
    pub fn set_advisor(&self, enabled: bool, model: Option<String>) {
        self.lock().advisor_override = Some((enabled, model));
    }

    /// Current live advisor override, if set.
    pub fn advisor_override(&self) -> Option<(bool, Option<String>)> {
        self.lock().advisor_override.clone()
    }

    /// Queue a passive advisor note to be surfaced in the next turn's context.
    pub fn push_advisor_note(&self, note: String) {
        let mut state = self.lock();
        if state.advisor_notes.len() >= 4 {
            state.advisor_notes.pop_front();
        }
        state.advisor_notes.push_back(note);
    }

    /// Take all pending advisor notes queued from earlier turns.
    /// The queued notes without consuming them (the `/context` estimate must not change what
    /// the next turn's prompt says).
    pub fn peek_advisor_notes(&self) -> Vec<String> {
        self.lock().advisor_notes.iter().cloned().collect()
    }

    /// Take the queued notes out (a turn's prompt consumes them).
    pub fn drain_advisor_notes(&self) -> Vec<String> {
        self.lock().advisor_notes.drain(..).collect()
    }

    /// Queue a validated `/handoff` draft to commit at the next tool boundary (or when idle).
    pub fn set_pending_handoff(&self, draft: HandoffDraft) {
        self.lock().pending_handoff = Some(draft);
    }

    /// Take any pending `/handoff` draft awaiting application at a tool boundary.
    pub fn take_pending_handoff(&self) -> Option<HandoffDraft> {
        self.lock().pending_handoff.take()
    }

    pub(crate) fn has_steers(&self) -> bool {
        !self.lock().steers.is_empty()
    }

    pub(crate) fn begin_turn(&self, cancel: CancelFlag) -> Result<TurnGuard> {
        let mut state = self.lock();
        if state.active.is_some() {
            return Err(HrnError::Internal(
                "a turn is already running for this session".into(),
            ));
        }
        state.active = Some(cancel);
        drop(state);
        Ok(TurnGuard {
            control: self.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_turn_is_refused_until_the_first_guard_drops() {
        let control = SessionControl::default();
        let guard = control.begin_turn(CancelFlag::new()).expect("first turn");
        assert!(control.begin_turn(CancelFlag::new()).is_err());
        assert!(control.is_running());
        drop(guard);
        assert!(control.begin_turn(CancelFlag::new()).is_ok());
    }

    #[test]
    fn interrupt_cancels_the_active_turn_only() {
        let control = SessionControl::default();
        control.interrupt();
        let flag = CancelFlag::new();
        let _guard = control.begin_turn(flag.clone()).expect("turn");
        control.interrupt();
        assert!(flag.is_cancelled());
    }

    #[test]
    fn unsent_steers_can_be_taken_back_and_the_mode_overridden() {
        let control = SessionControl::default();
        assert_eq!(control.permission_mode(), None);
        control.push_steer("a".into());
        control.push_steer("b".into());
        assert_eq!(control.drain_steers(), ["a", "b"]);
        assert!(!control.has_steers());
        control.set_permission_mode(PermissionMode::AutoEdit);
        assert_eq!(control.permission_mode(), Some(PermissionMode::AutoEdit));
    }
}
