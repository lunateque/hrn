//! `bash` tool: runs a command in the session's persistent shell (Git Bash on Windows, `bash`
//! elsewhere) through `hrn-edge-proc`, with a timeout and live `ToolDelta` output.
//!
//! One long-lived shell per agent session keeps `cd`, `export` and shell functions across calls
//! and removes the per-call process start. A call that arrives while the session's shell is busy
//! (parallel tool calls) runs in a one-shot shell instead, so it never waits behind a slow one.
//! `async` / `notify` run the command as a background job and `name` as a supervised service;
//! see `bash_bg`. `pty: true` runs it on a pseudo-terminal (see `pty`); a foreground command
//! longer than `[shell] auto_background_secs` becomes a background job (see `fg`).

use crate::bash_bg::{Ready, start_job, start_service};
use crate::bridge::Publication;
use crate::policy;
use crate::{Settings, fg, pty};
use hrn_edge_proc::{JobManager, RunOutput, ShellPool};
use hrn_edge_tools::args::{bounded_usize, flag, opt_str, required_str};
use hrn_edge_tools::notify::OutputRegex;
use hrn_edge_tools::paths::resolve;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, HrnError, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The `bash` tool description and the tool-commands help page, as data.
const DESCRIPTION: &str = include_str!("../prompts/bash.md");
pub(crate) const HELP_TOOL_COMMANDS: &str = include_str!("../prompts/help-tool-commands.md");

const DEFAULT_TIMEOUT_SECS: usize = 120;
const MAX_TIMEOUT_SECS: usize = 3600;
pub(crate) const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const NAME_MAX: usize = 48;
const DEFAULT_READY_SECS: f64 = 30.0;

/// Shell command runner: foreground streaming (with a pty option and auto-background), background
/// jobs and services.
pub(crate) struct BashTool {
    jobs: Arc<JobManager>,
    shells: Arc<ShellPool>,
    settings: Arc<Settings>,
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new(
            Arc::new(JobManager::new()),
            Arc::new(Settings {
                auto_background: None,
                pty_default: false,
                pty_idle: std::time::Duration::from_secs(2),
                rc: std::path::PathBuf::new(),
                command_policy: hrn_edge_tools::command_policy::CommandPolicy::default(),
                git_guard: false,
                interceptor: policy::InterceptorConfig::default(),
            }),
        )
    }
}

impl BashTool {
    /// Create a `bash tool` running jobs through `jobs` under `settings`.
    pub(crate) fn new(jobs: Arc<JobManager>, settings: Arc<Settings>) -> Self {
        Self {
            jobs,
            shells: Arc::default(),
            settings,
        }
    }
}

/// A service name: 1-48 letters, digits, dots, underscores or hyphens, starting with a letter
/// (agent ids start with a digit) and not shaped like a job id.
fn validate_name(name: &str) -> Result<()> {
    let shaped_like_job = name
        .strip_prefix("job-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let ok = name.len() <= NAME_MAX
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !shaped_like_job;
    if ok {
        Ok(())
    } else {
        Err(HrnError::Tool(format!(
            "bash: invalid service name `{name}`: use 1-{NAME_MAX} letters, digits, `.`, `_` or              `-`, starting with a letter and not shaped like `job-<n>`"
        )))
    }
}

/// The `ready` object of a service call; `None` (with a notice) when it names no condition.
fn parse_ready(args: &Value, notices: &mut Vec<String>) -> Result<Option<Ready>> {
    let Some(ready) = args.get("ready").filter(|r| r.is_object()) else {
        return Ok(None);
    };
    let log = opt_str(ready, "log")
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| OutputRegex::whole(l, "ready.log"))
        .transpose()?;
    let port = match ready.get("port") {
        None | Some(Value::Null) => None,
        Some(p) => Some(
            p.as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p > 0)
                .ok_or_else(|| {
                    HrnError::Tool("bash: ready.port must be an integer in 1..65535".to_owned())
                })?,
        ),
    };
    if log.is_none() && port.is_none() {
        notices
            .push("[ready names neither `log` nor `port`; not waiting for readiness]".to_owned());
        return Ok(None);
    }
    let secs = ready
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_READY_SECS)
        .clamp(0.05, 3600.0);
    Ok(Some(Ready {
        log,
        port,
        host: opt_str(ready, "host")
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .unwrap_or("127.0.0.1")
            .to_owned(),
        timeout: Duration::from_secs_f64(secs),
    }))
}

/// `timeout_secs` of a background job: absent means the default, `0` no deadline.
fn job_deadline(args: &Value) -> Option<Duration> {
    let secs = match args.get("timeout_secs").and_then(Value::as_u64) {
        Some(0) => return None,
        Some(s) => usize::try_from(s).unwrap_or(MAX_TIMEOUT_SECS),
        None => DEFAULT_TIMEOUT_SECS,
    };
    let secs = secs.min(MAX_TIMEOUT_SECS);
    Some(Duration::from_secs(u64::try_from(secs).unwrap_or(u64::MAX)))
}

/// How a call goes on after [`run_background`].
enum Mode {
    /// It ran as a service or a background job; this is the answer.
    Done(Box<ToolOutput>),
    /// It is a plain foreground call; the notices go on its result.
    Foreground(Vec<String>),
}

/// Start `command` as a service (`name`) or a background job (`async` / `notify`).
async fn run_background(
    jobs: &JobManager,
    ctx: &ToolContext,
    args: &Value,
    command: &str,
    cwd: Option<&Path>,
) -> Result<Mode> {
    let name = opt_str(args, "name")
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let notify = opt_str(args, "notify")
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| OutputRegex::per_line(n, "notify"))
        .transpose()?;
    let mut notices = Vec::new();
    let cwd = cwd.map_or_else(|| ctx.cwd.clone(), Path::to_path_buf);
    if let Some(name) = name {
        validate_name(name)?;
        if flag(args, "async") {
            notices.push(
                "[`async` is ignored: a named service always runs in the background]".to_owned(),
            );
        }
        if args.get("timeout_secs").is_some() {
            notices.push("[`timeout_secs` is ignored for a service]".to_owned());
        }
        let ready = parse_ready(args, &mut notices)?;
        let out = start_service(jobs, ctx, command, cwd, name, notify, ready).await?;
        return Ok(Mode::Done(Box::new(with_notices(out, &notices))));
    }
    if args.get("ready").is_some_and(Value::is_object) {
        notices.push("[`ready` is ignored without `name`]".to_owned());
    }
    if flag(args, "async") || notify.is_some() {
        let out = start_job(jobs, ctx, command, cwd, job_deadline(args), notify)?;
        return Ok(Mode::Done(Box::new(with_notices(out, &notices))));
    }
    Ok(Mode::Foreground(notices))
}

impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command line." },
                    "cwd": { "type": "string", "description": "Working directory (default: session cwd)." },
                    "timeout_secs": { "type": "integer", "minimum": 0, "maximum": MAX_TIMEOUT_SECS, "description": "Seconds before the command is killed (default 120; 0 with `async` means no deadline)." },
                    "async": { "type": "boolean", "description": "Run in the background; the result is delivered automatically (default false)." },
                    "pty": { "type": "boolean", "description": "Run on a pseudo-terminal for interactive programs (default false)." },
                    "notify": { "type": "string", "description": "Regex; matching output lines arrive while the command runs (implies `async`)." },
                    "name": { "type": "string", "description": "Start a supervised service under this unique name (1-48 letters, digits, `.`, `_`, `-`; starts with a letter)." },
                    "ready": {
                        "type": "object",
                        "description": "With `name`: when the service counts as started; every given condition must hold.",
                        "properties": {
                            "log": { "type": "string", "description": "Rust regex over the service's accumulated output; `^`/`$` match at line boundaries." },
                            "port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "TCP port that must accept connections." },
                            "host": { "type": "string", "description": "Host for `port` (default 127.0.0.1)." },
                            "timeout": { "type": "number", "minimum": 0.05, "maximum": 3600, "description": "Seconds to wait for readiness (default 30)." }
                        }
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, args: &Value) -> ToolApproval {
        let command = opt_str(args, "command").unwrap_or("");
        policy::bash_approval(&self.settings.command_policy, command)
    }

    fn summarize_call(&self, args: &Value) -> String {
        let command = opt_str(args, "command").unwrap_or("<empty>");
        let prefix = if policy::match_critical(command).is_some() {
            "[CRITICAL] ".to_owned()
        } else if let Some(name) = opt_str(args, "name").filter(|n| !n.trim().is_empty()) {
            format!("[service {name}] ")
        } else if flag(args, "async")
            || opt_str(args, "notify").is_some_and(|n| !n.trim().is_empty())
        {
            "[bg] ".to_owned()
        } else {
            String::new()
        };
        format!("{prefix}{command}")
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let ctx = ctx.clone();
        let jobs = Arc::clone(&self.jobs);
        let shells = Arc::clone(&self.shells);
        let settings = Arc::clone(&self.settings);
        Box::pin(async move {
            let command = required_str(&args, "command", "bash")?.to_owned();
            let cwd_arg = opt_str(&args, "cwd").map(|p| resolve(&ctx.cwd, p));
            let effective_cwd = cwd_arg.as_deref().unwrap_or(&ctx.cwd);
            if let Some(refusal) =
                policy::check_git_guard(settings.git_guard, effective_cwd, &command)
            {
                return Ok(ToolOutput::err(refusal));
            }
            let registered_tools = ctx
                .caller
                .as_ref()
                .map(|c| c.tool_names())
                .unwrap_or_default();
            if let Some(redirect) =
                policy::check_interceptor(&settings.interceptor, &registered_tools, &command)
            {
                return Ok(ToolOutput::err(redirect));
            }
            let notices =
                match run_background(&jobs, &ctx, &args, &command, cwd_arg.as_deref()).await? {
                    Mode::Done(out) => return Ok(*out),
                    Mode::Foreground(notices) => notices,
                };
            if args.get("timeout_secs").and_then(Value::as_u64) == Some(0) {
                return Err(HrnError::Tool(
                    "bash: `timeout_secs: 0` (no deadline) is only for `async` jobs".to_owned(),
                ));
            }
            let secs = bounded_usize(
                &args,
                "timeout_secs",
                DEFAULT_TIMEOUT_SECS,
                1,
                MAX_TIMEOUT_SECS,
            );
            let timeout = Duration::from_secs(u64::try_from(secs).unwrap_or(u64::MAX));
            // Tool commands of this call's shell address the call by its published context.
            let publication = Publication::new(&ctx)?;
            let use_pty = flag(&args, "pty") || (publication.is_some() && settings.pty_default);
            if use_pty {
                let cwd = cwd_arg.unwrap_or_else(|| ctx.cwd.clone());
                return pty::run(
                    jobs,
                    settings,
                    ctx,
                    command,
                    cwd,
                    timeout,
                    publication.as_ref(),
                )
                .await;
            }
            fg::foreground(fg::Foreground {
                jobs,
                shells,
                settings,
                ctx,
                command,
                cwd_override: cwd_arg,
                timeout,
                publication: publication.as_ref(),
            })
            .await
            .map(|out| with_notices(out, &notices))
        })
    }
}

/// `out` with the call's notices on the lines after its text.
fn with_notices(mut out: ToolOutput, notices: &[String]) -> ToolOutput {
    for notice in notices {
        let _ = write!(out.content, "\n{notice}");
    }
    out
}

/// Compose the model-facing text: stdout, a `[stderr]` section, warnings, notes and the status.
pub(crate) fn render(out: RunOutput, notes: &[String], timeout: Duration) -> ToolOutput {
    let mut text = out.stdout;
    if !out.stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&out.stderr);
    }
    for warning in &out.warnings {
        let _ = write!(text, "\n[warning] {warning}");
    }
    for note in notes {
        let _ = write!(text, "\n{note}");
    }
    if out.timed_out {
        let _ = write!(
            text,
            "\n[timed out after {}s; process tree killed]",
            timeout.as_secs_f64()
        );
    }
    match out.exit_code {
        Some(code) => {
            let _ = write!(text, "\n[exit code {code}]");
        }
        None if !out.timed_out => text.push_str("\n[terminated by signal]"),
        None => {}
    }
    let failed = out.timed_out || out.exit_code != Some(0);
    if failed {
        ToolOutput::err(text.trim_start().to_owned())
    } else {
        ToolOutput::ok(text.trim_start().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::testing::tool_context as test_ctx;

    #[tokio::test]
    async fn directory_and_environment_persist_between_calls() {
        let bash = BashTool::default();
        let ctx = test_ctx(std::env::temp_dir());
        let first = bash
            .execute(&ctx, json!({"command": "mkdir -p hrn_persist_probe && cd hrn_persist_probe && export HRN_PROBE=kept"}))
            .await
            .expect("first call");
        assert!(first.content.contains("[cwd: "), "{}", first.content);
        let second = bash
            .execute(&ctx, json!({"command": "basename \"$PWD\"; echo $HRN_PROBE; cd .. && rmdir hrn_persist_probe"}))
            .await
            .expect("second call");
        assert!(
            second.content.starts_with("hrn_persist_probe\nkept"),
            "{}",
            second.content
        );
    }
}
