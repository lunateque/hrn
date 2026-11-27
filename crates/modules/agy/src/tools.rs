//! Harness [`Tool`] adapters (`agy_run`, `agy_spawn`, `agy_wait`, `agy_jobs`) backed by the
//! official `agy` CLI subprocess bridge.

use crate::runner::{AgyConfig, AgyRequest, AgyResponse, run_agy};
use hrn_edge_rt::tokio;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, HrnError, Result, Tool, ToolApproval, ToolContext,
    ToolEffect, ToolOutput, ToolSpec,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The tool descriptions, as data.
const DESCRIPTION_RUN: &str = include_str!("../prompts/agy_run.md");
const DESCRIPTION_SPAWN: &str = include_str!("../prompts/agy_spawn.md");
const DESCRIPTION_WAIT: &str = include_str!("../prompts/agy_wait.md");
const DESCRIPTION_JOBS: &str = include_str!("../prompts/agy_jobs.md");

#[derive(Clone, Debug)]
enum JobState {
    Running,
    Done(AgyResponse),
    Failed(String),
}

struct JobRecord {
    id: String,
    cwd: String,
    model: Option<String>,
    started: Instant,
    cancel: CancelFlag,
    state_rx: watch::Receiver<JobState>,
}

/// Shared background job pool backing `agy_spawn`, `agy_wait`, and `agy_jobs`.
#[derive(Default)]
pub struct AgyJobPool {
    next_id: AtomicU64,
    jobs: Mutex<BTreeMap<String, JobRecord>>,
}

impl AgyJobPool {
    /// Create a fresh job pool.
    pub fn new() -> Self {
        Self::default()
    }

    fn spawn(&self, cfg: AgyConfig, req: AgyRequest) -> String {
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("agy-{seq}");
        let (tx, rx) = watch::channel(JobState::Running);
        let cancel = CancelFlag::new();
        let rec = JobRecord {
            id: id.clone(),
            cwd: req.cwd.display().to_string(),
            model: req.model.clone().or_else(|| cfg.default_model.clone()),
            started: Instant::now(),
            cancel: cancel.clone(),
            state_rx: rx,
        };
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), rec);

        tokio::spawn(async move {
            match run_agy(&cfg, &req, &cancel, |_| {}).await {
                Ok(resp) => {
                    let _ = tx.send(JobState::Done(resp));
                }
                Err(err) => {
                    let _ = tx.send(JobState::Failed(err.to_string()));
                }
            }
        });
        id
    }

    fn receiver(&self, id: &str) -> Option<watch::Receiver<JobState>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .map(|r| r.state_rx.clone())
    }

    fn list(&self) -> Vec<Value> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|r| {
                let status = match &*r.state_rx.borrow() {
                    JobState::Running => "running",
                    JobState::Done(resp) if resp.exit_code == Some(0) && !resp.timed_out => {
                        "completed"
                    }
                    JobState::Done(_) | JobState::Failed(_) => "failed",
                };
                json!({
                    "job_id": r.id,
                    "status": status,
                    "cwd": r.cwd,
                    "model": r.model,
                    "elapsed_ms": r.started.elapsed().as_millis(),
                })
            })
            .collect()
    }

    fn cancel(&self, id: &str) -> bool {
        let guard = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(job) = guard.get(id) {
            job.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// Cancel all currently running `agy` background jobs.
    pub fn cancel_all(&self) {
        let guard = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in guard.values() {
            job.cancel.cancel();
        }
    }
}

/// Build the 4 harness tools (`agy_run`, `agy_spawn`, `agy_wait`, `agy_jobs`) sharing one pool.
pub fn agy_tools(cfg: AgyConfig) -> Vec<Arc<dyn Tool>> {
    let pool = Arc::new(AgyJobPool::new());
    vec![
        Arc::new(AgyRunTool { cfg: cfg.clone() }),
        Arc::new(AgySpawnTool {
            cfg,
            pool: Arc::clone(&pool),
        }),
        Arc::new(AgyWaitTool {
            pool: Arc::clone(&pool),
        }),
        Arc::new(AgyJobsTool { pool }),
    ]
}

fn parse_request(ctx: &ToolContext, args: &Value) -> Result<AgyRequest> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HrnError::Tool("missing required string field `prompt`".into()))?
        .to_owned();
    let cwd = match args.get("cwd").and_then(Value::as_str) {
        Some(raw) if !raw.trim().is_empty() => PathBuf::from(raw),
        _ => ctx.cwd.clone(),
    };
    let str_opt = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let str_list = |key: &str| -> Vec<String> {
        args.get(key)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let keep_worktree = args
        .get("keep_worktree")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let worktree = args
        .get("worktree")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || keep_worktree;
    Ok(AgyRequest {
        prompt,
        cwd,
        model: str_opt("model"),
        effort: str_opt("effort"),
        mode: str_opt("mode"),
        resume: str_opt("resume"),
        add_dirs: str_list("add_dirs"),
        skip_permissions: args
            .get("skip_permissions")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        timeout_secs: args.get("timeout_secs").and_then(Value::as_u64),
        worktree,
        allowed_paths: str_list("allowed_paths"),
        denied_paths: str_list("denied_paths"),
        keep_worktree,
    })
}

fn run_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": { "type": "string", "description": "Task prompt for the official agy CLI." },
            "cwd": { "type": "string", "description": "Working directory (defaults to current session cwd)." },
            "model": { "type": "string", "description": "Optional agy --model override." },
            "effort": { "type": "string", "description": "Optional reasoning effort: low | medium | high | xhigh | max." },
            "mode": { "type": "string", "description": "Optional execution mode: plan | accept-edits | auto." },
            "resume": { "type": "string", "description": "Optional agy conversation ID to resume." },
            "add_dirs": { "type": "array", "items": { "type": "string" }, "description": "Extra workspace directories." },
            "skip_permissions": { "type": "boolean", "description": "Pass --dangerously-skip-permissions to agy." },
            "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 300)." },
            "worktree": { "type": "boolean", "description": "Run inside an isolated temporary git worktree and apply clean edits back." },
            "allowed_paths": { "type": "array", "items": { "type": "string" }, "description": "Allowed workspace-relative globs/prefixes (implies worktree; rolls back on violation)." },
            "denied_paths": { "type": "array", "items": { "type": "string" }, "description": "Denied workspace-relative globs/prefixes (always wins; implies worktree)." },
            "keep_worktree": { "type": "boolean", "description": "Keep the temporary worktree directory after a clean pass (implies worktree)." }
        },
        "required": ["prompt"]
    })
}

fn format_response(resp: &AgyResponse) -> ToolOutput {
    let ok = resp.exit_code == Some(0) && !resp.timed_out;
    let out = ToolOutput::ok_json(resp);
    if ok {
        out
    } else {
        ToolOutput::err(out.content)
    }
}

struct AgyRunTool {
    cfg: AgyConfig,
}

impl Tool for AgyRunTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agy_run".to_owned(),
            description: DESCRIPTION_RUN.to_owned(),
            parameters: run_schema(),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let cfg = self.cfg.clone();
        let parsed = parse_request(ctx, &args);
        let cancel = ctx.cancel.clone();
        let events = Arc::clone(&ctx.events);
        let call_id = ctx.call_id.clone();
        Box::pin(async move {
            let req = parsed?;
            let resp = run_agy(&cfg, &req, &cancel, move |chunk| {
                let events = Arc::clone(&events);
                let call_id = call_id.clone();
                tokio::spawn(async move {
                    let _ = events
                        .emit(AgentEvent::ToolDelta {
                            call_id,
                            delta: chunk,
                        })
                        .await;
                });
            })
            .await?;
            Ok(format_response(&resp))
        })
    }
}

struct AgySpawnTool {
    cfg: AgyConfig,
    pool: Arc<AgyJobPool>,
}

impl Tool for AgySpawnTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agy_spawn".to_owned(),
            description: DESCRIPTION_SPAWN.to_owned(),
            parameters: run_schema(),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let parsed = parse_request(ctx, &args);
        let cfg = self.cfg.clone();
        let pool = Arc::clone(&self.pool);
        Box::pin(async move {
            let req = parsed?;
            let job_id = pool.spawn(cfg, req);
            Ok(ToolOutput::ok_json(&json!({
                "job_id": job_id,
                "status": "running",
            })))
        })
    }
}

struct AgyWaitTool {
    pool: Arc<AgyJobPool>,
}

impl Tool for AgyWaitTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agy_wait".to_owned(),
            description: DESCRIPTION_WAIT.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "Single job ID returned by agy_spawn." },
                    "job_ids": { "type": "array", "items": { "type": "string" }, "description": "Multiple job IDs to wait for together." },
                    "timeout_secs": { "type": "integer", "description": "Max seconds to wait (default 300)." }
                }
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let single_id = args
            .get("job_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let batch_ids: Vec<String> = args
            .get("job_ids")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .max(1);
        let pool = Arc::clone(&self.pool);
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            if !batch_ids.is_empty() {
                let deadline = Instant::now() + Duration::from_secs(timeout_secs);
                let mut results = Vec::with_capacity(batch_ids.len());
                for id in batch_ids {
                    let rem = deadline
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(50));
                    let entry = wait_one_json(&pool, &id, rem, &cancel).await?;
                    results.push(entry);
                }
                return Ok(ToolOutput::ok_json(&json!({ "results": results })));
            }
            let Some(job_id) = single_id else {
                return Err(HrnError::Tool(
                    "agy_wait requires `job_id` or non-empty `job_ids`".into(),
                ));
            };
            wait_one_output(&pool, &job_id, Duration::from_secs(timeout_secs), &cancel).await
        })
    }
}

async fn wait_one_output(
    pool: &AgyJobPool,
    job_id: &str,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<ToolOutput> {
    let Some(mut rx) = pool.receiver(job_id) else {
        return Err(HrnError::Tool(format!("unknown agy job_id `{job_id}`")));
    };
    let wait_fut = rx.wait_for(|s| !matches!(s, JobState::Running));
    let res = tokio::select! {
        r = tokio::time::timeout(timeout, wait_fut) => r,
        () = wait_cancelled(cancel) => return Err(HrnError::Cancelled),
    };
    match res {
        Ok(Ok(state)) => match &*state {
            JobState::Done(resp) => Ok(format_response(resp)),
            JobState::Failed(msg) => Ok(ToolOutput::err(msg.clone())),
            JobState::Running => Ok(ToolOutput::ok_json(&json!({
                "job_id": job_id,
                "status": "running",
            }))),
        },
        Ok(Err(_)) => Err(HrnError::Tool(format!("agy job `{job_id}` channel closed"))),
        Err(_) => Ok(ToolOutput::ok_json(&json!({
            "job_id": job_id,
            "status": "running",
            "timed_out": true,
        }))),
    }
}

async fn wait_one_json(
    pool: &AgyJobPool,
    job_id: &str,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<Value> {
    let Some(mut rx) = pool.receiver(job_id) else {
        return Ok(json!({ "job_id": job_id, "error": "unknown job_id" }));
    };
    let wait_fut = rx.wait_for(|s| !matches!(s, JobState::Running));
    let res = tokio::select! {
        r = tokio::time::timeout(timeout, wait_fut) => r,
        () = wait_cancelled(cancel) => return Err(HrnError::Cancelled),
    };
    match res {
        Ok(Ok(state)) => match &*state {
            JobState::Done(resp) => Ok(json!({
                "job_id": job_id,
                "status": if resp.exit_code == Some(0) && !resp.timed_out { "completed" } else { "failed" },
                "response": resp,
            })),
            JobState::Failed(msg) => Ok(json!({
                "job_id": job_id,
                "status": "failed",
                "error": msg,
            })),
            JobState::Running => Ok(json!({ "job_id": job_id, "status": "running" })),
        },
        Ok(Err(_)) => Ok(json!({ "job_id": job_id, "error": "channel closed" })),
        Err(_) => Ok(json!({ "job_id": job_id, "status": "running", "timed_out": true })),
    }
}

struct AgyJobsTool {
    pool: Arc<AgyJobPool>,
}

impl Tool for AgyJobsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agy_jobs".to_owned(),
            description: DESCRIPTION_JOBS.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "cancel_job_id": { "type": "string", "description": "Optional job_id to cancel before listing." }
                }
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn execute(&self, _ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let cancel_target = args
            .get("cancel_job_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let cancelled = cancel_target
            .as_deref()
            .map(|id| (id.to_owned(), self.pool.cancel(id)));
        let jobs = self.pool.list();
        Box::pin(async move {
            let mut body = json!({ "jobs": jobs });
            if let Some((id, ok)) = cancelled
                && let Some(map) = body.as_object_mut()
            {
                map.insert("cancelled".to_owned(), json!({ "job_id": id, "found": ok }));
            }
            Ok(ToolOutput::ok_json(&body))
        })
    }
}

async fn wait_cancelled(flag: &CancelFlag) {
    while !flag.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_four_expected_tools() {
        let tools = agy_tools(AgyConfig::default());
        let names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(names, vec!["agy_run", "agy_spawn", "agy_wait", "agy_jobs"]);
        let tiers: Vec<hrn_kernel::ToolTier> =
            tools.iter().map(|t| t.approval(&json!({})).tier).collect();
        assert_eq!(
            tiers,
            vec![
                hrn_kernel::ToolTier::Exec,
                hrn_kernel::ToolTier::Exec,
                hrn_kernel::ToolTier::Read,
                hrn_kernel::ToolTier::Read,
            ]
        );
    }
}
