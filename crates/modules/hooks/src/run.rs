//! One subprocess round-trip of the user-extension protocol: a JSON payload on stdin, a JSON
//! decision (or tool result) on stdout, under a wall-clock timeout.

use hrn_edge_rt::tokio;
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// What a finished hook/tool run produced.
pub(crate) struct RunResult {
    /// Exit code (`None` when the run was killed).
    pub code: Option<i32>,
    /// Raw stdout.
    pub stdout: String,
    /// Raw stderr.
    pub stderr: String,
    /// The run exceeded its timeout (the process was killed).
    pub timed_out: bool,
}

/// How a run ended: normally with an exit status, or killed by the timeout.
enum Exit {
    Done(std::process::ExitStatus),
    Killed,
}

/// Spawn `program` with `args` in `cwd`, feed it `stdin`, collect stdout/stderr, enforce
/// `timeout`, and abort with [`HrnError::Cancelled`] when `cancel` fires.
pub(crate) async fn run_json(
    program: &Path,
    args: &[String],
    cwd: &Path,
    stdin: &str,
    timeout: Duration,
    cancel: Option<&CancelFlag>,
) -> Result<RunResult> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The harness runs inside a kill-on-close job, so the tree dies with the session.
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| HrnError::Io(format!("spawn `{}`: {e}", program.display())))?;

    let Some(mut stdin_pipe) = child.stdin.take() else {
        return Err(HrnError::Internal("child stdin was not piped".to_owned()));
    };
    let payload = stdin.as_bytes().to_vec();
    let writer = tokio::spawn(async move {
        // A hook that never reads its stdin dies young; the broken pipe is not our error.
        let _ = stdin_pipe.write_all(&payload).await;
        let _ = stdin_pipe.flush().await;
        drop(stdin_pipe);
    });
    let out_task = slurp(child.stdout.take());
    let err_task = slurp(child.stderr.take());

    // The child lives inside the wait task: aborting it drops the child, whose `kill_on_drop`
    // takes the process down with it.
    let wait_task = tokio::spawn(async move { child.wait().await });
    let abort = wait_task.abort_handle();
    let exit = if let Some(cancel) = cancel {
        tokio::select! {
            result = wait_task => match result {
                Ok(Ok(status)) => Exit::Done(status),
                Ok(Err(e)) => {
                    let _ = cancel;
                    return Err(HrnError::Io(format!("wait `{}`: {e}", program.display())));
                }
                Err(_) => Exit::Killed,
            },
            () = tokio::time::sleep(timeout) => {
                abort.abort();
                Exit::Killed
            }
            () = cancel.cancelled() => {
                abort.abort();
                return Err(HrnError::Cancelled);
            }
        }
    } else {
        tokio::select! {
            result = wait_task => match result {
                Ok(Ok(status)) => Exit::Done(status),
                Ok(Err(e)) => {
                    return Err(HrnError::Io(format!("wait `{}`: {e}", program.display())));
                }
                Err(_) => Exit::Killed,
            },
            () = tokio::time::sleep(timeout) => {
                abort.abort();
                Exit::Killed
            }
        }
    };
    drop(writer);
    let code = match exit {
        Exit::Done(status) => status.code(),
        Exit::Killed => None,
    };
    Ok(RunResult {
        code,
        stdout: read(out_task).await,
        stderr: read(err_task).await,
        timed_out: code.is_none(),
    })
}

fn slurp<R: tokio::io::AsyncRead + Send + Unpin + 'static>(
    pipe: Option<R>,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let Some(pipe) = pipe else {
            return String::new();
        };
        let mut lines = BufReader::new(pipe).lines();
        let mut text = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            text.push_str(&line);
            text.push('\n');
        }
        text
    })
}

async fn read(handle: tokio::task::JoinHandle<String>) -> String {
    handle.await.unwrap_or_default()
}
