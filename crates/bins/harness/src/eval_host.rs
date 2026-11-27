//! Binary-layer [`EvalHost`] and [`DynamicToolResolver`] adapters wiring `hrn-mod-eval` to the
//! assembled [`AgentRuntime`] cleanly without reference cycles or module-feature coupling.

use hrn_edge_tools::EvalDynamicToolsSlot;
use hrn_harness::{AgentRecord, AgentRuntime, DynamicToolResolver, EvalRuntimeParts, JobStatus};
use hrn_kernel::{BoxFuture, EvalHost, HrnError, Result, Tool, ToolContext, ToolOutput};
use serde_json::{Map, Value, json};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::time::Instant;

/// Poll fallback interval inside [`RuntimeEvalHost::wait_agents`] in case a status transition
/// happened without waking the inbox.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Late-bound, non-cyclic [`EvalHost`] implementation backed by weak links to the assembled
/// [`AgentRuntime`].
pub(crate) struct RuntimeEvalHost {
    bound: OnceLock<EvalRuntimeParts>,
}

impl RuntimeEvalHost {
    /// Construct an unbound host; call [`Self::bind`] immediately after assembling [`AgentRuntime`].
    pub(crate) fn new() -> Self {
        Self {
            bound: OnceLock::new(),
        }
    }

    /// Bind weak references to `runtime` once assembly finishes (no strong `Arc` cycle).
    pub(crate) fn bind(&self, runtime: &AgentRuntime) {
        let _ = self.bound.set(runtime.eval_runtime_parts());
    }

    fn upgrade_runtime(&self) -> Result<AgentRuntime> {
        self.bound
            .get()
            .and_then(EvalRuntimeParts::upgrade_runtime)
            .ok_or_else(|| HrnError::Tool("eval host: owning runtime is no longer active".into()))
    }

    fn upgrade_agents(&self) -> Result<Arc<hrn_harness::AgentRegistry>> {
        self.bound
            .get()
            .and_then(EvalRuntimeParts::agents)
            .ok_or_else(|| HrnError::Tool("eval host: owning runtime is no longer active".into()))
    }
}

impl EvalHost for RuntimeEvalHost {
    fn call_tool(
        &self,
        ctx: &ToolContext,
        tool_name: &str,
        args: Value,
    ) -> BoxFuture<Result<ToolOutput>> {
        let ctx = ctx.clone();
        let name = tool_name.to_owned();
        let runtime = self.upgrade_runtime();
        Box::pin(async move {
            if name == "eval" {
                return Ok(ToolOutput::err(
                    "tool `eval` cannot be called recursively from inside eval",
                ));
            }
            if matches!(name.as_str(), "checkpoint" | "rewind" | "yield") {
                return Ok(ToolOutput::err(format!(
                    "tool `{name}` cannot be called from eval: session-control tools \
                     (`checkpoint`, `rewind`, `yield`) are restricted to direct agent calls"
                )));
            }
            let runtime = runtime?;
            runtime.execute_single_tool(&ctx, &name, args).await
        })
    }

    fn spawn_agent(&self, ctx: &ToolContext, request: Value) -> BoxFuture<Result<Value>> {
        let ctx = ctx.clone();
        let runtime = self.upgrade_runtime();
        Box::pin(async move {
            let runtime = runtime?;
            let normalized = normalize_spawn_request(&request)?;
            let record = runtime.spawn_eval_agent(&ctx, normalized).await?;
            let snap = record.snapshot();
            Ok(json!({
                "id": record.id,
                "name": record.name,
                "profile": record.profile,
                "status": snap.status.as_str(),
                "session_id": snap.session_id,
            }))
        })
    }

    fn wait_agents(
        &self,
        ctx: &ToolContext,
        ids: Vec<String>,
        timeout_ms: Option<u64>,
    ) -> BoxFuture<Result<Value>> {
        let ctx = ctx.clone();
        let agents = self.upgrade_agents();
        Box::pin(async move {
            let agents = agents?;
            let records = resolve_agent_records(&agents, &ctx, &ids)?;
            let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
            wait_for_records(&ctx, &records, timeout_ms, deadline).await
        })
    }

    fn complete(
        &self,
        ctx: &ToolContext,
        prompt: String,
        role: Option<String>,
    ) -> BoxFuture<Result<String>> {
        let session_id = ctx.session_id.clone();
        let cancel = ctx.cancel.clone();
        let runtime = self.upgrade_runtime();
        Box::pin(async move {
            let runtime = runtime?;
            runtime
                .complete_text(&session_id, prompt, role, &cancel)
                .await
        })
    }
}

/// Bridges `hrn-edge-tools::EvalDynamicToolsSlot` into `hrn-harness::DynamicToolResolver`.
pub(crate) struct SharedDynamicToolResolver {
    slot: Arc<EvalDynamicToolsSlot>,
}

impl SharedDynamicToolResolver {
    pub(crate) fn new(slot: Arc<EvalDynamicToolsSlot>) -> Self {
        Self { slot }
    }
}

impl DynamicToolResolver for SharedDynamicToolResolver {
    fn resolve(&self, session_id: &str, names: &[String]) -> Result<Vec<Arc<dyn Tool>>> {
        let lookup = self.slot.get().ok_or_else(|| {
            HrnError::Tool("eval kernel tools are not enabled in this runtime".to_owned())
        })?;
        lookup.resolve(session_id, names)
    }
}

fn normalize_spawn_request(request: &Value) -> Result<Value> {
    let prompt = request
        .get("prompt")
        .or_else(|| request.get("task"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let opts = request
        .get("opts")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = Map::new();
    for (k, v) in &opts {
        if !v.is_null() {
            out.insert(k.clone(), v.clone());
        }
    }
    if !out.contains_key("name")
        && let Some(label) = out.remove("label")
    {
        out.insert("name".to_owned(), label);
    }
    if !out.contains_key("outputSchema")
        && let Some(schema) = out.remove("schema")
    {
        out.insert("outputSchema".to_owned(), schema);
    }
    if !out.contains_key("task") && !out.contains_key("steps") {
        let p = prompt.ok_or_else(|| {
            HrnError::Tool("eval agent(): `prompt` must be a non-empty string".to_owned())
        })?;
        out.insert("task".to_owned(), Value::String(p.to_owned()));
    } else if let Some(p) = prompt
        && !out.contains_key("task")
    {
        out.insert("task".to_owned(), Value::String(p.to_owned()));
    }
    if !out.contains_key("solutionSpace") && !out.contains_key("solution_space") {
        let fallback = prompt
            .or_else(|| out.get("task").and_then(Value::as_str))
            .unwrap_or("eval subagent task");
        out.insert(
            "solutionSpace".to_owned(),
            Value::String(fallback.to_owned()),
        );
    }
    Ok(Value::Object(out))
}

fn resolve_agent_records(
    agents: &hrn_harness::AgentRegistry,
    ctx: &ToolContext,
    ids: &[String],
) -> Result<Vec<Arc<AgentRecord>>> {
    let root = if ctx.root_session_id.is_empty() {
        ctx.session_id.as_str()
    } else {
        ctx.root_session_id.as_str()
    };
    let mut records = Vec::with_capacity(ids.len());
    for id in ids {
        let rec = agents
            .get_in_tree(root, id)
            .ok_or_else(|| HrnError::Tool(format!("unknown agent `{id}`")))?;
        records.push(rec);
    }
    Ok(records)
}

async fn wait_for_records(
    ctx: &ToolContext,
    records: &[Arc<AgentRecord>],
    timeout_ms: Option<u64>,
    deadline: Option<Instant>,
) -> Result<Value> {
    loop {
        let seen = ctx.inbox.version();
        let all_done = records
            .iter()
            .all(|rec| rec.snapshot().status != JobStatus::Running);
        let expired = timeout_ms == Some(0) || deadline.is_some_and(|at| Instant::now() >= at);
        if all_done || expired {
            let mut results = Vec::with_capacity(records.len());
            for rec in records {
                let snap = rec.snapshot();
                let running = snap.status == JobStatus::Running;
                if !running {
                    // Consuming the inbox delivery prevents double delivery for waited handles;
                    // unwaited handles stay in the inbox and auto-deliver to the session.
                    let _ = ctx.inbox.take_source(&ctx.session_id, &rec.id);
                }
                results.push(json!({
                    "id": rec.id,
                    "name": rec.name,
                    "profile": rec.profile,
                    "status": snap.status.as_str(),
                    "output": snap.output,
                    "yield_json": snap.yield_json,
                    "steps": snap.steps,
                    "done": !running,
                    "timed_out": running && expired,
                }));
            }
            return Ok(Value::Array(results));
        }

        let wake = ctx.inbox.changed(seen);
        let cancel = ctx.cancel.cancelled();
        let next_poll = Instant::now() + WAIT_POLL_INTERVAL;
        let sleep_until = deadline.map_or(next_poll, |d| d.min(next_poll));
        tokio::select! {
            () = wake => {}
            () = cancel => return Err(HrnError::Cancelled),
            () = tokio::time::sleep_until(sleep_until) => {}
        }
    }
}

#[cfg(test)]
#[path = "eval_host_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "eval_host_security_tests.rs"]
mod security_tests;
