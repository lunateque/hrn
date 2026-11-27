//! Process execution with per-tree Win32 Job Objects (`KILL_ON_JOB_CLOSE`, see `tree_guard`) /
//! `process-wrap` Unix `ProcessGroup`, `PATHEXT` + `.cmd` resolution, Git Bash discovery, bounded output capture, and
//! persistent shell sessions.

mod ansi;
mod capture;
mod jobs;
mod jobs_proc;
mod pool;
mod session;
#[cfg(test)]
mod session_tests;
mod session_wire;
mod shell;
#[cfg(windows)]
mod tree_guard;

use capture::{Capture, Utf8Chunker};
use hrn_kernel::{CancelFlag, HrnError, Result};
use process_wrap::tokio::CommandWrap;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
#[cfg(windows)]
pub use tree_guard::{CREATE_BREAKAWAY_FROM_JOB, guard_process_tree};

pub use jobs::{
    Adopted, JobChunk, JobFeed, JobHandle, JobInfo, JobManager, JobSpec, JobStatus, terminal_input,
};
pub use pool::ShellPool;
pub use process_wrap::tokio::ChildWrapper;
pub use session::{ShellCall, ShellResult, ShellSession};
pub use shell::{Shell, resolve_on_path, resolve_program, resolve_shell, set_shell_path};

/// How long to wait for a killed leader to be reaped.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for reaping a leader after explicit cancellation.
const CANCEL_REAP_TIMEOUT: Duration = Duration::from_millis(120);
/// How long output pipes may stay open after the process tree was killed.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// Smallest accepted `max_output_bytes` (keeps head/tail splits meaningful).
const MIN_OUTPUT_BYTES: usize = 4096;
/// `CREATE_NO_WINDOW`.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Specification for spawning a one-shot command with clean process-tree teardown.
#[derive(Clone, Debug)]
pub struct RunSpec {
    /// Executable name or path (resolved via `PATHEXT` on Windows).
    pub program: String,
    /// Command arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Wall-clock timeout.
    pub timeout: Duration,
    /// Maximum output bytes retained in memory (split 3:1 between stdout and stderr, each kept
    /// as head + tail with an explicit omission marker).
    pub max_output_bytes: usize,
}

/// Completed process execution output.
#[derive(Clone, Debug)]
pub struct RunOutput {
    /// Process exit code (`None` if killed/signalled/timed out).
    pub exit_code: Option<i32>,
    /// Captured stdout (UTF-8 lossy).
    pub stdout: String,
    /// Captured stderr (UTF-8 lossy).
    pub stderr: String,
    /// Whether the command timed out.
    pub timed_out: bool,
    /// Whether output exceeded the retention limit (a marker is embedded in the text).
    pub truncated: bool,
    /// Non-fatal problems: failed descendant cleanup, pipe read errors, pipes held open.
    pub warnings: Vec<String>,
}

/// Extra environment of every shell, contributed by the `shell` module (its tool bridge).
static SHELL_EXTRA: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

/// Add environment variables to every shell this process starts from now on: one-shot runs,
/// persistent sessions, background jobs and pty jobs alike.
pub fn set_shell_env(vars: BTreeMap<String, String>) {
    SHELL_EXTRA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(vars);
}

/// Environment of every shell: pagers and credential prompts are disabled, since stdin is not a
/// terminal, plus the module contributions of [`set_shell_env`].
fn shell_env() -> BTreeMap<String, String> {
    [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_PAGER", "cat"),
        ("PAGER", "cat"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .chain(
        SHELL_EXTRA
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .collect()
}

/// Build a [`RunSpec`] that runs `command` through the POSIX shell from [`resolve_shell`] as a
/// fresh one-shot process.
pub fn shell_run_spec(
    command: &str,
    cwd: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<RunSpec> {
    let shell = resolve_shell()?;
    Ok(RunSpec {
        args: shell.command_args(command),
        program: shell.program,
        cwd,
        env: shell_env(),
        timeout,
        max_output_bytes,
    })
}

fn wrap_tree_guard(wrap: &mut CommandWrap) {
    // Dropping the child (task abort, panic, crash) must not leak the process tree.
    wrap.wrap(process_wrap::tokio::KillOnDrop);
    #[cfg(windows)]
    {
        wrap.wrap(process_wrap::tokio::CreationFlags(CREATE_NO_WINDOW));
        wrap.wrap(tree_guard::TreeJob);
    }
    #[cfg(unix)]
    {
        wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
    }
}

/// Build a [`CommandWrap`] configured with `PATHEXT`/`.cmd` handling and OS process-tree guard
/// (Win32 Job Object on Windows, `ProcessGroup` on Unix).
pub fn build_wrapped_command(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> CommandWrap {
    let resolved = resolve_program(program, cwd);
    let mut wrap = CommandWrap::with_new(&resolved, |cmd| {
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
    });
    wrap_tree_guard(&mut wrap);
    wrap
}

/// Build a [`CommandWrap`] for long-lived stdio MCP servers (`stdin` + `stdout` piped).
pub fn build_stdio_command(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> CommandWrap {
    let base = cwd.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let resolved = resolve_program(program, &base);
    let mut wrap = CommandWrap::with_new(&resolved, |cmd| {
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
    });
    wrap_tree_guard(&mut wrap);
    wrap
}

enum Outcome {
    Exited(io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Wait for the leader only, racing the deadline and cancellation.
async fn wait_outcome(
    child: &mut Box<dyn ChildWrapper>,
    deadline: tokio::time::Instant,
    cancel: &CancelFlag,
) -> Outcome {
    if cancel.is_cancelled() {
        return Outcome::Cancelled;
    }
    tokio::select! {
        biased;
        () = cancel.cancelled() => Outcome::Cancelled,
        status = child.inner_mut().wait() => Outcome::Exited(status),
        () = tokio::time::sleep_until(deadline) => Outcome::TimedOut,
    }
}

/// A killed process group/job may already be gone; that is success, not an error.
fn is_already_gone(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound || (cfg!(unix) && err.raw_os_error() == Some(3))
}

/// Execute a command with live chunk callbacks, timeout, and cooperative cancellation.
///
/// Only the *leader* is waited on, so a `server &`-style command cannot hang the call; once the
/// leader is gone the remaining tree is killed so no orphan outlives the call.
pub async fn run_streaming<F>(spec: &RunSpec, cancel: &CancelFlag, on_chunk: F) -> Result<RunOutput>
where
    F: Fn(String) + Send + Sync + 'static,
{
    if cancel.is_cancelled() {
        return Err(HrnError::Cancelled);
    }
    let mut wrap = build_wrapped_command(&spec.program, &spec.args, &spec.cwd, &spec.env);
    let mut child = wrap
        .spawn()
        .map_err(|e| HrnError::Io(format!("spawn `{}`: {e}", spec.program)))?;

    let max = spec.max_output_bytes.max(MIN_OUTPUT_BYTES);
    let (out_cap, err_cap) = (max / 4 * 3 / 2, max / 4 / 2);
    let out_buf = Arc::new(Mutex::new(Capture::new(out_cap, out_cap)));
    let err_buf = Arc::new(Mutex::new(Capture::new(err_cap, err_cap)));
    let warnings = Arc::new(Mutex::new(Vec::<String>::new()));
    let live_budget = Arc::new(AtomicUsize::new(max));
    let cb: Arc<dyn Fn(String) + Send + Sync> = Arc::new(on_chunk);

    let pump_ctx = |name: &'static str, buffer: &Arc<Mutex<Capture>>| Pump {
        name,
        buffer: Arc::clone(buffer),
        warnings: Arc::clone(&warnings),
        live_budget: Arc::clone(&live_budget),
        on_chunk: Arc::clone(&cb),
    };
    let out_task = child
        .stdout()
        .take()
        .map(|pipe| tokio::spawn(pump(pipe, pump_ctx("stdout", &out_buf))));
    let err_task = child
        .stderr()
        .take()
        .map(|pipe| tokio::spawn(pump(pipe, pump_ctx("stderr", &err_buf))));
    let mut readers: Vec<tokio::task::JoinHandle<()>> =
        out_task.into_iter().chain(err_task).collect();

    let deadline = tokio::time::Instant::now() + spec.timeout;
    let outcome = wait_outcome(&mut child, deadline, cancel).await;

    let leader_exited = matches!(outcome, Outcome::Exited(Ok(_)));
    // Kill the job/group: the leader itself on timeout/cancel, its leftover descendants after a
    // normal exit (`server &`, daemonised helpers).
    let mut local_warnings = Vec::new();
    if let Err(err) = child.start_kill()
        && !is_already_gone(&err)
    {
        if leader_exited {
            local_warnings.push(format!("process tree cleanup failed: {err}"));
        } else {
            for r in &readers {
                r.abort();
            }
            return Err(HrnError::Io(format!(
                "failed to kill `{}` after timeout/cancel: {err}",
                spec.program
            )));
        }
    }
    if matches!(outcome, Outcome::Cancelled) {
        for r in &readers {
            r.abort();
        }
        let _ = tokio::time::timeout(CANCEL_REAP_TIMEOUT, child.inner_mut().wait()).await;
        return Err(HrnError::Cancelled);
    }
    if !leader_exited && !matches!(outcome, Outcome::Exited(Err(_))) {
        match tokio::time::timeout(REAP_TIMEOUT, child.inner_mut().wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => local_warnings.push(format!("reaping killed process failed: {err}")),
            Err(_) => local_warnings.push("killed process was not reaped in time".to_owned()),
        }
    }

    drain_readers(&mut readers, &mut local_warnings).await;

    let timed_out = matches!(outcome, Outcome::TimedOut);
    let exit_code = match outcome {
        Outcome::Exited(Ok(status)) => status.code(),
        Outcome::Exited(Err(err)) => {
            return Err(HrnError::Io(format!("wait `{}`: {err}", spec.program)));
        }
        Outcome::TimedOut => None,
        Outcome::Cancelled => return Err(HrnError::Cancelled),
    };

    let (stdout, out_trunc) = render(&out_buf);
    let (stderr, err_trunc) = render(&err_buf);
    let mut all_warnings = std::mem::take(&mut *lock(&warnings));
    all_warnings.extend(local_warnings);
    Ok(RunOutput {
        exit_code,
        stdout,
        stderr,
        timed_out,
        truncated: out_trunc || err_trunc,
        warnings: all_warnings,
    })
}

async fn drain_readers(
    readers: &mut [tokio::task::JoinHandle<()>],
    local_warnings: &mut Vec<String>,
) {
    let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
        for r in readers.iter_mut() {
            if let Err(err) = r.await {
                local_warnings.push(format!("output reader failed: {err}"));
            }
        }
    })
    .await;
    if drained.is_err() {
        for r in readers.iter() {
            r.abort();
        }
        local_warnings.push(
            "output pipes stayed open after the process tree was killed (a descendant escaped the job)"
                .to_owned(),
        );
    }
}

/// Everything one pipe-reader task needs.
struct Pump {
    name: &'static str,
    buffer: Arc<Mutex<Capture>>,
    warnings: Arc<Mutex<Vec<String>>>,
    /// Bytes still allowed to be forwarded live through `on_chunk` (shared by both pipes).
    live_budget: Arc<AtomicUsize>,
    on_chunk: Arc<dyn Fn(String) + Send + Sync>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn render(buffer: &Mutex<Capture>) -> (String, bool) {
    let guard = lock(buffer);
    (guard.render(), guard.truncated())
}

/// Atomically subtract up to `n` from `budget`, returning the remaining budget before subtraction.
pub(crate) fn take_budget(budget: &AtomicUsize, n: usize) -> usize {
    let mut left = budget.load(Ordering::Relaxed);
    loop {
        let next = left.saturating_sub(n);
        match budget.compare_exchange_weak(left, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(prev) => return prev,
            Err(cur) => left = cur,
        }
    }
}

/// Drain one pipe to EOF. Always keeps reading (a full pipe would block the child); live
/// forwarding stops once the shared budget is spent so a flood cannot swamp the UI.
async fn pump<R: AsyncRead + Unpin>(mut reader: R, ctx: Pump) {
    let mut chunker = Utf8Chunker::default();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let bytes = buf.get(..n).unwrap_or_default();
                lock(&ctx.buffer).push(bytes);
                let granted = take_budget(&ctx.live_budget, n);
                let text = chunker.push(bytes);
                if granted > 0 && !text.is_empty() {
                    (ctx.on_chunk)(text);
                }
            }
            // The write end closing abruptly (killed tree) surfaces as a broken pipe on Windows.
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => break,
            Err(err) => {
                lock(&ctx.warnings).push(format!("{} read error: {err}", ctx.name));
                break;
            }
        }
    }
    let tail = chunker.finish();
    if !tail.is_empty() && ctx.live_budget.load(Ordering::Relaxed) > 0 {
        (ctx.on_chunk)(tail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Re-executes this test binary as a controllable child process (no external tools needed).
    fn self_spec(mode: &str, timeout: Duration, max_output_bytes: usize) -> RunSpec {
        RunSpec {
            program: std::env::current_exe()
                .expect("current exe")
                .to_string_lossy()
                .into_owned(),
            args: [
                "--exact",
                "tests::child_helper",
                "--nocapture",
                "--test-threads=1",
            ]
            .map(str::to_owned)
            .to_vec(),
            cwd: std::env::temp_dir(),
            env: BTreeMap::from([("HRN_PROC_CHILD".to_owned(), mode.to_owned())]),
            timeout,
            max_output_bytes,
        }
    }

    /// Not a real test: it is the body of the child process spawned by the tests below.
    #[test]
    fn child_helper() {
        use std::io::Write;
        let Ok(mode) = std::env::var("HRN_PROC_CHILD") else {
            return;
        };
        match mode.as_str() {
            "flood" => {
                // Interleave ~400 KB on each pipe: deadlocks if the pipes are not read concurrently.
                let mut out = std::io::stdout();
                let mut err = std::io::stderr();
                writeln!(out, "OUT-START").expect("stdout");
                writeln!(err, "ERR-START").expect("stderr");
                for i in 0..8000 {
                    writeln!(
                        out,
                        "out line {i:05} ................................................"
                    )
                    .expect("stdout");
                    writeln!(
                        err,
                        "err line {i:05} ................................................"
                    )
                    .expect("stderr");
                }
                writeln!(out, "OUT-END").expect("stdout");
                writeln!(err, "ERR-END").expect("stderr");
            }
            "sleep" => std::thread::sleep(Duration::from_mins(1)),
            "leave_grandchild" => {
                // Detached grandchild inherits our stdout pipe and outlives us.
                let exe = std::env::current_exe().expect("exe");
                #[expect(
                    clippy::zombie_processes,
                    reason = "the grandchild must outlive this helper"
                )]
                let _grandchild = std::process::Command::new(exe)
                    .args(["--exact", "tests::child_helper", "--nocapture"])
                    .env("HRN_PROC_CHILD", "sleep")
                    .spawn()
                    .expect("spawn grandchild");
                println!("PARENT-DONE");
            }
            "exit7" => std::process::exit(7),
            other => panic!("unknown child mode {other}"),
        }
    }

    #[tokio::test]
    async fn concurrent_flood_on_both_pipes_neither_deadlocks_nor_loses_head_or_tail() {
        let spec = self_spec("flood", Duration::from_mins(1), 64 * 1024);
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_cb = Arc::clone(&seen);
        let out = run_streaming(&spec, &CancelFlag::new(), move |c| {
            seen_cb.fetch_add(c.len(), Ordering::Relaxed);
        })
        .await
        .expect("run");
        assert_eq!(out.exit_code, Some(0), "stderr: {}", out.stderr);
        assert!(out.truncated);
        assert!(
            out.stdout.contains("OUT-START") && out.stdout.contains("OUT-END"),
            "{}",
            out.stdout.len()
        );
        assert!(out.stdout.contains("bytes omitted"));
        assert!(out.stderr.contains("ERR-START") && out.stderr.contains("ERR-END"));
        assert!(out.stdout.len() + out.stderr.len() <= 64 * 1024 + 256);
        assert!(
            seen.load(Ordering::Relaxed) <= 64 * 1024 + 8192,
            "live forwarding must be bounded"
        );
    }

    #[tokio::test]
    async fn timeout_kills_a_sleeping_child_promptly() {
        let spec = self_spec("sleep", Duration::from_millis(700), 8192);
        let started = Instant::now();
        let out = run_streaming(&spec, &CancelFlag::new(), |_| {})
            .await
            .expect("run");
        assert!(out.timed_out);
        assert_eq!(out.exit_code, None);
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_running_child() {
        let spec = self_spec("sleep", Duration::from_mins(2), 8192);
        let cancel = CancelFlag::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        let err = run_streaming(&spec, &cancel, |_| {})
            .await
            .expect_err("cancelled");
        assert!(matches!(err, HrnError::Cancelled), "{err}");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[tokio::test]
    async fn leftover_descendant_holding_the_pipe_does_not_hang_the_call() {
        let spec = self_spec("leave_grandchild", Duration::from_mins(1), 8192);
        let started = Instant::now();
        let out = run_streaming(&spec, &CancelFlag::new(), |_| {})
            .await
            .expect("run");
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("PARENT-DONE"), "{}", out.stdout);
        assert!(!out.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "waited for the grandchild"
        );
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    }

    #[tokio::test]
    async fn nonzero_exit_code_is_reported() {
        let spec = self_spec("exit7", Duration::from_secs(30), 8192);
        let out = run_streaming(&spec, &CancelFlag::new(), |_| {})
            .await
            .expect("run");
        assert_eq!(out.exit_code, Some(7));
    }
}
