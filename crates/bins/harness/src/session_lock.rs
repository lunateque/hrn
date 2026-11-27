//! Single-writer session lease files (`<data>/session-locks/<id>.lock`).
//!
//! Uses atomic create-exclusive files paired with OS PID liveness checks (no `unsafe`) so a
//! second `hrn` writer opening the same session degrades cleanly to read-only with a warning,
//! while stale locks left behind by a crashed process are reclaimed automatically.

use hrn_kernel::{HrnError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static TOKEN_SEQ: AtomicU64 = AtomicU64::new(1);
static HELD_PATHS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

/// Grace period before an empty/unparseable lock file is treated as stale rather than mid-write.
const CORRUPT_STALE_AGE: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
struct LockRecord {
    pid: u32,
    started_ms: i64,
    token: u64,
}

/// Owned write lock released automatically on [`Drop`].
#[derive(Debug)]
pub(crate) struct WriteLease {
    path: PathBuf,
    pid: u32,
    token: u64,
}

impl Drop for WriteLease {
    fn drop(&mut self) {
        if let Ok(raw) = fs::read_to_string(&self.path)
            && let Ok(rec) = serde_json::from_str::<LockRecord>(&raw)
            && rec.pid == self.pid
            && rec.token == self.token
        {
            let _ = fs::remove_file(&self.path);
        }
        if let Ok(mut held) = HELD_PATHS.lock() {
            held.remove(&self.path);
        }
    }
}

/// Result of attempting to lease a session for writing.
#[derive(Debug)]
pub(crate) enum SessionLease {
    /// Exclusive write ownership granted; dropped when the session closes.
    Write(#[expect(dead_code, reason = "held for its Drop impl")] WriteLease),
    /// Another live writer already owns the session; this instance must stay read-only.
    ReadOnly {
        /// PID of the active writer, if readable from the lock file.
        holder_pid: Option<u32>,
    },
}

impl SessionLease {
    /// True when this process does not hold the exclusive write lock.
    pub(crate) const fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }

    /// User-visible warning shown when a session opened read-only.
    pub(crate) fn warning(&self) -> Option<String> {
        match self {
            Self::Write(_) => None,
            Self::ReadOnly {
                holder_pid: Some(pid),
            } => Some(format!(
                "session is already open for writing (pid {pid}) · opened read-only"
            )),
            Self::ReadOnly { holder_pid: None } => {
                Some("session is already open for writing · opened read-only".to_owned())
            }
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn safe_lock_name(session_id: &str) -> String {
    let cleaned: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session.lock".to_owned()
    } else {
        format!("{cleaned}.lock")
    }
}

fn read_record(path: &Path) -> Option<LockRecord> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn is_stale_corrupt(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= CORRUPT_STALE_AGE)
}

/// Check whether OS process `pid` is still alive (zero `unsafe`).
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    #[cfg(windows)]
    {
        if i32::try_from(pid).is_err() {
            return false;
        }
        let Ok(out) = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            // Fail safe: if tasklist cannot run, do not steal an unknown lock.
            return true;
        };
        if !out.status.success() {
            return true;
        }
        let needle = format!("\"{pid}\"");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line.split(',').nth(1) == Some(needle.as_str()))
    }
    #[cfg(not(windows))]
    {
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        if Path::new("/proc").is_dir() {
            return proc_dir.exists();
        }
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_or(true, |s| s.success())
    }
}

fn try_create_lock(path: &Path, pid: u32, token: u64) -> std::io::Result<WriteLease> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let record = LockRecord {
        pid,
        started_ms: now_ms(),
        token,
    };
    let bytes = serde_json::to_vec(&record).unwrap_or_default();
    file.write_all(&bytes)?;
    file.sync_all()?;
    if let Ok(mut held) = HELD_PATHS.lock() {
        held.insert(path.to_path_buf());
    }
    Ok(WriteLease {
        path: path.to_path_buf(),
        pid,
        token,
    })
}

/// Liveness classification of a session `.lock` file used by `hrn gc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockState {
    /// Held by a live OS process (or recently created); must not be removed or GC'd.
    Active,
    /// Holder process is gone or lock file is stale and corrupt; safe to remove.
    Stale,
}

/// Filename (`<sanitized>.lock`) corresponding to `session_id` under `<data>/session-locks`.
pub(crate) fn lock_file_name(session_id: &str) -> String {
    safe_lock_name(session_id)
}

/// Inspect a single regular `.lock` file to determine whether it is active or stale.
pub(crate) fn inspect_lock_file(path: &Path) -> LockState {
    if let Some(rec) = read_record(path) {
        if is_pid_alive(rec.pid) {
            LockState::Active
        } else {
            LockState::Stale
        }
    } else if is_stale_corrupt(path) {
        LockState::Stale
    } else {
        LockState::Active
    }
}

/// Acquire a single-writer lease for `session_id` under `lock_dir` (`<data>/session-locks`).
///
/// # Errors
/// Returns an I/O error if `lock_dir` cannot be created or the lock file fails for a reason other
/// than contention.
pub(crate) fn acquire_lease(lock_dir: &Path, session_id: &str) -> Result<SessionLease> {
    fs::create_dir_all(lock_dir)
        .map_err(|e| HrnError::Io(format!("create session lock dir: {e}")))?;
    let path = lock_dir.join(safe_lock_name(session_id));
    let pid = std::process::id();
    if HELD_PATHS.lock().is_ok_and(|held| held.contains(&path)) {
        return Ok(SessionLease::ReadOnly {
            holder_pid: Some(pid),
        });
    }

    let token = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
    match try_create_lock(&path, pid, token) {
        Ok(write) => return Ok(SessionLease::Write(write)),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(HrnError::Io(format!(
                "create session lock `{}`: {err}",
                path.display()
            )));
        }
    }

    // Lock already exists: inspect the holder and reclaim only if clearly stale.
    if let Some(rec) = read_record(&path) {
        if is_pid_alive(rec.pid) {
            return Ok(SessionLease::ReadOnly {
                holder_pid: Some(rec.pid),
            });
        }
        let _ = fs::remove_file(&path);
    } else if is_stale_corrupt(&path) {
        let _ = fs::remove_file(&path);
    } else {
        return Ok(SessionLease::ReadOnly { holder_pid: None });
    }

    match try_create_lock(&path, pid, token) {
        Ok(write) => Ok(SessionLease::Write(write)),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(SessionLease::ReadOnly {
            holder_pid: read_record(&path).map(|r| r.pid),
        }),
        Err(err) => Err(HrnError::Io(format!(
            "reclaim session lock `{}`: {err}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock_dir(tag: &str) -> PathBuf {
        let n = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("hrn-locks-{tag}-{}-{n}", std::process::id()))
    }

    #[test]
    fn lease_second_writer_read_only() {
        let dir = temp_lock_dir("second");
        let first = acquire_lease(&dir, "sess_42").expect("first lease");
        assert!(!first.is_read_only());
        assert!(first.warning().is_none());

        let second = acquire_lease(&dir, "sess_42").expect("second lease");
        assert!(second.is_read_only());
        assert!(
            second
                .warning()
                .as_deref()
                .is_some_and(|w| w.contains("read-only"))
        );

        drop(first);
        let third = acquire_lease(&dir, "sess_42").expect("third lease after release");
        assert!(!third.is_read_only());
        drop(third);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_dead_pid_lock_is_reclaimed() {
        let dir = temp_lock_dir("stale");
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("sess_stale.lock");
        let dead = LockRecord {
            pid: u32::MAX - 1,
            started_ms: 1,
            token: 1,
        };
        fs::write(&path, serde_json::to_vec(&dead).expect("json")).expect("write");

        let lease = acquire_lease(&dir, "sess_stale").expect("reclaim");
        assert!(!lease.is_read_only(), "dead-PID lock should be reclaimed");
        drop(lease);
        let _ = fs::remove_dir_all(&dir);
    }
}
