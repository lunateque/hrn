//! `pty` jobs: the command runs on a pseudo-terminal (`ConPTY` on Windows, `openpty` elsewhere), so
//! programs that want a terminal (REPLs, installers, prompts) work and their output is captured as
//! the text of the screen ([`Stripper`]). Input arrives through [`JobHandle::send_input`].

use super::{InputWriter, JobEntry, JobHandle, JobManager, JobSpec, JobStatus};
use crate::ansi::Stripper;
use crate::{REAP_TIMEOUT, lock, resolve_program, resolve_shell, shell_env};
use hrn_kernel::{HrnError, Result};
use portable_pty::{Child, ChildKiller, CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
#[cfg(windows)]
use win32job::{ExtendedLimitInfo, Job};

/// Terminal size every `pty` job sees.
const SIZE: PtySize = PtySize {
    rows: 40,
    cols: 160,
    pixel_width: 0,
    pixel_height: 0,
};
/// How long output an exited program already wrote may still be on its way to the reader before
/// the console is closed; closing earlier can cut the last lines.
const FLUSH_GRACE: Duration = Duration::from_millis(150);

/// What to type for `text`: line breaks become the Enter key (`CR`), and Enter is appended unless
/// the text already ends with one.
#[must_use]
pub fn terminal_input(text: &str) -> Vec<u8> {
    let mut out = text.replace("\r\n", "\n").replace('\n', "\r").into_bytes();
    if out.last() != Some(&b'\r') {
        out.push(b'\r');
    }
    out
}

fn pty_error(what: &str, err: impl std::fmt::Display) -> HrnError {
    HrnError::Io(format!("{what}: {err}"))
}

fn spawn_thread(
    name: &str,
    body: impl FnOnce() + Send + 'static,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map_err(|e| pty_error(&format!("start {name} thread"), e))
}

/// The process tree of a `pty` job: closing its kill-on-close job ends the whole tree (Windows);
/// elsewhere killing the leader, which is a session leader, is what the platform offers.
struct Tree {
    #[cfg(windows)]
    job: Option<Job>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl Tree {
    /// Contain `child`, killing it here and now when containment fails on a process still running.
    #[cfg_attr(
        not(windows),
        expect(
            clippy::unnecessary_wraps,
            reason = "only the Windows job-object containment can fail"
        )
    )]
    fn new(child: &mut (dyn Child + Send + Sync)) -> Result<Self> {
        let killer = child.clone_killer();
        #[cfg(windows)]
        {
            let handle: std::os::windows::io::RawHandle =
                child.as_raw_handle().ok_or_else(|| {
                    let _ = child.kill();
                    HrnError::Io("pty child exposes no process handle".to_owned())
                })?;
            let mut limits = ExtendedLimitInfo::new();
            limits.limit_kill_on_job_close();
            let job = Job::create_with_limit_info(&limits).map_err(|e| {
                let _ = child.kill();
                pty_error("create pty job object", e)
            })?;
            let job = match job.assign_process(handle as isize) {
                Ok(()) => Some(job),
                // A child that already exited has no tree left to track.
                Err(err) => {
                    if !matches!(child.try_wait(), Ok(Some(_))) {
                        let _ = child.kill();
                        return Err(pty_error("contain pty process tree", err));
                    }
                    // A child that exited during the failed assign has no tree left to track.
                    None
                }
            };
            Ok(Self { job, killer })
        }
        #[cfg(not(windows))]
        Ok(Self { killer })
    }

    /// Stop the tree; a process that is already gone is not an error.
    fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(windows)]
        drop(self.job.take());
        match self.killer.kill() {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

impl JobManager {
    /// Run `spec.command` through the POSIX shell on a pseudo-terminal as a background job: its
    /// log is the terminal's text, [`JobHandle::send_input`] types into it, and `proc://<id>`
    /// shows and stops it like any other job.
    ///
    /// # Errors
    ///
    /// When the pseudo-terminal or the shell cannot be started, or a running service already has
    /// the spec's name.
    pub fn spawn_pty(&self, spec: JobSpec) -> Result<JobHandle> {
        let shell = resolve_shell()?;
        let mut cmd = CommandBuilder::new(resolve_program(&shell.program, Path::new(".")));
        cmd.args(shell.command_args(&spec.command));
        cmd.cwd(&spec.cwd);
        for (k, v) in shell_env().into_iter().chain(spec.env.clone()) {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        let id = self.allocate(&spec)?;

        let pair = native_pty_system()
            .openpty(SIZE)
            .map_err(|e| pty_error("open pseudo-terminal", e))?;
        let mut child = pair.slave.spawn_command(cmd).map_err(|e| {
            let _ = e;
            pty_error(&format!("spawn `{}` on a pty", shell.program), e)
        })?;
        drop(pair.slave);
        let mut tree = Tree::new(child.as_mut())?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| pty_error("pty reader", e))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| pty_error("pty writer", e))?;
        let (entry, mut kill_rx) = JobEntry::new(spec, id, Some(Box::new(writer) as InputWriter));

        let reader_thread = spawn_thread("hrn-pty-reader", {
            let entry = Arc::clone(&entry);
            move || read_screen(reader, &entry)
        })?;
        let (exit_tx, mut exit_rx) = oneshot::channel::<Option<i32>>();
        let master = pair.master;
        spawn_thread("hrn-pty-wait", move || {
            let code = child
                .wait()
                .ok()
                .and_then(|status| i32::try_from(status.exit_code()).ok());
            std::thread::sleep(FLUSH_GRACE);
            // Closing the console ends the reader; only then is all output in the log.
            drop(master);
            let _ = reader_thread.join();
            let _ = exit_tx.send(code);
        })?;

        let watched = Arc::clone(&entry);
        tokio::spawn(async move {
            let status = tokio::select! {
                code = &mut exit_rx => match kill_rx.try_recv() {
                    // A kill that let the child exit "normally" is still a kill.
                    Ok(()) | Err(oneshot::error::TryRecvError::Closed) => JobStatus::Killed,
                    Err(oneshot::error::TryRecvError::Empty) => JobStatus::Exited(code.ok().flatten()),
                },
                _ = &mut kill_rx => {
                    if let Err(err) = tree.kill() {
                        watched.push_output(format!("\n[kill warning: {err}]\n").as_bytes());
                    }
                    let _ = tokio::time::timeout(REAP_TIMEOUT, &mut exit_rx).await;
                    JobStatus::Killed
                }
            };
            watched.settle(status);
        });
        Ok(self.register(entry))
    }
}

/// Copy the terminal's output into the job's log as plain text until the console closes, and
/// answer the terminal queries in it: `ConPTY` asks for the cursor position at startup and, on
/// older hosts, prints nothing until it gets the answer.
fn read_screen(mut reader: Box<dyn Read + Send>, entry: &JobEntry) {
    let mut strip = Stripper::default();
    let (mut raw, mut text) = ([0u8; 4096], Vec::new());
    loop {
        match reader.read(&mut raw) {
            Ok(0) => break,
            Ok(n) => {
                text.clear();
                strip.feed(raw.get(..n).unwrap_or_default(), &mut text);
                let replies = strip.take_replies();
                if !replies.is_empty()
                    && let Some(input) = lock(&entry.input).as_mut()
                {
                    // A failed reply surfaces as the program's own hang or error, not here.
                    let _ = input.write_all(&replies).and_then(|()| input.flush());
                }
                entry.push_output(&text);
            }
            // The console closing mid-read surfaces as `EIO` on Linux.
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(err) if err.raw_os_error() == Some(5) => break,
            Err(err) => {
                entry.push_output(format!("\n[pty read error: {err}]\n").as_bytes());
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spec(command: &str) -> JobSpec {
        JobSpec {
            command: command.to_owned(),
            cwd: std::env::temp_dir(),
            owner: "o".to_owned(),
            root: "r".to_owned(),
            service: None,
            env: BTreeMap::new(),
        }
    }

    async fn settled(job: &JobHandle) {
        let mut rx = job.subscribe();
        let wait = async {
            while job.info().status == JobStatus::Running {
                rx.changed().await.expect("tick");
            }
        };
        tokio::time::timeout(Duration::from_secs(30), wait)
            .await
            .expect("job did not end");
    }

    async fn wait_for_output(job: &JobHandle, needle: &str) {
        let mut rx = job.subscribe();
        let wait = async {
            while !job.tail(4096).contains(needle) {
                assert_eq!(
                    job.info().status,
                    JobStatus::Running,
                    "job ended before printing {needle:?}: {:?}",
                    job.tail(4096)
                );
                rx.changed().await.expect("tick");
            }
        };
        tokio::time::timeout(Duration::from_secs(20), wait)
            .await
            .unwrap_or_else(|_| panic!("job never printed {needle:?}: {:?}", job.tail(4096)));
    }

    #[test]
    fn enter_is_a_carriage_return_and_added_once() {
        assert_eq!(terminal_input("1+1"), b"1+1\r");
        assert_eq!(terminal_input("a\nb\n"), b"a\rb\r");
        assert_eq!(terminal_input(""), b"\r");
    }

    #[tokio::test]
    async fn a_program_on_a_terminal_reads_what_is_typed() {
        let mgr = JobManager::new();
        let job = mgr
            .spawn_pty(spec("printf 'READY\\n'; read -r name; echo hello-$name"))
            .expect("spawn");
        assert!(job.info().interactive);
        wait_for_output(&job, "READY").await;
        job.send_input(&terminal_input("pty")).expect("type");
        settled(&job).await;
        let text = job.tail(4096);
        assert!(text.contains("hello-pty"), "{text:?}");
        assert_eq!(job.info().status, JobStatus::Exited(Some(0)));
        assert!(!job.info().interactive);
        assert!(job.send_input(b"x").is_err());
    }

    #[tokio::test]
    async fn killing_a_pty_job_ends_the_whole_tree() {
        let mgr = JobManager::new();
        let job = mgr
            .spawn_pty(spec("printf 'READY\\n'; sleep 60"))
            .expect("spawn");
        wait_for_output(&job, "READY").await;
        assert!(job.kill().await);
        let mut rx = job.subscribe();
        for _ in 0..60 {
            if job.info().status != JobStatus::Running {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_millis(250), rx.changed()).await;
        }
        assert_eq!(job.info().status, JobStatus::Killed);
    }
}
