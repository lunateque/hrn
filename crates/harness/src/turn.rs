//! The LLM/tool step loop of one user turn.

use crate::compaction::{CompactionPolicy, ProjectedContext, project_branch};
use crate::control::SessionControl;
use crate::hooks::builtin_hooks;
use crate::rules::RulesHook;
use crate::runtime::{AgentRuntime, TurnIo, TurnResult, emit};
use hrn_kernel::{
    AgentEvent, AgentStatus, BoxFuture, ChunkSink, Clock, DagPayload, EventSink, FinishReason,
    HrnError, ImageAttachment, LlmChunk, LlmRequest, LlmResponse, Msg, PermissionMode, Result,
    SessionMeta, ToolCall, ToolOutput, ToolSpec, Usage,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

mod goal_steps;
mod inline_edit;
mod recovery;
mod step_tail;

const NO_TOKEN_YET: i64 = i64::MIN;
pub(super) const EMPTY_RESPONSE_NUDGE: &str =
    "[system] Your previous response was empty. Continue the task, or state that you are done.";
pub(super) const TRUNCATED_CALL_ERROR: &str = "not executed: the model output hit the token limit, so this \
     tool call's arguments may be incomplete. Retry with a smaller call.";

/// Forwards streamed chunks to the event sink and records the time of the first one.
struct EventChunkSink {
    events: Arc<dyn EventSink>,
    clock: Arc<dyn Clock>,
    first_chunk_ms: AtomicI64,
}

impl ChunkSink for EventChunkSink {
    fn on_chunk(&self, chunk: LlmChunk) -> BoxFuture<()> {
        // Only the first chunk wins; later failures to swap are expected and harmless.
        let _first = self.first_chunk_ms.compare_exchange(
            NO_TOKEN_YET,
            self.clock.now_ms(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        let events = Arc::clone(&self.events);
        Box::pin(async move {
            let event = match chunk {
                LlmChunk::ContentDelta(delta) => AgentEvent::TextDelta { delta },
                LlmChunk::ReasoningDelta(delta) => AgentEvent::ThinkingDelta { delta },
            };
            emit(&events, event).await;
        })
    }
}

fn millis(delta: i64) -> u64 {
    u64::try_from(delta.max(0)).unwrap_or_default()
}

/// Bitflags tracking per-turn compaction and recovery state without triggering excessive-bool lints.
#[derive(Clone, Copy, Default)]
pub(super) struct RecoveryFlags(u8);

impl RecoveryFlags {
    const GAVE_UP: u8 = 1 << 0;
    const PROMOTED: u8 = 1 << 1;
    const OVERFLOW_RETRIED: u8 = 1 << 2;
    const LENGTH_RECOVERED: u8 = 1 << 3;
    const JUST_RECOVERED: u8 = 1 << 4;
    const DEFERRED: u8 = 1 << 5;
    const RECOVERED_INLINE: u8 = 1 << 6;

    pub(super) const fn gave_up(self) -> bool {
        self.0 & Self::GAVE_UP != 0
    }
    pub(super) const fn set_gave_up(&mut self) {
        self.0 |= Self::GAVE_UP;
    }
    pub(super) const fn promoted(self) -> bool {
        self.0 & Self::PROMOTED != 0
    }
    pub(super) const fn set_promoted(&mut self) {
        self.0 |= Self::PROMOTED;
    }
    pub(super) const fn overflow_retried(self) -> bool {
        self.0 & Self::OVERFLOW_RETRIED != 0
    }
    pub(super) const fn set_overflow_retried(&mut self) {
        self.0 |= Self::OVERFLOW_RETRIED;
    }
    pub(super) const fn length_recovered(self) -> bool {
        self.0 & Self::LENGTH_RECOVERED != 0
    }
    pub(super) const fn set_length_recovered(&mut self) {
        self.0 |= Self::LENGTH_RECOVERED;
    }
    pub(super) const fn take_just_recovered(&mut self) -> bool {
        let was = self.0 & Self::JUST_RECOVERED != 0;
        self.0 &= !Self::JUST_RECOVERED;
        was
    }
    pub(super) const fn set_just_recovered(&mut self) {
        self.0 |= Self::JUST_RECOVERED;
    }
    pub(super) const fn deferred(self) -> bool {
        self.0 & Self::DEFERRED != 0
    }
    pub(super) const fn set_deferred(&mut self) {
        self.0 |= Self::DEFERRED;
    }
    pub(super) const fn recovered_inline(self) -> bool {
        self.0 & Self::RECOVERED_INLINE != 0
    }
    pub(super) const fn set_recovered_inline(&mut self) {
        self.0 |= Self::RECOVERED_INLINE;
    }
}

/// Mutable state threaded through the steps of one turn.
#[derive(Default)]
pub(super) struct TurnState {
    pub(super) steps: usize,
    pub(super) usage: Option<Usage>,
    pub(super) model: String,
    pub(super) final_text: String,
    pub(super) nudged_empty: bool,
    pub(super) last_prompt_tokens: u64,
    /// Per-turn compaction, promotion, overflow, and length-stop recovery flags.
    pub(super) recovery: RecoveryFlags,
    /// Deferred tools activated on this session, in activation order.
    pub(super) activated: Vec<String>,
    /// A thinking level forced by a magic keyword for this turn only (chillax).
    pub(super) pinned_thinking: Option<hrn_kernel::ThinkingLevel>,
    /// The `auto` level resolved for this turn (keywords, classifier or heuristics), cached so
    /// every step of the turn asks for the same effort.
    pub(super) resolved_auto: Option<hrn_kernel::ThinkingLevel>,
    /// Structural todo plan signature captured at turn start to detect replans.
    pub(super) initial_todo_plan: Vec<(String, Vec<String>)>,
}

/// What one step's tool execution needs besides the response itself.
pub(crate) struct StepEnv<'a> {
    pub hooks: &'a [Arc<dyn hrn_kernel::HarnessHook>],
    pub control: &'a SessionControl,
    pub io: &'a TurnIo,
}

/// Output of driving a turn's step loop before [`AgentRuntime::finish_turn`] persists session
/// exit, emits `TurnDone`, and dispatches post-turn background auto-titling.
pub(crate) struct DrivenTurn {
    pub(crate) result: TurnResult,
    pub(crate) todo_replanned: bool,
}

impl AgentRuntime {
    /// Step boundary: adopt the live permission mode (persisted with the session's next update,
    /// so a resumed session keeps it) and inject the queued steers.
    async fn apply_control(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        control: &SessionControl,
        io: &TurnIo,
    ) -> Result {
        if let Some(mode) = control.permission_mode() {
            meta.permission_mode = mode;
        }
        for text in control.drain_steers() {
            self.append_message(meta, turn_id, Msg::user(format!("[steer] {text}")))?;
            emit(
                &io.events,
                AgentEvent::SteerInjected {
                    turn_id: turn_id.to_owned(),
                    text,
                },
            )
            .await;
        }
        // Advisor notes are volatile: they reach the model as a `[system]` user message at the
        // turn boundary instead of joining the cached system string.
        let notes = control.drain_advisor_notes();
        if let Some(body) = crate::advisor::format_pending_notes(&notes) {
            self.append_message(meta, turn_id, Msg::user(format!("[system] {body}")))?;
        }
        Ok(())
    }

    /// The step's system prompt: the base, the magic-keyword notices, and the live
    /// `goal_context` block (omp appends goal state to the system prompt each request).
    fn turn_system(base: &str, notices: &str, meta: &SessionMeta) -> String {
        let mut system = String::with_capacity(base.len() + notices.len() + 256);
        system.push_str(base);
        if !notices.is_empty() {
            system.push_str("\n\n");
            system.push_str(notices);
        }
        if let Some(goal) = &meta.goal {
            system.push('\n');
            system.push_str(&crate::goal::active_prompt(goal));
        }
        system
    }

    /// The system prompt + schema bytes' share of the context window, in rough tokens.
    fn context_overhead(&self, system: &str, activated: &[String]) -> u64 {
        let schema_bytes: usize = self
            .tools
            .specs_with(activated)
            .iter()
            .map(|s| s.name.len() + s.description.len() + s.parameters.to_string().len())
            .sum();
        u64::try_from((system.len() + schema_bytes) / 4).unwrap_or(u64::MAX)
    }

    /// Function list for one step: the non-deferred tools plus the session's activated ones.
    fn tools_for_mode(&self, mode: PermissionMode, activated: &[String]) -> Vec<ToolSpec> {
        if mode == PermissionMode::Plan {
            self.tools.read_only_view().specs_with(activated)
        } else {
            self.tools.specs_with(activated)
        }
    }

    /// The deferred tools activated on `meta`'s active branch, in activation order. Empty
    /// (without reading the store) when nothing is deferred.
    pub(crate) fn activated_tools(&self, meta: &SessionMeta) -> Result<Vec<String>> {
        if self.tools.deferred_count() == 0 {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = Vec::new();
        for node in self.load_head_branch(meta)? {
            if let DagPayload::ToolsActivated { names: added } = node.payload {
                for name in added {
                    if self.tools.is_deferred(&name) && !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        Ok(names)
    }

    pub(crate) async fn drive_turn(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        prompt: &str,
        images: Vec<ImageAttachment>,
        control: &SessionControl,
        io: &TurnIo,
    ) -> Result<DrivenTurn> {
        let cwd = Path::new(&meta.cwd).to_path_buf();
        let mut ctx = self.prepare_turn_context(meta, prompt, io).await?;
        // Lifecycle events run before the prompt reaches the DAG; a hook's returned context is
        // delivered to the model as messages around the prompt.
        self.open_turn(meta, &cwd, prompt, images, turn_id, io)
            .await?;
        self.drive_steps(meta, turn_id, prompt, &mut ctx, control, io)
            .await
    }

    /// The turn's stop path: post-turn maintenance, advisor look, goal continuation, and result.
    async fn end_turn(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        response: LlmResponse,
        ctx: &mut step_tail::TurnContext,
        control: &SessionControl,
        io: &TurnIo,
    ) -> Result<DrivenTurn> {
        let mut st = std::mem::take(&mut ctx.st);
        self.post_turn_maintenance(meta, &mut ctx.policy, &mut st, io)
            .await?;
        let current_plan = self
            .load_head_branch(meta)
            .ok()
            .and_then(|branch| crate::summary::latest_todo_board(&branch))
            .map(|phases| crate::runtime::todo_plan_signature(&phases))
            .unwrap_or_default();
        let todo_replanned = !current_plan.is_empty() && current_plan != st.initial_todo_plan;
        self.maybe_run_advisor(meta, turn_id, &response.content, control, io)
            .await;
        if let Some(goal) = meta.goal.clone()
            && goal.status == hrn_kernel::GoalStatus::Active
        {
            control.push_follow_up(crate::goal::continuation_prompt(&goal));
        }
        Ok(DrivenTurn {
            result: TurnResult {
                final_text: response.content,
                steps: st.steps,
                head_node_id: None,
                usage: st.usage,
                model: st.model,
            },
            todo_replanned,
        })
    }

    /// Project the branch, running threshold promotion or compaction walk when due (honoring
    /// `enabled` and `mid_turn_enabled`).
    async fn project_with_compaction(
        &self,
        meta: &mut SessionMeta,
        policy: &mut CompactionPolicy,
        st: &mut TurnState,
        io: &TurnIo,
    ) -> Result<ProjectedContext> {
        let branch = self.load_head_branch(meta)?;
        let projected = project_branch(&branch, policy);
        if st.recovery.take_just_recovered() {
            return Ok(projected);
        }
        let trigger = policy.trigger_tokens();
        let effective_tokens = projected.estimated_tokens.max(st.last_prompt_tokens);
        let due = policy.config.enabled
            && (projected.needs_tier3_summary || (trigger > 0 && st.last_prompt_tokens >= trigger));
        if !due || st.recovery.gave_up() {
            if !due && !st.recovery.gave_up() {
                self.maybe_arm_speculative(meta, &branch, policy, effective_tokens, &io.events);
            }
            return Ok(projected);
        }
        if st.steps > 0 && !policy.config.mid_turn_enabled {
            st.recovery.set_deferred();
            return Ok(projected);
        }
        if self.try_promote_model(meta, policy, st, io).await? {
            let _ = st.recovery.take_just_recovered();
            let reprojected = project_branch(&self.load_head_branch(meta)?, policy);
            if !reprojected.needs_tier3_summary {
                return Ok(reprojected);
            }
        }
        if self
            .try_commit_speculative(meta, &branch, policy, &io.events)
            .await?
            .is_some()
        {
            st.last_prompt_tokens = 0;
            return Ok(project_branch(&self.load_head_branch(meta)?, policy));
        }
        match self
            .compact_branch_walk(
                meta,
                policy,
                crate::runtime::CompactionReason::Threshold,
                None,
                &io.cancel,
                &io.events,
            )
            .await
        {
            Ok(outcome) if outcome.committed.is_some() => {
                st.last_prompt_tokens = 0;
                Ok(project_branch(&self.load_head_branch(meta)?, policy))
            }
            Ok(_) => {
                st.recovery.set_gave_up();
                Ok(projected)
            }
            Err(HrnError::Cancelled) => Err(HrnError::Cancelled),
            Err(err) => {
                // Not fatal: keep going with the uncompacted context, but tell the user and do
                // not retry on every step of this turn.
                st.recovery.set_gave_up();
                emit(
                    &io.events,
                    AgentEvent::StatusChanged {
                        session_id: meta.id.clone(),
                        status: AgentStatus::Working,
                        message: Some(format!("context compaction failed: {err}")),
                    },
                )
                .await;
                Ok(projected)
            }
        }
    }

    /// Deferred tools named by successful `outputs` that are not in `activated` yet.
    pub(super) fn fresh_activations(
        &self,
        outputs: &[ToolOutput],
        activated: &[String],
    ) -> Vec<String> {
        let mut fresh: Vec<String> = Vec::new();
        for name in outputs
            .iter()
            .filter(|out| !out.is_error)
            .flat_map(|out| &out.activated_tools)
        {
            if self.tools.is_deferred(name) && !activated.contains(name) && !fresh.contains(name) {
                fresh.push(name.clone());
            }
        }
        fresh
    }

    async fn call_llm(
        &self,
        turn_id: &str,
        request: &LlmRequest,
        io: &TurnIo,
        st: &mut TurnState,
    ) -> Result<LlmResponse> {
        let sink = EventChunkSink {
            events: Arc::clone(&io.events),
            clock: Arc::clone(&self.clock),
            first_chunk_ms: AtomicI64::new(NO_TOKEN_YET),
        };
        st.steps += 1;
        let started = self.clock.now_ms();
        let response = self.llm.stream_chat(request, &io.cancel, &sink).await?;
        let finished = self.clock.now_ms();
        let first = sink.first_chunk_ms.load(Ordering::Relaxed);
        let first_token_ms = (first != NO_TOKEN_YET).then(|| millis(first - started));
        if let Some(session_id) = request.session_id.as_deref() {
            self.record_usage(session_id, request.model.as_deref(), &response)?;
            crate::trace::record_llm_span(
                self.store.as_ref(),
                session_id,
                turn_id,
                st.steps,
                started,
                finished,
                &response,
                first_token_ms,
            );
        }
        emit(
            &io.events,
            AgentEvent::LlmCall {
                model: response.model_used.clone(),
                first_token_ms,
                duration_ms: millis(finished - started),
                finish_reason: response.finish_reason,
                usage: response.usage,
            },
        )
        .await;
        if let Some(from) = &response.fallback_from {
            emit(
                &io.events,
                AgentEvent::StatusChanged {
                    session_id: request.session_id.clone().unwrap_or_default(),
                    status: AgentStatus::Working,
                    message: Some(format!("model fallback: {from} -> {}", response.model_used)),
                },
            )
            .await;
        }
        if let Some(usage) = response.usage {
            st.last_prompt_tokens = usage.prompt_tokens;
            st.usage
                .get_or_insert_with(Usage::default)
                .accumulate(usage);
        }
        st.model.clone_from(&response.model_used);
        Ok(response)
    }

    fn append_tool_results(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        calls: &[ToolCall],
        outputs: Vec<hrn_kernel::ToolOutput>,
    ) -> Result {
        for (call, output) in calls.iter().zip(outputs) {
            let mut msg = Msg::tool_result(call.id.clone(), call.name.clone(), output.content);
            msg.is_error = output.is_error;
            // A failed read must never supersede a successful one.
            msg.read_key = output.read_key.filter(|_| !output.is_error);
            if !output.is_error {
                msg.images = output.images;
            }
            self.append_payload(
                meta,
                DagPayload::Message {
                    turn_id: turn_id.to_owned(),
                    msg,
                },
            )?;
        }
        Ok(())
    }
}

pub(super) fn empty_response_error(
    finish_reason: FinishReason,
    nudged_empty: bool,
) -> Option<HrnError> {
    match finish_reason {
        FinishReason::ContentFilter => Some(provider_error(
            "the response was blocked by the provider's content filter",
        )),
        FinishReason::Length => Some(provider_error(
            "the model hit its output limit before producing any text or tool call",
        )),
        FinishReason::Error => Some(provider_error(
            "the provider terminated the response with an error",
        )),
        FinishReason::Aborted => Some(HrnError::Cancelled),
        _ if nudged_empty => Some(provider_error(
            "the model returned empty responses twice in a row",
        )),
        _ => None,
    }
}

pub(super) fn system_for_mode(base: &str, mode: PermissionMode) -> String {
    if mode == PermissionMode::Plan {
        format!("{base}\n\n{}", crate::prompt_data::PLAN.trim_end())
    } else {
        base.to_owned()
    }
}

impl AgentRuntime {
    /// The turn's hook chain: user extras, then the session's rule auto-inject, the nested
    /// context inject, and the built-ins.
    fn turn_hooks(
        &self,
        book: Option<crate::rules::Rulebook>,
    ) -> Vec<Arc<dyn hrn_kernel::HarnessHook>> {
        let mut hooks = self.extra_hooks.clone();
        if let Some(book) = book {
            hooks.push(Arc::new(RulesHook::new(book, Arc::clone(&self.rules_seen))));
        }
        hooks.push(Arc::new(crate::nested_context::NestedContextHook::new(
            Arc::clone(&self.fs),
        )));
        hooks.extend(builtin_hooks());
        hooks
    }
}

fn provider_error(message: &str) -> HrnError {
    HrnError::Provider {
        message: message.to_owned(),
        retryable: false,
        retry_after_secs: None,
    }
}

// ===== merged from turn_events.rs =====
// The lifecycle seams of a turn: rule discovery, the hook events that bracket a turn, and how
// their injected context reaches the conversation. Extracted from `turn.rs`/`runtime.rs`.

use crate::rules::Rulebook;
use hrn_kernel::HookEvent;

impl AgentRuntime {
    /// Fire one lifecycle event at every extra (module/builtin) hook. A returned context block is
    /// the first one the hooks asked for; a failing hook reports itself to the user through the
    /// event sink rather than failing the turn.
    pub(crate) async fn notify_hooks(
        &self,
        event: &HookEvent,
        session_id: &str,
        io: &TurnIo,
    ) -> Result<Option<String>> {
        let mut first = None;
        for hook in &self.extra_hooks {
            match hook.on_event(event).await {
                Ok(Some(text)) => {
                    if first.is_none() {
                        first = Some(text);
                    }
                }
                Ok(None) => {}
                Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
                Err(err) => {
                    emit(
                        &io.events,
                        AgentEvent::StatusChanged {
                            session_id: session_id.to_owned(),
                            status: AgentStatus::Working,
                            message: Some(format!("user hook failed: {err}")),
                        },
                    )
                    .await;
                }
            }
        }
        Ok(first)
    }

    /// The session's rulebook, or `None` when disabled or unreadable (a broken rule file reports
    /// itself to the user but must not stop the session).
    pub(crate) async fn discover_rules(
        &self,
        cwd: &Path,
        meta: &SessionMeta,
        io: &TurnIo,
    ) -> Option<Rulebook> {
        if !self.config.context.rules {
            return None;
        }
        match Rulebook::discover(self.fs.as_ref(), cwd) {
            Ok(book) => Some(book),
            Err(err) => {
                emit(
                    &io.events,
                    AgentEvent::StatusChanged {
                        session_id: meta.id.clone(),
                        status: AgentStatus::Working,
                        message: Some(format!("rules are off for this turn: {err}")),
                    },
                )
                .await;
                None
            }
        }
    }

    /// Fire `session_start` (first turn of a session) and `user_prompt`; returns the context
    /// blocks the hooks asked the model to see.
    pub(crate) async fn fire_turn_start_events(
        &self,
        meta: &SessionMeta,
        cwd: &Path,
        prompt: &str,
        io: &TurnIo,
    ) -> Result<Vec<String>> {
        let mut events = Vec::new();
        if meta.head_node_id.is_none() {
            events.push(HookEvent::SessionStart {
                session_id: meta.id.clone(),
                cwd: cwd.to_path_buf(),
            });
        }
        events.push(HookEvent::UserPrompt {
            session_id: meta.id.clone(),
            cwd: cwd.to_path_buf(),
            prompt: prompt.to_owned(),
        });
        let mut contexts = Vec::new();
        for event in &events {
            if let Some(text) = self.notify_hooks(event, &meta.id, io).await? {
                contexts.push(text);
            }
        }
        Ok(contexts)
    }

    /// The turn's opening: lifecycle events first (`session_start`, `user_prompt`), then the
    /// prompt message, then any context the hooks asked the model to see.
    pub(crate) async fn open_turn(
        &self,
        meta: &mut SessionMeta,
        cwd: &Path,
        prompt: &str,
        images: Vec<ImageAttachment>,
        turn_id: &str,
        io: &TurnIo,
    ) -> Result {
        let contexts = self.fire_turn_start_events(meta, cwd, prompt, io).await?;
        self.append_message(meta, turn_id, Msg::user_with_images(prompt, images))?;
        for text in contexts {
            if !text.trim().is_empty() {
                self.append_message(
                    meta,
                    turn_id,
                    Msg::user(format!("<hook-context>\n{}\n</hook-context>", text.trim())),
                )?;
            }
        }
        Ok(())
    }

    /// Fire `turn_end` for a finished turn. Context a hook returns joins the branch (the turn is
    /// over, so the next turn's model reads it); a hook error propagates, cancellation does not.
    pub(crate) async fn fire_turn_end(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        done: &TurnResult,
        io: &TurnIo,
    ) -> Result {
        match self
            .notify_hooks(
                &HookEvent::TurnEnd {
                    session_id: meta.id.clone(),
                    cwd: Path::new(&meta.cwd).to_path_buf(),
                    final_text: Some(done.final_text.clone()),
                    error: None,
                },
                &meta.id,
                io,
            )
            .await
        {
            Ok(Some(text)) if !text.trim().is_empty() => {
                self.append_message(
                    meta,
                    turn_id,
                    Msg::user(format!("<hook-context>\n{}\n</hook-context>", text.trim())),
                )?;
            }
            // A cancelled turn, or nothing queued: continue; a real hook error propagates.
            Ok(_) | Err(HrnError::Cancelled) => {}
            Err(err) => return Err(err),
        }
        Ok(())
    }
}
