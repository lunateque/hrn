//! `bash` in the background: `async` jobs and supervised services. A job's result is delivered
//! to the calling session's inbox when it ends; a service is started, waited on until ready, and
//! lives until it is stopped or the process ends. Both can report matching output lines while
//! they run (`notify`).

use hrn_edge_proc::{JobHandle, JobManager, JobSpec, JobStatus};
use hrn_edge_rt::tokio;
use hrn_edge_tools::notify::{Notifier, OutputRegex};
use hrn_kernel::{Expectation, HrnError, InboxItem, Result, ToolContext, ToolOutput};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Output carried whole in a job's result; the rest stays at `proc://<id>/streams`.
const RESULT_BYTES: usize = 12_000;
/// Output tail shown when a service starts, exits or misses its readiness deadline.
const SERVICE_TAIL_BYTES: usize = 2_000;
/// Bytes read from a job's log per drain step.
const DRAIN_BYTES: usize = 64 * 1024;
/// How often a TCP readiness probe retries.
const PORT_RETRY: Duration = Duration::from_millis(100);
/// How long one TCP connect attempt may take.
const PORT_CONNECT: Duration = Duration::from_millis(250);

/// How the output of a running job reaches `notify`: complete lines only, in order.
pub(crate) struct Tailer {
    handle: JobHandle,
    notifier: Notifier,
    offset: usize,
    partial: Vec<u8>,
}

impl Tailer {
    fn drain(&mut self, last: bool) -> Result<()> {
        loop {
            let chunk = self.handle.read(self.offset, DRAIN_BYTES);
            if chunk.bytes.is_empty() {
                break;
            }
            self.offset = chunk.next_offset;
            self.partial.extend_from_slice(&chunk.bytes);
        }
        let end = if last {
            self.partial.len()
        } else {
            self.partial
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |i| i + 1)
        };
        if end == 0 {
            return Ok(());
        }
        let rest = self.partial.split_off(end);
        let text = String::from_utf8_lossy(&self.partial).into_owned();
        self.partial = rest;
        self.notifier.feed(&text)
    }
}

/// What a supervised process did.
struct Ended {
    status: JobStatus,
    timed_out: bool,
}

/// Follow `handle` until it ends, feeding `notify` and enforcing `deadline`.
async fn supervise(
    handle: &JobHandle,
    mut tailer: Option<Tailer>,
    deadline: Option<Duration>,
) -> Ended {
    let mut rx = handle.subscribe();
    let deadline = deadline.map(|d| Instant::now() + d);
    let mut timed_out = false;
    loop {
        let status = handle.info().status;
        if let Some(t) = tailer.as_mut()
            && let Err(err) = t.drain(status != JobStatus::Running)
        {
            t.notifier.fail(&err.to_string());
            tailer = None;
        }
        if status != JobStatus::Running {
            if let Some(t) = &tailer {
                t.notifier
                    .finish(handle.info().service && status != JobStatus::Killed);
            }
            return Ended { status, timed_out };
        }
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return Ended { status: handle.info().status, timed_out };
                }
            }
            () = until(deadline), if !timed_out => {
                timed_out = true;
                handle.request_kill();
            }
        }
    }
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn describe_deadline(deadline: Option<Duration>) -> String {
    deadline.map_or_else(
        || "no deadline".to_owned(),
        |d| format!("killed once it has run {}s", d.as_secs()),
    )
}

fn notifier_for(ctx: &ToolContext, id: &str, notify: Option<OutputRegex>) -> Option<Notifier> {
    notify.map(|re| Notifier::new(re, Arc::clone(&ctx.inbox), &ctx.session_id, id))
}

fn tailer_for(handle: &JobHandle, notifier: Option<Notifier>) -> Option<Tailer> {
    notifier.map(|notifier| Tailer {
        handle: handle.clone(),
        notifier,
        offset: 0,
        partial: Vec::new(),
    })
}

fn tail_note(handle: &JobHandle, id: &str, max_bytes: usize) -> String {
    let mut text = String::new();
    if handle.info().total_bytes > max_bytes {
        let _ = writeln!(
            text,
            "… (earlier output omitted; the full log is at proc://{id}/streams)"
        );
    }
    text.push_str(&handle.tail(max_bytes));
    text
}

/// The inbox item for a job that ended on its own or by its deadline.
fn job_result(handle: &JobHandle, ended: &Ended, deadline: Option<Duration>) -> InboxItem {
    let info = handle.info();
    let (status, code) = match ended.status {
        JobStatus::Exited(Some(0)) if !ended.timed_out => ("completed", "0".to_owned()),
        JobStatus::Exited(Some(code)) => ("failed", code.to_string()),
        _ => ("failed", String::new()),
    };
    let mut text = format!(
        "<bash id=\"{}\" status=\"{status}\" exit_code=\"{code}\" seconds=\"{}\">\n$ {}\n",
        info.id,
        info.elapsed_secs(),
        info.command
    );
    if ended.timed_out {
        let _ = writeln!(
            text,
            "[timed out: killed after {}s]",
            deadline.map_or(0, |d| d.as_secs())
        );
    }
    text.push_str(&tail_note(handle, &info.id, RESULT_BYTES));
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("</bash>");
    InboxItem {
        source: info.id.clone(),
        text,
        summary: format!("{} {status}", info.id),
    }
}

/// Run `command` as a background job: return at once, deliver the result when it ends.
pub(crate) fn start_job(
    jobs: &JobManager,
    ctx: &ToolContext,
    command: &str,
    cwd: PathBuf,
    deadline: Option<Duration>,
    notify: Option<OutputRegex>,
) -> Result<ToolOutput> {
    let expectation = ctx.inbox.expect(&ctx.session_id);
    let handle = jobs.spawn(JobSpec {
        command: command.to_owned(),
        cwd,
        owner: ctx.session_id.clone(),
        root: ctx.root_session_id.clone(),
        service: None,
        env: crate::bridge::shell_env(),
    })?;
    ctx.inbox.wake();
    let id = handle.info().id;
    let tailer = tailer_for(&handle, notifier_for(ctx, &id, notify));
    let watching = tailer.is_some();
    drop(tokio::spawn(async move {
        settle_job(&handle, tailer, deadline, expectation).await;
    }));
    let mut text = format!(
        "Backgrounded as job {id} ({}). Its result is delivered automatically when it \
         finishes",
        describe_deadline(deadline)
    );
    if watching {
        text.push_str(", and matching output lines arrive as they appear");
    }
    let _ = write!(
        text,
        "; do not poll. `read proc://{id}` shows progress, `write proc://{id}/kill` with \
         empty content stops it."
    );
    Ok(ToolOutput::ok(text))
}

pub(crate) async fn settle_job(
    handle: &JobHandle,
    tailer: Option<Tailer>,
    deadline: Option<Duration>,
    expectation: Expectation,
) {
    let ended = supervise(handle, tailer, deadline).await;
    // Whoever stopped a job on purpose already knows; only its deadline is news.
    if ended.status == JobStatus::Killed && !ended.timed_out {
        drop(expectation);
    } else {
        expectation.deliver(job_result(handle, &ended, deadline));
    }
}

/// `ready` of a service.
pub(crate) struct Ready {
    pub log: Option<OutputRegex>,
    pub port: Option<u16>,
    pub host: String,
    pub timeout: Duration,
}

/// Start a named service, wait until it is ready (or exits, or the wait times out) and report.
pub(crate) async fn start_service(
    jobs: &JobManager,
    ctx: &ToolContext,
    command: &str,
    cwd: PathBuf,
    name: &str,
    notify: Option<OutputRegex>,
    ready: Option<Ready>,
) -> Result<ToolOutput> {
    let restarted = match jobs.handle(name) {
        Ok(old) => old.kill().await,
        Err(_) => false,
    };
    let handle = jobs.spawn(JobSpec {
        command: command.to_owned(),
        cwd,
        owner: ctx.session_id.clone(),
        root: ctx.root_session_id.clone(),
        service: Some(name.to_owned()),
        env: crate::bridge::shell_env(),
    })?;
    let tailer = tailer_for(&handle, notifier_for(ctx, name, notify));
    let (inbox, session) = (Arc::clone(&ctx.inbox), ctx.session_id.clone());
    let watched = handle.clone();
    drop(tokio::spawn(async move {
        let ended = supervise(&watched, tailer, None).await;
        if ended.status == JobStatus::Killed {
            inbox.wake();
        } else {
            inbox.push(&session, service_exit(&watched, ended.status));
        }
    }));
    ctx.inbox.wake();

    let mut text = String::new();
    if restarted {
        let _ = writeln!(text, "[restarted: the running `{name}` was stopped first]");
    }
    let Some(ready) = ready else {
        let _ = write!(
            text,
            "Service `{name}` started (no readiness check). `read proc://{name}` shows its \
             output, `write proc://{name}/kill` with empty content stops it."
        );
        return Ok(ToolOutput::ok(text));
    };
    let outcome = await_ready(&handle, &ready, ctx).await?;
    let tail = handle.tail(SERVICE_TAIL_BYTES);
    let secs = handle.info().elapsed_secs();
    match outcome {
        Readiness::Ready => {
            let _ = write!(text, "Service `{name}` is ready after {secs}s.");
            append_tail(&mut text, &tail);
            Ok(ToolOutput::ok(text))
        }
        Readiness::Exited(status) => {
            let _ = write!(
                text,
                "Service `{name}` {} before it was ready.",
                status.describe()
            );
            append_tail(&mut text, &tail);
            Ok(ToolOutput::err(text))
        }
        Readiness::TimedOut => {
            let _ = write!(
                text,
                "Service `{name}` was not ready within {}s; it is still running (`read \
                 proc://{name}` for its output, `write proc://{name}/kill` with empty \
                 content stops it).",
                ready.timeout.as_secs_f64()
            );
            append_tail(&mut text, &tail);
            Ok(ToolOutput::err(text))
        }
    }
}

fn append_tail(text: &mut String, tail: &str) {
    if !tail.trim().is_empty() {
        let _ = write!(text, "\nLast output:\n{}", tail.trim_end());
    }
}

fn service_exit(handle: &JobHandle, status: JobStatus) -> InboxItem {
    let id = handle.info().id;
    let tail = handle.tail(SERVICE_TAIL_BYTES);
    let mut text = format!(
        "<bash id=\"{id}\" service=\"true\" status=\"{}\">\nService `{id}` {}.",
        status.as_str(),
        status.describe()
    );
    append_tail(&mut text, &tail);
    text.push_str("\n</bash>");
    InboxItem {
        source: id.clone(),
        text,
        summary: format!("{id} {}", status.describe()),
    }
}

enum Readiness {
    Ready,
    Exited(JobStatus),
    TimedOut,
}

/// Wait until every given condition holds. Cancelling the call stops the service.
async fn await_ready(handle: &JobHandle, ready: &Ready, ctx: &ToolContext) -> Result<Readiness> {
    let mut rx = handle.subscribe();
    let deadline = Instant::now() + ready.timeout;
    loop {
        let status = handle.info().status;
        if status != JobStatus::Running {
            return Ok(Readiness::Exited(status));
        }
        let log_ok = match &ready.log {
            Some(re) => {
                let out = handle.read(0, usize::MAX);
                re.is_match(&String::from_utf8_lossy(&out.bytes))?
            }
            None => true,
        };
        let port_ok = match ready.port {
            Some(port) if log_ok => port_open(&ready.host, port).await,
            _ => ready.port.is_none() && log_ok,
        };
        if log_ok && port_ok {
            return Ok(Readiness::Ready);
        }
        if Instant::now() >= deadline {
            return Ok(Readiness::TimedOut);
        }
        let retry = if ready.port.is_some() && log_ok {
            PORT_RETRY
        } else {
            Duration::MAX
        };
        tokio::select! {
            _ = rx.changed() => {}
            () = tokio::time::sleep(retry.min(deadline.saturating_duration_since(Instant::now()))) => {}
            () = ctx.cancel.cancelled() => {
                handle.request_kill();
                return Err(HrnError::Cancelled);
            }
        }
    }
}

async fn port_open(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(PORT_CONNECT, tokio::net::TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}
