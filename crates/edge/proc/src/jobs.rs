//! Managed background shell jobs and services with Job Object / process-group isolation and
//! bounded incremental output capture. A job ends by itself; a service (it has a name) runs
//! until it is stopped.

use crate::{REAP_TIMEOUT, build_wrapped_command, is_already_gone, lock, shell_run_spec};
use hrn_kernel::{Clock, HrnError, Result, SystemClock};
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{oneshot, watch};

mod external;
mod pty;

pub use external::{Adopted, JobFeed};
pub use pty::terminal_input;

const MAX_JOB_LOG_BYTES: usize = 512 * 1024;
const DEFAULT_READ_BYTES: usize = 32 * 1024;

/// Lifecycle state of a background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    /// Process is still running.
    Running,
    /// Leader process exited with optional status code.
    Exited(Option<i32>),
    /// Explicitly terminated via [`JobHandle::kill`] or manager teardown.
    Killed,
}

impl JobStatus {
    /// Compact lowercase label for tool output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited(_) => "exited",
            Self::Killed => "killed",
        }
    }
}

/// What to start.
#[derive(Clone, Debug)]
pub struct JobSpec {
    /// Shell command line.
    pub command: String,
    /// Working directory.
    pub cwd: PathBuf,
    /// Session whose inbox receives the result.
    pub owner: String,
    /// Root session of the owner's tree (visibility scope of `proc://`).
    pub root: String,
    /// `Some(name)` starts a service with that id; `None` a numbered job.
    pub service: Option<String>,
    /// Environment added to the shell's.
    pub env: BTreeMap<String, String>,
}

/// Snapshot of a background job's metadata and status.
#[derive(Clone, Debug)]
pub struct JobInfo {
    /// Stable identifier: `job-<n>`, or the name of a service.
    pub id: String,
    /// Original shell command string.
    pub command: String,
    /// Working directory.
    pub cwd: PathBuf,
    /// Session whose inbox receives the result.
    pub owner: String,
    /// Root session of the owner's tree.
    pub root: String,
    /// Whether it is a long-lived service.
    pub service: bool,
    /// Whether it still has a terminal that takes input ([`JobHandle::send_input`]).
    pub interactive: bool,
    /// Current lifecycle state.
    pub status: JobStatus,
    /// Start (ms since epoch).
    pub started_ms: i64,
    /// Settle time (ms since epoch); `0` while running.
    pub finished_ms: i64,
    /// Total bytes emitted so far across stdout + stderr.
    pub total_bytes: usize,
}

impl JobInfo {
    /// Seconds the job has run (total runtime once finished).
    #[must_use]
    pub fn elapsed_secs(&self) -> u64 {
        let end = if self.finished_ms > 0 {
            self.finished_ms
        } else {
            SystemClock.now_ms()
        };
        u64::try_from((end - self.started_ms).max(0) / 1000).unwrap_or(0)
    }
}

/// Incremental slice of a job's output.
#[derive(Clone, Debug)]
pub struct JobChunk {
    /// The raw bytes (a UTF-8 sequence may be cut at either end).
    pub bytes: Vec<u8>,
    /// Byte offset to pass on the next incremental read.
    pub next_offset: usize,
    /// `true` when the requested `offset` fell behind the rolling retention window.
    pub truncated_prefix: bool,
}

struct RollingLog {
    base_offset: usize,
    data: Vec<u8>,
}

impl RollingLog {
    const fn new() -> Self {
        Self {
            base_offset: 0,
            data: Vec::new(),
        }
    }

    fn total_bytes(&self) -> usize {
        self.base_offset.saturating_add(self.data.len())
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.data.extend_from_slice(bytes);
        if self.data.len() > MAX_JOB_LOG_BYTES {
            let drop_n = self.data.len() - MAX_JOB_LOG_BYTES;
            self.data.drain(..drop_n);
            self.base_offset = self.base_offset.saturating_add(drop_n);
        }
    }

    fn read(&self, offset: usize, max_bytes: usize) -> JobChunk {
        let cap = if max_bytes == 0 {
            DEFAULT_READ_BYTES
        } else {
            max_bytes
        };
        let effective = offset.max(self.base_offset);
        let rel_start = effective
            .saturating_sub(self.base_offset)
            .min(self.data.len());
        let rel_end = rel_start.saturating_add(cap).min(self.data.len());
        JobChunk {
            bytes: self
                .data
                .get(rel_start..rel_end)
                .unwrap_or_default()
                .to_vec(),
            next_offset: self.base_offset.saturating_add(rel_end),
            truncated_prefix: offset < self.base_offset,
        }
    }
}

/// Where a job's standard input goes (the pseudo-terminal of a `pty` job).
type InputWriter = Box<dyn std::io::Write + Send>;

struct JobState {
    status: JobStatus,
    finished_ms: i64,
    log: RollingLog,
    kill_tx: Option<oneshot::Sender<()>>,
}

struct JobEntry {
    spec: JobSpec,
    id: String,
    started_ms: i64,
    state: Mutex<JobState>,
    /// The job's terminal input; its own lock so a blocked write never stalls the output.
    input: Mutex<Option<InputWriter>>,
    /// Bumped whenever output arrives or the status changes.
    tick: watch::Sender<u64>,
}

impl JobEntry {
    fn info(&self) -> JobInfo {
        let st = lock(&self.state);
        JobInfo {
            id: self.id.clone(),
            command: self.spec.command.clone(),
            cwd: self.spec.cwd.clone(),
            owner: self.spec.owner.clone(),
            root: self.spec.root.clone(),
            service: self.spec.service.is_some(),
            interactive: lock(&self.input).is_some(),
            status: st.status,
            started_ms: self.started_ms,
            finished_ms: st.finished_ms,
            total_bytes: st.log.total_bytes(),
        }
    }

    fn bump(&self) {
        self.tick.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn new(
        spec: JobSpec,
        id: String,
        input: Option<InputWriter>,
    ) -> (Arc<Self>, oneshot::Receiver<()>) {
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let entry = Arc::new(Self {
            spec,
            id,
            started_ms: SystemClock.now_ms(),
            state: Mutex::new(JobState {
                status: JobStatus::Running,
                finished_ms: 0,
                log: RollingLog::new(),
                kill_tx: Some(kill_tx),
            }),
            input: Mutex::new(input),
            tick: watch::channel(0).0,
        });
        (entry, kill_rx)
    }

    /// Append output and wake the followers.
    fn push_output(&self, bytes: &[u8]) {
        lock(&self.state).log.push(bytes);
        self.bump();
    }

    /// Record how the job ended and wake the followers.
    fn settle(&self, status: JobStatus) {
        *lock(&self.input) = None;
        {
            let mut st = lock(&self.state);
            st.status = status;
            st.finished_ms = SystemClock.now_ms();
            st.kill_tx = None;
        }
        self.bump();
    }
}

/// A job or service, as it was when it started: stays valid when the same name is started again.
#[derive(Clone)]
pub struct JobHandle(Arc<JobEntry>);

impl JobHandle {
    /// Current metadata and status.
    #[must_use]
    pub fn info(&self) -> JobInfo {
        self.0.info()
    }

    /// A receiver that changes whenever the job produces output or settles.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.tick.subscribe()
    }

    /// Output starting at byte `offset` (at most `max_bytes`).
    #[must_use]
    pub fn read(&self, offset: usize, max_bytes: usize) -> JobChunk {
        lock(&self.0.state).log.read(offset, max_bytes)
    }

    /// The last `max_bytes` of output as text (a cut character at the start is dropped).
    #[must_use]
    pub fn tail(&self, max_bytes: usize) -> String {
        let from = self.info().total_bytes.saturating_sub(max_bytes);
        let text = String::from_utf8_lossy(&self.read(from, max_bytes).bytes).into_owned();
        match text.strip_prefix('\u{fffd}') {
            Some(rest) if from > 0 => rest.to_owned(),
            _ => text,
        }
    }

    /// Ask a running job to stop without waiting; `false` when it was not running.
    pub fn request_kill(&self) -> bool {
        let tx = lock(&self.0.state).kill_tx.take();
        tx.is_some_and(|tx| tx.send(()).is_ok())
    }

    /// Stop the job and wait briefly for its status to settle; `false` when it was not running.
    pub async fn kill(&self) -> bool {
        if !self.request_kill() {
            return false;
        }
        for _ in 0..40 {
            if lock(&self.0.state).status != JobStatus::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        true
    }

    /// Write `bytes` to the job's terminal (what a keyboard would type).
    ///
    /// # Errors
    ///
    /// When the job has no terminal or already ended, or the write fails.
    pub fn send_input(&self, bytes: &[u8]) -> Result {
        let mut input = lock(&self.0.input);
        let writer = input.as_mut().ok_or_else(|| {
            HrnError::Tool(format!(
                "job `{}` takes no input: it has no terminal (`pty: true`) or it ended",
                self.0.id
            ))
        })?;
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .map_err(|e| HrnError::Io(format!("write to job `{}`: {e}", self.0.id)))
    }
}

/// Registry of background shell jobs and services tied to the current runtime.
///
/// Dropping the last clone sends a kill signal to any still-running job so child process trees
/// do not leak when `hrn` exits.
#[derive(Default)]
pub struct JobManager {
    next_id: AtomicU64,
    jobs: Mutex<BTreeMap<String, Arc<JobEntry>>>,
    input_wait_ms: AtomicU64,
}

impl Drop for JobManager {
    fn drop(&mut self) {
        self.kill_all();
    }
}

impl JobManager {
    /// Create an empty background job manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How long typing into a job waits for its answer to go quiet before returning; zero (the
    /// default) does not wait; the wait itself lives in the `proc://` source
    /// implementation.
    pub fn set_input_wait(&self, wait: Duration) {
        self.input_wait_ms.store(
            u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// The configured input wait.
    #[must_use]
    pub fn input_wait(&self) -> Duration {
        Duration::from_millis(self.input_wait_ms.load(Ordering::Relaxed))
    }

    /// The id a new job (or service) gets; a service refuses a name that is still running.
    fn allocate(&self, spec: &JobSpec) -> Result<String> {
        match &spec.service {
            Some(name) => {
                if lock(&self.jobs)
                    .get(name)
                    .is_some_and(|e| lock(&e.state).status == JobStatus::Running)
                {
                    return Err(HrnError::Tool(format!(
                        "service `{name}` is already running"
                    )));
                }
                Ok(name.clone())
            }
            None => Ok(format!(
                "job-{}",
                self.next_id.fetch_add(1, Ordering::Relaxed) + 1
            )),
        }
    }

    /// Make `entry` visible to `handle`, `list` and `proc://`.
    fn register(&self, entry: Arc<JobEntry>) -> JobHandle {
        lock(&self.jobs).insert(entry.id.clone(), Arc::clone(&entry));
        JobHandle(entry)
    }

    /// Spawn `spec.command` in the background under a POSIX shell + OS Job Object/process group.
    /// A service replaces an earlier finished one of the same name; stop a running one first.
    pub fn spawn(&self, spec: JobSpec) -> Result<JobHandle> {
        let mut run = shell_run_spec(
            &spec.command,
            spec.cwd.clone(),
            Duration::from_secs(1),
            MAX_JOB_LOG_BYTES,
        )?;
        run.env.extend(spec.env.clone());
        let id = self.allocate(&spec)?;
        let mut wrap = build_wrapped_command(&run.program, &run.args, &run.cwd, &run.env);
        let mut child = wrap
            .spawn()
            .map_err(|e| HrnError::Io(format!("spawn background `{}`: {e}", spec.command)))?;
        let (entry, kill_rx) = JobEntry::new(spec, id, None);

        let out_reader = child.stdout().take().map(|pipe| {
            let e = Arc::clone(&entry);
            tokio::spawn(async move { drain_pipe(pipe, e).await })
        });
        let err_reader = child.stderr().take().map(|pipe| {
            let e = Arc::clone(&entry);
            tokio::spawn(async move { drain_pipe(pipe, e).await })
        });

        let watcher_entry = Arc::clone(&entry);
        tokio::spawn(async move {
            let status = tokio::select! {
                res = child.inner_mut().wait() => {
                    let _ = child.start_kill();
                    match res {
                        Ok(st) => JobStatus::Exited(st.code()),
                        Err(_) => JobStatus::Exited(None),
                    }
                }
                _ = kill_rx => {
                    if let Err(err) = child.start_kill()
                        && !is_already_gone(&err)
                    {
                        watcher_entry.push_output(format!("\n[kill warning: {err}]\n").as_bytes());
                    }
                    let _ = tokio::time::timeout(REAP_TIMEOUT, child.inner_mut().wait()).await;
                    JobStatus::Killed
                }
            };
            if let Some(h) = out_reader {
                let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
            }
            if let Some(h) = err_reader {
                let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
            }
            watcher_entry.settle(status);
        });

        Ok(self.register(entry))
    }

    /// The job or service registered as `id`.
    pub fn handle(&self, id: &str) -> Result<JobHandle> {
        lock(&self.jobs)
            .get(id)
            .cloned()
            .map(JobHandle)
            .ok_or_else(|| HrnError::Tool(format!("unknown background job `{id}`")))
    }

    /// Snapshot all tracked background jobs, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<JobInfo> {
        let mut all: Vec<_> = lock(&self.jobs).values().map(|e| e.info()).collect();
        all.sort_by_key(|i| i.started_ms);
        all
    }

    /// Signal every still-running background job to terminate immediately.
    pub fn kill_all(&self) {
        let entries: Vec<Arc<JobEntry>> = lock(&self.jobs).values().cloned().collect();
        for entry in entries {
            if let Some(tx) = lock(&entry.state).kill_tx.take() {
                let _ = tx.send(());
            }
        }
    }
}

async fn drain_pipe<R: AsyncRead + Unpin>(mut reader: R, entry: Arc<JobEntry>) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let bytes = buf.get(..n).unwrap_or_default();
                entry.push_output(bytes);
            }
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => break,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str, service: Option<&str>) -> JobSpec {
        JobSpec {
            command: command.to_owned(),
            cwd: std::env::temp_dir(),
            owner: "o".to_owned(),
            root: "r".to_owned(),
            service: service.map(str::to_owned),
            env: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn background_job_captures_output_and_exits() {
        let mgr = JobManager::new();
        let job = mgr
            .spawn(spec("echo bg-hello && echo bg-second", None))
            .expect("spawn");
        assert_eq!(job.info().id, "job-1");

        let mut rx = job.subscribe();
        while job.info().status == JobStatus::Running {
            rx.changed().await.expect("tick");
        }
        assert_eq!(job.info().status, JobStatus::Exited(Some(0)));
        let out = job.read(0, 4096);
        let text = String::from_utf8_lossy(&out.bytes);
        assert!(text.contains("bg-hello") && text.contains("bg-second"));
        assert!(job.read(out.next_offset, 4096).bytes.is_empty());
        assert!(job.tail(8).len() <= 8);
    }

    #[tokio::test]
    async fn background_job_can_be_killed_cleanly() {
        let mgr = JobManager::new();
        let job = mgr.spawn(spec("sleep 30", None)).expect("spawn");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(job.kill().await);
        assert_eq!(job.info().status, JobStatus::Killed);
        assert!(!job.request_kill());
    }

    #[tokio::test]
    async fn a_service_keeps_its_name_and_refuses_a_second_live_start() {
        let mgr = JobManager::new();
        let job = mgr.spawn(spec("sleep 30", Some("web"))).expect("spawn");
        assert!(job.info().service);
        assert_eq!(job.info().id, "web");
        assert!(mgr.spawn(spec("sleep 30", Some("web"))).is_err());
        assert!(mgr.handle("web").expect("registered").kill().await);
        mgr.spawn(spec("sleep 30", Some("web"))).expect("restart");
    }
}
