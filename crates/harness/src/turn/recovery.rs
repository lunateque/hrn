//! Context-overflow recovery, context-window promotion, `FinishReason::Length` recovery, and
//! post-turn maintenance for [`AgentRuntime`].

use super::TurnState;
use super::step_tail::TurnContext;
use crate::compaction::{CompactionPolicy, project_branch};
use crate::runtime::CompactionReason;
use crate::runtime::{AgentRuntime, TurnIo, emit};
use hrn_kernel::{
    AgentEvent, AgentStatus, HrnError, LevelSuffix, Result, SessionMeta, split_level_suffix,
};

pub(super) const LENGTH_CONTINUE_NUDGE: &str = "[system] Your previous response was cut off by the output limit. Continue from where you stopped.";

impl AgentRuntime {
    /// Promote `meta` to a strictly larger `context_promotion_target` when `[llm] context_promotion`
    /// is enabled and a valid target is available. Never mutates `[roles]`.
    pub(super) async fn try_promote_model(
        &self,
        meta: &mut SessionMeta,
        policy: &mut CompactionPolicy,
        st: &mut TurnState,
        io: &TurnIo,
    ) -> Result<bool> {
        if !self.config.context_promotion || st.recovery.promoted() {
            return Ok(false);
        }
        let Some(target) = self.llm.context_promotion_target(meta.model.as_deref()) else {
            return Ok(false);
        };
        let (base, suffix) = split_level_suffix(&target);
        let thinking = match suffix {
            LevelSuffix::Level(lvl) => Some(lvl),
            LevelSuffix::None | LevelSuffix::Inherit => None,
        };
        let updated = self.switch_model_temporary(
            &meta.id,
            &target,
            base.trim(),
            thinking,
            Some("fallback"),
        )?;
        *meta = updated;
        let overhead = policy.fixed_overhead_tokens;
        *policy = self.policy_for(meta.model.as_deref());
        policy.fixed_overhead_tokens = overhead;
        st.recovery.set_promoted();
        st.recovery.set_just_recovered();
        emit(
            &io.events,
            AgentEvent::StatusChanged {
                session_id: meta.id.clone(),
                status: AgentStatus::Working,
                message: Some(format!("context promoted -> {target}")),
            },
        )
        .await;
        Ok(true)
    }

    /// Recover from a context-overflow error returned by `call_llm`:
    /// 1. Try context promotion first (skipping compaction if a larger target exists).
    /// 2. Otherwise run the compaction walk once and allow one retry without duplicating input.
    pub(super) async fn recover_from_overflow(
        &self,
        meta: &mut SessionMeta,
        ctx: &mut TurnContext,
        io: &TurnIo,
    ) -> Result<bool> {
        if self
            .try_promote_model(meta, &mut ctx.policy, &mut ctx.st, io)
            .await?
        {
            return Ok(true);
        }
        if ctx.st.recovery.overflow_retried() || !ctx.policy.config.enabled {
            return Ok(false);
        }
        ctx.st.recovery.set_overflow_retried();
        match self
            .compact_branch_walk(
                meta,
                &ctx.policy,
                CompactionReason::Overflow,
                None,
                &io.cancel,
                &io.events,
            )
            .await
        {
            Ok(outcome) if outcome.committed.is_some() && !outcome.no_progress => {
                ctx.st.last_prompt_tokens = 0;
                ctx.st.recovery.set_just_recovered();
                Ok(true)
            }
            Ok(_) => {
                ctx.st.recovery.set_gave_up();
                Ok(false)
            }
            Err(HrnError::Cancelled) => Err(HrnError::Cancelled),
            Err(err) => {
                ctx.st.recovery.set_gave_up();
                emit(
                    &io.events,
                    AgentEvent::StatusChanged {
                        session_id: meta.id.clone(),
                        status: AgentStatus::Working,
                        message: Some(format!("overflow compaction failed: {err}")),
                    },
                )
                .await;
                Ok(false)
            }
        }
    }

    /// Recover when the model stops with `FinishReason::Length`. Returns `true` when recovery
    /// succeeded and `auto_continue` allows retrying/continuing the step.
    pub(super) async fn recover_from_length_stop(
        &self,
        meta: &mut SessionMeta,
        ctx: &mut TurnContext,
        io: &TurnIo,
    ) -> Result<bool> {
        if !ctx.policy.config.enabled || ctx.st.recovery.length_recovered() {
            return Ok(false);
        }
        ctx.st.recovery.set_length_recovered();
        if self
            .try_promote_model(meta, &mut ctx.policy, &mut ctx.st, io)
            .await?
        {
            return Ok(ctx.policy.config.auto_continue);
        }
        match self
            .compact_branch_walk(
                meta,
                &ctx.policy,
                CompactionReason::IncompleteOutput,
                None,
                &io.cancel,
                &io.events,
            )
            .await
        {
            Ok(outcome) if outcome.committed.is_some() => {
                ctx.st.last_prompt_tokens = 0;
                ctx.st.recovery.set_just_recovered();
                Ok(outcome.allow_auto_continue)
            }
            Ok(outcome) => {
                if outcome.no_progress {
                    ctx.st.recovery.set_gave_up();
                }
                Ok(false)
            }
            Err(HrnError::Cancelled) => Err(HrnError::Cancelled),
            Err(err) => {
                ctx.st.recovery.set_gave_up();
                emit(
                    &io.events,
                    AgentEvent::StatusChanged {
                        session_id: meta.id.clone(),
                        status: AgentStatus::Working,
                        message: Some(format!("incomplete-output compaction failed: {err}")),
                    },
                )
                .await;
                Ok(false)
            }
        }
    }

    /// Post-turn compaction / promotion maintenance (handles deferred compaction when
    /// `mid_turn_enabled = false`, as well as context crossing threshold on the final reply).
    pub(super) async fn post_turn_maintenance(
        &self,
        meta: &mut SessionMeta,
        policy: &mut CompactionPolicy,
        st: &mut TurnState,
        io: &TurnIo,
    ) -> Result {
        if !policy.config.enabled || st.recovery.gave_up() {
            return Ok(());
        }
        let branch = self.load_head_branch(meta)?;
        let projected = project_branch(&branch, policy);
        let trigger = policy.trigger_tokens();
        let effective_tokens = projected.estimated_tokens.max(st.last_prompt_tokens);
        let due = st.recovery.deferred()
            || projected.needs_tier3_summary
            || (trigger > 0 && st.last_prompt_tokens >= trigger);
        if !due {
            self.maybe_arm_speculative(meta, &branch, policy, effective_tokens, &io.events);
            return Ok(());
        }
        if self.try_promote_model(meta, policy, st, io).await? {
            let reprojected = project_branch(&self.load_head_branch(meta)?, policy);
            if !reprojected.needs_tier3_summary {
                return Ok(());
            }
        }
        if self
            .try_commit_speculative(meta, &branch, policy, &io.events)
            .await?
            .is_some()
        {
            return Ok(());
        }
        match self
            .compact_branch_walk(
                meta,
                policy,
                CompactionReason::Threshold,
                None,
                &io.cancel,
                &io.events,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(HrnError::Cancelled) => Err(HrnError::Cancelled),
            Err(err) => {
                emit(
                    &io.events,
                    AgentEvent::StatusChanged {
                        session_id: meta.id.clone(),
                        status: AgentStatus::Working,
                        message: Some(format!("post-turn compaction failed: {err}")),
                    },
                )
                .await;
                Ok(())
            }
        }
    }
}
