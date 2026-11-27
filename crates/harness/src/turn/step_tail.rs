//! Shared step loop tail for `drive_turn` and `restore_turn`.
//!
//! Keeping the setup, LLM step loop, and tool-batch execution here lets crash restore re-drive
//! either an interrupted tool batch or the follow-up LLM step using the exact same hook,
//! compaction, steering, and goal-accounting machinery as a fresh turn.

use super::recovery::LENGTH_CONTINUE_NUDGE;
use super::{
    DrivenTurn, EMPTY_RESPONSE_NUDGE, StepEnv, TRUNCATED_CALL_ERROR, TurnState,
    empty_response_error, system_for_mode,
};
use crate::batch::{BatchEnv, execute_tool_batch};
use crate::compaction::CompactionPolicy;
use crate::control::SessionControl;
use crate::runtime::{AgentRuntime, TurnIo};
use hrn_kernel::{
    DagPayload, FinishReason, HarnessHook, HrnError, LlmRequest, LlmResponse, Msg, Result,
    SessionEffect, SessionMeta, ToolCall, ToolOutput,
};
use std::path::Path;
use std::sync::Arc;

/// Prepared per-turn context threaded into [`AgentRuntime::drive_steps`].
pub(crate) struct TurnContext {
    pub(crate) base_system: String,
    pub(crate) notices: String,
    pub(crate) policy: CompactionPolicy,
    pub(crate) hooks: Vec<Arc<dyn HarnessHook>>,
    pub(crate) st: TurnState,
}

impl AgentRuntime {
    /// Discover rules, seed todos, activate session tools, evaluate magic keywords, and build the
    /// hook chain for one turn (fresh or restored).
    pub(crate) async fn prepare_turn_context(
        &self,
        meta: &SessionMeta,
        prompt: &str,
        io: &TurnIo,
    ) -> Result<TurnContext> {
        let cwd = Path::new(&meta.cwd).to_path_buf();
        let book = self.discover_rules(&cwd, meta, io).await;
        let base_system = self.build_system_prompt(&cwd, book.as_ref(), meta.model.as_deref());
        let root = self.agents.root_of(&meta.id);
        let initial_todos =
            crate::summary::latest_todo_board(&self.load_head_branch(meta)?).unwrap_or_default();
        let initial_todo_plan = crate::runtime::todo_plan_signature(&initial_todos);
        self.todos.seed(&root, initial_todos);
        let mut st = TurnState {
            activated: self.activated_tools(meta)?,
            initial_todo_plan,
            ..TurnState::default()
        };
        // Hidden tools the session's state activates (omp's HIDDEN_TOOLS): think everywhere,
        // goal while a goal is live, yield inside subagent sessions.
        st.activated.extend(self.hidden_names(meta));
        // Magic keywords (embedded data): a forced level, notices for the system prompt.
        let keywords = {
            let tools = self.tools_for_mode(meta.permission_mode, &st.activated);
            self.keyword_effects(prompt, meta.thinking, &tools)?
        };
        st.pinned_thinking = keywords.level.or(keywords.auto_override);
        let notices = keywords.notices.join("\n\n");
        let mut policy = self.policy_for(meta.model.as_deref());
        policy.fixed_overhead_tokens = self.context_overhead(&base_system, &st.activated);
        let hooks = self.turn_hooks(book);
        Ok(TurnContext {
            base_system,
            notices,
            policy,
            hooks,
            st,
        })
    }

    /// Build one step's [`LlmRequest`] after applying projection, compaction, hooks, and thinking.
    async fn build_step_request(
        &self,
        meta: &mut SessionMeta,
        prompt: &str,
        ctx: &mut TurnContext,
        io: &TurnIo,
    ) -> Result<LlmRequest> {
        let projected = self
            .project_with_compaction(meta, &mut ctx.policy, &mut ctx.st, io)
            .await?;
        let system = Self::turn_system(&ctx.base_system, &ctx.notices, meta);
        let thinking = self.resolve_thinking(meta, prompt, &mut ctx.st).await?;
        let mut messages = Vec::with_capacity(projected.messages.len() + 1);
        messages.push(Msg::system(system_for_mode(&system, meta.permission_mode)));
        messages.extend(projected.messages);
        for hook in &ctx.hooks {
            hook.before_llm(&mut messages).await?;
        }
        Ok(LlmRequest {
            model: meta.model.clone(),
            messages,
            tools: self.tools_for_mode(meta.permission_mode, &ctx.st.activated),
            thinking,
            thinking_budgets: None,
            temperature: None,
            max_tokens: None,
            session_id: Some(meta.id.clone()),
            service_tier: if self.config.subagent {
                self.config.subagent_tier
            } else {
                None
            },
        })
    }

    /// Run the LLM + tool step loop until the model finishes without tool calls (or cancellation /
    /// step cap / error stops it).
    pub(crate) async fn drive_steps(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        prompt: &str,
        ctx: &mut TurnContext,
        control: &SessionControl,
        io: &TurnIo,
    ) -> Result<DrivenTurn> {
        loop {
            if io.cancel.is_cancelled() {
                return Err(HrnError::Cancelled);
            }
            if ctx.st.steps >= self.config.max_steps {
                return Err(HrnError::MaxSteps(self.config.max_steps));
            }
            self.apply_control(meta, turn_id, control, io).await?;

            let request = self.build_step_request(meta, prompt, ctx, io).await?;
            let mut response = match self.call_llm(turn_id, &request, io, &mut ctx.st).await {
                Ok(resp) => resp,
                Err(err) if self.llm.is_context_overflow(&err) => {
                    if self.recover_from_overflow(meta, ctx, io).await? {
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => return Err(err),
            };
            self.account_goal(meta, response.usage, control, io).await?;
            self.try_recover_inline_edits(meta, ctx, &mut response);

            let has_calls = !response.tool_calls.is_empty();
            if response.content.trim().is_empty() && !has_calls {
                if response.finish_reason == FinishReason::Length
                    && self.recover_from_length_stop(meta, ctx, io).await?
                {
                    continue;
                }
                if let Some(err) = empty_response_error(response.finish_reason, ctx.st.nudged_empty)
                {
                    return Err(err);
                }
                ctx.st.nudged_empty = true;
                self.append_message(meta, turn_id, Msg::user(EMPTY_RESPONSE_NUDGE))?;
                continue;
            }
            ctx.st.nudged_empty = false;

            let mut assistant = Msg::assistant_tool_calls(
                response.content.clone(),
                (!response.reasoning.is_empty()).then(|| response.reasoning.clone()),
                response.tool_calls.clone(),
            );
            assistant
                .thinking_blocks
                .clone_from(&response.thinking_blocks);
            self.append_message(meta, turn_id, assistant)?;

            if !has_calls {
                if response.finish_reason == FinishReason::Length
                    && self.recover_from_length_stop(meta, ctx, io).await?
                {
                    self.append_message(meta, turn_id, Msg::user(LENGTH_CONTINUE_NUDGE))?;
                    continue;
                }
                if control.has_steers() {
                    ctx.st.final_text = response.content;
                    continue;
                }
                return self
                    .end_turn(meta, turn_id, response, ctx, control, io)
                    .await;
            }
            if !response.content.trim().is_empty() {
                ctx.st.final_text.clone_from(&response.content);
            }

            let step = StepEnv {
                hooks: &ctx.hooks,
                control,
                io,
            };
            let truncated = response.finish_reason == FinishReason::Length;
            self.run_tool_calls(
                meta,
                turn_id,
                &step,
                &mut ctx.st.activated,
                &response.tool_calls,
                truncated,
            )
            .await?;
            if truncated {
                let _ = self.recover_from_length_stop(meta, ctx, io).await?;
            }
            // Apply any background `/handoff` draft that finished while the tool batch ran.
            if let Some(draft) = control.take_pending_handoff()
                && self.commit_handoff(&draft, &io.events).await?
                && let Some(fresh) = self.store.get_session(&meta.id)?
            {
                *meta = fresh;
            }
        }
    }

    /// Execute (or refuse, when truncated) `calls`, persist every result, record newly activated
    /// deferred tools, and apply session effects. Fails with [`HrnError::Cancelled`] when the
    /// turn was cancelled meanwhile.
    pub(crate) async fn run_tool_calls(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        step: &StepEnv<'_>,
        activated: &mut Vec<String>,
        calls: &[ToolCall],
        truncated: bool,
    ) -> Result {
        let (mut outputs, cancelled) = if truncated {
            // Never execute calls whose arguments may have been cut off mid-JSON.
            let outs = calls
                .iter()
                .map(|_| ToolOutput::err(TRUNCATED_CALL_ERROR))
                .collect();
            (outs, false)
        } else {
            let cwd = Path::new(&meta.cwd);
            let root_session_id = self.agents.root_of(&meta.id);
            let mut env = BatchEnv {
                registry: &self.tools,
                hooks: step.hooks,
                session_id: &meta.id,
                turn_id,
                cwd,
                permission_mode: meta.permission_mode,
                control: step.control,
                cancel: &step.io.cancel,
                events: &step.io.events,
                approval: step.io.approval.as_ref(),
                ask: step.io.ask.as_ref(),
                urls: &self.urls,
                root_session_id: &root_session_id,
                inbox: self.agents.inbox(),
                caller: None,
                todos: Arc::clone(&self.todos),
                yield_data: step.control.yield_data(),
                is_subagent: meta.parent_session_id.is_some(),
                approval_policy: &self.config.approval_policy,
                grants: &self.grants,
                fs: self.fs.as_ref(),
                trace: Some(crate::trace::TraceSink {
                    store: self.store.as_ref(),
                    clock: self.clock.as_ref(),
                }),
                store: Some(self.store.as_ref()),
            };
            // Tool commands of this step's `bash` calls run through the same env.
            env.caller = Some(crate::caller::NestedCaller::new(
                step,
                &env,
                Arc::clone(&self.fs),
                Some(Arc::clone(&self.store)),
            ));
            let outcome = execute_tool_batch(&env, calls).await;
            (outcome.outputs, outcome.cancelled)
        };
        self.apply_goal_effects(meta, turn_id, &mut outputs, step.io)
            .await?;
        let fresh = self.fresh_activations(&outputs, activated);
        let effects: Vec<SessionEffect> = outputs
            .iter_mut()
            .filter(|out| !out.is_error)
            .filter_map(|out| out.session_effect.take())
            .collect();
        self.append_tool_results(meta, turn_id, calls, outputs)?;
        if !fresh.is_empty() {
            self.append_payload(
                meta,
                DagPayload::ToolsActivated {
                    names: fresh.clone(),
                },
            )?;
            activated.extend(fresh);
        }
        self.apply_effects(meta, turn_id, effects)?;
        if cancelled {
            return Err(HrnError::Cancelled);
        }
        Ok(())
    }

    fn try_recover_inline_edits(
        &self,
        meta: &SessionMeta,
        ctx: &mut TurnContext,
        response: &mut LlmResponse,
    ) {
        if !self.config.recover_inline_edits
            || !response.tool_calls.is_empty()
            || response.finish_reason != FinishReason::Stop
            || ctx.st.recovery.recovered_inline()
        {
            return;
        }
        let tools = self.tools_for_mode(meta.permission_mode, &ctx.st.activated);
        let recovered = super::inline_edit::recover_calls(&response.content, &tools, || {
            self.next_id("inline_edit")
        });
        if !recovered.is_empty() {
            ctx.st.recovery.set_recovered_inline();
            response.tool_calls = recovered;
        }
    }
}
