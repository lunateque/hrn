//! Tool-call approvals: queue management, 500 ms guard lifecycle, mode settlement, and
//! decision dispatch (`Once`, `File`, `Directory`, `ToolSession`, `Persistent`, `Deny`,
//! `DenyAndSend`).

use super::App;
use crate::approvals::{
    ApprovalDock, DockAction, DockChoice, choices_for, handle_key, is_plain_only,
};
use crate::port::ApprovalAsk;
use crate::transcript::{Entry, SteerState};
use crossterm::event::KeyEvent;
use hrn_kernel::{ApprovalDecision, ApprovalRequest, PermissionMode, ToolTier};

impl App {
    /// Whether the UI-local session `always` map covers `request` at its required risk tier.
    fn always_covers(&self, request: &ApprovalRequest) -> bool {
        let req_tier = request.tier.unwrap_or(ToolTier::Read);
        self.always
            .get(&request.tool_name)
            .is_some_and(|granted| granted.covers(req_tier))
    }

    /// Whether an arriving ask is already covered by a UI-local session "always" grant.
    /// Plain-only prompts (restricted folders, critical rules, explicit `[tools.approval] prompt`)
    /// are NEVER auto-approved here.
    fn auto_approves_on_arrival(&self, ask: &ApprovalAsk) -> bool {
        !is_plain_only(&ask.request) && self.always_covers(&ask.request)
    }

    /// Whether a mode change or new "always" grant now covers an already-waiting ask.
    fn auto_approves_on_settle(&self, ask: &ApprovalAsk) -> bool {
        if is_plain_only(&ask.request) {
            return false;
        }
        let tool = ask.request.tool_name.as_str();
        self.always_covers(&ask.request)
            || match self.settings.mode {
                PermissionMode::Yolo => true,
                PermissionMode::AutoEdit => ask.request.tier.map_or_else(
                    || matches!(tool, "edit" | "write" | "apply_patch"),
                    |t| t != ToolTier::Exec,
                ),
                PermissionMode::Default | PermissionMode::AskCached | PermissionMode::Plan => false,
            }
    }

    pub(super) fn on_approval(&mut self, ask: ApprovalAsk) {
        if self.quit {
            return;
        }
        if self.auto_approves_on_arrival(&ask) {
            ask.answer(ApprovalDecision::tool_session());
            return;
        }
        let was_empty = self.approvals.is_empty();
        self.approvals.push_back(ask);
        if was_empty {
            self.open_front_dock();
        }
        self.announce_input_needed();
    }

    /// Answer queued asks that the current mode or grants now cover (never while the front dock's
    /// 500 ms accidental-press guard is still active).
    pub(super) fn settle_approvals(&mut self) {
        let now = self.now();
        if self
            .approval_dock
            .as_ref()
            .is_some_and(|dock| dock.is_guarded(now))
        {
            return;
        }
        let mut front_changed = false;
        let old_front = self.approvals.front().map(|a| a.request.id.clone());
        for ask in std::mem::take(&mut self.approvals) {
            if self.auto_approves_on_settle(&ask) {
                let decision = if self.always_covers(&ask.request) {
                    ApprovalDecision::tool_session()
                } else {
                    ApprovalDecision::once()
                };
                ask.answer(decision);
            } else {
                self.approvals.push_back(ask);
            }
        }
        let new_front = self.approvals.front().map(|a| a.request.id.clone());
        if new_front != old_front {
            front_changed = true;
        }
        if self.approvals.is_empty() {
            self.approval_dock = None;
        } else if front_changed {
            self.open_front_dock();
        }
    }

    /// Route a key press to the active approval dock; `true` if consumed.
    pub(super) fn on_approval_key(&mut self, key: KeyEvent) -> bool {
        let Some(front) = self.approvals.front() else {
            return false;
        };
        let has_draft = !self.editor.is_blank();
        let choices = choices_for(&front.request, self.settings.mode, has_draft);
        let now = self.now();
        let dock = self
            .approval_dock
            .get_or_insert_with(|| ApprovalDock::open(now, &choices, has_draft));
        match handle_key(dock, key, &self.keymap, &choices, now) {
            DockAction::Ignored => false,
            DockAction::Consumed => true,
            DockAction::Decide(choice) => {
                self.apply_approval_choice(choice);
                true
            }
        }
    }

    fn apply_approval_choice(&mut self, choice: DockChoice) {
        let Some(ask) = self.approvals.pop_front() else {
            self.approval_dock = None;
            return;
        };
        match choice {
            DockChoice::DenyAndSend => {
                let draft = self.editor.take();
                if !draft.trim().is_empty() {
                    self.transcript.push(Entry::Steer {
                        text: draft.clone(),
                        state: SteerState::Queued,
                    });
                }
                ask.answer(choice.into_decision(Some(draft)));
            }
            DockChoice::ToolSession => {
                if !is_plain_only(&ask.request) {
                    let tier = ask.request.tier.unwrap_or(ToolTier::Read);
                    let next = self
                        .always
                        .get(&ask.request.tool_name)
                        .map_or(tier, |prev| prev.stronger(tier));
                    self.always.insert(ask.request.tool_name.clone(), next);
                }
                ask.answer(choice.into_decision(None));
                // Temporarily drop the old front dock so remaining queued asks covered by the
                // just-committed session grant settle cleanly.
                self.approval_dock = None;
                self.settle_approvals();
            }
            other => {
                ask.answer(other.into_decision(None));
            }
        }
        if self.approvals.is_empty() {
            self.approval_dock = None;
        } else {
            self.open_front_dock();
        }
    }

    pub(super) fn open_front_dock(&mut self) {
        let Some(front) = self.approvals.front() else {
            self.approval_dock = None;
            return;
        };
        let has_draft = !self.editor.is_blank();
        let choices = choices_for(&front.request, self.settings.mode, has_draft);
        self.approval_dock = Some(ApprovalDock::open(self.now(), &choices, has_draft));
    }
}
