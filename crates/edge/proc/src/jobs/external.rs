//! Jobs whose process is driven by the caller: a foreground command that outlived its wait and
//! became a background job. The job table shows it, `proc://` reads its output and kills it; the
//! caller feeds the output and reports how it ended.

use super::{JobEntry, JobHandle, JobManager, JobSpec, JobStatus};
use hrn_kernel::Result;
use std::sync::Arc;
use tokio::sync::oneshot;

/// The caller's end of an adopted job.
#[derive(Clone)]
pub struct JobFeed(Arc<JobEntry>);

impl JobFeed {
    /// Append output.
    pub fn push(&self, bytes: &[u8]) {
        self.0.push_output(bytes);
    }

    /// Record how the job ended.
    pub fn finish(&self, status: JobStatus) {
        self.0.settle(status);
    }
}

/// A job registered by [`JobManager::adopt`].
pub struct Adopted {
    /// The job as `proc://` and followers see it.
    pub handle: JobHandle,
    /// Where the driver puts its output and outcome.
    pub feed: JobFeed,
    /// Resolves when the job is killed (`write proc://<id>/kill`): the driver must stop.
    pub kill: oneshot::Receiver<()>,
}

impl JobManager {
    /// Register a job the caller runs itself.
    ///
    /// # Errors
    ///
    /// When a service of that name is already running.
    pub fn adopt(&self, spec: JobSpec) -> Result<Adopted> {
        let id = self.allocate(&spec)?;
        let (entry, kill) = JobEntry::new(spec, id, None);
        Ok(Adopted {
            feed: JobFeed(Arc::clone(&entry)),
            handle: self.register(entry),
            kill,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn an_adopted_job_is_listed_fed_killed_and_settled_by_its_driver() {
        let mgr = JobManager::new();
        let adopted = mgr
            .adopt(JobSpec {
                command: "slow".to_owned(),
                cwd: std::env::temp_dir(),
                owner: "o".to_owned(),
                root: "r".to_owned(),
                service: None,
                env: BTreeMap::new(),
            })
            .expect("adopt");
        adopted.feed.push(b"so far\n");
        assert_eq!(mgr.handle("job-1").expect("listed").tail(100), "so far\n");
        assert!(adopted.handle.request_kill());
        adopted.kill.await.expect("kill delivered");
        adopted.feed.finish(JobStatus::Killed);
        assert_eq!(adopted.handle.info().status, JobStatus::Killed);
    }
}
