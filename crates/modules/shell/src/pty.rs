//! `bash` with `pty: true`: the command runs on a pseudo-terminal as a managed job, so programs
//! that want a terminal (REPLs, installers, prompts) work. The call waits for the command to end,
//! for its output to go quiet (`[shell] pty_idle_ms`), or for the turn to be cancelled; on quiet
//! output the job keeps running and the answer says how to type into it (`write proc://<id>`,
//! which answers once the program's output settles) and how to read what came after
//! (`read proc://<id>`).

use crate::Settings;
use crate::bash::MAX_OUTPUT_BYTES;
use crate::bash_bg::settle_job;
use crate::bridge::Publication;
use hrn_edge_proc::{JobHandle, JobManager, JobSpec, JobStatus};
use hrn_edge_rt::tokio;
use hrn_kernel::{Expectation, HrnError, Result, ToolContext, ToolOutput};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Run `command` on a pseudo-terminal as a job and wait, per the module header.
pub(crate) async fn run(
    jobs: Arc<JobManager>,
    settings: Arc<Settings>,
    ctx: ToolContext,
    command: String,
    cwd: PathBuf,
    timeout: Duration,
    publication: Option<&Publication>,
) -> Result<ToolOutput> {
    // Process-wide vars first, the call's endpoint last: the endpoint must not be blanked.
    let mut env = crate::bridge::shell_env();
    env.extend(publication.map_or_else(BTreeMap::new, |p| p.env().clone()));
    let handle = jobs.spawn_pty(JobSpec {
        command,
        cwd,
        owner: ctx.session_id.clone(),
        root: ctx.root_session_id.clone(),
        service: None,
        env,
    })?;
    let id = handle.info().id;
    watch(settings, ctx, handle, id, timeout).await
}

/// Wait for the pty job: it ends (report), its output goes quiet (hand it over), the deadline
/// passes (kill it), or the turn is cancelled (kill it and report the cancel).
async fn watch(
    settings: Arc<Settings>,
    ctx: ToolContext,
    handle: JobHandle,
    id: String,
    timeout: Duration,
) -> Result<ToolOutput> {
    let mut rx = handle.subscribe();
    let started = Instant::now();
    let mut seen = 0usize;
    let mut last_output = started;
    loop {
        let info = handle.info();
        if info.total_bytes > seen {
            seen = info.total_bytes;
            last_output = Instant::now();
        }
        if info.status != JobStatus::Running {
            return Ok(finished(&handle, &id, info.status));
        }
        if started.elapsed() >= timeout {
            handle.request_kill();
            await_status(&mut rx, &handle).await;
            let mut text = text_of(&handle);
            let _ = write!(
                text,
                "\n[timed out after {}s on the pty; the job was killed]",
                timeout.as_secs_f64()
            );
            return Ok(ToolOutput::err(text));
        }
        let quiet_at = last_output + settings.pty_idle;
        tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => {
                handle.request_kill();
                await_status(&mut rx, &handle).await;
                return Err(HrnError::Cancelled);
            }
            _ = rx.changed() => {}
            () = tokio::time::sleep(quiet_at.saturating_duration_since(Instant::now())), if quiet_at < Instant::now() + timeout => {
                if handle.info().total_bytes == seen && handle.info().status == JobStatus::Running {
                    return Ok(quiet(&ctx, &handle, &id, timeout.saturating_sub(started.elapsed())));
                }
            }
        }
    }
}

/// Wait (bounded) until the job's status is no longer running.
async fn await_status(rx: &mut tokio::sync::watch::Receiver<u64>, handle: &JobHandle) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.info().status == JobStatus::Running && Instant::now() < deadline {
        if rx.changed().await.is_err() {
            break;
        }
    }
}

/// The text of everything the job printed (bounded like a foreground run).
fn text_of(handle: &JobHandle) -> String {
    let total = handle.info().total_bytes;
    let from = total.saturating_sub(MAX_OUTPUT_BYTES);
    let chunk = handle.read(from, MAX_OUTPUT_BYTES);
    let mut text = String::from_utf8_lossy(&chunk.bytes).into_owned();
    if from > 0 {
        text = format!("[output truncated: {from} bytes earlier]\n{text}");
    }
    text
}

/// The job ended: report the terminal text and how it ended.
fn finished(handle: &JobHandle, id: &str, status: JobStatus) -> ToolOutput {
    let mut text = text_of(handle);
    if text.trim().is_empty() {
        text.clear();
        text.push_str("(no output)");
    }
    let _ = write!(text, "\n[pty job {id} {}]", status.describe());
    if status == JobStatus::Exited(Some(0)) {
        ToolOutput::ok(text)
    } else {
        ToolOutput::err(text)
    }
}

/// Output went quiet while the job runs: keep it alive, promise the inbox its result, and tell
/// the model how to drive it.
fn quiet(ctx: &ToolContext, handle: &JobHandle, id: &str, remaining: Duration) -> ToolOutput {
    let expectation: Expectation = ctx.inbox.expect(&ctx.session_id);
    let watching = handle.clone();
    drop(tokio::spawn(async move {
        settle_job(&watching, None, Some(remaining), expectation).await;
    }));
    let mut text = text_of(handle);
    let _ = write!(
        text,
        "\n[still running as pty job {id} (timeout {}s): `write proc://{id}` types a line into \
         it and answers with what the program printed back, `read proc://{id}` shows its output, \
         `write proc://{id}/kill` with empty content stops it; it is killed at its original \
         timeout, and the result arrives as a follow-up message when it ends — do not poll]",
        remaining.as_secs_f64()
    );
    ctx.inbox.wake();
    ToolOutput::ok(text)
}
