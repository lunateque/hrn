//! `hrn mcp list | ping | serve`: MCP client inspection and strict stdio MCP server.

use crate::config::Config;
use crate::paths;
use crate::term::{err_line, out};
use crate::wiring::{build_runtime, open_store};
use hrn_edge_http::HttpClient;
use hrn_edge_llm::resolve_alias;
use hrn_edge_mcp::{
    CallToolResult, McpCache, McpClient, McpServerConfig, McpServerHandler, McpToolSpec,
    discover_mcp_configs, serve_stdio,
};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{AgentRuntime, ScopePolicy, SessionControl, TurnIo};
use hrn_kernel::{
    AgentEvent, ApprovalDecision, ApprovalGate, ApprovalRequest, BoxFuture, CancelFlag, EventSink,
    HrnError, PermissionMode, Result, ThinkingLevel, WorkspaceFs,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Resolve merged MCP server configs from global JSON, `config.toml`, `.mcp.json`, and
/// `.harness/mcp.json`.
pub(crate) fn resolve_servers(cfg: &Config, cwd: &Path) -> (Vec<McpServerConfig>, Vec<String>) {
    discover_mcp_configs(&StdWorkspaceFs, cwd, &cfg.mcp_servers)
}

/// The tool-list cache for this user, if a home directory exists.
pub(crate) fn tool_cache() -> Option<McpCache> {
    paths::mcp_cache_path(StdWorkspaceFs.home_dir().as_deref()).map(McpCache::new)
}

/// `hrn mcp list`: display all discovered MCP servers without connecting.
pub(crate) fn list(cfg: &Config, cwd: &Path) {
    let (servers, warnings) = resolve_servers(cfg, cwd);
    for warn in warnings {
        err_line(&format!("warning: {warn}"));
    }
    if servers.is_empty() {
        out("no MCP servers configured or discovered\n");
        return;
    }
    out("NAME\tTRANSPORT\tSTATUS\tAUTO_APPROVE\tSOURCE\tTARGET\n");
    for s in &servers {
        let status = if s.enabled { "enabled" } else { "disabled" };
        let approve = if s.auto_approve { "yes" } else { "confirm" };
        out(&format!(
            "{}\t{}\t{status}\t{approve}\t{}\t{}\n",
            s.name,
            s.transport.kind_label(),
            s.source,
            s.transport.target_summary()
        ));
    }
}

/// `hrn mcp ping [server]`: connect to one or all enabled servers, measure RTT and list tools.
pub(crate) async fn ping(cfg: &Config, cwd: &Path, target: Option<&str>) -> Result {
    let (servers, warnings) = resolve_servers(cfg, cwd);
    for warn in warnings {
        err_line(&format!("warning: {warn}"));
    }
    let selected: Vec<McpServerConfig> = match target {
        Some(name) => {
            let found = servers
                .into_iter()
                .find(|s| s.name == name)
                .ok_or_else(|| {
                    HrnError::Config(format!(
                        "MCP server `{name}` not found in config or .mcp.json"
                    ))
                })?;
            vec![found]
        }
        None => servers.into_iter().filter(|s| s.enabled).collect(),
    };
    if selected.is_empty() {
        out("no enabled MCP servers to ping\n");
        return Ok(());
    }

    let http = HttpClient::new()?;
    let cache = tool_cache();
    let mut failed = 0usize;
    for srv in selected {
        let name = srv.name.clone();
        let kind = srv.transport.kind_label();
        match McpClient::connect(srv, http.clone()).await {
            Ok(client) => {
                let rtt = client.ping().await;
                let tools = client.list_tools().await;
                client.shutdown().await;
                match (rtt, tools) {
                    (Ok(dur), Ok(list)) => {
                        if let Some(cache) = &cache
                            && let Err(err) = cache.record_tools(client.config(), list.clone())
                        {
                            err_line(&format!("warning: {err}"));
                        }
                        let names: Vec<&str> = list.iter().map(|t| t.name.as_str()).collect();
                        out(&format!(
                            "{name}\t{kind}\tok\t{}ms\t{} tool(s): {}\n",
                            dur.as_millis(),
                            list.len(),
                            names.join(", ")
                        ));
                    }
                    (Err(err), _) | (_, Err(err)) => {
                        failed += 1;
                        out(&format!("{name}\t{kind}\terror\t{err}\n"));
                    }
                }
            }
            Err(err) => {
                failed += 1;
                out(&format!("{name}\t{kind}\terror\t{err}\n"));
            }
        }
    }
    if failed > 0 {
        return Err(HrnError::Tool(format!(
            "{failed} MCP server(s) failed ping"
        )));
    }
    Ok(())
}

/// `hrn mcp serve [--profile default|agy]`: run a strict stdio MCP server exposing agent tools.
pub(crate) async fn serve(cfg: &Config, db_path: &Path, profile: &str) -> Result {
    let store = open_store(db_path, false)?;
    let runtime = build_runtime(cfg, store)?;
    let agy_profile = profile.eq_ignore_ascii_case("agy");
    let handler = AgentMcpServer::new(cfg.clone(), runtime, agy_profile);
    serve_stdio(&handler).await
}

struct FixedApproval(bool);

impl ApprovalGate for FixedApproval {
    fn await_decision(
        &self,
        _request: ApprovalRequest,
        _cancel: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        let allow = ApprovalDecision::from_bool(self.0);
        Box::pin(async move { Ok(allow) })
    }
}

#[derive(Default)]
struct TextCollector(Mutex<String>);

impl TextCollector {
    fn text(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl EventSink for TextCollector {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        if let AgentEvent::TextDelta { delta } = event {
            let mut guard = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.push_str(&delta);
        }
        Box::pin(async { true })
    }
}

#[derive(Clone, Debug)]
enum JobState {
    Running,
    Completed(Value),
    Failed(String),
}

struct JobEntry {
    id: String,
    cwd: String,
    model: String,
    started: Instant,
    state_rx: watch::Receiver<JobState>,
}

struct AgentMcpServer {
    cfg: Config,
    runtime: AgentRuntime,
    agy_profile: bool,
    next_job: AtomicU64,
    jobs: Mutex<BTreeMap<String, JobEntry>>,
}

struct RunParams {
    prompt: String,
    cwd: PathBuf,
    model: String,
    thinking: ThinkingLevel,
    approve: bool,
    scope: ScopePolicy,
}

impl AgentMcpServer {
    fn new(cfg: Config, runtime: AgentRuntime, agy_profile: bool) -> Self {
        Self {
            cfg,
            runtime,
            agy_profile,
            next_job: AtomicU64::new(1),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    fn resolve_run_params(&self, args: &Value) -> Result<RunParams> {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| HrnError::Tool("missing required string field `prompt`".into()))?
            .to_owned();
        let cwd_raw = match args.get("cwd").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => std::env::current_dir()?,
        };
        let cwd = std::path::absolute(&cwd_raw)?;
        if !cwd.is_dir() {
            return Err(HrnError::Tool(format!(
                "`cwd` is not a directory: {}",
                cwd.display()
            )));
        }
        let model_arg = args.get("model").and_then(Value::as_str);
        let model = resolve_alias(&self.cfg.llm, model_arg)?;
        let thinking = match args.get("thinking").and_then(Value::as_str) {
            Some(raw) => ThinkingLevel::parse(raw)
                .ok_or_else(|| HrnError::Tool(format!("invalid `thinking` level `{raw}`")))?,
            None => self.cfg.thinking,
        };
        let approve = args
            .get("yes")
            .and_then(Value::as_bool)
            .unwrap_or(self.agy_profile);
        let scope = ScopePolicy::from_tool_args(args);
        Ok(RunParams {
            prompt,
            cwd,
            model,
            thinking,
            approve,
            scope,
        })
    }

    async fn execute_turn(runtime: AgentRuntime, params: RunParams) -> Result<Value> {
        let scoped = params.scope.apply_to_runtime(&runtime, params.cwd.clone());
        let perm = if params.scope.read_only {
            PermissionMode::Plan
        } else {
            PermissionMode::Default
        };
        let title: String = params.prompt.chars().take(72).collect();
        let session = scoped.create_session(
            title,
            &params.cwd,
            Some(params.model),
            params.thinking,
            perm,
        )?;
        let collector = Arc::new(TextCollector::default());
        let io = TurnIo {
            cancel: CancelFlag::new(),
            events: Arc::clone(&collector) as Arc<dyn EventSink>,
            approval: Some(Arc::new(FixedApproval(params.approve))),
            ask: None,
        };
        let done = scoped
            .run_turn(&session.id, &params.prompt, &SessionControl::default(), io)
            .await?;
        Ok(json!({
            "ok": true,
            "session_id": session.id,
            "model": done.model,
            "steps": done.steps,
            "output": collector.text(),
            "usage": done.usage,
        }))
    }

    fn spawn_job(&self, args: &Value) -> Result<CallToolResult> {
        let params = self.resolve_run_params(args)?;
        let seq = self.next_job.fetch_add(1, Ordering::Relaxed);
        let id = format!("job-{seq}");
        let (tx, rx) = watch::channel(JobState::Running);
        let entry = JobEntry {
            id: id.clone(),
            cwd: params.cwd.display().to_string(),
            model: params.model.clone(),
            started: Instant::now(),
            state_rx: rx,
        };
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), entry);

        let runtime = self.runtime.clone();
        tokio::spawn(async move {
            match Self::execute_turn(runtime, params).await {
                Ok(val) => {
                    let _ = tx.send(JobState::Completed(val));
                }
                Err(err) => {
                    let _ = tx.send(JobState::Failed(err.to_string()));
                }
            }
        });
        let payload = json!({ "job_id": id, "status": "running" });
        Ok(CallToolResult::text(payload.to_string()))
    }

    fn list_jobs(&self) -> CallToolResult {
        let jobs: Vec<Value> = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|j| {
                let status = match &*j.state_rx.borrow() {
                    JobState::Running => "running",
                    JobState::Completed(_) => "completed",
                    JobState::Failed(_) => "failed",
                };
                json!({
                    "job_id": j.id,
                    "status": status,
                    "cwd": j.cwd,
                    "model": j.model,
                    "elapsed_ms": j.started.elapsed().as_millis(),
                })
            })
            .collect();
        CallToolResult::text(json!({ "jobs": jobs }).to_string())
    }
}

impl McpServerHandler for AgentMcpServer {
    fn server_name(&self) -> &'static str {
        if self.agy_profile { "hrn-agy" } else { "hrn" }
    }

    fn list_tools(&self) -> Vec<McpToolSpec> {
        let run_schema = json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "Task prompt for the hrn agent." },
                "cwd": { "type": "string", "description": "Working directory (defaults to server cwd)." },
                "model": { "type": "string", "description": "Configured model alias or role." },
                "thinking": { "type": "string", "description": "off | low | medium | high | xhigh." },
                "yes": { "type": "boolean", "description": "Auto-approve mutating tool calls." },
                "allowed_paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Mechanically enforced workspace-relative globs or directory prefixes for file edits (drops bash unless allow_bash_with_paths=true)."
                },
                "denied_paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Mechanically enforced denied path globs or directory prefixes (always wins over allowed_paths)."
                },
                "read_only": {
                    "type": "boolean",
                    "description": "Strip all mutating tools and run in Plan permission mode."
                },
                "allow_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional explicit tool allowlist."
                },
                "deny_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional explicit tool denylist."
                },
                "allow_bash_with_paths": {
                    "type": "boolean",
                    "description": "Keep bash enabled even when allowed_paths/denied_paths are active (default false)."
                },
                "max_steps": {
                    "type": "integer",
                    "description": "Hard cap on LLM/tool steps for this turn."
                }
            },
            "required": ["prompt"]
        });
        vec![
            McpToolSpec {
                name: "run_agent".to_owned(),
                description: Some(
                    "Run one hrn agent turn synchronously and return its output.".to_owned(),
                ),
                input_schema: run_schema.clone(),
            },
            McpToolSpec {
                name: "spawn_agent".to_owned(),
                description: Some(
                    "Spawn an async background hrn agent job and return job_id immediately."
                        .to_owned(),
                ),
                input_schema: run_schema,
            },
            McpToolSpec {
                name: "wait_agent".to_owned(),
                description: Some(
                    "Wait for a spawned hrn agent job to finish and return its output.".to_owned(),
                ),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string", "description": "Job ID returned by spawn_agent." },
                        "timeout_secs": { "type": "integer", "description": "Max seconds to wait (default 300)." }
                    },
                    "required": ["job_id"]
                }),
            },
            McpToolSpec {
                name: "list_jobs".to_owned(),
                description: Some(
                    "List all background hrn agent jobs and their status.".to_owned(),
                ),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
        ]
    }

    fn call_tool(&self, name: &str, arguments: Value) -> BoxFuture<Result<CallToolResult>> {
        match name {
            "run_agent" => match self.resolve_run_params(&arguments) {
                Ok(params) => {
                    let runtime = self.runtime.clone();
                    Box::pin(async move {
                        let val = Self::execute_turn(runtime, params).await?;
                        Ok(CallToolResult::text(val.to_string()))
                    })
                }
                Err(err) => Box::pin(async move { Err(err) }),
            },
            "spawn_agent" => {
                let res = self.spawn_job(&arguments);
                Box::pin(async move { res })
            }
            "wait_agent" => {
                let job_id = arguments
                    .get("job_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let timeout_secs = arguments
                    .get("timeout_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(300)
                    .max(1);
                let rx_opt = self
                    .jobs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&job_id)
                    .map(|j| j.state_rx.clone());
                Box::pin(async move {
                    let Some(mut rx) = rx_opt else {
                        return Err(HrnError::Tool(format!("unknown job_id `{job_id}`")));
                    };
                    let wait_res = tokio::time::timeout(
                        Duration::from_secs(timeout_secs),
                        rx.wait_for(|st| !matches!(st, JobState::Running)),
                    )
                    .await;
                    match wait_res {
                        Ok(Ok(state)) => match &*state {
                            JobState::Completed(val) => Ok(CallToolResult::text(val.to_string())),
                            JobState::Failed(msg) => Ok(CallToolResult::error(msg.clone())),
                            JobState::Running => Ok(CallToolResult::text(
                                json!({ "job_id": job_id, "status": "running" }).to_string(),
                            )),
                        },
                        Ok(Err(_)) => Err(HrnError::Tool(format!("job `{job_id}` channel closed"))),
                        Err(_) => Ok(CallToolResult::text(
                            json!({
                                "job_id": job_id,
                                "status": "running",
                                "timed_out": true,
                            })
                            .to_string(),
                        )),
                    }
                })
            }
            "list_jobs" => {
                let res = self.list_jobs();
                Box::pin(async move { Ok(res) })
            }
            other => {
                let msg = format!("unknown MCP server tool `{other}`");
                Box::pin(async move { Err(HrnError::Tool(msg)) })
            }
        }
    }
}
