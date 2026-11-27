//! Subagent delegation tool (`task`): starts isolated child agents (async in background or
//! synchronous when `!async_mode` / `profile.blocking`), with bounded recursion depth and
//! concurrency from `[task]` (`TaskConfig`), multi-step chains, structured output validation,
//! per-call model gating, and optional git worktree isolation.

use crate::agents::run as run_agent_job;
use crate::agents::{AgentRecord, GuardSlot, JobStatus, LiveHandle, NewAgent, ReviveSpec};
use crate::agents::{Job, JobIsolation};
use crate::eval_support::{attach_dynamic_tools, build_job_prompts, resolve_child_thinking};
use crate::join::join_all;
use crate::registry::ToolRegistry;
use crate::runtime::AgentRuntime;
use crate::think_yield::YieldTool;
use crate::wait::WaitTool;
use hrn_kernel::{
    BoxFuture, CancelFlag, HrnError, PermissionMode, Result, SessionMeta, Tool, ToolApproval,
    ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

pub use crate::profiles::{
    SubagentProfile, default_subagent_profiles, format_agents_catalog, load_subagent_profiles,
    lookup_profile,
};

const TOOL_NAME: &str = "task";
/// The `task` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/tools/task.md");

/// Tool allowing an agent to delegate scoped subtasks to isolated child sessions.
pub struct SubagentTool {
    runtime: AgentRuntime,
    depth: usize,
    parent_profile: Option<SubagentProfile>,
}

impl SubagentTool {
    /// Construct a root-level `task` tool bound to `runtime` (whose registry tracks the
    /// agents it starts).
    pub fn new(runtime: AgentRuntime) -> Self {
        runtime.agents().attach_host(&runtime);
        Self {
            runtime,
            depth: 0,
            parent_profile: None,
        }
    }

    /// Construct a `task` tool at explicit recursion `depth` (used by `eval` `agent()` spawns).
    pub(crate) fn with_depth(runtime: AgentRuntime, depth: usize) -> Self {
        Self {
            runtime,
            depth,
            parent_profile: None,
        }
    }

    fn nested(runtime: AgentRuntime, depth: usize, parent_profile: SubagentProfile) -> Self {
        Self {
            runtime,
            depth,
            parent_profile: Some(parent_profile),
        }
    }

    /// Spawn a single asynchronous background subagent on behalf of an `eval` cell (`agent()`).
    ///
    /// Leaves the session inbox expectation active so unwaited handles auto-deliver their results;
    /// callers that explicitly `.wait()` consume the inbox item on settlement.
    ///
    /// # Errors
    /// Fails if recursion depth or concurrency limits are hit, arguments are invalid, or startup fails.
    pub async fn spawn_eval_agent(
        &self,
        ctx: &ToolContext,
        args: Value,
    ) -> Result<Arc<AgentRecord>> {
        let max_depth = self.runtime.task_config().max_recursion_depth;
        if self.depth >= max_depth {
            return Err(HrnError::Tool(format!(
                "subagent recursion depth limit ({max_depth}) reached"
            )));
        }
        let batch = prepare_call(&self.runtime, ctx, &args, self.parent_profile.as_ref())?;
        let mut items = batch.items.into_iter();
        let Some(prepared) = items.next() else {
            return Err(HrnError::Tool(
                "eval agent(): missing task specification".into(),
            ));
        };
        if items.next().is_some() {
            return Err(HrnError::Tool(
                "eval agent(): spawns one agent per call".into(),
            ));
        }
        let parent = self
            .runtime
            .store
            .get_session(&ctx.session_id)?
            .ok_or_else(|| {
                HrnError::Store(format!("unknown parent session `{}`", ctx.session_id))
            })?;
        let launch = Launch {
            runtime: &self.runtime,
            ctx,
            depth: self.depth,
            parent: &parent,
            context: batch.context.as_deref(),
        };
        launch.start_async_record(prepared).await
    }
}

fn preview(task: &str) -> String {
    if task.len() > 80 {
        format!(
            "{}...",
            task.get(..task.floor_char_boundary(80)).unwrap_or_default()
        )
    } else {
        task.to_owned()
    }
}

pub(crate) fn filter_child_tools(
    runtime: &AgentRuntime,
    profile: &SubagentProfile,
    depth: usize,
    force_read_only: bool,
) -> ToolRegistry {
    let effective_read_only = profile.read_only || force_read_only;
    let mut child_tools = if effective_read_only {
        runtime.tools().read_only_view()
    } else {
        runtime.tools().clone()
    };
    if !profile.tools.is_empty() {
        let allow: BTreeSet<&str> = profile.tools.iter().map(String::as_str).collect();
        let existing = child_tools.names();
        for name in existing {
            if !allow.contains(name.as_str()) {
                child_tools.remove(&name);
            }
        }
    }
    for denied in &profile.deny_tools {
        child_tools.remove(denied);
    }
    // The child never sees `ask` (no UI); it gets nested `task` (and `wait`) only when allowed.
    child_tools.remove(TOOL_NAME);
    child_tools.remove("ask");
    child_tools.remove("wait");
    let allows_nested = profile.tools.is_empty() || profile.tools.iter().any(|t| t == TOOL_NAME);
    let denies_nested = profile.deny_tools.iter().any(|t| t == TOOL_NAME);
    let spawns_forbids_all = profile.spawns.as_deref() == Some(&[]);
    let max_depth = runtime.task_config().max_recursion_depth;
    if depth + 1 < max_depth
        && !effective_read_only
        && allows_nested
        && !denies_nested
        && !spawns_forbids_all
    {
        let nested = SubagentTool::nested(
            runtime.with_tools(child_tools.clone()),
            depth + 1,
            profile.clone(),
        );
        child_tools.register(Arc::new(nested));
        child_tools.register(Arc::new(WaitTool::new(Arc::clone(runtime.procs()))));
    }
    child_tools
}

/// What happened to one prepared item.
enum Started {
    Running {
        id: String,
        profile: String,
        record: Arc<AgentRecord>,
    },
    Sync {
        record: Arc<AgentRecord>,
        job: Box<Job>,
    },
    Refused(String),
}

/// The fixed parts of one `task` call, shared by all of its items.
struct Launch<'a> {
    runtime: &'a AgentRuntime,
    ctx: &'a ToolContext,
    depth: usize,
    parent: &'a SessionMeta,
    context: Option<&'a str>,
}

impl Launch<'_> {
    /// Register, set up (including worktree isolation if requested), and spawn or stage one agent.
    async fn start(&self, p: Prepared) -> Result<Started> {
        let sync = !self.runtime.task_config().async_mode || p.profile.blocking;
        self.start_inner(p, sync).await
    }

    async fn start_async_record(&self, p: Prepared) -> Result<Arc<AgentRecord>> {
        match self.start_inner(p, false).await? {
            Started::Running { record, .. } | Started::Sync { record, .. } => Ok(record),
            Started::Refused(why) => Err(HrnError::Tool(why)),
        }
    }

    async fn start_inner(&self, p: Prepared, sync: bool) -> Result<Started> {
        let agents = self.runtime.agents();
        agents.attach_host(self.runtime);
        let max_concurrency = self.runtime.task_config().max_concurrency;
        let parent_agent = agents.agent_id_for_session(&self.ctx.session_id);
        let cancel = if self.depth == 0 && !sync {
            CancelFlag::new()
        } else {
            self.ctx.cancel.child()
        };
        let mut guard = match agents.register(
            NewAgent {
                name: p.name.clone(),
                profile: p.profile.name.clone(),
                owner: self.ctx.session_id.clone(),
                root: self.ctx.root_session_id.clone(),
                parent: parent_agent,
                task: preview(&p.task),
                cancel,
            },
            max_concurrency,
        ) {
            Ok(guard) => guard,
            Err(running) => {
                return Ok(Started::Refused(format!(
                    "max concurrent subagents ({max_concurrency}) reached \
                     ({running} running); `wait` for one to finish, then spawn again"
                )));
            }
        };
        if sync {
            guard.suppress_delivery();
        }
        let record = Arc::clone(guard.record());
        let slot: GuardSlot = Arc::new(Mutex::new(Some(guard)));
        let job = match self.prepare_job(p, &record, &slot).await {
            Ok(job) => job,
            Err(err) => {
                discard(&slot, &err);
                return Err(err);
            }
        };
        if sync {
            return Ok(Started::Sync {
                record,
                job: Box::new(job),
            });
        }
        if let Err(err) = agents.spawn(Box::pin(run_agent_job(job))) {
            discard(&slot, &err);
            return Err(err);
        }
        Ok(Started::Running {
            id: record.id.clone(),
            profile: record.profile.clone(),
            record,
        })
    }

    async fn prepare_isolation(
        &self,
        p: &Prepared,
        record: &AgentRecord,
    ) -> Result<(std::path::PathBuf, Option<JobIsolation>)> {
        if !p.isolated {
            return Ok((self.ctx.cwd.clone(), None));
        }
        let isolator = self.runtime.isolator().cloned().ok_or_else(|| {
            HrnError::Tool("worktree isolation is not configured in this runtime".into())
        })?;
        let wt = isolator.prepare(&self.ctx.cwd, &record.id).await?;
        record.set_worktree(wt.clone());
        let cwd = wt.path.clone();
        Ok((
            cwd,
            Some(JobIsolation {
                isolator,
                worktree: wt,
                allowed_paths: p.scope.allowed_paths.clone(),
                denied_paths: p.scope.denied_paths.clone(),
                config: self.runtime.task_config().isolation.clone(),
            }),
        ))
    }

    async fn prepare_job(
        &self,
        p: Prepared,
        record: &AgentRecord,
        slot: &GuardSlot,
    ) -> Result<Job> {
        let (child_cwd, isolation) = self.prepare_isolation(&p, record).await?;
        let profile = p.profile;
        let plan_mode = self.ctx.permission_mode == PermissionMode::Plan;
        let mut child_tools = filter_child_tools(self.runtime, &profile, self.depth, plan_mode);
        if p.output_schema.is_some() && child_tools.get("yield").is_none() {
            child_tools.register(Arc::new(YieldTool));
        }
        attach_dynamic_tools(
            self.runtime,
            &self.ctx.session_id,
            &profile,
            &p.scope,
            plan_mode,
            &p.tools,
            &mut child_tools,
        )?;

        let mut child_runtime = self.runtime.with_tools(child_tools).as_subagent();
        if profile.advisor != Some(true) {
            child_runtime = child_runtime.without_advisor();
        }
        // A child always gets an explicit step budget from `[task]` config (a per-call
        // `max_steps` in `p.scope` still overrides it below); it never inherits the parent's.
        let max_steps = self
            .runtime
            .task_config()
            .max_steps_for(&profile.name, &profile.aliases, profile.max_steps);
        child_runtime = child_runtime.with_max_steps(max_steps);
        if !p.scope.is_empty() {
            child_runtime = p.scope.apply_to_runtime(&child_runtime, child_cwd.clone());
        }

        let mode = if plan_mode || profile.read_only || p.scope.read_only {
            PermissionMode::Plan
        } else {
            self.ctx.permission_mode
        };
        let mut session = child_runtime.create_child_session_in(
            self.parent,
            &child_cwd,
            self.parent.head_node_id.clone(),
            format!("subagent:{}", profile.name),
            p.model.clone(),
            mode,
        )?;
        if let Some(thinking) = resolve_child_thinking(
            self.runtime,
            session.model.as_deref(),
            p.effort,
            p.model_level,
            profile.thinking,
        ) {
            session.thinking = thinking;
            child_runtime.store.update_session(&session)?;
        }
        record.set_session(&session.id);

        let instructions = profile.instructions.clone();
        let revive_spec = ReviveSpec {
            profile,
            scope: p.scope,
            depth: self.depth,
            parent_model: self.parent.model.clone(),
            model_from_profile: p.model_from_profile,
            approval: self.ctx.approval.clone(),
            cwd: child_cwd,
        };
        record.set_live(
            LiveHandle {
                runtime: child_runtime.clone(),
                session: session.clone(),
                parent_model: self.parent.model.clone(),
                model_from_profile: p.model_from_profile,
                approval: self.ctx.approval.clone(),
            },
            Some(revive_spec),
        );

        let (prompt, extra_steps) = build_job_prompts(
            &instructions,
            self.context,
            &p.task,
            p.steps,
            &p.solution_space,
            p.output_schema.as_ref(),
        );
        Ok(Job {
            runtime: child_runtime,
            session,
            parent_model: self.parent.model.clone(),
            model_from_profile: p.model_from_profile,
            prompt,
            extra_steps,
            output_schema: p.output_schema,
            schema_mode: p.schema_mode,
            isolation,
            approval: self.ctx.approval.clone(),
            slot: Arc::clone(slot),
        })
    }
}

fn discard(slot: &GuardSlot, err: &HrnError) {
    let guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(guard) = guard {
        guard.discard(err.to_string());
    }
}

fn report(
    async_started: &[(String, String)],
    sync_settled: &[Arc<AgentRecord>],
    notes: &[String],
) -> ToolOutput {
    let mut out = String::new();
    let any_started = !async_started.is_empty() || !sync_settled.is_empty();
    if !any_started {
        out.push_str("No agent was started:");
    }
    if !async_started.is_empty() {
        let _ = write!(
            out,
            "Spawned {} background agent{} (results arrive on their own as a follow-up message):",
            async_started.len(),
            if async_started.len() == 1 { "" } else { "s" }
        );
        for (id, profile) in async_started {
            let _ = write!(out, "\n- `{id}` [{profile}]");
        }
        out.push_str(
            "\nCall `wait` only when you have nothing else to do. `read agent://<id>` shows an \
             agent's status and output, `write agent://<id>` messages a running agent, \
             `write proc://<id>/kill` (empty content) cancels it.",
        );
    }
    for rec in sync_settled {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&rec.format_summary());
    }
    for note in notes {
        out.push_str("\n- ");
        out.push_str(note);
    }
    if !any_started {
        return ToolOutput::err(out);
    }
    if async_started.is_empty()
        && !sync_settled.is_empty()
        && sync_settled
            .iter()
            .all(|r| r.snapshot().status == JobStatus::Failed)
    {
        return ToolOutput::err(out);
    }
    ToolOutput::ok(out)
}

impl Tool for SubagentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_NAME.to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: build_spec_parameters(
                self.runtime.task_config(),
                self.runtime.dynamic_tools().is_some(),
            ),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn summarize_call(&self, args: &Value) -> String {
        if let Some(items) = args.get("tasks").and_then(Value::as_array) {
            let first = items
                .first()
                .and_then(|i| {
                    i.get("task").and_then(Value::as_str).or_else(|| {
                        i.get("steps")
                            .and_then(Value::as_array)
                            .and_then(|s| s.first())
                            .and_then(Value::as_str)
                    })
                })
                .unwrap_or("");
            return match items.len() {
                0 => String::new(),
                1 => preview(first),
                n => format!("{n} agents: {}", preview(first)),
            };
        }
        let flat = args
            .get("task")
            .and_then(Value::as_str)
            .or_else(|| {
                args.get("steps")
                    .and_then(Value::as_array)
                    .and_then(|s| s.first())
                    .and_then(Value::as_str)
            })
            .unwrap_or("");
        preview(flat)
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let runtime = self.runtime.clone();
        let depth = self.depth;
        let parent_profile = self.parent_profile.clone();
        let ctx = ctx.clone();

        Box::pin(async move {
            let max_depth = runtime.task_config().max_recursion_depth;
            if depth >= max_depth {
                return Ok(ToolOutput::err(format!(
                    "subagent recursion depth limit ({max_depth}) reached"
                )));
            }
            let batch = prepare_call(&runtime, &ctx, &args, parent_profile.as_ref())?;
            let parent = runtime.store.get_session(&ctx.session_id)?.ok_or_else(|| {
                HrnError::Store(format!("unknown parent session `{}`", ctx.session_id))
            })?;
            let launch = Launch {
                runtime: &runtime,
                ctx: &ctx,
                depth,
                parent: &parent,
                context: batch.context.as_deref(),
            };
            let mut async_started = Vec::new();
            let mut sync_records = Vec::new();
            let mut sync_futs: Vec<BoxFuture<()>> = Vec::new();
            let mut notes = Vec::new();
            for mut p in batch.items {
                notes.append(&mut p.warnings);
                let label = p.name.clone().unwrap_or_else(|| p.profile.name.clone());
                match launch.start(p).await {
                    Ok(Started::Running { id, profile, .. }) => async_started.push((id, profile)),
                    Ok(Started::Sync { record, job }) => {
                        sync_records.push(record);
                        sync_futs.push(Box::pin(run_agent_job(*job)));
                    }
                    Ok(Started::Refused(why)) => {
                        notes.push(format!("`{label}` not started: {why}"));
                    }
                    Err(err) => notes.push(format!("`{label}` failed to start: {err}")),
                }
            }
            if !sync_futs.is_empty() {
                let _ = join_all(sync_futs).await;
            }
            Ok(report(&async_started, &sync_records, &notes))
        })
    }
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod tests;

// ===== merged from subagent_prepare.rs =====
// `task` tool call-shape normalization (batch vs flat), per-call model gating, parent
// `spawns` / self-recursion policy, and per-item validation.

use crate::profiles::available_profile_names;
use crate::scope::ScopePolicy;
use crate::task_config::TaskConfig;
use hrn_kernel::{LevelSuffix, ThinkingLevel, split_level_suffix};
use std::collections::BTreeMap;

/// One validated `task` item ready to launch.
pub(crate) struct Prepared {
    pub name: Option<String>,
    pub task: String,
    /// Bucketed prompt chain (`len <= 4`); empty when a plain `task` string was given.
    pub steps: Vec<String>,
    /// Verbatim `solutionSpace` string passed to the `auto` thinking classifier.
    pub solution_space: String,
    /// Optional explicit reasoning effort override (`lo` -> Low, `med` -> Medium, `hi` -> High).
    pub effort: Option<ThinkingLevel>,
    /// Effective structured JSON Schema (`item.outputSchema -> profile.output -> None`).
    pub output_schema: Option<Value>,
    /// How structured schema failures are handled (`Permissive` vs `Strict`).
    pub schema_mode: SchemaMode,
    /// Whether this item runs in a dedicated git worktree.
    pub isolated: bool,
    /// Requested eval-defined dynamic tool names (wired in Wave 10c).
    pub tools: Vec<String>,
    pub profile: SubagentProfile,
    pub model: Option<String>,
    pub model_from_profile: bool,
    pub model_level: Option<ThinkingLevel>,
    pub scope: ScopePolicy,
    /// Non-fatal lenient-validation warnings (e.g. blank `solutionSpace` fallback).
    pub warnings: Vec<String>,
}

/// Validated `task` invocation (either batch `{ context, tasks[] }` or flat single item).
pub(crate) struct PreparedBatch {
    pub context: Option<String>,
    pub items: Vec<Prepared>,
}

/// Parse and validate a `task` tool call in either batch (`{ context, tasks[] }`) or flat
/// single-item (`{ agent?, task|steps, solutionSpace?, ... }`) shape.
pub(crate) fn prepare_call(
    runtime: &AgentRuntime,
    ctx: &ToolContext,
    args: &Value,
    parent_profile: Option<&SubagentProfile>,
) -> Result<PreparedBatch> {
    let (context, raw_items) = extract_shape(args)?;
    let profiles = load_subagent_profiles(runtime.fs.as_ref(), &ctx.cwd)?;
    let effective_parent = parent_profile.cloned().or_else(|| {
        let parent_id = runtime.agents().agent_id_for_session(&ctx.session_id)?;
        let rec = runtime.agents().get(&parent_id)?;
        lookup_profile(&profiles, &rec.profile).cloned()
    });

    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(raw_items.len());
    for (i, item) in raw_items.iter().enumerate() {
        out.push(prepare_item(
            runtime,
            ctx,
            item,
            i,
            &profiles,
            effective_parent.as_ref(),
            &mut seen,
        )?);
    }
    Ok(PreparedBatch {
        context,
        items: out,
    })
}

fn extract_shape(args: &Value) -> Result<(Option<String>, Vec<&Value>)> {
    if args.get("tasks").is_some() {
        if args.get("agent").is_some() || args.get("task").is_some() || args.get("steps").is_some()
        {
            return Err(HrnError::Tool(
                "task: cannot mix top-level `agent`, `task`, or `steps` with `tasks`".into(),
            ));
        }
        let context = args
            .get("context")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                HrnError::Tool(
                    "task: `context` is required (non-empty string) when using batched `tasks`"
                        .into(),
                )
            })?
            .to_owned();
        let items = args
            .get("tasks")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| {
                HrnError::Tool(
                    "task requires a non-empty `tasks` array of `{ name?, agent?, task, solutionSpace }`"
                        .into(),
                )
            })?;
        return Ok((Some(context), items.iter().collect()));
    }
    if args.get("task").is_none() && args.get("steps").is_none() {
        return Err(HrnError::Tool(
            "task requires `{ context, tasks: [...] }` or a flat `{ task, ... }` object".into(),
        ));
    }
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    Ok((context, vec![args]))
}

fn prepare_item(
    runtime: &AgentRuntime,
    ctx: &ToolContext,
    item: &Value,
    idx: usize,
    profiles: &BTreeMap<String, SubagentProfile>,
    parent_profile: Option<&SubagentProfile>,
    seen_names: &mut BTreeSet<String>,
) -> Result<Prepared> {
    let cfg = runtime.task_config();
    let name = parse_unique_name(item, seen_names)?;
    let profile = resolve_item_profile(item, profiles, cfg, parent_profile)?;
    let (task, steps) = parse_task_and_steps(item, idx, cfg)?;
    let label = name.as_deref().unwrap_or(&profile.name);
    let mut warnings = Vec::new();
    let solution_space = parse_solution_space(item, &task, label, &mut warnings)?;
    let effort = parse_effort(item, cfg)?;
    let (output_schema, schema_mode) = parse_structured(item, cfg, &profile)?;
    let isolated = parse_isolated(item, ctx, runtime, cfg)?;
    let tools = parse_dynamic_tools(item, ctx, runtime)?;
    let (model, model_from_profile, model_level) = parse_model(item, runtime, cfg, &profile)?;

    Ok(Prepared {
        name,
        task,
        steps,
        solution_space,
        effort,
        output_schema,
        schema_mode,
        isolated,
        tools,
        profile,
        model,
        model_from_profile,
        model_level,
        scope: ScopePolicy::from_tool_args(item),
        warnings,
    })
}

fn parse_unique_name(item: &Value, seen: &mut BTreeSet<String>) -> Result<Option<String>> {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned);
    if let Some(name) = &name
        && !seen.insert(name.to_ascii_lowercase())
    {
        return Err(HrnError::Tool(format!(
            "task: the name `{name}` is used twice in one call"
        )));
    }
    Ok(name)
}

fn resolve_item_profile(
    item: &Value,
    profiles: &BTreeMap<String, SubagentProfile>,
    cfg: &TaskConfig,
    parent_profile: Option<&SubagentProfile>,
) -> Result<SubagentProfile> {
    let profile_name = item.get("agent").and_then(Value::as_str).unwrap_or("scout");
    let profile = lookup_profile(profiles, profile_name)
        .cloned()
        .ok_or_else(|| {
            let known = available_profile_names(profiles);
            HrnError::Tool(format!(
                "unknown subagent profile `{profile_name}` (available: {})",
                known.join(", ")
            ))
        })?;
    if cfg.is_agent_disabled(&profile.name)
        || cfg.is_agent_disabled(profile_name)
        || profile.aliases.iter().any(|a| cfg.is_agent_disabled(a))
    {
        return Err(HrnError::Tool(format!(
            "subagent profile `{}` is disabled by `[task] disabled_agents`",
            profile.name
        )));
    }
    if let Some(parent) = parent_profile {
        check_spawn_policy(parent, &profile)?;
    }
    Ok(profile)
}

fn profiles_same(a: &SubagentProfile, b: &SubagentProfile) -> bool {
    a.name.eq_ignore_ascii_case(&b.name)
        || a.aliases.iter().any(|x| x.eq_ignore_ascii_case(&b.name))
        || b.aliases.iter().any(|x| x.eq_ignore_ascii_case(&a.name))
}

fn check_spawn_policy(parent: &SubagentProfile, target: &SubagentProfile) -> Result {
    let is_self = profiles_same(parent, target);
    if let Some(allow) = &parent.spawns {
        if allow.is_empty() {
            return Err(HrnError::Tool(format!(
                "agent `{}` is not permitted to spawn subagents (`spawns` is empty)",
                parent.name
            )));
        }
        let explicitly_named = allow.iter().any(|entry| {
            entry.eq_ignore_ascii_case(&target.name)
                || target.aliases.iter().any(|a| entry.eq_ignore_ascii_case(a))
        });
        let wildcard = allow.iter().any(|entry| entry == "*");
        if !explicitly_named && !wildcard {
            return Err(HrnError::Tool(format!(
                "agent `{}` cannot spawn `{}` (allowed `spawns`: {})",
                parent.name,
                target.name,
                allow.join(", ")
            )));
        }
        if is_self && !explicitly_named {
            return Err(HrnError::Tool(format!(
                "self-recursion blocked (`blocked_agent`): agent `{}` cannot spawn itself",
                parent.name
            )));
        }
        return Ok(());
    }
    if is_self {
        return Err(HrnError::Tool(format!(
            "self-recursion blocked (`blocked_agent`): agent `{}` cannot spawn itself",
            parent.name
        )));
    }
    Ok(())
}

fn parse_task_and_steps(
    item: &Value,
    idx: usize,
    cfg: &TaskConfig,
) -> Result<(String, Vec<String>)> {
    let raw_task = item
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    if let Some(raw_steps) = item.get("steps") {
        if !cfg.steps {
            return Err(HrnError::Tool(
                "task: `steps` is disabled (enable `[task] steps = true`)".into(),
            ));
        }
        let arr = raw_steps
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                HrnError::Tool(format!(
                    "task: tasks[{idx}].steps must be a non-empty array of strings"
                ))
            })?;
        let mut parsed = Vec::with_capacity(arr.len());
        for (si, val) in arr.iter().enumerate() {
            let text = val
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    HrnError::Tool(format!(
                        "task: tasks[{idx}].steps[{si}] must be a non-empty string"
                    ))
                })?;
            parsed.push(text.to_owned());
        }
        let mut bucketed = bucket_steps(&parsed);
        let summary = raw_task
            .clone()
            .unwrap_or_else(|| parsed.first().cloned().unwrap_or_default());
        if let Some(intro) = raw_task
            && let Some(first) = bucketed.first_mut()
        {
            *first = format!("{intro}\n\n{first}");
        }
        return Ok((summary, bucketed));
    }
    let task = raw_task.ok_or_else(|| {
        HrnError::Tool(format!(
            "task: tasks[{idx}] needs a non-empty string `task`"
        ))
    })?;
    Ok((task, Vec::new()))
}

fn parse_solution_space(
    item: &Value,
    fallback_task: &str,
    label: &str,
    warnings: &mut Vec<String>,
) -> Result<String> {
    match item
        .get("solutionSpace")
        .or_else(|| item.get("solution_space"))
    {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        None | Some(Value::Null | Value::String(_)) => {
            warnings.push(format!(
                "`{label}`: `solutionSpace` was blank or missing; fell back to `task` text"
            ));
            Ok(fallback_task.to_owned())
        }
        Some(_) => Err(HrnError::Tool(
            "task: `solutionSpace` must be a string".into(),
        )),
    }
}

fn parse_effort(item: &Value, cfg: &TaskConfig) -> Result<Option<ThinkingLevel>> {
    let Some(raw) = item.get("effort") else {
        return Ok(None);
    };
    if !cfg.enable_effort {
        return Err(HrnError::Tool(
            "task: `effort` is disabled (enable `[task] enable_effort = true`)".into(),
        ));
    }
    let s = raw
        .as_str()
        .ok_or_else(|| HrnError::Tool("task: `effort` must be one of `lo`, `med`, `hi`".into()))?;
    match s.trim().to_ascii_lowercase().as_str() {
        "lo" => Ok(Some(ThinkingLevel::Low)),
        "med" => Ok(Some(ThinkingLevel::Medium)),
        "hi" => Ok(Some(ThinkingLevel::High)),
        other => Err(HrnError::Tool(format!(
            "task: invalid `effort` `{other}` (expected `lo`, `med`, or `hi`)"
        ))),
    }
}

fn parse_structured(
    item: &Value,
    cfg: &TaskConfig,
    profile: &SubagentProfile,
) -> Result<(Option<Value>, SchemaMode)> {
    let raw_schema = item.get("outputSchema");
    let raw_mode = item.get("schemaMode");
    if (raw_schema.is_some() || raw_mode.is_some()) && !cfg.structured && !profile.structured {
        return Err(HrnError::Tool(
            "task: `outputSchema`/`schemaMode` require `[task] structured = true` or a profile with `structured: true`"
                .into(),
        ));
    }
    let mode = match raw_mode {
        None => SchemaMode::Permissive,
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "permissive" => SchemaMode::Permissive,
            "strict" => SchemaMode::Strict,
            other => {
                return Err(HrnError::Tool(format!(
                    "task: invalid `schemaMode` `{other}` (expected `permissive` or `strict`)"
                )));
            }
        },
        Some(_) => {
            return Err(HrnError::Tool(
                "task: `schemaMode` must be `permissive` or `strict`".into(),
            ));
        }
    };
    let item_schema = match raw_schema {
        None => None,
        Some(v @ (Value::Object(_) | Value::Bool(_))) => Some(v.clone()),
        Some(_) => {
            return Err(HrnError::Tool(
                "task: `outputSchema` must be a JSON Schema object".into(),
            ));
        }
    };
    Ok((item_schema.or_else(|| profile.output.clone()), mode))
}

fn parse_isolated(
    item: &Value,
    ctx: &ToolContext,
    runtime: &AgentRuntime,
    cfg: &TaskConfig,
) -> Result<bool> {
    let isolated = match item.get("isolated") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(HrnError::Tool("task: `isolated` must be a boolean".into())),
    };
    if isolated {
        if ctx.permission_mode == PermissionMode::Plan {
            return Err(HrnError::Tool(
                "task: `isolated: true` is rejected in Plan mode".into(),
            ));
        }
        if !cfg.isolation.enabled {
            return Err(HrnError::Tool(
                "task: `isolated: true` requires `[task.isolation] enabled = true`".into(),
            ));
        }
        if runtime.isolator().is_none() {
            return Err(HrnError::Tool(
                "task: worktree isolation is not configured in this runtime".into(),
            ));
        }
    }
    Ok(isolated)
}

fn parse_dynamic_tools(
    item: &Value,
    ctx: &ToolContext,
    runtime: &AgentRuntime,
) -> Result<Vec<String>> {
    let Some(raw) = item.get("tools") else {
        return Ok(Vec::new());
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| HrnError::Tool("task: `tools` must be an array of tool names".into()))?;
    let mut names = Vec::with_capacity(arr.len());
    for entry in arr {
        let name = entry
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                HrnError::Tool("task: `tools` entries must be non-empty strings".into())
            })?;
        names.push(name.to_owned());
    }
    if !names.is_empty() {
        if ctx.permission_mode == PermissionMode::Plan {
            return Err(HrnError::Tool(
                "task: custom `tools` are rejected in Plan mode".into(),
            ));
        }
        if runtime.dynamic_tools().is_none() {
            return Err(HrnError::Tool(
                "eval kernel tools are not enabled in this runtime".into(),
            ));
        }
    }
    Ok(names)
}

fn parse_model(
    item: &Value,
    runtime: &AgentRuntime,
    cfg: &TaskConfig,
    profile: &SubagentProfile,
) -> Result<(Option<String>, bool, Option<ThinkingLevel>)> {
    let agent_override = cfg.model_override_for(&profile.name, &profile.aliases);
    if let Some(raw_val) = item.get("model") {
        return validate_per_call_model(raw_val, runtime, cfg, profile, agent_override);
    }
    if let Some(override_sel) = agent_override {
        let (base, level) = split_selector(override_sel);
        return Ok((Some(base), false, level));
    }
    if let Some(prof_sel) = &profile.model {
        let (base, level) = split_selector(prof_sel);
        return Ok((Some(base), true, level));
    }
    Ok((None, false, None))
}

fn validate_per_call_model(
    raw_val: &Value,
    runtime: &AgentRuntime,
    cfg: &TaskConfig,
    profile: &SubagentProfile,
    agent_override: Option<&str>,
) -> Result<(Option<String>, bool, Option<ThinkingLevel>)> {
    let raw = raw_val
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HrnError::Tool("task: `model` must be a non-empty string".into()))?;
    if cfg.per_call_model == "off" {
        return Err(HrnError::Tool(
            "task: per-call `model` override is disabled (`[task] per_call_model = \"off\"`)"
                .into(),
        ));
    }
    if agent_override.is_some() {
        return Err(HrnError::Tool(format!(
            "task: per-call `model` conflicts with `[task.agent_models]` override for `{}`",
            profile.name
        )));
    }
    if profile
        .model
        .as_deref()
        .is_some_and(|m| split_level_suffix(m).1 != LevelSuffix::None)
    {
        return Err(HrnError::Tool(format!(
            "task: per-call `model` conflicts with level-tagged profile `{}`",
            profile.name
        )));
    }
    let (base, level) = split_selector(raw);
    let valid_shape = base.split_once('/').is_some_and(|(prov, id)| {
        !prov.is_empty()
            && !id.is_empty()
            && !prov.starts_with('@')
            && !base.contains(',')
            && !base.chars().any(char::is_whitespace)
    });
    if !valid_shape {
        return Err(HrnError::Tool(format!(
            "task: per-call `model` `{raw}` must use exact `provider/id[:level]` format"
        )));
    }
    let granted = runtime
        .llm
        .models()
        .iter()
        .any(|m| format!("{}/{}", m.provider, m.id).eq_ignore_ascii_case(&base));
    if !granted {
        return Err(HrnError::Tool(format!(
            "task: per-call `model` `{raw}` is not an available model"
        )));
    }
    if !cfg.per_call_model_allow.is_empty()
        && !matches_model_allow(&cfg.per_call_model_allow, &base, raw)
    {
        return Err(HrnError::Tool(format!(
            "task: per-call `model` `{raw}` is not allowed by `[task] per_call_model_allow`"
        )));
    }
    Ok((Some(base), false, level))
}

fn split_selector(raw: &str) -> (String, Option<ThinkingLevel>) {
    let (base, suffix) = split_level_suffix(raw.trim());
    let level = match suffix {
        LevelSuffix::Level(l) => Some(l),
        LevelSuffix::None | LevelSuffix::Inherit => None,
    };
    (base.to_owned(), level)
}

fn matches_model_allow(allow: &[String], base: &str, raw: &str) -> bool {
    allow.iter().any(|rule| {
        let trimmed = rule.trim();
        if let Some(prefix) = trimmed.strip_suffix('*') {
            let low = prefix.to_ascii_lowercase();
            base.to_ascii_lowercase().starts_with(&low)
                || raw.to_ascii_lowercase().starts_with(&low)
        } else {
            base.eq_ignore_ascii_case(trimmed) || raw.eq_ignore_ascii_case(trimmed)
        }
    })
}

// ===== merged from subagent_schema.rs =====
// Dynamic JSON Schema builder for `task` tool parameters, `SchemaMode`, and `steps[]`
// prompt bucketing (`<= 4` prompts; `> 4` steps merged evenly into 4 buckets).

use serde_json::{Map, json};

/// Maximum prompt buckets for multi-step `steps[]` chains (`> 4` steps are merged into 4 buckets).
pub(crate) const MAX_STEP_BUCKETS: usize = 4;

/// How structured `outputSchema` mismatches are handled after yield reminders are exhausted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SchemaMode {
    /// Accept non-conforming output with a warning note (default).
    #[default]
    Permissive,
    /// Fail the agent with a validation error when output does not conform.
    Strict,
}

/// Build the dynamic JSON Schema for `task` tool parameters from [`TaskConfig`].
/// Disabled fields (`effort`, `steps`, `outputSchema`/`schemaMode`, `isolated`, `model`) are
/// omitted from the schema so the model only sees active capabilities.
#[must_use]
pub(crate) fn build_spec_parameters(cfg: &TaskConfig, has_dynamic_tools: bool) -> Value {
    let mut item_props = Map::new();
    insert_core_schema(&mut item_props, cfg, has_dynamic_tools);
    insert_scope_schema(&mut item_props);

    let item_required = if cfg.steps {
        json!(["solutionSpace"])
    } else {
        json!(["task", "solutionSpace"])
    };

    if cfg.batch {
        json!({
            "type": "object",
            "properties": {
                "context": {
                    "type": "string",
                    "description": "Required shared background given to every agent of this call."
                },
                "tasks": {
                    "type": "array",
                    "minItems": 1,
                    "description": "One child agent per item; they run concurrently.",
                    "items": {
                        "type": "object",
                        "properties": item_props,
                        "required": item_required
                    }
                }
            },
            "required": ["context", "tasks"]
        })
    } else {
        item_props.insert(
            "context".to_owned(),
            json!({
                "type": "string",
                "description": "Optional background context for the agent."
            }),
        );
        json!({
            "type": "object",
            "properties": item_props,
            "required": item_required
        })
    }
}

fn insert_core_schema(
    item_props: &mut Map<String, Value>,
    cfg: &TaskConfig,
    has_dynamic_tools: bool,
) {
    item_props.insert(
        "name".to_owned(),
        json!({"type": "string", "description": "Short unique name (<=32 chars)."}),
    );
    item_props.insert(
        "agent".to_owned(),
        json!({"type": "string", "description": "Profile (`scout`, `task`, `sonic`, `plan`, `reviewer`, `security-reviewer`, or custom). Default `scout`."}),
    );
    item_props.insert(
        "task".to_owned(),
        json!({"type": "string", "description": "Self-contained instructions for this agent."}),
    );
    item_props.insert(
        "solutionSpace".to_owned(),
        json!({"type": "string", "description": "Brief summary of the solution space and complexity."}),
    );
    if cfg.steps {
        item_props.insert(
            "steps".to_owned(),
            json!({"type": "array", "items": {"type": "string"}, "description": "Sequential step prompts (<=4; >4 bucketed into 4)."}),
        );
    }
    if cfg.enable_effort {
        item_props.insert(
            "effort".to_owned(),
            json!({"type": "string", "enum": ["lo", "med", "hi"], "description": "Reasoning effort override."}),
        );
    }
    if cfg.structured {
        item_props.insert(
            "outputSchema".to_owned(),
            json!({"type": "object", "description": "JSON Schema enforced on the child's hidden yield result."}),
        );
        item_props.insert(
            "schemaMode".to_owned(),
            json!({"type": "string", "enum": ["permissive", "strict"], "description": "Schema enforcement mode (default permissive)."}),
        );
    }
    if cfg.isolation.enabled {
        item_props.insert(
            "isolated".to_owned(),
            json!({"type": "boolean", "description": "Run child in a dedicated git worktree."}),
        );
    }
    if cfg.per_call_model != "off" {
        item_props.insert(
            "model".to_owned(),
            json!({"type": "string", "description": "Exact `provider/id[:level]` model override."}),
        );
    }
    if has_dynamic_tools {
        item_props.insert(
            "tools".to_owned(),
            json!({"type": "array", "items": {"type": "string"}, "description": "Eval-defined tool names exposed to this child."}),
        );
    }
}

fn insert_scope_schema(props: &mut Map<String, Value>) {
    props.insert(
        "allowed_paths".to_owned(),
        json!({"type": "array", "items": {"type": "string"}, "description": "Allowed relative path globs/prefixes for file edits."}),
    );
    props.insert(
        "denied_paths".to_owned(),
        json!({"type": "array", "items": {"type": "string"}, "description": "Blocked relative path globs/prefixes (overrides allowed_paths)."}),
    );
    props.insert(
        "read_only".to_owned(),
        json!({"type": "boolean", "description": "Force read-only mode."}),
    );
    props.insert(
        "allow_tools".to_owned(),
        json!({"type": "array", "items": {"type": "string"}, "description": "Explicit tool allowlist."}),
    );
    props.insert(
        "deny_tools".to_owned(),
        json!({"type": "array", "items": {"type": "string"}, "description": "Explicit tool denylist."}),
    );
    props.insert(
        "allow_bash_with_paths".to_owned(),
        json!({"type": "boolean", "description": "Keep `bash` enabled when `allowed_paths` is set."}),
    );
    props.insert(
        "max_steps".to_owned(),
        json!({"type": "integer", "description": "Optional step limit for this run; overrides `[task] max_steps` / `[task.agent_max_steps]` (0 = unlimited)."}),
    );
}

/// Bucket `steps` into at most [`MAX_STEP_BUCKETS`] (4) prompts.
/// When `steps.len() <= 4`, each step stays its own prompt; when `> 4`, steps are evenly
/// front-loaded into 4 buckets (e.g. `5 -> 2-1-1-1`, `6 -> 2-2-1-1`).
#[must_use]
pub(crate) fn bucket_steps(steps: &[String]) -> Vec<String> {
    let n = steps.len();
    if n <= MAX_STEP_BUCKETS {
        return steps.to_vec();
    }
    let base = n / MAX_STEP_BUCKETS;
    let rem = n % MAX_STEP_BUCKETS;
    let mut out = Vec::with_capacity(MAX_STEP_BUCKETS);
    let mut cursor = 0usize;
    for bucket_idx in 0..MAX_STEP_BUCKETS {
        let count = base + usize::from(bucket_idx < rem);
        let slice = steps.get(cursor..cursor + count).unwrap_or_default();
        cursor += count;
        if slice.len() == 1 {
            if let Some(only) = slice.first() {
                out.push(only.clone());
            }
        } else {
            let mut combined = String::new();
            for (i, s) in slice.iter().enumerate() {
                if i > 0 {
                    combined.push_str("\n\n");
                }
                let step_num = cursor - count + i + 1;
                combined.push_str("Step ");
                combined.push_str(&step_num.to_string());
                combined.push_str(":\n");
                combined.push_str(s);
            }
            out.push(combined);
        }
    }
    out
}
