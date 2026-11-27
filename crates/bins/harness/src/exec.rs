//! `hrn exec`: one headless agent turn. Assistant text goes to stdout; tool activity and
//! approvals go to stderr as short lines.

use crate::cli::{ExecArgs, OutputMode};
use crate::commands::format_utc;
use crate::config::Config;
use crate::stats::dollars;
use crate::term::{err_line, out, short};
use crate::wiring::{Background, build_runtime_with_mcp, build_runtime_with_tools, open_store};
use hrn_edge_llm::resolve_alias;
use hrn_edge_mcp::McpToolSet;
use hrn_edge_store::SqliteStore;
use hrn_harness::{AgentRuntime, ScopePolicy, SessionControl, ToolRegistry, TurnIo};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, EventSink, HrnError, PermissionMode, Result, SessionMeta,
    SessionStore, ThinkingLevel, Usage,
};
use serde_json::json;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Exit status when the turn finished but the approval policy refused tool calls.
const EXIT_REFUSED: u8 = 3;
/// Exit status after Ctrl-C (the shell convention for SIGINT).
const EXIT_INTERRUPTED: u8 = 130;

/// Prints events as they arrive and counts refused approvals.
#[derive(Default)]
struct ConsoleSink {
    mid_line: AtomicBool,
    refused: AtomicUsize,
    saw_turn_error: AtomicBool,
    /// `--mode json`: events go to stdout as JSON lines instead of the human progress lines.
    json: bool,
}

impl ConsoleSink {
    /// End a partially printed assistant line so stderr/stdout lines start clean.
    fn finish_line(&self) {
        if self.mid_line.swap(false, Ordering::Relaxed) {
            out("\n");
        }
    }

    /// `--mode json`: every event is one line of stdout; nothing else is printed there.
    fn handle_json(&self, event: &AgentEvent) {
        if matches!(
            event,
            AgentEvent::ApprovalResolved {
                approved: false,
                ..
            }
        ) {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
        if matches!(event, AgentEvent::TurnError { .. }) {
            self.saw_turn_error.store(true, Ordering::Relaxed);
        }
        match serde_json::to_string(event) {
            Ok(line) => out(&format!("{line}\n")),
            Err(err) => err_line(&format!("event could not be serialized: {err}")),
        }
    }

    fn handle(&self, event: AgentEvent) {
        if self.json {
            self.handle_json(&event);
            return;
        }
        match event {
            AgentEvent::TextDelta { delta } if !delta.is_empty() => {
                self.mid_line
                    .store(!delta.ends_with('\n'), Ordering::Relaxed);
                out(&delta);
            }
            AgentEvent::ToolStart {
                name, arguments, ..
            } => {
                self.finish_line();
                err_line(&format!("> {name} {}", short(&arguments, 160)));
            }
            AgentEvent::ToolDone {
                name,
                output,
                is_error,
                ..
            } => {
                let verdict = if is_error { "error" } else { "ok" };
                err_line(&format!("< {name} {verdict}: {}", short(&output, 120)));
            }
            AgentEvent::ApprovalRequested { request } => {
                err_line(&format!(
                    "? approve {}: {}",
                    request.tool_name,
                    short(&request.summary, 140)
                ));
            }
            AgentEvent::ApprovalResolved { approved, .. } => {
                if approved {
                    err_line("  approved");
                } else {
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    err_line("  refused (headless approval policy)");
                }
            }
            AgentEvent::Compacted { saved_chars, .. } => {
                err_line(&format!("~ compacted history, saved {saved_chars} chars"));
            }
            AgentEvent::AdvisorNote { model, note } => {
                self.finish_line();
                err_line(&format!("[advisor · {model}]\n{note}"));
            }
            AgentEvent::AssistantNote { text } if !text.is_empty() => {
                self.finish_line();
                out(&text);
                if !text.ends_with('\n') {
                    out("\n");
                }
            }
            _ => {}
        }
    }
}

struct SharedSink(Arc<ConsoleSink>);

impl EventSink for SharedSink {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        self.0.handle(event);
        Box::pin(async { true })
    }
}

pub(crate) fn absolute_cwd(flag: Option<PathBuf>) -> Result<PathBuf> {
    let raw = match flag {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    // `canonicalize` would yield `\\?\` verbatim paths on Windows, which shells mangle.
    let abs = std::path::absolute(&raw)?;
    if !abs.is_dir() {
        return Err(HrnError::Config(format!(
            "--cwd `{}` is not a directory",
            abs.display()
        )));
    }
    Ok(abs)
}

pub(crate) fn session_title(prompt: &str) -> String {
    short(prompt, 80)
}

/// The first line of `--mode json`: which session the event lines that follow belong to.
fn session_header(session: &SessionMeta, persistent: bool) -> String {
    json!({
        "type": "session",
        "version": 1,
        "id": session.id,
        "title": session.title,
        "timestamp": format!("{}Z", format_utc(session.created_at_ms).replacen(' ', "T", 1)),
        "cwd": session.cwd,
        "model": session.model,
        // False for a `--no-session` run: the id is not something to resume.
        "persistent": persistent,
    })
    .to_string()
}

fn usage_line(usage: Option<Usage>) -> String {
    usage.map_or_else(String::new, |u| {
        let cost = u
            .cost_micros
            .map_or_else(String::new, |micros| format!(", {}", dollars(micros)));
        format!(
            ", {} in / {} out tokens{cost}",
            u.prompt_tokens, u.completion_tokens
        )
    })
}

/// Resolve the prompt from positional argument, piped stdin, or `--goal`.
fn resolve_prompt(prompt: Option<&str>, goal: Option<&str>) -> Result<String> {
    let stdin = std::io::stdin();
    let is_tty = stdin.is_terminal();
    resolve_prompt_from(prompt, goal, is_tty, stdin.lock())
}

fn resolve_prompt_from<R: Read>(
    prompt: Option<&str>,
    goal: Option<&str>,
    stdin_is_tty: bool,
    mut reader: R,
) -> Result<String> {
    if prompt == Some("-") {
        let mut buf = String::new();
        reader.read_to_string(&mut buf)?;
        if buf.trim().is_empty() {
            return Err(HrnError::Config("empty prompt on stdin".to_owned()));
        }
        return Ok(buf);
    }
    if let Some(text) = prompt {
        if text.trim().is_empty() {
            return Err(HrnError::Config("empty prompt".to_owned()));
        }
        return Ok(text.to_owned());
    }
    if !stdin_is_tty {
        let mut buf = String::new();
        reader.read_to_string(&mut buf)?;
        if !buf.trim().is_empty() {
            return Ok(buf);
        }
    }
    if let Some(objective) = goal.filter(|s| !s.trim().is_empty()) {
        return Ok(objective.to_owned());
    }
    Err(HrnError::Config(
        "missing prompt (pass a prompt argument, `--goal`, or pipe text to stdin)".to_owned(),
    ))
}

fn open_exec_store(
    default_db: &Path,
    override_db: Option<&Path>,
    no_session: bool,
) -> Result<Arc<SqliteStore>> {
    if no_session {
        return open_store(default_db, true);
    }
    let path = override_db.unwrap_or(default_db);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    open_store(path, false)
}

fn parse_tool_csv(raw: Option<&str>) -> Vec<String> {
    raw.map(|csv| {
        csv.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

fn apply_tool_scope(runtime: AgentRuntime, args: &ExecArgs, cwd: &Path) -> AgentRuntime {
    if args.no_tools {
        return runtime.with_tools(ToolRegistry::new());
    }
    let allow_tools = parse_tool_csv(args.tools.as_deref());
    if !args.read_only && allow_tools.is_empty() {
        return runtime;
    }
    let policy = ScopePolicy {
        read_only: args.read_only,
        allow_tools,
        ..ScopePolicy::default()
    };
    policy.apply_to_runtime(&runtime, cwd.to_path_buf())
}

#[expect(
    clippy::too_many_arguments,
    reason = "session creation/resume needs runtime, store, cwd, resolved model/thinking/mode, prompt, and flags"
)]
fn resolve_exec_session(
    runtime: &AgentRuntime,
    store: &Arc<SqliteStore>,
    cwd: &Path,
    args: &ExecArgs,
    first_prompt: &str,
    model: String,
    thinking: ThinkingLevel,
    mode: PermissionMode,
) -> Result<SessionMeta> {
    if let Some(wanted) = args.resume.as_deref() {
        if args.no_session {
            return Err(HrnError::Config(
                "--resume cannot be combined with --no-session".to_owned(),
            ));
        }
        let id = crate::resume::find_session(store.as_ref(), wanted)?;
        let mut meta = store
            .get_session(&id)?
            .ok_or_else(|| HrnError::Store(format!("session `{id}` not found")))?;
        if args.model.is_some() || meta.model.is_none() {
            meta.model = Some(model);
        }
        if args.think.is_some() {
            meta.thinking = thinking;
        }
        if args.yes {
            meta.permission_mode = PermissionMode::Yolo;
        }
        store.update_session(&meta)?;
        return Ok(meta);
    }
    runtime.create_session(
        session_title(first_prompt),
        cwd,
        Some(model),
        thinking,
        mode,
    )
}

/// Run `args.prompt` to completion and map the outcome to a process exit code.
pub(crate) async fn run(
    cfg: &Config,
    db_path: &Path,
    args: ExecArgs,
    discovered: Vec<hrn_edge_llm::Catalog>,
) -> Result<ExitCode> {
    let no_session = args.no_session;
    let first_prompt = resolve_prompt(args.prompt.as_deref(), args.goal.as_deref())?;
    let cwd = absolute_cwd(args.cwd.clone())?;
    let mut cfg = cfg.clone();
    if args.no_title || no_session {
        cfg.auto_title = false;
        cfg.title_refresh_on_replan = false;
    }
    let background = Background::new();
    background.set_discovered(discovered);
    background.reapply_discovered(&mut cfg);
    crate::roles::apply(&mut cfg, &args.roles, args.think)?;
    // Fail on a mistyped alias before any session is created.
    let model = resolve_alias(&cfg.llm, args.model.as_deref())?;
    let advisor_model = match args.advisor_model.as_deref() {
        Some(alias) => Some(resolve_alias(&cfg.llm, Some(alias))?),
        None => cfg.advisor.model.clone(),
    };
    let advisor_enabled = cfg.advisor.enabled || args.advisor || args.advisor_model.is_some();
    let thinking = crate::roles::thinking_for(&cfg, args.model.as_deref(), args.think);

    let store = open_exec_store(db_path, args.db.as_deref(), no_session)?;
    if let Some(tier) = args.service_tier {
        background.overrides.set_service_tier(Some(tier));
    }
    let (runtime, mcp_manager) = if args.no_mcp {
        let rt = build_runtime_with_tools(
            &cfg,
            Arc::clone(&store),
            &cwd,
            &McpToolSet::default(),
            &background,
        )?;
        (rt, None)
    } else {
        let (rt, mgr, mcp_warnings) =
            build_runtime_with_mcp(&mut cfg, Arc::clone(&store), &cwd, &background).await?;
        for warn in mcp_warnings {
            err_line(&format!("warning: {warn}"));
        }
        (rt, Some(mgr))
    };
    let runtime = apply_tool_scope(runtime, &args, &cwd);
    let mode = if args.yes {
        PermissionMode::Yolo
    } else {
        cfg.approval_mode
    };
    let session = resolve_exec_session(
        &runtime,
        &store,
        &cwd,
        &args,
        &first_prompt,
        model,
        thinking,
        mode,
    )?;
    if args.mode == OutputMode::Json {
        out(&format!("{}\n", session_header(&session, !no_session)));
    }

    let cancel = CancelFlag::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });

    if let Some(objective) = args.goal.as_deref() {
        runtime.start_goal(
            &session.id,
            objective,
            args.goal_budget,
            args.goal_time_budget,
        )?;
    }
    let sink = Arc::new(ConsoleSink {
        json: args.mode == OutputMode::Json,
        ..ConsoleSink::default()
    });
    let io = TurnIo {
        cancel,
        events: Arc::new(SharedSink(Arc::clone(&sink))),
        approval: None,
        ask: None,
    };
    let control = SessionControl::default();
    control.set_advisor(advisor_enabled, advisor_model);
    // Runs on until the session is idle with no background agent of its own left in flight.
    let outcome = runtime
        .run_turn_settled(&session.id, &first_prompt, &control, io)
        .await;
    if outcome.is_err() {
        runtime.agents().cancel_tree(&session.id);
    }
    if let Some(mgr) = mcp_manager {
        mgr.shutdown().await;
    }
    sink.finish_line();
    report_outcome(outcome, &session.id, no_session, &sink)
}

fn report_outcome(
    outcome: Result<hrn_harness::TurnResult>,
    session_id: &str,
    no_session: bool,
    sink: &ConsoleSink,
) -> Result<ExitCode> {
    match outcome {
        Ok(done) => {
            err_line(&format!(
                "[{} {} | {} | {} steps{}]",
                if no_session {
                    "throwaway session"
                } else {
                    "session"
                },
                session_id,
                done.model,
                done.steps,
                usage_line(done.usage)
            ));
            let refused = sink.refused.load(Ordering::Relaxed);
            if refused > 0 {
                err_line(&format!(
                    "{refused} tool call(s) were refused by the approval policy; re-run with --yes / --approval-mode yolo where appropriate"
                ));
                return Ok(ExitCode::from(EXIT_REFUSED));
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(HrnError::Cancelled) => {
            if sink.json && !sink.saw_turn_error.load(Ordering::Relaxed) {
                let line = json!({
                    "type": "error",
                    "code": "cancelled",
                    "message": "interrupted",
                });
                out(&format!("{line}\n"));
            }
            err_line("interrupted");
            Ok(ExitCode::from(EXIT_INTERRUPTED))
        }
        Err(err) => {
            if sink.json && !sink.saw_turn_error.load(Ordering::Relaxed) {
                let line = json!({
                    "type": "error",
                    "message": err.to_string(),
                });
                out(&format!("{line}\n"));
            }
            Err(err)
        }
    }
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
