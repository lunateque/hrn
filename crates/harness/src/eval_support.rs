//! Runtime support APIs backing `hrn_kernel::EvalHost`: non-cyclic weak runtime handles,
//! full-pipeline single-tool execution (`BatchEnv` + 9d approvals + hooks), and lightweight
//! no-tool completions (`judge()` / `completion()`).

use crate::agents::{AgentRecord, AgentRegistry};
use crate::approvals::GrantCache;
use crate::batch::{AuthorizedCall, BatchEnv, authorize_call, run_call};
use crate::control::SessionControl;
use crate::hooks::builtin_hooks;
use crate::isolation::{DynamicToolResolver, WorktreeIsolator};
use crate::profiles::SubagentProfile;
use crate::registry::{ToolRegistry, WeakToolRegistry};
use crate::runtime::{AgentRuntime, HarnessConfig, NullSink};
use crate::scope::ScopePolicy;
use crate::subagent::SubagentTool;
use hrn_kernel::{
    CancelFlag, Clock, HarnessHook, HrnError, LlmProvider, LlmRequest, Msg, ProcTable, Result,
    SessionStore, ThinkingLevel, ToolCall, ToolContext, ToolEffect, ToolOutput, ToolTier,
    UrlRouter, WorkspaceFs,
};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

static EVAL_CALL_SEQ: AtomicU64 = AtomicU64::new(1);

/// Non-cyclic handle to an assembled [`AgentRuntime`] used by `EvalHost` implementations.
///
/// Holds `Weak` links to every tool, hook, `AgentRegistry`, `UrlRouter`, `ProcTable`, and
/// `DynamicToolResolver` so dropping the owning [`AgentRuntime`] releases every allocation and
/// kills any live eval kernels cleanly without reference cycles.
#[derive(Clone)]
pub struct EvalRuntimeParts {
    llm: Arc<dyn LlmProvider>,
    store: Arc<dyn SessionStore>,
    tools: WeakToolRegistry,
    fs: Arc<dyn WorkspaceFs>,
    clock: Arc<dyn Clock>,
    config: HarnessConfig,
    extra_hooks: Vec<Weak<dyn HarnessHook>>,
    rules_seen: Arc<Mutex<HashSet<(String, String)>>>,
    urls: Weak<UrlRouter>,
    agents: Weak<AgentRegistry>,
    procs: Weak<ProcTable>,
    todos: Arc<hrn_kernel::todo::TodoBoards>,
    grants: Arc<GrantCache>,
    isolator: Option<Arc<dyn WorktreeIsolator>>,
    dynamic_tools: Option<Weak<dyn DynamicToolResolver>>,
    snap_renderer: Option<Arc<dyn hrn_kernel::SnapRenderer>>,
    speculative: crate::compaction::SpeculativeMap,
}

impl EvalRuntimeParts {
    /// Capture non-cyclic parts of `runtime`.
    #[must_use]
    pub fn from_runtime(runtime: &AgentRuntime) -> Self {
        Self {
            llm: Arc::clone(&runtime.llm),
            store: Arc::clone(&runtime.store),
            tools: runtime.tools.downgrade(),
            fs: Arc::clone(&runtime.fs),
            clock: Arc::clone(&runtime.clock),
            config: runtime.config.clone(),
            extra_hooks: runtime.extra_hooks.iter().map(Arc::downgrade).collect(),
            rules_seen: Arc::clone(&runtime.rules_seen),
            urls: Arc::downgrade(&runtime.urls),
            agents: Arc::downgrade(&runtime.agents),
            procs: Arc::downgrade(&runtime.procs),
            todos: Arc::clone(&runtime.todos),
            grants: Arc::clone(&runtime.grants),
            isolator: runtime.isolator.clone(),
            dynamic_tools: runtime.dynamic_tools.as_ref().map(Arc::downgrade),
            snap_renderer: runtime.snap_renderer.clone(),
            speculative: Arc::clone(&runtime.speculative),
        }
    }

    /// Upgrade the [`AgentRegistry`] if the runtime is still alive.
    #[must_use]
    pub fn agents(&self) -> Option<Arc<AgentRegistry>> {
        self.agents.upgrade()
    }

    /// Reconstruct a temporary [`AgentRuntime`] if its owning runtime is still alive.
    #[must_use]
    pub fn upgrade_runtime(&self) -> Option<AgentRuntime> {
        let agents = self.agents.upgrade()?;
        let urls = self.urls.upgrade()?;
        let procs = self.procs.upgrade()?;
        let tools = self.tools.upgrade();
        let extra_hooks = self.extra_hooks.iter().filter_map(Weak::upgrade).collect();
        Some(AgentRuntime {
            llm: Arc::clone(&self.llm),
            store: Arc::clone(&self.store),
            tools,
            fs: Arc::clone(&self.fs),
            clock: Arc::clone(&self.clock),
            config: self.config.clone(),
            extra_hooks,
            rules_seen: Arc::clone(&self.rules_seen),
            urls,
            agents,
            procs,
            todos: Arc::clone(&self.todos),
            grants: Arc::clone(&self.grants),
            isolator: self.isolator.clone(),
            dynamic_tools: self.dynamic_tools.as_ref().and_then(Weak::upgrade),
            snap_renderer: self.snap_renderer.clone(),
            speculative: Arc::clone(&self.speculative),
            title_revs: Arc::default(),
        })
    }
}

impl AgentRuntime {
    /// Capture non-cyclic weak-backed parts of this runtime for an [`hrn_kernel::EvalHost`].
    #[must_use]
    pub fn eval_runtime_parts(&self) -> EvalRuntimeParts {
        EvalRuntimeParts::from_runtime(self)
    }

    /// Execute one tool call through the full `BatchEnv` + 9d approval ladder + hooks pipeline.
    ///
    /// When `ctx.caller` is present (during a live turn, including scoped subagent turns),
    /// delegates through `ctx.caller` so the calling session's filtered tool set, `ScopeGuardHook`,
    /// and turn hooks are strictly enforced. Otherwise runs directly through `run_call` with
    /// this runtime's tools, `extra_hooks`, `builtin_hooks`, `approval_policy`, and `grants`.
    ///
    /// # Errors
    /// Propagates [`HrnError::Cancelled`] or unexpected runtime failures; normal tool errors and
    /// approval refusals return `Ok(ToolOutput::err(..))`.
    pub async fn execute_single_tool(
        &self,
        ctx: &ToolContext,
        tool_name: &str,
        args: Value,
    ) -> Result<ToolOutput> {
        if let Some(caller) = &ctx.caller {
            if caller.spec(tool_name).is_none() {
                let mut valid = caller.tool_names();
                valid.retain(|n| !matches!(n.as_str(), "checkpoint" | "rewind" | "yield" | "eval"));
                valid.sort();
                valid.dedup();
                return Ok(ToolOutput::err(format_unknown_tool(tool_name, &valid)));
            }
            return caller
                .call(tool_name, args, ctx.cwd.clone(), ctx.cancel.clone())
                .await;
        }
        if self.tools.get(tool_name).is_none() {
            let mut valid: Vec<String> = self
                .tools
                .names()
                .into_iter()
                .filter(|n| !matches!(n.as_str(), "checkpoint" | "rewind" | "yield" | "eval"))
                .collect();
            valid.sort();
            valid.dedup();
            return Ok(ToolOutput::err(format_unknown_tool(tool_name, &valid)));
        }

        let seq = EVAL_CALL_SEQ.fetch_add(1, Ordering::Relaxed);
        let call = ToolCall {
            id: format!("eval-{seq}"),
            name: tool_name.to_owned(),
            arguments: args.to_string(),
            signature: None,
        };
        let mut hooks = self.extra_hooks.clone();
        hooks.extend(builtin_hooks());
        let control = SessionControl::default();
        let env = BatchEnv {
            registry: &self.tools,
            hooks: &hooks,
            session_id: &ctx.session_id,
            turn_id: &ctx.turn_id,
            cwd: &ctx.cwd,
            permission_mode: ctx.permission_mode,
            control: &control,
            cancel: &ctx.cancel,
            events: &ctx.events,
            approval: ctx.approval.as_ref(),
            ask: ctx.ask.as_ref(),
            urls: &ctx.urls,
            root_session_id: &ctx.root_session_id,
            inbox: &ctx.inbox,
            caller: None,
            todos: Arc::clone(&ctx.todos),
            yield_data: &ctx.yield_data,
            is_subagent: ctx.is_subagent,
            approval_policy: &self.config.approval_policy,
            grants: &self.grants,
            fs: self.fs.as_ref(),
            trace: None,
            store: Some(self.store.as_ref()),
        };
        run_call(&env, &call).await
    }

    /// Spawn a background subagent from an `eval` cell (`agent()`) enforcing `"task"` scope
    /// availability, the full 9d approval ladder + `BeforeTool` hooks for `"task"`, and the
    /// calling session's true recursion depth + scoped runtime.
    ///
    /// # Errors
    /// Fails if `"task"` is absent from the active scope, denied by `[tools.approval]` / mode /
    /// hooks, recursion depth is exhausted, or launch validation fails.
    pub async fn spawn_eval_agent(
        &self,
        ctx: &ToolContext,
        args: Value,
    ) -> Result<Arc<AgentRecord>> {
        let caller_runtime = self
            .agents
            .live_runtime_for_session(&ctx.session_id)
            .unwrap_or_else(|| self.clone());
        if ctx
            .caller
            .as_ref()
            .is_some_and(|c| c.spec("task").is_none())
        {
            return Err(HrnError::Tool(
                "eval agent(): `task` tool is not available in this scope".into(),
            ));
        }
        let Some(task_tool) = caller_runtime.tools.get("task") else {
            return Err(HrnError::Tool(
                "eval agent(): `task` tool is not available in this scope".into(),
            ));
        };
        let seq = EVAL_CALL_SEQ.fetch_add(1, Ordering::Relaxed);
        let call = ToolCall {
            id: format!("eval-agent-{seq}"),
            name: "task".to_owned(),
            arguments: args.to_string(),
            signature: None,
        };
        let mut hooks = caller_runtime.extra_hooks.clone();
        hooks.extend(builtin_hooks());
        let control = SessionControl::default();
        let env = BatchEnv {
            registry: &caller_runtime.tools,
            hooks: &hooks,
            session_id: &ctx.session_id,
            turn_id: &ctx.turn_id,
            cwd: &ctx.cwd,
            permission_mode: ctx.permission_mode,
            control: &control,
            cancel: &ctx.cancel,
            events: &ctx.events,
            approval: ctx.approval.as_ref(),
            ask: ctx.ask.as_ref(),
            urls: &ctx.urls,
            root_session_id: &ctx.root_session_id,
            inbox: &ctx.inbox,
            caller: None,
            todos: Arc::clone(&ctx.todos),
            yield_data: &ctx.yield_data,
            is_subagent: ctx.is_subagent,
            approval_policy: &caller_runtime.config.approval_policy,
            grants: &caller_runtime.grants,
            fs: caller_runtime.fs.as_ref(),
            trace: None,
            store: Some(caller_runtime.store.as_ref()),
        };
        let revised_args = match authorize_call(&env, &call, task_tool.as_ref(), args).await? {
            AuthorizedCall::Allowed { args, .. } => args,
            AuthorizedCall::Refused(out) => return Err(HrnError::Tool(out.content)),
        };
        let depth = self
            .agents
            .session_depth(&ctx.session_id)
            .max(usize::from(ctx.is_subagent));
        let spawner = SubagentTool::with_depth(caller_runtime, depth);
        spawner.spawn_eval_agent(ctx, revised_args).await
    }

    /// Run a lightweight text-only LLM completion (no tools) on `role`, defaulting to `"judge"`
    /// and falling back to `"smol"` then the session's active model when a role is unconfigured.
    ///
    /// # Errors
    /// Propagates cancellation, provider errors, or configuration errors when no candidate works.
    pub async fn complete_text(
        &self,
        session_id: &str,
        prompt: String,
        role: Option<String>,
        cancel: &CancelFlag,
    ) -> Result<String> {
        let session_model = self
            .store
            .get_session(session_id)
            .ok()
            .flatten()
            .and_then(|meta| meta.model);
        let candidates = completion_candidates(role.as_deref(), session_model);
        let mut last_cfg_err: Option<HrnError> = None;
        for model in candidates {
            let request = LlmRequest {
                model: model.clone(),
                messages: vec![Msg::user(prompt.clone())],
                tools: Vec::new(),
                thinking: ThinkingLevel::Off,
                thinking_budgets: None,
                temperature: None,
                max_tokens: None,
                session_id: None,
                service_tier: None,
            };
            match self.llm.stream_chat(&request, cancel, &NullSink).await {
                Ok(resp) => {
                    let _ = self.record_usage(session_id, model.as_deref(), &resp);
                    return Ok(resp.content);
                }
                Err(HrnError::Config(msg)) => {
                    last_cfg_err = Some(HrnError::Config(msg));
                }
                Err(other) => return Err(other),
            }
        }
        Err(last_cfg_err.unwrap_or_else(|| {
            HrnError::Config("eval complete: no configured model available".to_owned())
        }))
    }
}

fn completion_candidates(role: Option<&str>, session_model: Option<String>) -> Vec<Option<String>> {
    let trimmed = role.map(str::trim).filter(|r| !r.is_empty());
    let mut list: Vec<Option<String>> = Vec::new();
    let mut push_unique = |cand: Option<String>| {
        if !list.contains(&cand) {
            list.push(cand);
        }
    };
    match trimmed {
        None | Some("judge") => {
            push_unique(Some("judge".to_owned()));
            push_unique(Some("smol".to_owned()));
            push_unique(session_model);
            push_unique(None);
        }
        Some(explicit) => {
            push_unique(Some(explicit.to_owned()));
            push_unique(session_model);
        }
    }
    list
}

fn format_unknown_tool(tool_name: &str, valid: &[String]) -> String {
    if valid.is_empty() {
        format!("unknown tool `{tool_name}` (no callable tools registered)")
    } else {
        format!(
            "unknown tool `{tool_name}` (valid tools: {})",
            valid.join(", ")
        )
    }
}

pub(crate) fn attach_dynamic_tools(
    runtime: &AgentRuntime,
    session_id: &str,
    profile: &SubagentProfile,
    scope: &ScopePolicy,
    plan_mode: bool,
    requested: &[String],
    child_tools: &mut ToolRegistry,
) -> Result {
    if requested.is_empty() {
        return Ok(());
    }
    if plan_mode || profile.read_only || scope.read_only {
        return Err(HrnError::Tool(
            "dynamic `eval` tools cannot be attached to a read-only or plan-mode subagent".into(),
        ));
    }
    if profile.deny_tools.iter().any(|t| t == "eval")
        || scope.deny_tools.iter().any(|t| t == "eval")
    {
        return Err(HrnError::Tool(
            "dynamic `eval` tools are blocked because `eval` is denied for this subagent".into(),
        ));
    }
    if (!scope.allowed_paths.is_empty() || !scope.denied_paths.is_empty())
        && !scope.allow_bash_with_paths
    {
        return Err(HrnError::Tool(
            "dynamic `eval` tools cannot be attached when path scope restrictions are active"
                .into(),
        ));
    }
    if !profile.tools.is_empty() && !profile.tools.iter().any(|t| t == "eval") {
        for name in requested {
            if !profile.tools.iter().any(|t| t == name) {
                return Err(HrnError::Tool(format!(
                    "dynamic tool `{name}` is not permitted by profile `{}` tool allowlist",
                    profile.name
                )));
            }
        }
    }
    if !scope.allow_tools.is_empty() && !scope.allow_tools.iter().any(|t| t == "eval") {
        for name in requested {
            if !scope.allow_tools.iter().any(|t| t == name) {
                return Err(HrnError::Tool(format!(
                    "dynamic tool `{name}` is not permitted by scope `allow_tools`"
                )));
            }
        }
    }
    let Some(resolver) = runtime.dynamic_tools() else {
        return Err(HrnError::Tool(
            "dynamic `eval` tools are not enabled in this runtime".into(),
        ));
    };
    let dynamic_tools = resolver.resolve(session_id, requested)?;
    for tool in dynamic_tools {
        let name = tool.spec().name;
        if name.starts_with("mcp__")
            || child_tools.get(&name).is_some()
            || runtime.tools().get(&name).is_some()
        {
            return Err(HrnError::Tool(format!(
                "dynamic tool `{name}` cannot shadow an existing tool"
            )));
        }
        if tool.effect() != ToolEffect::Mutating
            || tool.approval(&Value::Null).tier != ToolTier::Exec
        {
            return Err(HrnError::Tool(format!(
                "dynamic tool `{name}` must declare mutating/exec approval"
            )));
        }
        child_tools.register(tool);
    }
    Ok(())
}

/// Extract target paths from `%load <target>` magic lines in an `eval` cell so
/// `restricted_folders` gates host-side and runner-side `%load` reads.
pub(crate) fn extract_eval_load_targets(code: &str) -> Vec<String> {
    code.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("%load")?;
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            let raw = rest.trim();
            let s = raw
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| raw.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(raw)
                .trim();
            (!s.is_empty()).then(|| s.to_owned())
        })
        .collect()
}

pub(crate) fn resolve_child_thinking(
    runtime: &AgentRuntime,
    model: Option<&str>,
    effort: Option<ThinkingLevel>,
    model_level: Option<ThinkingLevel>,
    profile_thinking: Option<ThinkingLevel>,
) -> Option<ThinkingLevel> {
    if let Some(eff) = effort {
        let caps = runtime.llm.capabilities(model);
        return Some(if caps.thinking {
            eff
        } else {
            ThinkingLevel::Off
        });
    }
    model_level.or(profile_thinking)
}

pub(crate) fn build_job_prompts(
    instructions: &str,
    context: Option<&str>,
    task: &str,
    steps: Vec<String>,
    solution_space: &str,
    output_schema: Option<&Value>,
) -> (String, Vec<String>) {
    let shared = context
        .map(|c| format!("Context:\n{}\n\n", c.trim()))
        .unwrap_or_default();
    let schema_block = output_schema
        .and_then(|s| serde_json::to_string_pretty(s).ok())
        .map(|pretty| {
            format!(
                "\n\nStructured Output:\nReturn your final answer by calling the `yield` tool \
                 with `result` conforming to this JSON Schema:\n```json\n{pretty}\n```"
            )
        })
        .unwrap_or_default();
    let trailer = format!(
        "{}{solution_space}]",
        crate::thinking::SOLUTION_SPACE_PREFIX
    );

    if steps.len() > 1 {
        let total = steps.len();
        let mut iter = steps.into_iter();
        let first = iter.next().unwrap_or_default();
        let prompt = format!(
            "{instructions}\n\n{shared}Task (step 1/{total}):\n{first}{schema_block}{trailer}"
        );
        (prompt, iter.collect())
    } else {
        let body = steps.into_iter().next().unwrap_or_else(|| task.to_owned());
        let prompt = format!("{instructions}\n\n{shared}Task:\n{body}{schema_block}{trailer}");
        (prompt, Vec::new())
    }
}
