//! The goal-mode and thinking-resolution steps of the turn loop: hidden-tool activation, the
//! per-turn `auto` ladder, budget accounting, and applying the `goal` tool's requests. Only the
//! runtime owns goal state, so these stay off the step loop itself.

use super::TurnState;
use crate::runtime::{AgentRuntime, TurnIo, emit};
use hrn_kernel::{
    AgentEvent, GoalStatus, Result, SessionEffect, SessionMeta, ThinkingLevel, ToolOutput, Usage,
};

impl AgentRuntime {
    /// The hidden tools the session's state activates for the model.
    pub(super) fn hidden_names(&self, meta: &SessionMeta) -> Vec<String> {
        let mut names = Vec::new();
        if self.tools.get("think").is_some() {
            names.push("think".to_owned());
        }
        if self.tools.get("yield").is_some() && meta.parent_session_id.is_some() {
            names.push("yield".to_owned());
        }
        if self.tools.get("goal").is_some() && meta.goal.is_some() {
            names.push("goal".to_owned());
        }
        names
    }

    /// The thinking level this turn's steps request: the keyword-pinned one, else the session
    /// level; `auto` is resolved once per turn from the keywords (ultrathink forces max tier,
    /// chillax already pins) then a cheap classifier call.
    pub(super) async fn resolve_thinking(
        &self,
        meta: &SessionMeta,
        prompt: &str,
        st: &mut TurnState,
    ) -> Result<ThinkingLevel> {
        let wanted = st.pinned_thinking.unwrap_or(meta.thinking);
        if wanted != ThinkingLevel::Auto {
            return Ok(wanted);
        }
        if let Some(level) = st.resolved_auto {
            return Ok(level);
        }
        let level = self.classify_auto(prompt, meta.model.as_deref()).await;
        st.resolved_auto = Some(level);
        Ok(level)
    }

    /// Charge one step's usage to the live goal and flip it to `budget-limited` at the
    /// boundary (omp's goal accounting; cached prompt reads are not new work).
    pub(super) async fn account_goal(
        &self,
        meta: &mut SessionMeta,
        usage: Option<Usage>,
        control: &crate::SessionControl,
        io: &TurnIo,
    ) -> Result {
        let Some(goal) = meta.goal.as_mut() else {
            return Ok(());
        };
        if !goal.status.is_accounting() {
            return Ok(());
        }
        let now = self.clock.now_ms();
        if let Some(usage) = usage {
            goal.tokens_used = goal
                .tokens_used
                .saturating_add(crate::goal::chargeable_tokens(
                    usage.prompt_tokens,
                    usage.cache_read_tokens,
                    usage.completion_tokens,
                ));
        }
        goal.time_used_seconds = goal.time_used_seconds.saturating_add(
            u64::try_from((now - goal.updated_at_ms).max(0) / 1000).unwrap_or(u64::MAX),
        );
        goal.updated_at_ms = now;
        let limited =
            if crate::goal::budget_exhausted(goal, now) && goal.status == GoalStatus::Active {
                goal.status = GoalStatus::BudgetLimited;
                // One wrap-up turn carrying the limit instructions; no further continuations
                // (queued as a follow-up so it never interleaves with tool results).
                Some(crate::goal::budget_limit_prompt(goal))
            } else {
                None
            };
        // The accounting change is worth showing on the status band right away.
        let snapshot = meta.goal.clone();
        if let Some(text) = limited {
            control.push_follow_up(text);
        }
        emit(&io.events, AgentEvent::GoalUpdated { goal: snapshot }).await;
        Ok(())
    }

    /// Apply the hidden `goal` tool's requests straight to the session's goal (only the runtime
    /// owns goal state), replacing the placeholder output with the real result text.
    pub(super) async fn apply_goal_effects(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        outputs: &mut [ToolOutput],
        io: &TurnIo,
    ) -> Result {
        let _ = turn_id;
        for out in outputs
            .iter_mut()
            .filter(|out| matches!(&out.session_effect, Some(SessionEffect::Goal { .. })))
        {
            let Some(SessionEffect::Goal {
                op,
                objective,
                token_budget,
                time_budget_secs,
            }) = out.session_effect.take()
            else {
                continue;
            };
            let now = self.clock.now_ms();
            match crate::goal::apply_op(
                &mut meta.goal,
                op,
                objective.as_deref(),
                token_budget,
                time_budget_secs,
                now,
            ) {
                Ok((text, _cleared)) => {
                    out.content = text;
                    if meta
                        .goal
                        .as_ref()
                        .is_some_and(|g| g.status == GoalStatus::Dropped)
                    {
                        meta.goal = None;
                    }
                    emit(
                        &io.events,
                        AgentEvent::GoalUpdated {
                            goal: meta.goal.clone(),
                        },
                    )
                    .await;
                }
                Err(message) => *out = ToolOutput::err(message),
            }
        }
        Ok(())
    }
}
