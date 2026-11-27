//! Persistent shell sessions: one long-lived POSIX shell per agent session, started lazily and
//! driven through a sentinel-delimited protocol on its stdin/stdout/stderr.
//!
//! A call writes the command as a quoted heredoc, `eval`s it in the shell itself (so `cd`,
//! `export`, functions and aliases persist across calls), then prints a per-call marker with the
//! exit code and working directory on stdout and a bare marker on stderr. Output is streamed up to
//! the marker and never includes it. A timeout or cancellation kills only the process tree the
//! shell is currently running; a shell that does not come back is dropped (Job Object / process
//! group teardown) and replaced on the next call.

use crate::session_wire::{
    INIT, Marked, PWD, Pipe, Sink, call_script, make_marker, pump_to_marker, single_quote,
};
use crate::shell::{Shell, resolve_shell};
use crate::{MIN_OUTPUT_BYTES, RunOutput, resolve_program, shell_env, wrap_tree_guard};
use hrn_kernel::{CancelFlag, HrnError, Result};
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

/// How long a freshly spawned shell may take to answer the handshake.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the shell may take to come back after its running command was interrupted.
const RECOVER_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause before interrupting again when the shell has not come back yet (a command that spawns
/// one blocking child after another needs a kill per child).
const RETRY_INTERRUPT: Duration = Duration::from_secs(1);
/// Upper bound for the helper that kills the running command tree.
const KILL_HELPER_TIMEOUT: Duration = Duration::from_secs(20);

/// Interrupts the running command: `INT` makes the shell's `__hrn_run` return (abandoning the rest
/// of the command line), and the direct children of the shell (pid in `$1`) plus everything below
/// them are killed so the shell stops waiting. The shell itself stays alive. Windows: MSYS `/proc`
/// knows the shell's direct children and their Windows pids; `taskkill /T` then walks the native
/// tree below them.
const INTERRUPT_SCRIPT_WINDOWS: &str = r#"kill -INT "$1" 2>/dev/null
a=()
for d in /proc/[0-9]*; do
  pp=; read -r pp < "$d/ppid" 2>/dev/null
  if [ "$pp" = "$1" ]; then w=; read -r w < "$d/winpid" 2>/dev/null; [ -n "$w" ] && a+=(//PID "$w"); fi
done
[ ${#a[@]} -gt 0 ] && taskkill //F //T "${a[@]}" >/dev/null 2>&1
exit 0"#;
const INTERRUPT_SCRIPT_UNIX: &str = r#"kill -INT "$1" 2>/dev/null
k() { for c in $(pgrep -P "$1" 2>/dev/null); do k "$c"; kill -KILL "$c" 2>/dev/null; done; }
k "$1"
exit 0"#;

/// One command to run in a [`ShellSession`].
#[derive(Clone, Copy, Debug)]
pub struct ShellCall<'a> {
    /// Shell command line.
    pub command: &'a str,
    /// The agent's working directory. A live shell is moved there when this differs from the
    /// value of the previous call, so a session that changes directory is followed.
    pub cwd: &'a Path,
    /// Explicit directory for this call (the tool's `cwd` argument). It sticks, like `cd`.
    pub cwd_override: Option<&'a Path>,
    /// Environment exported for this call (the `shell` module's tool bridge); the newest call's
    /// value wins, and a call without one leaves the previous export in place.
    pub exports: &'a BTreeMap<String, String>,
    /// Wall-clock timeout of the command.
    pub timeout: Duration,
    /// Maximum output bytes retained in memory, as for [`crate::RunSpec`].
    pub max_output_bytes: usize,
}

/// Result of [`ShellSession::run`].
#[derive(Clone, Debug)]
pub struct ShellResult {
    /// Output and exit status of the command (`exit_code` is `None` after a timeout).
    pub run: RunOutput,
    /// The shell's working directory after the command, when the command changed it.
    pub cwd_changed: Option<String>,
    /// A previous shell had exited and a new one (with fresh cwd/env) served this call.
    pub restarted: bool,
}

/// A lazily started persistent shell. Not `Sync`-shared: callers serialise with a mutex (see
/// [`crate::ShellPool`]).
#[derive(Default)]
pub struct ShellSession {
    live: Option<Live>,
    /// The agent cwd supplied by the previous call.
    last_cwd: Option<PathBuf>,
    calls: u64,
    /// The previous shell exited or was dropped; the next call reports a restart.
    died: bool,
}

struct Live {
    shell: Shell,
    child: Box<dyn ChildWrapper>,
    stdin: ChildStdin,
    out: Pipe<ChildStdout>,
    err: Pipe<ChildStderr>,
    /// The shell's own `$$` (an MSYS pid on Windows), needed to find its children.
    pid: String,
    /// Working directory as the shell reports it (`pwd`).
    cwd: String,
}

impl Live {
    /// Start the shell in `cwd` and wait for its handshake (pid and working directory).
    async fn spawn(cwd: &Path, call: u64) -> Result<Self> {
        let shell = resolve_shell()?;
        let program = resolve_program(&shell.program, cwd);
        let mut wrap = CommandWrap::with_new(&program, |cmd| {
            cmd.args(&shell.args)
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (k, v) in shell_env() {
                cmd.env(k, v);
            }
        });
        wrap_tree_guard(&mut wrap);
        let mut child = wrap
            .spawn()
            .map_err(|e| HrnError::Io(format!("spawn `{}`: {e}", shell.program)))?;
        let fail = |why: &str| HrnError::Io(format!("`{}` did not start: {why}", shell.program));
        let (Some(stdin), Some(stdout), Some(stderr)) = (
            child.stdin().take(),
            child.stdout().take(),
            child.stderr().take(),
        ) else {
            return Err(fail("no stdio pipes"));
        };
        let mut live = Self {
            shell: shell.clone(),
            child,
            stdin,
            out: Pipe::new(stdout),
            err: Pipe::new(stderr),
            pid: String::new(),
            cwd: String::new(),
        };

        let marker = make_marker(call);
        let q = single_quote(&marker);
        live.send(&format!(
            "{INIT}; printf '%s%d:' {q} \"$$\"; {PWD}; printf '%s\\n' {q} >&2\n"
        ))
        .await?;
        let budget = AtomicUsize::new(0);
        let discard = |_: String| {};
        let mut sink_out = Sink::new(MIN_OUTPUT_BYTES, &budget, &discard);
        let mut sink_err = Sink::new(MIN_OUTPUT_BYTES, &budget, &discard);
        let Live { out, err, .. } = &mut live;
        let replies = tokio::time::timeout(START_TIMEOUT, async {
            tokio::join!(
                pump_to_marker(out, marker.as_bytes(), &mut sink_out),
                pump_to_marker(err, marker.as_bytes(), &mut sink_err)
            )
        })
        .await
        .map_err(|_| fail("handshake timed out"))?;
        let (Marked::Line(hello), Marked::Line(_)) = (
            replies.0.map_err(|e| fail(&e.to_string()))?,
            replies.1.map_err(|e| fail(&e.to_string()))?,
        ) else {
            return Err(fail("exited during startup"));
        };
        let (pid, cwd) = hello
            .split_once(':')
            .ok_or_else(|| fail("malformed handshake"))?;
        live.pid = pid.to_owned();
        live.cwd = cwd.to_owned();
        Ok(live)
    }

    async fn send(&mut self, script: &str) -> Result<()> {
        let io_err = |e: io::Error| HrnError::Io(format!("write to `{}`: {e}", self.shell.program));
        self.stdin
            .write_all(script.as_bytes())
            .await
            .map_err(io_err)?;
        self.stdin.flush().await.map_err(io_err)
    }

    /// Whether the shell process is still running.
    fn alive(&mut self) -> bool {
        matches!(self.child.inner_mut().try_wait(), Ok(None))
    }
}

/// Interrupt the command the shell is running (see `INTERRUPT_SCRIPT_*`), keeping the shell.
/// Returns a warning when the helper could not do it.
async fn interrupt_running_command(shell: &Shell, pid: &str) -> Option<String> {
    let script = if cfg!(windows) {
        INTERRUPT_SCRIPT_WINDOWS
    } else {
        INTERRUPT_SCRIPT_UNIX
    };
    let mut cmd = tokio::process::Command::new(resolve_program(&shell.program, Path::new(".")));
    cmd.args(&shell.args)
        .args(["-c", script, "_", pid])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(crate::CREATE_NO_WINDOW);
    match tokio::time::timeout(KILL_HELPER_TIMEOUT, cmd.status()).await {
        Ok(Ok(_)) => None,
        Ok(Err(err)) => Some(format!("killing the running command failed: {err}")),
        Err(_) => Some("killing the running command timed out".to_owned()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Interrupt {
    Timeout,
    Cancel,
}

impl ShellSession {
    /// An empty session; the shell starts on the first [`run`](Self::run).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            live: None,
            last_cwd: None,
            calls: 0,
            died: false,
        }
    }

    /// Make a live shell available and hand it `command`; reports whether a previous shell had
    /// to be replaced.
    async fn start_call(&mut self, call: &ShellCall<'_>, marker: &str) -> Result<bool> {
        let mut replaced = std::mem::take(&mut self.died);
        if let Some(live) = &mut self.live
            && !live.alive()
        {
            self.live = None;
            replaced = true;
        }
        let moved = self.last_cwd.as_deref() != Some(call.cwd);
        self.last_cwd = Some(call.cwd.to_path_buf());
        let cd = call.cwd_override.or(moved.then_some(call.cwd));
        for attempt in 0..2 {
            let fresh = self.live.is_none();
            if fresh {
                let live = Live::spawn(call.cwd_override.unwrap_or(call.cwd), self.calls).await?;
                self.live = Some(live);
            }
            let script = call_script(
                call.command,
                if fresh { None } else { cd },
                call.exports,
                marker,
            );
            let Some(live) = self.live.as_mut() else {
                return Err(HrnError::Internal("shell vanished after spawn".to_owned()));
            };
            match live.send(&script).await {
                Ok(()) => return Ok(replaced),
                // The shell died while idle: replace it once, then give up.
                Err(_) if !fresh && attempt == 0 => {
                    self.live = None;
                    replaced = true;
                }
                Err(err) => return Err(err),
            }
        }
        Err(HrnError::Internal(
            "shell restart loop exhausted".to_owned(),
        ))
    }

    /// Run `call.command` in the session's shell, streaming output through `on_chunk`.
    ///
    /// On timeout only the command's process tree is killed and the shell (with its cwd and
    /// environment) survives; a shell that fails to come back, or exits by itself (`exit`), is
    /// replaced on the next call.
    pub async fn run<F>(
        &mut self,
        call: &ShellCall<'_>,
        cancel: &CancelFlag,
        on_chunk: &F,
    ) -> Result<ShellResult>
    where
        F: Fn(String) + Sync,
    {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        self.calls += 1;
        let marker = make_marker(self.calls);
        let restarted = self.start_call(call, &marker).await?;
        let Some(live) = self.live.as_mut() else {
            return Err(HrnError::Internal("shell vanished after start".to_owned()));
        };
        let cwd_before = live.cwd.clone();

        let limit = call.max_output_bytes.max(MIN_OUTPUT_BYTES);
        let budget = AtomicUsize::new(limit);
        let mut sink_out = Sink::new(limit / 4 * 3 / 2, &budget, on_chunk);
        let mut sink_err = Sink::new(limit / 4 / 2, &budget, on_chunk);
        let driven = drive(
            live,
            &marker,
            (&mut sink_out, &mut sink_err),
            call.timeout,
            cancel,
        )
        .await;
        let Driven {
            joined,
            interrupt,
            mut warnings,
        } = driven;

        let mut exit_code = None;
        let mut cwd_changed = None;
        match joined {
            Some((Ok(Marked::Line(status)), Ok(Marked::Line(_)))) => {
                if let Some((rc, cwd)) = status.split_once(':') {
                    exit_code = rc.parse::<i32>().ok();
                    if cwd != cwd_before {
                        cwd.clone_into(&mut live.cwd);
                        cwd_changed = Some(cwd.to_owned());
                    }
                } else {
                    warnings.push(format!("malformed shell status line `{status}`"));
                }
            }
            Some((out_res, err_res)) => {
                // EOF or a read error: the shell is gone (`exit`, crash, external kill).
                for res in [out_res, err_res] {
                    if let Err(err) = res {
                        warnings.push(format!("shell pipe error: {err}"));
                    }
                }
                exit_code = self.reap_dead_shell().await;
                warnings.push(
                    "the shell exited; a new shell (fresh cwd and environment) starts on the next call"
                        .to_owned(),
                );
            }
            None => {
                if let Some(mut live) = self.live.take() {
                    let _ = live.child.start_kill();
                }
                self.died = true;
                warnings.push(
                    "the shell did not recover after its command was killed; a new shell (fresh cwd and environment) starts on the next call"
                        .to_owned(),
                );
            }
        }
        if interrupt == Some(Interrupt::Cancel) {
            return Err(HrnError::Cancelled);
        }
        let timed_out = interrupt == Some(Interrupt::Timeout);
        if timed_out {
            exit_code = None;
        }
        let truncated = sink_out.truncated() || sink_err.truncated();
        Ok(ShellResult {
            run: RunOutput {
                exit_code,
                stdout: sink_out.render(),
                stderr: sink_err.render(),
                timed_out,
                truncated,
                warnings,
            },
            cwd_changed,
            restarted,
        })
    }

    /// Drop the shell whose pipes closed and return its exit code when it can still be reaped.
    async fn reap_dead_shell(&mut self) -> Option<i32> {
        let mut live = self.live.take()?;
        self.died = true;
        match tokio::time::timeout(Duration::from_secs(2), live.child.inner_mut().wait()).await {
            Ok(Ok(status)) => status.code(),
            _ => None,
        }
    }
}

/// What waiting for one command's markers produced.
struct Driven {
    /// Both pipes' results, or `None` when the shell did not come back after being interrupted.
    joined: Option<(io::Result<Marked>, io::Result<Marked>)>,
    interrupt: Option<Interrupt>,
    warnings: Vec<String>,
}

/// Stream a command's output until its markers arrive, enforcing the timeout and cancellation by
/// killing the shell's running process tree (not the shell).
async fn drive(
    live: &mut Live,
    marker: &str,
    (sink_out, sink_err): (&mut Sink<'_>, &mut Sink<'_>),
    timeout: Duration,
    cancel: &CancelFlag,
) -> Driven {
    let Live {
        out,
        err,
        shell,
        pid,
        ..
    } = live;
    let work = async {
        tokio::join!(
            pump_to_marker(out, marker.as_bytes(), sink_out),
            pump_to_marker(err, marker.as_bytes(), sink_err)
        )
    };
    tokio::pin!(work);
    let mut interrupt = None;
    let mut warnings = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    // Meaningful once interrupted: when to interrupt again, and when to give up on the shell.
    let (mut next_interrupt, mut give_up) = (deadline, deadline);
    let joined = loop {
        let cause = tokio::select! {
            biased;
            () = cancel.cancelled(), if interrupt.is_none() => {
                interrupt = Some(Interrupt::Cancel);
                break None;
            }
            res = &mut work => break Some(res),
            () = tokio::time::sleep_until(deadline), if interrupt.is_none() => Some(Interrupt::Timeout),
            () = tokio::time::sleep_until(next_interrupt), if interrupt.is_some() => interrupt,
            () = tokio::time::sleep_until(give_up), if interrupt.is_some() => break None,
        };
        if let Some(cause) = cause {
            if interrupt.is_none() {
                give_up = tokio::time::Instant::now() + RECOVER_TIMEOUT;
            }
            interrupt = Some(cause);
            if let Some(warning) = interrupt_running_command(shell, pid).await
                && !warnings.contains(&warning)
            {
                warnings.push(warning);
            }
            next_interrupt = tokio::time::Instant::now() + RETRY_INTERRUPT;
        }
    };
    Driven {
        joined,
        interrupt,
        warnings,
    }
}
