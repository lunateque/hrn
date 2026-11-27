//! Foreground `bash` execution with its auto-background handoff. The command runs in a task that
//! owns the shell (the session's persistent one, or a one-shot when it is busy); the tool call
//! waits on that task. When the task outlives `[shell] auto_background_secs`, the command is
//! adopted into the job table: the call returns the job id, the task keeps streaming output into
//! the job's log, and the final result arrives through the session inbox — a slow command costs
//! one message instead of a blocked turn.

use crate::Settings;
use crate::bash::{MAX_OUTPUT_BYTES, render};
use crate::bridge::Publication;
use hrn_edge_proc::{
    Adopted, JobFeed, JobHandle, JobManager, JobSpec, JobStatus, ShellCall, ShellPool,
    run_streaming, shell_run_spec,
};
use hrn_edge_rt::tokio;
use hrn_kernel::{
    AgentEvent, CancelFlag, Expectation, HrnError, InboxItem, Result, ToolContext, ToolOutput,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

/// Bytes of already-seen output the handoff message shows.
const PREVIEW_BYTES: usize = 2000;
/// Cap on what the call buffers before the job takes over the log.
const SEEN_BYTES: usize = 128 * 1024;
/// Handing off is pointless when the command would hit its own deadline this soon after.
const MIN_REMAINING: Duration = Duration::from_secs(1);

/// What the running command shares with the call waiting on it.
struct Inner {
    seen: Vec<u8>,
    job: Option<(JobHandle, JobFeed)>,
    expectation: Option<Expectation>,
    backgrounded: bool,
    outcome: Option<ToolOutput>,
}

/// The handoff state of one foreground command.
struct Handoff {
    inner: Mutex<Inner>,
    done: Notify,
}

impl Handoff {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                seen: Vec::new(),
                job: None,
                expectation: None,
                backgrounded: false,
                outcome: None,
            }),
            done: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record output: into the job log once adopted, and into the preview buffer until then.
    fn on_chunk(&self, chunk: &str) {
        let mut inner = self.lock();
        if let Some((_, feed)) = &inner.job {
            feed.push(chunk.as_bytes());
        }
        inner.seen.extend_from_slice(chunk.as_bytes());
        let over = inner.seen.len().saturating_sub(SEEN_BYTES);
        inner.seen.drain(..over);
    }

    /// The end of the command: hand the answer to the adopted job's inbox, or to the waiting call.
    fn finish(&self, outcome: ToolOutput, status: JobStatus) {
        let mut inner = self.lock();
        let job = inner.job.take();
        let expectation = inner.expectation.take();
        let backgrounded = inner.backgrounded;
        if !backgrounded {
            inner.outcome = Some(outcome);
            drop(inner);
            self.done.notify_waiters();
            return;
        }
        drop(inner);
        if let (Some((handle, feed)), Some(expectation)) = (job, expectation) {
            feed.finish(status);
            let id = handle.info().id;
            expectation.deliver(InboxItem {
                source: id.clone(),
                text: format!(
                    "<bash id=\"{id}\" status=\"{}\">\n{}\n</bash>",
                    status.describe(),
                    outcome.content
                ),
                summary: format!("{id} {}", status.describe()),
            });
        }
    }
}

/// Cancels the running command when the call is dropped and nobody took over.
struct Guard {
    cancel: CancelFlag,
    handoff: Arc<Handoff>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.handoff.lock().backgrounded {
            self.cancel.cancel();
        }
    }
}

/// One foreground call: everything the run and a possible handoff need.
pub(crate) struct Foreground<'a> {
    pub jobs: Arc<JobManager>,
    pub shells: Arc<ShellPool>,
    pub settings: Arc<Settings>,
    pub ctx: ToolContext,
    pub command: String,
    pub cwd_override: Option<PathBuf>,
    pub timeout: Duration,
    pub publication: Option<&'a Publication>,
}

/// Run `command` in the session's shell (or a one-shot shell when it is busy), returning when it
/// ends, the turn is cancelled, or the auto-background threshold hands it to the job table.
pub(crate) async fn foreground(call: Foreground<'_>) -> Result<ToolOutput> {
    let Foreground {
        jobs,
        shells,
        settings,
        ctx,
        command,
        cwd_override,
        timeout,
        publication,
    } = call;
    let handoff = Arc::new(Handoff::new());
    let cancel = CancelFlag::new();
    let exports = publication.map_or_else(BTreeMap::new, |p| p.env().clone());
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let forward = {
        let events = Arc::clone(&ctx.events);
        let call_id = ctx.call_id.clone();
        tokio::spawn(async move { pump(events, call_id, &mut rx).await })
    };

    let task = spawn_run(
        Arc::clone(&handoff),
        cancel.clone(),
        ctx.clone(),
        Arc::clone(&shells),
        command.clone(),
        cwd_override.clone(),
        exports.clone(),
        timeout,
        tx,
    );
    let _guard = Guard {
        cancel: cancel.clone(),
        handoff: Arc::clone(&handoff),
    };

    let threshold = settings
        .auto_background
        .filter(|wait| timeout.saturating_sub(*wait) >= MIN_REMAINING);
    let mut result = None;
    while result.is_none() {
        let done = handoff.done.notified();
        tokio::select! {
            biased;
            () = done => {
                result = handoff.lock().outcome.take().map(Ok);
            }
            () = tokio::time::sleep(threshold.unwrap_or(Duration::MAX)), if threshold.is_some() => {
                result = handoff_of(
                    &jobs,
                    &ctx,
                    &handoff,
                    &command,
                    cwd_override.as_deref(),
                    cancel.clone(),
                )
                .map(Ok);
            }
            () = ctx.cancel.cancelled() => {
                cancel.cancel();
                result = Some(Err(HrnError::Cancelled));
            }
        }
    }
    let result = match result {
        Some(done) => done,
        // The notified arm only loops while no outcome is stored, so this can only be reached if
        // the command's task was reaped without one.
        None => Ok(handoff.lock().outcome.take().unwrap_or_else(|| {
            ToolOutput::err("bash: the command's task ended without an answer".to_owned())
        })),
    };
    if result.is_ok() && !handoff.lock().backgrounded {
        let _ = task.await;
    } else {
        drop(task);
    }
    let _ = forward.await;
    result
}

/// The threshold passed while the command still runs: adopt it and answer with its job id. `None`
/// when the outcome arrived first (the caller keeps waiting for it).
fn handoff_of(
    jobs: &JobManager,
    ctx: &ToolContext,
    handoff: &Arc<Handoff>,
    command: &str,
    cwd_override: Option<&Path>,
    cancel: CancelFlag,
) -> Option<ToolOutput> {
    let mut inner = handoff.lock();
    if inner.outcome.is_some() || inner.backgrounded {
        return inner.outcome.take();
    }
    let spec = JobSpec {
        command: command.to_owned(),
        cwd: cwd_override.map_or_else(|| ctx.cwd.clone(), ToOwned::to_owned),
        owner: ctx.session_id.clone(),
        root: ctx.root_session_id.clone(),
        service: None,
        env: BTreeMap::new(),
    };
    let Adopted {
        handle,
        feed,
        mut kill,
    } = jobs.adopt(spec).ok()?;
    feed.push(&std::mem::take(&mut inner.seen));
    let preview = preview_text(&handle);
    let id = handle.info().id.clone();
    inner.expectation = Some(ctx.inbox.expect(&ctx.session_id));
    inner.backgrounded = true;
    inner.job = Some((handle.clone(), feed));
    drop(inner);
    ctx.inbox.wake();
    // `write proc://<id>/kill` must stop the command the task still runs.
    let watching = Arc::clone(handoff);
    tokio::spawn(async move {
        if (&mut kill).await.is_ok() {
            cancel.cancel();
            let inner = watching.lock();
            if let Some((handle, _)) = &inner.job {
                let _ = handle.request_kill();
            }
        }
    });
    Some(ToolOutput::ok(format!(
        "{preview}\n[backgrounded as job {id}: it keeps running (killed at its own timeout). \
         `read proc://{id}` shows progress, `write proc://{id}/kill` with empty content stops \
         it; the result arrives as a follow-up message when it ends — do not poll.]"
    )))
}

fn preview_text(handle: &JobHandle) -> String {
    let total = handle.info().total_bytes;
    let from = total.saturating_sub(PREVIEW_BYTES);
    let chunk = handle.read(from, PREVIEW_BYTES);
    String::from_utf8_lossy(&chunk.bytes).into_owned()
}

/// Stream output to the UI: forward chunks as `ToolDelta` until a subscriber drops.
async fn pump(
    events: Arc<dyn hrn_kernel::EventSink>,
    call_id: String,
    rx: &mut mpsc::UnboundedReceiver<String>,
) {
    while let Some(delta) = rx.recv().await {
        let event = AgentEvent::ToolDelta {
            call_id: call_id.clone(),
            delta,
        };
        if !events.emit(event).await {
            break;
        }
    }
}

/// Spawn the task that runs the command and reports its answer into the handoff.
#[expect(
    clippy::too_many_arguments,
    reason = "one move of the call context into the task"
)]
fn spawn_run(
    finish_handoff: Arc<Handoff>,
    cancel_inner: CancelFlag,
    ctx: ToolContext,
    shells_inner: Arc<ShellPool>,
    command_inner: String,
    cwd_inner: Option<PathBuf>,
    exports_inner: BTreeMap<String, String>,
    timeout: Duration,
    tx: mpsc::UnboundedSender<String>,
) -> tokio::task::JoinHandle<()> {
    let stream_handoff = Arc::clone(&finish_handoff);
    tokio::spawn(async move {
        let on_chunk = move |chunk: String| {
            stream_handoff.on_chunk(&chunk);
            let _ = tx.send(chunk);
        };
        let exec = Exec {
            ctx: &ctx,
            shells: &shells_inner,
            command: &command_inner,
            cwd_override: cwd_inner.as_deref(),
            exports: &exports_inner,
            timeout,
            cancel: &cancel_inner,
        };
        let outcome = run(&exec, on_chunk).await;
        let (output, status) = match outcome {
            Ok(done) => done,
            Err(HrnError::Cancelled) => (
                ToolOutput::err("cancelled: the command's process tree was killed"),
                JobStatus::Killed,
            ),
            Err(err) => (
                ToolOutput::err(format!("bash: {err}")),
                JobStatus::Exited(None),
            ),
        };
        finish_handoff.finish(output, status);
    })
}

/// A command's answer plus the status a backgrounded job would report.
type Done = (ToolOutput, JobStatus);

/// Run the command once, in the session's shell or a one-shot, streaming through `on_chunk`.
/// One command run: everything `run` needs besides the chunk sink.
struct Exec<'a> {
    ctx: &'a ToolContext,
    shells: &'a ShellPool,
    command: &'a str,
    cwd_override: Option<&'a Path>,
    exports: &'a BTreeMap<String, String>,
    timeout: Duration,
    cancel: &'a CancelFlag,
}

async fn run(
    exec: &Exec<'_>,
    on_chunk: impl Fn(String) + Send + Sync + Clone + 'static,
) -> Result<Done> {
    let Exec {
        ctx,
        shells,
        command,
        cwd_override,
        exports,
        timeout,
        cancel,
    } = *exec;
    if let Some(mut shell) = shells.acquire(&ctx.session_id) {
        let call = ShellCall {
            command,
            cwd: &ctx.cwd,
            cwd_override,
            exports,
            timeout,
            max_output_bytes: MAX_OUTPUT_BYTES,
        };
        let result = shell.run(&call, cancel, &on_chunk).await?;
        let mut notes = Vec::new();
        if result.restarted {
            notes.push(
                "[shell restarted: the previous shell had exited; cwd and environment were reset]"
                    .to_owned(),
            );
        }
        if let Some(cwd) = result.cwd_changed {
            notes.push(format!("[cwd: {cwd}]"));
        }
        let status = match (result.run.timed_out, result.run.exit_code) {
            (true, _) => JobStatus::Exited(None),
            (false, code) => JobStatus::Exited(code),
        };
        Ok((render(result.run, &notes, timeout), status))
    } else {
        let cwd = cwd_override.unwrap_or(ctx.cwd.as_path()).to_path_buf();
        let mut spec = shell_run_spec(command, cwd, timeout, MAX_OUTPUT_BYTES)?;
        spec.env.extend(exports.clone());
        let run = run_streaming(&spec, cancel, on_chunk).await?;
        let status = match (run.timed_out, run.exit_code) {
            (true, _) => JobStatus::Exited(None),
            (false, code) => JobStatus::Exited(code),
        };
        Ok((render(run, &[], timeout), status))
    }
}
