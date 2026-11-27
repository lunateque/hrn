//! Cross-process lock guarding one credential file while it is written or refreshed (another
//! `hrn` may be refreshing the same login): `create_new` on `<file>.lock`, broken when stale.

use hrn_kernel::{HrnError, Result};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A holder still busy after this long is presumed dead: token requests time out well before.
const STALE_AFTER: Duration = Duration::from_secs(30);
/// Give up waiting for a live holder after this long.
const WAIT_MAX: Duration = Duration::from_secs(40);
/// Poll interval while another process holds the lock.
const POLL: Duration = Duration::from_millis(50);

/// A held lock; the lock file is removed on drop.
pub(crate) struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Take the lock for `target`, waiting while another live process holds it.
    pub(crate) async fn acquire(target: &Path) -> Result<Self> {
        Self::acquire_with(target, STALE_AFTER, WAIT_MAX).await
    }

    async fn acquire_with(
        target: &Path,
        stale_after: Duration,
        wait_max: Duration,
    ) -> Result<Self> {
        let mut name = target.as_os_str().to_owned();
        name.push(".lock");
        let path = PathBuf::from(name);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| HrnError::Io(format!("create `{}`: {e}", parent.display())))?;
        }
        let started = Instant::now();
        loop {
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path);
            let Err(err) = opened else {
                return Ok(Self { path });
            };
            // Windows reports a lock file that is still being deleted as "access denied"; only a
            // missing file with that error is a real permission problem.
            let contended = match err.kind() {
                ErrorKind::AlreadyExists => true,
                ErrorKind::PermissionDenied => !matches!(
                    std::fs::symlink_metadata(&path),
                    Err(e) if e.kind() == ErrorKind::NotFound
                ),
                _ => false,
            };
            if !contended {
                return Err(HrnError::Io(format!(
                    "create lock `{}`: {err}",
                    path.display()
                )));
            }
            if is_stale(&path, stale_after) {
                // A crashed holder: break its lock and race for a fresh one.
                std::fs::remove_file(&path).ok();
                continue;
            }
            if started.elapsed() >= wait_max {
                return Err(HrnError::Auth(format!(
                    "timed out waiting for another hrn to finish with `{}` (delete `{}` if none is running)",
                    target.display(),
                    path.display()
                )));
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

fn is_stale(path: &Path, stale_after: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().unwrap_or_default() >= stale_after)
}

impl Drop for FileLock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_held_lock_blocks_until_released_and_a_stale_one_is_broken() {
        let dir = std::env::temp_dir().join(format!("hrn-oauth-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("claude.json");

        let held = FileLock::acquire(&target).await.expect("first");
        let waiting = FileLock::acquire_with(&target, Duration::from_mins(1), Duration::ZERO)
            .await
            .err()
            .expect("live holder blocks");
        assert!(waiting.to_string().contains("timed out"), "{waiting}");
        drop(held);
        let again = FileLock::acquire_with(&target, Duration::from_mins(1), Duration::ZERO)
            .await
            .expect("released lock is free");
        // Leak the file as a crashed holder would; a zero staleness window breaks it.
        std::mem::forget(again);
        FileLock::acquire_with(&target, Duration::ZERO, Duration::ZERO)
            .await
            .expect("stale lock is broken");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
