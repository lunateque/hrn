//! Official `agy` CLI discovery, argument construction, and streaming subprocess execution.

use crate::worktree::TempWorktree;
use hrn_edge_proc::{RunSpec, resolve_on_path, run_streaming};
use hrn_kernel::serde::{Deserialize, Serialize};
use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default wall-clock timeout per `agy -p` invocation in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Default captured stdout+stderr retention budget (256 KiB).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Configuration for the official `agy` CLI bridge.
#[derive(Clone, Debug)]
pub struct AgyConfig {
    /// Explicit binary path (`None` auto-discovers `agy` on `PATH` and standard install dirs).
    pub binary: Option<String>,
    /// Optional default `--model` flag.
    pub default_model: Option<String>,
    /// Optional default `--effort` flag (`low`, `medium`, `high`, `xhigh`, `max`).
    pub default_effort: Option<String>,
    /// Optional default `--mode` flag (`plan`, `accept-edits`, `auto`).
    pub default_mode: Option<String>,
    /// Default timeout in seconds.
    pub timeout_secs: u64,
    /// Maximum output bytes retained from a single run.
    pub max_output_bytes: usize,
}

impl Default for AgyConfig {
    fn default() -> Self {
        Self {
            binary: None,
            default_model: None,
            default_effort: None,
            default_mode: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

impl AgyConfig {
    /// Build configuration from optional `HRN_AGY_*` environment variables.
    pub fn from_env() -> Self {
        let read = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let timeout_secs = read("HRN_AGY_TIMEOUT_SECS")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        Self {
            binary: read("HRN_AGY_BIN"),
            default_model: read("HRN_AGY_MODEL"),
            default_effort: read("HRN_AGY_EFFORT"),
            default_mode: read("HRN_AGY_MODE"),
            timeout_secs,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// Parameters for one `agy -p` invocation.
#[derive(Clone, Debug, Default)]
pub struct AgyRequest {
    /// Prompt sent to `agy`.
    pub prompt: String,
    /// Working directory for the subprocess.
    pub cwd: PathBuf,
    /// Optional `--model` override.
    pub model: Option<String>,
    /// Optional `--effort` override (`low|medium|high|xhigh|max`).
    pub effort: Option<String>,
    /// Optional `--mode` override (`plan|accept-edits|auto`).
    pub mode: Option<String>,
    /// Optional `--resume` conversation id.
    pub resume: Option<String>,
    /// Extra workspace directories passed via repeatable `--add-dir`.
    pub add_dirs: Vec<String>,
    /// Pass `--dangerously-skip-permissions`.
    pub skip_permissions: bool,
    /// Per-call timeout override in seconds.
    pub timeout_secs: Option<u64>,
    /// Execute inside a temporary detached `git worktree` and apply clean edits back.
    pub worktree: bool,
    /// Workspace-relative glob/prefix allowlist enforced after the run (implies `worktree`).
    pub allowed_paths: Vec<String>,
    /// Workspace-relative glob/prefix denylist enforced after the run (implies `worktree`).
    pub denied_paths: Vec<String>,
    /// Keep the temporary worktree directory after a clean pass instead of removing it.
    pub keep_worktree: bool,
}

impl AgyRequest {
    /// Whether this request runs inside an isolated temporary git worktree.
    pub fn needs_worktree(&self) -> bool {
        self.worktree || !self.allowed_paths.is_empty() || !self.denied_paths.is_empty()
    }
}

/// Structured result returned from one `agy` CLI invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "hrn_kernel::serde")]
pub struct AgyResponse {
    /// Parsed final text response (or raw stdout when plain text).
    pub output: String,
    /// Conversation ID when emitted by `agy --output-format json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// Process exit code (`None` when killed/timed out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Captured stderr (trimmed, omitted when empty).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
    /// `true` when the invocation exceeded its timeout.
    pub timed_out: bool,
    /// `true` when output exceeded the retention cap.
    pub truncated: bool,
    /// Workspace-relative files changed when executed inside an isolated worktree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    /// Preserved worktree directory path when `keep_worktree` was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// Whether worktree edits were applied back to the parent repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<bool>,
}

/// Locate the official `agy` executable on `PATH` or standard per-user install paths.
pub fn discover_agy_binary(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(raw) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(raw);
        if p.is_file() {
            return Some(p);
        }
        if let Some(found) = resolve_on_path(raw) {
            return Some(found);
        }
        return None;
    }
    if let Some(found) = resolve_on_path("agy") {
        return Some(found);
    }
    #[cfg(windows)]
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let candidate = PathBuf::from(local_app_data)
            .join("agy")
            .join("bin")
            .join("agy.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    #[cfg(unix)]
    if let Ok(home) = std::env::var("HOME") {
        for rel in [".local/bin/agy", ".agy/bin/agy"] {
            let candidate = PathBuf::from(&home).join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Build the CLI arguments for `agy -p`.
pub(crate) fn build_args(cfg: &AgyConfig, req: &AgyRequest, timeout_secs: u64) -> Vec<String> {
    let mut args = vec![
        "-p".to_owned(),
        "--output-format".to_owned(),
        "json".to_owned(),
        "--print-timeout".to_owned(),
        format!("{timeout_secs}s"),
    ];
    if let Some(model) = req.model.as_deref().or(cfg.default_model.as_deref()) {
        let trimmed = model.trim();
        if !trimmed.is_empty() {
            args.push("--model".to_owned());
            args.push(trimmed.to_owned());
        }
    }
    if let Some(effort) = req.effort.as_deref().or(cfg.default_effort.as_deref()) {
        let trimmed = effort.trim();
        if !trimmed.is_empty() {
            args.push("--effort".to_owned());
            args.push(trimmed.to_owned());
        }
    }
    if let Some(mode) = req.mode.as_deref().or(cfg.default_mode.as_deref()) {
        let trimmed = mode.trim();
        if !trimmed.is_empty() {
            args.push("--mode".to_owned());
            args.push(trimmed.to_owned());
        }
    }
    if let Some(resume) = req
        .resume
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.push("--resume".to_owned());
        args.push(resume.to_owned());
    }
    for dir in &req.add_dirs {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            args.push("--add-dir".to_owned());
            args.push(trimmed.to_owned());
        }
    }
    if req.skip_permissions {
        args.push("--dangerously-skip-permissions".to_owned());
    }
    args.push(req.prompt.clone());
    args
}

/// Extract text output and optional conversation id from `agy --output-format json` stdout.
pub(crate) fn parse_agy_stdout(stdout: &str) -> (String, Option<String>) {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return (String::new(), None);
    }
    // Try whole-stdout JSON first, then last JSON line (in case preamble lines were printed).
    let parsed = serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
        trimmed
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
    });
    let Some(val) = parsed else {
        return (trimmed.to_owned(), None);
    };
    let conv_id = [
        "conversation_id",
        "conversationId",
        "session_id",
        "sessionId",
    ]
    .into_iter()
    .find_map(|k| val.get(k).and_then(Value::as_str).map(str::to_owned));
    let text = ["result", "output", "response", "text", "content", "message"]
        .into_iter()
        .find_map(|k| val.get(k).and_then(Value::as_str).map(str::to_owned));
    match text {
        Some(t) => (t, conv_id),
        None => (trimmed.to_owned(), conv_id),
    }
}

/// Human-readable status report for `hrn agy status` and `/agy status`.
pub fn status_report() -> String {
    let cfg = AgyConfig::from_env();
    let lines = match discover_agy_binary(cfg.binary.as_deref()) {
        Some(path) => [
            Some(format!("agy binary: {}", path.display())),
            cfg.default_model
                .map(|m| format!("default model (HRN_AGY_MODEL): {m}")),
            cfg.default_effort
                .map(|e| format!("default effort (HRN_AGY_EFFORT): {e}")),
            cfg.default_mode
                .map(|m| format!("default mode (HRN_AGY_MODE): {m}")),
            Some(format!("timeout: {}s", cfg.timeout_secs)),
            Some("usage: /agy status | /agy [--model M] [--effort E] [--mode MODE] [--worktree] <prompt>".to_owned()),
        ]
        .into_iter()
        .flatten()
        .collect(),
        None => vec!["agy binary: NOT FOUND (install `agy` or set HRN_AGY_BIN)".to_owned()],
    };
    lines.join("\n")
}

/// Parse `/agy [status|[flags] <prompt>]` arguments for TUI/TSP slash dispatch.
/// Returns `Ok(None)` for bare `/agy` or `/agy status`.
pub fn parse_slash_command(
    raw: Option<&str>,
    cwd: PathBuf,
) -> std::result::Result<Option<AgyRequest>, String> {
    let trimmed = raw.map(str::trim).unwrap_or_default();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        return Ok(None);
    }
    let tokens = split_slash_tokens(trimmed)?;
    let mut req = AgyRequest {
        cwd,
        ..AgyRequest::default()
    };
    let mut prompt_parts = Vec::new();
    let mut i = 0usize;
    while let Some(tok) = tokens.get(i) {
        if !prompt_parts.is_empty() {
            prompt_parts.push(tok.clone());
            i += 1;
            continue;
        }
        let next_val = |flag: &str, idx: usize| -> std::result::Result<String, String> {
            tokens
                .get(idx + 1)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match tok.as_str() {
            "--model" => {
                req.model = Some(next_val("--model", i)?);
                i += 2;
            }
            "--effort" => {
                req.effort = Some(next_val("--effort", i)?);
                i += 2;
            }
            "--mode" => {
                req.mode = Some(next_val("--mode", i)?);
                i += 2;
            }
            "--resume" => {
                req.resume = Some(next_val("--resume", i)?);
                i += 2;
            }
            "--allow" => {
                req.allowed_paths.push(next_val("--allow", i)?);
                i += 2;
            }
            "--deny" => {
                req.denied_paths.push(next_val("--deny", i)?);
                i += 2;
            }
            "--worktree" => {
                req.worktree = true;
                i += 1;
            }
            "--keep-worktree" => {
                req.worktree = true;
                req.keep_worktree = true;
                i += 1;
            }
            "--skip-permissions" => {
                req.skip_permissions = true;
                i += 1;
            }
            "--" => {
                prompt_parts.extend(tokens.get(i + 1..).unwrap_or_default().iter().cloned());
                break;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown /agy flag `{flag}`")),
            _ => {
                prompt_parts.push(tok.clone());
                i += 1;
            }
        }
    }
    let prompt = prompt_parts.join(" ");
    if prompt.trim().is_empty() {
        return Err(
            "missing prompt · use `/agy status` or `/agy [--worktree] <prompt>`".to_owned(),
        );
    }
    req.prompt = prompt;
    Ok(Some(req))
}

fn split_slash_tokens(raw: &str) -> std::result::Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in raw.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            Some(_) | None => cur.push(ch),
        }
    }
    if quote.is_some() {
        return Err("unclosed quote in /agy command".to_owned());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Format an [`AgyResponse`] into a readable report block for `/agy` in TUI and Tern TSP.
pub fn format_slash_response(resp: &AgyResponse) -> String {
    let mut out = String::new();
    let status = if resp.timed_out {
        "timed out"
    } else if resp.exit_code == Some(0) {
        "ok"
    } else {
        "error"
    };
    let _ = write!(out, "[agy · {status}");
    if let Some(id) = resp.conversation_id.as_deref() {
        let _ = write!(out, " · conv {id}");
    }
    if !resp.changed_files.is_empty() {
        let state = if resp.applied == Some(true) {
            "applied"
        } else {
            "changed"
        };
        let _ = write!(out, " · {state} {} file(s)", resp.changed_files.len());
    }
    out.push(']');
    if !resp.output.trim().is_empty() {
        out.push('\n');
        out.push_str(resp.output.trim());
    }
    if !resp.changed_files.is_empty() {
        let _ = write!(out, "\nchanged: {}", resp.changed_files.join(", "));
    }
    if let Some(wt) = resp.worktree_path.as_deref() {
        let _ = write!(out, "\nworktree: {wt}");
    }
    if !resp.stderr.trim().is_empty() && resp.exit_code != Some(0) {
        let _ = write!(out, "\nstderr: {}", resp.stderr.trim());
    }
    out
}

/// Execute one `agy` CLI call with live chunk callback and cooperative cancellation.
pub async fn run_agy<F>(
    cfg: &AgyConfig,
    req: &AgyRequest,
    cancel: &CancelFlag,
    on_chunk: F,
) -> Result<AgyResponse>
where
    F: Fn(String) + Send + Sync + 'static,
{
    if req.prompt.trim().is_empty() {
        return Err(HrnError::Tool("agy prompt must not be empty".into()));
    }
    let binary = discover_agy_binary(cfg.binary.as_deref()).ok_or_else(|| {
        HrnError::Config(
            "official `agy` binary not found on PATH (set HRN_AGY_BIN or install `agy`)".into(),
        )
    })?;
    let cwd = resolve_cwd(&req.cwd)?;
    let worktree = if req.needs_worktree() {
        Some(TempWorktree::create(&cwd).await?)
    } else {
        None
    };
    let run_cwd = worktree.as_ref().map_or(cwd, TempWorktree::run_cwd);
    let timeout_secs = req.timeout_secs.unwrap_or(cfg.timeout_secs).max(5);
    let args = build_args(cfg, req, timeout_secs);
    let spec = RunSpec {
        program: binary.to_string_lossy().into_owned(),
        args,
        cwd: run_cwd,
        env: BTreeMap::new(),
        // Give the outer Job Object guard a small grace window over `--print-timeout`.
        timeout: Duration::from_secs(timeout_secs.saturating_add(15)),
        max_output_bytes: cfg.max_output_bytes.max(16 * 1024),
    };
    let out = match run_streaming(&spec, cancel, on_chunk).await {
        Ok(out) => out,
        Err(err) => {
            if let Some(wt) = worktree {
                wt.remove().await;
            }
            return Err(err);
        }
    };
    let (output, conversation_id) = parse_agy_stdout(&out.stdout);
    let mut resp = AgyResponse {
        output,
        conversation_id,
        exit_code: out.exit_code,
        stderr: out.stderr.trim().to_owned(),
        timed_out: out.timed_out,
        truncated: out.truncated,
        changed_files: Vec::new(),
        worktree_path: None,
        applied: None,
    };
    if let Some(wt) = worktree {
        if resp.timed_out || resp.exit_code != Some(0) {
            wt.remove().await;
            resp.applied = Some(false);
        } else {
            let outcome = wt
                .finalize(&req.allowed_paths, &req.denied_paths, req.keep_worktree)
                .await?;
            resp.changed_files = outcome.changed_files;
            resp.worktree_path = outcome.worktree_path;
            resp.applied = Some(outcome.applied);
        }
    }
    Ok(resp)
}

fn resolve_cwd(cwd: &Path) -> Result<PathBuf> {
    let raw = if cwd.as_os_str().is_empty() {
        std::env::current_dir()?
    } else {
        cwd.to_path_buf()
    };
    let abs = std::path::absolute(&raw)?;
    if !abs.is_dir() {
        return Err(HrnError::Tool(format!(
            "agy working directory does not exist: {}",
            abs.display()
        )));
    }
    Ok(abs)
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
