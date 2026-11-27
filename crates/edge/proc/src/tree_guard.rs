//! Windows process-tree containment without `process-wrap`'s `JobObject`.
//!
//! `process-wrap` spawns every child `CREATE_SUSPENDED` and resumes it through a
//! `CreateToolhelp32Snapshot` of *every thread in the system*: ~135 ms per spawn against ~5 ms for
//! a bare one. This module keeps both guarantees without that cost:
//!
//! * **One tree dies on timeout/cancel/drop**: every child is put into its own kill-on-close job
//!   right after `CreateProcess` ([`TreeJob`]); closing that job terminates the child and all its
//!   descendants, and nothing else. The assignment is not atomic with process creation, but a
//!   freshly created process needs milliseconds of loader work before it can start a grandchild,
//!   against microseconds until the assignment.
//! * **Everything dies when hrn exits or crashes**: [`guard_process_tree`] puts hrn itself into a
//!   kill-on-close job once at startup. Every descendant inherits it (including the ones that
//!   beat the per-tree assignment) and the kernel closes its only handle when hrn goes away.
//!   Per-tree jobs nest under it.

use process_wrap::tokio::{ChildWrapper, CommandWrap, CommandWrapper};
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use win32job::{ExtendedLimitInfo, Job};

/// `CREATE_BREAKAWAY_FROM_JOB`: lets one spawn leave the hrn-wide job (see [`guard_process_tree`]).
pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// Put the current process into a kill-on-close job that is never closed by us: the kernel closes
/// it when the process exits or crashes and then terminates every process still in it.
///
/// Call once, early in `main`. A spawn that must outlive hrn (a browser opened for a login) passes
/// [`CREATE_BREAKAWAY_FROM_JOB`].
pub fn guard_process_tree() -> io::Result<()> {
    let mut limits = ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close().limit_breakaway_ok();
    let job = Job::create_with_limit_info(&limits).map_err(io::Error::other)?;
    job.assign_current_process().map_err(io::Error::other)?;
    // Keep the handle open for the rest of the process lifetime; closing it would kill hrn itself.
    let _kept_open = job.into_handle();
    Ok(())
}

/// Wrapper that gives each spawned child its own kill-on-close job.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TreeJob;

impl CommandWrapper for TreeJob {
    fn wrap_child(
        &mut self,
        mut inner: Box<dyn ChildWrapper>,
        _core: &CommandWrap,
    ) -> io::Result<Box<dyn ChildWrapper>> {
        let Some(process) = inner.process_handle().map(|h| h.as_raw_handle() as isize) else {
            let _ = inner.start_kill();
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "spawned child exposes no Windows process handle",
            ));
        };
        let mut limits = ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close();
        let job = Job::create_with_limit_info(&limits).map_err(|e| kill_and(&mut *inner, e))?;
        let job = match job.assign_process(process) {
            Ok(()) => Some(job),
            // A child that already exited (`cmd /c exit`) cannot be assigned and has no tree left
            // to track; anything else is a real failure.
            Err(err) => match inner.try_wait() {
                Ok(Some(_)) => None,
                _ => return Err(kill_and(&mut *inner, err)),
            },
        };
        Ok(Box::new(TreeChild { inner, job }))
    }
}

/// Kill the half-wrapped child and turn `err` into an `io::Error`.
fn kill_and(child: &mut dyn ChildWrapper, err: win32job::JobError) -> io::Error {
    let _ = child.start_kill();
    io::Error::other(err)
}

/// A child whose whole process tree lives in `job`; dropping or killing it closes the job.
#[derive(Debug)]
struct TreeChild {
    inner: Box<dyn ChildWrapper>,
    /// `None` once killed, or when the child had already exited at wrap time.
    job: Option<Job>,
}

impl ChildWrapper for TreeChild {
    fn inner(&self) -> &dyn ChildWrapper {
        self.inner.as_ref()
    }

    fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
        self.inner.as_mut()
    }

    fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
        let Self { inner, job } = *self;
        // Unwrapping must not kill the tree it just stopped tracking: leak the job handle.
        if let Some(job) = job {
            let _ = job.into_handle();
        }
        inner
    }

    fn process_handle(&self) -> Option<BorrowedHandle<'_>> {
        self.inner.process_handle()
    }

    /// Closing the kill-on-close job terminates the leader and every descendant.
    fn start_kill(&mut self) -> io::Result<()> {
        match self.job.take() {
            Some(job) => {
                drop(job);
                Ok(())
            }
            None => self.inner.start_kill(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_wrapped_command;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::process::Command;

    const MODE: &str = "HRN_TREE_MODE";
    const WAIT: Duration = Duration::from_secs(20);

    fn exe() -> String {
        std::env::current_exe()
            .expect("current exe")
            .to_string_lossy()
            .into_owned()
    }

    fn helper_args() -> Vec<String> {
        [
            "--exact",
            "tree_guard::tests::tree_helper",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn mode(m: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(MODE.to_owned(), m.to_owned())])
    }

    /// Not a real test: the body of the processes spawned by the tests below.
    #[test]
    fn tree_helper() {
        let Ok(mode) = std::env::var(MODE) else {
            return;
        };
        let spawn_sleeper = || {
            #[expect(
                clippy::zombie_processes,
                reason = "the grandchild is the test subject"
            )]
            let _grandchild = std::process::Command::new(exe())
                .args(helper_args())
                .env(MODE, "sleep")
                .spawn()
                .expect("spawn grandchild");
        };
        match mode.as_str() {
            "sleep" => {}
            "tree" => {
                spawn_sleeper();
                println!("\nREADY");
            }
            "guarded" => {
                guard_process_tree().expect("guard");
                spawn_sleeper();
                println!("\nREADY");
            }
            other => panic!("unknown mode {other}"),
        }
        std::thread::sleep(Duration::from_mins(1));
    }

    /// Wait for the helper's `READY` line (the libtest banner comes first).
    async fn ready(out: &mut (impl AsyncBufReadExt + Unpin)) {
        tokio::time::timeout(WAIT, async {
            let mut line = String::new();
            loop {
                line.clear();
                let n = out.read_line(&mut line).await.expect("read");
                assert!(n > 0, "helper closed stdout before READY");
                if line.trim() == "READY" {
                    break;
                }
            }
        })
        .await
        .expect("READY in time");
    }

    /// The pipe closes only once every holder (leader and the grandchild that inherited it) is gone.
    async fn whole_tree_gone(out: &mut (impl AsyncReadExt + Unpin)) {
        let mut rest = Vec::new();
        tokio::time::timeout(WAIT, out.read_to_end(&mut rest))
            .await
            .expect("every process of the tree died")
            .expect("read");
    }

    #[tokio::test]
    async fn killing_one_tree_kills_its_descendants_and_spares_siblings() {
        let cwd = Path::new(".");
        let mut a = build_wrapped_command(&exe(), &helper_args(), cwd, &mode("tree"))
            .spawn()
            .expect("spawn a");
        let mut b = build_wrapped_command(&exe(), &helper_args(), cwd, &mode("sleep"))
            .spawn()
            .expect("spawn b");
        let mut out = BufReader::new(a.stdout().take().expect("stdout"));
        ready(&mut out).await;

        a.start_kill().expect("kill tree a");
        whole_tree_gone(&mut out).await;
        assert!(
            b.inner_mut().try_wait().expect("try_wait b").is_none(),
            "a sibling tree must survive"
        );
        b.start_kill().expect("kill tree b");
    }

    #[tokio::test]
    async fn dropping_the_child_kills_its_descendants() {
        let mut child =
            build_wrapped_command(&exe(), &helper_args(), Path::new("."), &mode("tree"))
                .spawn()
                .expect("spawn");
        let mut out = BufReader::new(child.stdout().take().expect("stdout"));
        ready(&mut out).await;
        drop(child);
        whole_tree_gone(&mut out).await;
    }

    #[tokio::test]
    async fn crashing_hrn_kills_every_descendant_even_unwrapped_ones() {
        // The helper plays hrn: it guards itself, then starts a *plain* grandchild that no
        // per-tree job knows about; killing the helper stands in for a crash.
        let mut hrn = Command::new(exe())
            .args(helper_args())
            .env(MODE, "guarded")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn helper");
        let mut out = BufReader::new(hrn.stdout.take().expect("stdout"));
        ready(&mut out).await;
        hrn.kill().await.expect("kill helper");
        whole_tree_gone(&mut out).await;
    }
}
