//! Parallel `ReadOnly` batch execution with `PermissionMode` gating and hook execution.
//!
//! Invariant: `execute_tool_batch` returns exactly one [`ToolOutput`] per call, in call order,
//! no matter what happens (cancellation, hook failure, denied approval). The caller persists all of
//! them so every `tool_use` in the DAG has a matching `tool_result`.

use crate::approvals::{
    ApprovalResolution, GrantCache, UserApprovalPolicy, normalized_call_targets, resolve_for_call,
};
use crate::control::SessionControl;
use crate::join::join_all;
use crate::registry::ToolRegistry;
#[cfg(test)]
use hrn_kernel::ToolApproval;
use hrn_kernel::{
    AgentEvent, AgentStatus, ApprovalDecision, ApprovalGate, ApprovalGrantScope, ApprovalRequest,
    AskHandler, CancelFlag, EventSink, HarnessHook, HookVerdict, HrnError, Inbox, PermissionMode,
    Result, ScopedFuture, SessionId, SessionStore, ToolApprovalPolicy, ToolCall, ToolContext,
    ToolEffect, ToolOutput, ToolTier, UrlRouter, WorkspaceFs,
};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Shared execution environment for a batch of tool calls in a single model step.
/// Everything one tool call needs from the turn that runs it: registry, hooks, permission mode,
/// approval channel, session ports. A tool that runs another tool (the `bash` tool commands)
/// builds one for the nested call, so both are gated identically.
pub(crate) struct BatchEnv<'a> {
    /// Tools run through this env may themselves run tools (tool commands of `bash`).
    pub registry: &'a ToolRegistry,
    pub hooks: &'a [Arc<dyn HarnessHook>],
    pub session_id: &'a SessionId,
    pub turn_id: &'a str,
    pub cwd: &'a Path,
    /// Mode at the start of the step; a live override in `control` wins per call.
    pub permission_mode: PermissionMode,
    pub control: &'a SessionControl,
    pub cancel: &'a CancelFlag,
    pub events: &'a Arc<dyn EventSink>,
    pub approval: Option<&'a Arc<dyn ApprovalGate>>,
    pub ask: Option<&'a Arc<dyn AskHandler>>,
    pub urls: &'a Arc<UrlRouter>,
    /// Root of the session tree (`local://` scratch is shared through it).
    pub root_session_id: &'a str,
    pub inbox: &'a Arc<Inbox>,
    /// How a tool of this step runs another tool (`bash` tool commands).
    pub caller: Option<Arc<dyn hrn_kernel::ToolCaller>>,
    /// The runtime's live todo checklist table.
    pub todos: Arc<hrn_kernel::todo::TodoBoards>,
    /// Per-run `yield` result slot (hidden `yield` tool); `None` outside subagent runs.
    pub yield_data: &'a Arc<std::sync::Mutex<Option<String>>>,
    /// Whether this batch is executing inside a child subagent session.
    pub is_subagent: bool,
    /// User-configured restricted folders and per-tool approval overrides.
    pub approval_policy: &'a UserApprovalPolicy,
    /// Shared session + persistent approval grant cache.
    pub grants: &'a Arc<GrantCache>,
    /// Workspace filesystem port for path canonicalization and home directory resolution.
    pub fs: &'a dyn WorkspaceFs,
    /// Optional trace store + clock for recording per-tool `/trace` spans.
    pub trace: Option<crate::trace::TraceSink<'a>>,
    /// Optional session store for recording edit attribution traces (`kind = "edit"`).
    pub store: Option<&'a dyn SessionStore>,
}

/// Outputs for every call of a batch plus whether the turn was cancelled meanwhile.
pub(crate) struct BatchOutcome {
    /// One output per call, aligned with the input slice.
    pub outputs: Vec<ToolOutput>,
    /// `true` when cancellation interrupted the batch (remaining calls got synthetic errors).
    pub cancelled: bool,
}

fn not_executed() -> ToolOutput {
    ToolOutput::err("not executed: the turn was cancelled before this tool call ran")
}

/// Execute tool calls preserving order: consecutive `ReadOnly` auto-approved calls run
/// concurrently, while `Mutating` or confirmation-gated calls act as sequential barriers.
pub(crate) async fn execute_tool_batch(env: &BatchEnv<'_>, calls: &[ToolCall]) -> BatchOutcome {
    let mut outputs: Vec<ToolOutput> = Vec::with_capacity(calls.len());
    let mut cancelled = false;
    let mut idx = 0usize;

    while idx < calls.len() {
        if cancelled || env.cancel.is_cancelled() {
            cancelled = true;
            outputs.push(not_executed());
            idx += 1;
            continue;
        }
        let mode = env.control.permission_mode().unwrap_or(env.permission_mode);
        let end = if calls
            .get(idx)
            .is_some_and(|c| is_parallel_safe(env, mode, c))
        {
            let mut end = idx + 1;
            while calls
                .get(end)
                .is_some_and(|c| is_parallel_safe(env, mode, c))
            {
                end += 1;
            }
            end
        } else {
            idx + 1
        };
        let group = calls.get(idx..end).unwrap_or_default();
        let futs: Vec<ScopedFuture<'_, (ToolOutput, bool)>> = group
            .iter()
            .map(|call| -> ScopedFuture<'_, (ToolOutput, bool)> {
                Box::pin(execute_one(env, call))
            })
            .collect();
        for (output, was_cancelled) in join_all(futs).await {
            cancelled |= was_cancelled;
            outputs.push(output);
        }
        idx = end;
    }
    BatchOutcome { outputs, cancelled }
}

fn is_parallel_safe(env: &BatchEnv<'_>, mode: PermissionMode, call: &ToolCall) -> bool {
    let args = serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null);
    env.registry.get(&call.name).is_some_and(|t| {
        if t.effect() != ToolEffect::ReadOnly {
            return false;
        }
        let approval = t.approval(&args);
        let home = env.fs.home_dir();
        let prev_bash_cwd = env.grants.session_bash_cwd(env.session_id);
        let call_targets = normalized_call_targets(
            &call.name,
            &args,
            env.cwd,
            prev_bash_cwd.as_deref(),
            home.as_deref(),
            Some(env.fs),
        );
        resolve_for_call(
            env.session_id,
            &call.name,
            &approval,
            env.approval_policy,
            mode,
            &call_targets,
            env.cwd,
            home.as_deref(),
            Some(env.fs),
            env.approval.is_some(),
            env.grants,
        ) == ApprovalResolution::Allow
    })
}

/// Run one call with `ToolStart`/`ToolDone` framing. The bool is `true` when it was cancelled.
async fn execute_one(env: &BatchEnv<'_>, call: &ToolCall) -> (ToolOutput, bool) {
    let started_ms = env.trace.as_ref().map(|t| t.clock.now_ms());
    // `emit` returns `false` once the subscriber is gone; the turn must keep running regardless,
    // so the flag is intentionally not acted upon.
    let _ = env
        .events
        .emit(AgentEvent::ToolStart {
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        })
        .await;
    let (output, cancelled) = match run_call(env, call).await {
        Ok(out) => (out, false),
        Err(HrnError::Cancelled) => (ToolOutput::err("cancelled before the tool finished"), true),
        Err(err) => (ToolOutput::err(err.to_string()), false),
    };
    if let (Some(trace), Some(started)) = (&env.trace, started_ms) {
        crate::trace::record_tool_span(
            trace.store,
            env.session_id,
            env.turn_id,
            call,
            started,
            trace.clock.now_ms(),
            output.is_error,
            cancelled,
        );
    }
    let _ = env
        .events
        .emit(AgentEvent::ToolDone {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: output.content.clone(),
            is_error: output.is_error,
            diff: output.diff.clone(),
            images: output.images.clone(),
        })
        .await;
    (output, cancelled)
}

/// Outcome of running `plan` mode, 9d approval gating, and pre-tool hooks for a single call.
pub(crate) enum AuthorizedCall {
    /// Approved; carries the (possibly hook-revised) arguments, hook context strings, and bash cwd.
    Allowed {
        ctx: ToolContext,
        args: Value,
        contexts: Vec<String>,
        bash_next_cwd: Option<std::path::PathBuf>,
    },
    /// Denied by policy, approval gate, or pre-tool hook.
    Refused(ToolOutput),
}

/// Run `plan` mode, the 9d approval resolution ladder, and all `before_tool` /
/// `before_mutating_tool` hooks for `call` without executing `tool.execute`.
pub(crate) async fn authorize_call(
    env: &BatchEnv<'_>,
    call: &ToolCall,
    tool: &dyn hrn_kernel::Tool,
    args: Value,
) -> Result<AuthorizedCall> {
    let mode = env.control.permission_mode().unwrap_or(env.permission_mode);
    let mut bash_next_cwd = match gate_call(env, call, tool, &args, mode).await? {
        GateOutcome::Refused(refusal) => return Ok(AuthorizedCall::Refused(*refusal)),
        GateOutcome::Allowed { bash_next_cwd } => bash_next_cwd,
    };

    let ctx = build_tool_context(env, call, mode);
    let mut args = args;
    let mut revised = false;
    let mut contexts: Vec<String> = Vec::new();
    for hook in env.hooks {
        match hook.before_tool(&ctx, &call.name, &args).await {
            Ok(HookVerdict::Continue) => {}
            Ok(HookVerdict::Revise { arguments, context }) => {
                if let Some(next) = arguments {
                    if next != args {
                        revised = true;
                    }
                    args = next;
                }
                if let Some(text) = context.filter(|t| !t.trim().is_empty()) {
                    contexts.push(text);
                }
            }
            Ok(HookVerdict::ShortCircuit(out)) => return Ok(AuthorizedCall::Refused(out)),
            Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
            Err(err) => {
                return Ok(AuthorizedCall::Refused(ToolOutput::err(format!(
                    "pre-tool hook failed: {err}"
                ))));
            }
        }
    }
    if revised {
        let mode = env.control.permission_mode().unwrap_or(env.permission_mode);
        bash_next_cwd = match gate_call(env, call, tool, &args, mode).await? {
            GateOutcome::Refused(refusal) => return Ok(AuthorizedCall::Refused(*refusal)),
            GateOutcome::Allowed { bash_next_cwd } => bash_next_cwd,
        };
    }
    if tool.effect() == ToolEffect::Mutating {
        for hook in env.hooks {
            match hook.before_mutating_tool(&ctx, &call.name).await {
                Ok(()) => {}
                Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
                Err(err) => {
                    return Ok(AuthorizedCall::Refused(ToolOutput::err(format!(
                        "pre-tool hook failed: {err}"
                    ))));
                }
            }
        }
    }
    Ok(AuthorizedCall::Allowed {
        ctx,
        args,
        contexts,
        bash_next_cwd,
    })
}

/// Resolve policy, approval and hooks, then execute. `Err` is only returned for cancellation or
/// unexpected failures; ordinary tool failures are `Ok(ToolOutput::err(..))`. A tool that runs
/// other tools (the `bash` tool commands) goes through the same path via
/// [`crate::caller::NestedCaller`].
pub(crate) async fn run_call(env: &BatchEnv<'_>, call: &ToolCall) -> Result<ToolOutput> {
    let Some(tool) = env.registry.get(&call.name) else {
        return Ok(ToolOutput::err(format!("unknown tool `{}`", call.name)));
    };
    let args: Value = match serde_json::from_str(&call.arguments) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ToolOutput::err(format!(
                "invalid JSON arguments for `{}`: {e}. Re-send the call with valid JSON.",
                call.name
            )));
        }
    };

    let (ctx, args, contexts, bash_next_cwd) =
        match authorize_call(env, call, tool.as_ref(), args).await? {
            AuthorizedCall::Refused(refusal) => return Ok(refusal),
            AuthorizedCall::Allowed {
                ctx,
                args,
                contexts,
                bash_next_cwd,
            } => (ctx, args, contexts, bash_next_cwd),
        };

    if let Some(next_cwd) = bash_next_cwd {
        env.grants.set_session_bash_cwd(env.session_id, next_cwd);
    }

    let mut output = match tool.execute(&ctx, args.clone()).await {
        Ok(out) => out,
        Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
        Err(err) => ToolOutput::err(err.to_string()),
    };
    for hook in env.hooks {
        output = match hook
            .after_tool(&ctx, &call.name, &args, output.clone())
            .await
        {
            Ok(next) => next,
            Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
            // A failing post-hook (redaction/cap) must not let the unfiltered output through.
            Err(err) => ToolOutput::err(format!("post-tool hook failed, output withheld: {err}")),
        };
    }
    if !output.is_error
        && let Some(store) = env.store
    {
        crate::attrib::record_tool_edit(store, env.session_id, env.cwd, &call.name, &args, &output);
    }
    if !contexts.is_empty() {
        // Hooks' `additionalContext` is trusted instruction; hrn delivers it attached to the
        // call's result so it persists on the branch and reaches the model on the next step.
        output.content.push_str("\n\n<hook-context>\n");
        output.content.push_str(&contexts.join("\n\n"));
        output.content.push_str("\n</hook-context>");
    }
    Ok(output)
}

enum GateOutcome {
    Allowed {
        bash_next_cwd: Option<std::path::PathBuf>,
    },
    Refused(Box<ToolOutput>),
}

/// Whether `call` is `write out://...`, which is read-approved and allowed in Plan mode.
fn is_out_url_write(tool_name: &str, args: &Value) -> bool {
    tool_name == "write"
        && args
            .get("path")
            .and_then(Value::as_str)
            .and_then(UrlRouter::split)
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("out"))
}

/// Evaluate `plan` mode, the approval resolution ladder, and interactive confirmation for `call`.
async fn gate_call(
    env: &BatchEnv<'_>,
    call: &ToolCall,
    tool: &dyn hrn_kernel::Tool,
    args: &Value,
    mode: PermissionMode,
) -> Result<GateOutcome> {
    if mode == PermissionMode::Plan
        && tool.effect() == ToolEffect::Mutating
        && !is_out_url_write(&call.name, args)
    {
        return Ok(GateOutcome::Refused(Box::new(ToolOutput::err(format!(
            "blocked by `plan` mode: mutating tool `{}` cannot run until the mode is switched",
            call.name
        )))));
    }
    let approval = tool.approval(args);
    let home = env.fs.home_dir();
    let prev_bash_cwd = env.grants.session_bash_cwd(env.session_id);
    let call_targets = normalized_call_targets(
        &call.name,
        args,
        env.cwd,
        prev_bash_cwd.as_deref(),
        home.as_deref(),
        Some(env.fs),
    );
    let bash_next_cwd = call_targets.bash_next_cwd.clone();
    match resolve_for_call(
        env.session_id,
        &call.name,
        &approval,
        env.approval_policy,
        mode,
        &call_targets,
        env.cwd,
        home.as_deref(),
        Some(env.fs),
        env.approval.is_some(),
        env.grants,
    ) {
        ApprovalResolution::Allow => Ok(GateOutcome::Allowed { bash_next_cwd }),
        ApprovalResolution::Deny { reason } => {
            let _ = env
                .events
                .emit(AgentEvent::ApprovalResolved {
                    id: call.id.clone(),
                    approved: false,
                })
                .await;
            Ok(GateOutcome::Refused(Box::new(ToolOutput::err(reason))))
        }
        ApprovalResolution::Prompt {
            reason,
            choices,
            restricted,
        } => {
            let forced_prompt = approval.force_prompt
                || approval.policy == Some(ToolApprovalPolicy::Prompt)
                || env.approval_policy.policy_for(&call.name) == Some(ToolApprovalPolicy::Prompt);
            let prompt = ConfirmParams {
                summary: tool.summarize_call(args),
                reason,
                choices,
                targets: call_targets.grant,
                tier: approval.tier,
                cacheable: !restricted && !forced_prompt,
            };
            match confirm(env, call, prompt).await? {
                Some(refusal) => Ok(GateOutcome::Refused(Box::new(refusal))),
                None => Ok(GateOutcome::Allowed { bash_next_cwd }),
            }
        }
    }
}

fn build_tool_context(env: &BatchEnv<'_>, call: &ToolCall, mode: PermissionMode) -> ToolContext {
    ToolContext {
        session_id: env.session_id.clone(),
        turn_id: env.turn_id.to_owned(),
        call_id: call.id.clone(),
        cwd: env.cwd.to_path_buf(),
        permission_mode: mode,
        cancel: env.cancel.clone(),
        events: Arc::clone(env.events),
        ask: env.ask.cloned(),
        // Inherited by subagents so their confirmations reach the same user.
        approval: env.approval.cloned(),
        urls: Arc::clone(env.urls),
        root_session_id: env.root_session_id.to_owned(),
        inbox: Arc::clone(env.inbox),
        caller: env.caller.clone(),
        todos: Arc::clone(&env.todos),
        yield_data: Arc::clone(env.yield_data),
        is_subagent: env.is_subagent,
    }
}

struct ConfirmParams {
    summary: String,
    reason: Option<String>,
    choices: Vec<ApprovalGrantScope>,
    targets: Vec<String>,
    tier: ToolTier,
    cacheable: bool,
}

/// Ask the user. `Ok(None)` = approved; `Ok(Some(out))` = refused with the message for the model.
/// Fails closed: without an approval gate a confirmation-requiring tool is refused.
async fn confirm(
    env: &BatchEnv<'_>,
    call: &ToolCall,
    params: ConfirmParams,
) -> Result<Option<ToolOutput>> {
    let Some(gate) = env.approval else {
        return Ok(Some(ToolOutput::err(format!(
            "`{}` requires user approval but no approval channel is attached (headless run); \
             switch to a permission mode that allows it",
            call.name
        ))));
    };
    let summary = params.summary;
    let allowed_choices = params.choices.clone();
    let request = ApprovalRequest {
        id: call.id.clone(),
        session_id: env.session_id.clone(),
        tool_name: call.name.clone(),
        summary: summary.clone(),
        diff: None,
        reason: params.reason,
        targets: params.targets.clone(),
        choices: params.choices,
        tier: Some(params.tier),
    };
    // Subscriber disconnects (`false`) are irrelevant here; the gate itself decides.
    let _ = env
        .events
        .emit(AgentEvent::StatusChanged {
            session_id: env.session_id.clone(),
            status: AgentStatus::Blocked,
            message: Some(format!("approval needed: {summary}")),
        })
        .await;
    let _ = env
        .events
        .emit(AgentEvent::ApprovalRequested {
            request: request.clone(),
        })
        .await;
    let decision = gate.await_decision(request, env.cancel.clone()).await;
    let approved = decision.as_ref().is_ok_and(ApprovalDecision::is_approved);
    let _ = env
        .events
        .emit(AgentEvent::ApprovalResolved {
            id: call.id.clone(),
            approved,
        })
        .await;
    let _ = env
        .events
        .emit(AgentEvent::StatusChanged {
            session_id: env.session_id.clone(),
            status: AgentStatus::Working,
            message: None,
        })
        .await;
    match decision {
        Ok(ApprovalDecision::Approve { scope }) => {
            let scope_permitted = allowed_choices.is_empty() || allowed_choices.contains(&scope);
            if params.cacheable && scope_permitted {
                let _ = env.grants.record_for_session(
                    env.session_id,
                    scope,
                    &call.name,
                    params.tier,
                    &params.targets,
                );
            }
            Ok(None)
        }
        Ok(ApprovalDecision::Deny) => Ok(Some(ToolOutput::err(format!(
            "user denied execution of `{}` ({summary})",
            call.name
        )))),
        Ok(ApprovalDecision::DenyAndSend { message }) => {
            if !message.trim().is_empty() {
                env.control.push_steer(message);
            }
            Ok(Some(ToolOutput::err(format!(
                "user denied execution of `{}` ({summary}) and sent guidance",
                call.name
            ))))
        }
        Err(HrnError::Cancelled) => Err(HrnError::Cancelled),
        Err(err) => Ok(Some(ToolOutput::err(format!(
            "approval failed for `{}`: {err}",
            call.name
        )))),
    }
}

#[cfg(test)]
pub(crate) fn needs_confirmation(mode: PermissionMode, approval: &ToolApproval) -> bool {
    if approval.force_prompt {
        return true;
    }
    if let Some(policy) = approval.policy {
        return match policy {
            ToolApprovalPolicy::Allow => false,
            ToolApprovalPolicy::Prompt | ToolApprovalPolicy::Deny => true,
        };
    }
    match mode {
        PermissionMode::Yolo => false,
        PermissionMode::AutoEdit => approval.tier == ToolTier::Exec,
        PermissionMode::Default | PermissionMode::Plan => approval.tier != ToolTier::Read,
        PermissionMode::AskCached => true,
    }
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
