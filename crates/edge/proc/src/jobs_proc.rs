//! The jobs and services of a [`JobManager`] as `proc://` entries.

use crate::{JobInfo, JobManager, JobStatus, terminal_input};
use hrn_kernel::{HrnError, ProcEntry, ProcSource, Result};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// Step of the wait for a typed command's answer.
const INPUT_POLL: Duration = Duration::from_millis(25);

/// Output tail shown by `proc://<id>`; the rest is at `proc://<id>/streams`.
const TAIL_BYTES: usize = 8 * 1024;
/// Largest slice `proc://<id>/streams` returns (the whole retained log).
const STREAM_BYTES: usize = 1024 * 1024;

impl JobStatus {
    /// `running`, `exited (code 0)`, `exited (signal)` or `killed`.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Running => "running".to_owned(),
            Self::Exited(Some(code)) => format!("exited (code {code})"),
            Self::Exited(None) => "exited (signal)".to_owned(),
            Self::Killed => "killed".to_owned(),
        }
    }
}

fn kind(info: &JobInfo) -> &'static str {
    if info.service { "service" } else { "bash" }
}

fn line(info: &JobInfo) -> String {
    format!(
        "{} [{}] {} · {}s · {} bytes\n    {}",
        info.id,
        kind(info),
        info.status.describe(),
        info.elapsed_secs(),
        info.total_bytes,
        info.command
    )
}

impl ProcSource for JobManager {
    fn entries(&self) -> Vec<ProcEntry> {
        self.list()
            .into_iter()
            .map(|info| ProcEntry {
                line: line(&info),
                running: info.status == JobStatus::Running,
                waitable: !info.service,
                id: info.id,
                root: info.root,
                owner: info.owner,
                started_ms: info.started_ms,
                finished_ms: info.finished_ms,
            })
            .collect()
    }

    fn detail(&self, id: &str) -> Option<String> {
        let job = self.handle(id).ok()?;
        let info = job.info();
        let mut text = format!(
            "{} {}\nstatus: {}\ncwd: {}\n{}s · {} bytes of output\ncommand: {}",
            kind(&info),
            info.id,
            info.status.describe(),
            info.cwd.display(),
            info.elapsed_secs(),
            info.total_bytes,
            info.command
        );
        if info.total_bytes > TAIL_BYTES {
            let _ = write!(
                text,
                "\n\n[output tail; the whole log is at proc://{id}/streams]"
            );
        }
        let tail = job.tail(TAIL_BYTES);
        if tail.is_empty() {
            text.push_str("\n\n(no output)");
        } else {
            let _ = write!(text, "\n\n{tail}");
        }
        Some(text)
    }

    fn streams(&self, id: &str) -> Option<String> {
        let out = self.handle(id).ok()?.read(0, STREAM_BYTES);
        let mut text = String::new();
        if out.truncated_prefix {
            text.push_str("[earlier output dropped from the rolling buffer]\n");
        }
        if out.bytes.is_empty() && text.is_empty() {
            text.push_str("(no output)");
        }
        text.push_str(&String::from_utf8_lossy(&out.bytes));
        Some(text)
    }

    fn kill(&self, id: &str) -> Option<Result> {
        let job = self.handle(id).ok()?;
        Some(if job.request_kill() {
            Ok(())
        } else {
            Err(HrnError::Tool(format!(
                "job `{id}` already {}",
                job.info().status.as_str()
            )))
        })
    }

    /// Type into a `pty` job and wait for its answer to go quiet, so the caller can read the
    /// answer back with `proc://<id>`. The wait is [`JobManager::input_wait`]; zero types and
    /// returns at once. Runs on a blocking thread (as every `proc://` write does).
    fn write(&self, id: &str, content: &str) -> Option<Result> {
        let job = self.handle(id).ok()?;
        let wait = self.input_wait();
        Some((|| {
            let typed_at = job.info().total_bytes;
            job.send_input(&terminal_input(content))?;
            let quiet = wait.saturating_sub(INPUT_POLL);
            let started = Instant::now();
            let mut answered = None;
            while started.elapsed() < wait {
                std::thread::sleep(INPUT_POLL);
                let info = job.info();
                if info.status != JobStatus::Running {
                    break;
                }
                if info.total_bytes > typed_at {
                    answered = Some(Instant::now());
                }
                if answered.is_some_and(|at| started.elapsed() >= quiet && at.elapsed() >= quiet) {
                    break;
                }
            }
            Ok(())
        })())
    }
}
