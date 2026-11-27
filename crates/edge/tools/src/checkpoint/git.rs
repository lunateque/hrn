//! Running `git` against the shadow repository: environment, timeout and `index.lock` retries.

use hrn_kernel::{HrnError, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// `CREATE_NO_WINDOW`.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How often a running git is checked for exit or timeout.
const POLL: Duration = Duration::from_millis(5);
/// Tries when another process holds the shadow index.
const LOCK_TRIES: u32 = 40;
const LOCK_WAIT: Duration = Duration::from_millis(50);

/// What a finished git call printed.
pub(super) struct Printed {
    pub(super) stdout: Vec<u8>,
    pub(super) status: Option<i32>,
}

impl Printed {
    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// A git bound to one shadow directory and the work tree it snapshots.
pub(super) struct Git<'a> {
    pub(super) dir: &'a Path,
    pub(super) work_tree: &'a Path,
    pub(super) timeout: Duration,
}

impl Git<'_> {
    /// Run `git <args>`; a non-zero exit is an error carrying git's message.
    pub(super) fn run(&self, args: &[&str]) -> Result<Printed> {
        let printed = self.run_unchecked(args)?;
        if printed.status == Some(0) {
            Ok(printed)
        } else {
            Err(HrnError::Tool(format!(
                "checkpoint: `git {}` failed ({})",
                args.first().copied().unwrap_or(""),
                printed.text().trim()
            )))
        }
    }

    /// Run `git <args>` and report its exit status instead of judging it. Stderr is folded into
    /// the printed text so a failure explains itself.
    pub(super) fn run_unchecked(&self, args: &[&str]) -> Result<Printed> {
        let mut tries = 0;
        loop {
            let printed = self.once(args)?;
            tries += 1;
            if printed.status != Some(0)
                && tries < LOCK_TRIES
                && printed.text().contains("index.lock")
            {
                thread::sleep(LOCK_WAIT);
                continue;
            }
            return Ok(printed);
        }
    }

    fn once(&self, args: &[&str]) -> Result<Printed> {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(self.dir)
            .arg("--work-tree")
            .arg(self.work_tree)
            .args(args)
            .current_dir(self.work_tree)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|e| {
            HrnError::Tool(format!(
                "checkpoint: cannot run `git` ({e}); is it installed?"
            ))
        })?;
        let out = child.stdout.take().map(drain);
        let err = child.stderr.take().map(drain);
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code(),
                Ok(None) if started.elapsed() < self.timeout => thread::sleep(POLL),
                Ok(None) => {
                    let killed = child.kill().and_then(|()| child.wait());
                    return Err(HrnError::Tool(format!(
                        "checkpoint: `git {}` ran over {}s in `{}` and was stopped{}; exclude the \
                         large directories (`[checkpoint] exclude`)",
                        args.first().copied().unwrap_or(""),
                        self.timeout.as_secs(),
                        self.work_tree.display(),
                        killed
                            .err()
                            .map_or_else(String::new, |e| format!(" (kill failed: {e})")),
                    )));
                }
                Err(e) => return Err(HrnError::Tool(format!("checkpoint: waiting for git: {e}"))),
            }
        };
        let mut stdout = out.map_or_else(|| Ok(Vec::new()), join)?;
        let stderr = err.map_or_else(|| Ok(Vec::new()), join)?;
        if status != Some(0) {
            stdout.extend(stderr);
        }
        Ok(Printed { stdout, status })
    }
}

/// Read a pipe to the end on its own thread so a chatty git cannot fill it and stall.
fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<std::io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn join(handle: thread::JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| HrnError::Internal("checkpoint: a git output reader panicked".into()))?
        .map_err(|e| HrnError::Tool(format!("checkpoint: reading git output: {e}")))
}
