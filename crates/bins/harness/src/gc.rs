//! `hrn gc [--apply]`: dry-run / applied garbage collection for expired sessions, orphan artifact
//! directories, stale session lock files, and conditional WAL checkpoint + `VACUUM`.
//!
//! Security invariants:
//! - Default mode (`apply = false`) performs ZERO disk or database writes.
//! - Pinned sessions and sessions with live-PID lock files are NEVER deleted.
//! - Every artifact directory and lock file target is verified via `symlink_metadata` and
//!   `canonicalize` to stay strictly inside `<data_dir>` (symlinks are never followed).

use crate::config::GcConfig;
use crate::config_layers::{LayerInput, load_layers};
use crate::session_lock::{LockState, inspect_lock_file, lock_file_name};
use crate::term::out;
use hrn_edge_store::SqliteStore;
use hrn_harness::ArtifactStore;
use hrn_kernel::{HrnError, Result, SessionMeta, SessionStore};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MILLIS_PER_DAY: i64 = 86_400_000;
const SESSION_ROW_OVERHEAD_BYTES: u64 = 256;
const MAX_CHILD_DEPTH: usize = 16;

/// Load only `[gc]` from operator config layers (`Default -> Global -> Overlays -> Env`),
/// ignoring untrusted project `.harness/config.toml` and failing closed on malformed global config,
/// missing/malformed `--config` overlays, or invalid `[gc]` settings.
pub(crate) fn load_gc_config(
    global_path: Option<PathBuf>,
    overlays: &[PathBuf],
    cwd: &Path,
    home: Option<PathBuf>,
    env: BTreeMap<String, String>,
) -> Result<GcConfig> {
    if let Some(ref gp) = global_path {
        match fs::symlink_metadata(gp) {
            Ok(_) if !gp.is_file() => {
                return Err(HrnError::Config(format!(
                    "global config `{}` is not a readable file",
                    gp.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(HrnError::Config(format!(
                    "cannot stat global config `{}`: {e}",
                    gp.display()
                )));
            }
        }
    }
    let input = LayerInput {
        global_path,
        project_path: None,
        overlays: overlays.to_vec(),
        cwd: cwd.to_path_buf(),
        home,
        env,
        runtime: toml::Table::new(),
    };
    let merged = load_layers(&input)?;
    let Some(gc_val) = merged.root.get("gc") else {
        return Ok(GcConfig::default());
    };
    gc_val
        .clone()
        .try_into::<GcConfig>()
        .map_err(|e| HrnError::Config(format!("invalid [gc] configuration: {e}")))
}

/// Derive the `SQLite` WAL sidecar path (`<db_path>-wal`) directly from `db_path`.
pub(crate) fn wal_path_for(db_path: &Path) -> PathBuf {
    let mut raw = db_path.as_os_str().to_os_string();
    raw.push("-wal");
    PathBuf::from(raw)
}

/// Summary of one `hrn gc` dry-run or apply execution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GcReport {
    /// `false` for dry-run (zero writes), `true` when `--apply` executed deletions.
    pub(crate) applied: bool,
    /// Total top-level sessions inspected.
    pub(crate) total_sessions: usize,
    /// Top-level session IDs eligible for deletion (deleted when `applied == true`).
    pub(crate) eligible_sessions: Vec<String>,
    /// Orphan artifact directories found (pruned when `applied == true`).
    pub(crate) orphan_artifacts: usize,
    /// Stale lock files found (removed when `applied == true`).
    pub(crate) stale_locks: usize,
    /// Live lock files skipped (their sessions are also protected from deletion).
    pub(crate) active_locks_skipped: usize,
    /// Estimated database bytes owned by eligible sessions.
    pub(crate) estimated_session_bytes: u64,
    /// Disk bytes owned by orphan artifact directories.
    pub(crate) orphan_artifact_bytes: u64,
    /// Disk bytes owned by stale lock files.
    pub(crate) stale_lock_bytes: u64,
    /// Whether `wal_checkpoint(TRUNCATE)` + `VACUUM` ran.
    pub(crate) vacuumed: bool,
}

impl GcReport {
    /// Combined estimated reclaimable (or reclaimed) bytes across DB, artifacts, and locks.
    pub(crate) const fn total_bytes(&self) -> u64 {
        self.estimated_session_bytes
            .saturating_add(self.orphan_artifact_bytes)
            .saturating_add(self.stale_lock_bytes)
    }
}

/// Normalize a directory path for per-`cwd` grouping (matching `resume.rs`).
fn normalize_cwd(raw: &str) -> String {
    let norm = raw
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned();
    if cfg!(windows) {
        norm.to_ascii_lowercase()
    } else {
        norm
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

struct LockScan {
    locks_canon: Option<PathBuf>,
    active_names: HashSet<String>,
    stale_files: Vec<(PathBuf, u64)>,
}

/// Scan `<data_dir>/session-locks` without following symlinks or escaping `data_dir`.
fn scan_locks(data_dir: &Path) -> Result<LockScan> {
    let mut scan = LockScan {
        locks_canon: None,
        active_names: HashSet::new(),
        stale_files: Vec::new(),
    };
    if !data_dir.exists() {
        return Ok(scan);
    }
    let data_canon = fs::canonicalize(data_dir)
        .map_err(|e| HrnError::Io(format!("canonicalize `{}`: {e}", data_dir.display())))?;
    let locks_dir = data_dir.join("session-locks");
    let dir_meta = match fs::symlink_metadata(&locks_dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(scan),
        Err(e) => return Err(HrnError::Io(format!("stat `{}`: {e}", locks_dir.display()))),
    };
    if dir_meta.file_type().is_symlink() || !dir_meta.file_type().is_dir() {
        return Err(HrnError::Io(format!(
            "refused symlink or non-directory lock root `{}`",
            locks_dir.display()
        )));
    }
    let locks_canon = fs::canonicalize(&locks_dir)
        .map_err(|e| HrnError::Io(format!("canonicalize `{}`: {e}", locks_dir.display())))?;
    if !locks_canon.starts_with(&data_canon) {
        return Err(HrnError::Io(format!(
            "lock directory `{}` escapes data_dir `{}`",
            locks_canon.display(),
            data_canon.display()
        )));
    }
    scan.locks_canon = Some(locks_canon.clone());

    for entry in fs::read_dir(&locks_canon)
        .map_err(|e| HrnError::Io(format!("list `{}`: {e}", locks_canon.display())))?
    {
        let entry =
            entry.map_err(|e| HrnError::Io(format!("entry `{}`: {e}", locks_canon.display())))?;
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"))
        {
            continue;
        }
        let Ok(canon) = fs::canonicalize(&path) else {
            continue;
        };
        if !canon.starts_with(&locks_canon) {
            continue;
        }
        match inspect_lock_file(&canon) {
            LockState::Active => {
                scan.active_names.insert(name.to_owned());
            }
            LockState::Stale => {
                scan.stale_files.push((canon, meta.len()));
            }
        }
    }
    scan.stale_files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(scan)
}

/// Remove one verified stale lock file after re-checking non-symlink confinement inside `locks_canon`.
fn remove_lock_confined(locks_canon: &Path, path: &Path) -> Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(HrnError::Io(format!("stat `{}`: {e}", path.display()))),
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Ok(false);
    }
    let canon = fs::canonicalize(path)
        .map_err(|e| HrnError::Io(format!("canonicalize `{}`: {e}", path.display())))?;
    if !canon.starts_with(locks_canon) {
        return Err(HrnError::Io(format!(
            "refused to delete lock outside lock root `{}`",
            path.display()
        )));
    }
    fs::remove_file(&canon)
        .map_err(|e| HrnError::Io(format!("remove lock `{}`: {e}", canon.display())))?;
    Ok(true)
}

fn has_protected_descendant(
    store: &dyn SessionStore,
    session_id: &str,
    pinned_ids: &HashSet<String>,
    active_lock_names: &HashSet<String>,
    cutoff_ms: i64,
    depth: usize,
) -> bool {
    if depth >= MAX_CHILD_DEPTH {
        return true;
    }
    let Ok(children) = store.list_children(session_id) else {
        return true;
    };
    for child in &children {
        let locked = active_lock_names.contains(&lock_file_name(&child.id));
        if child.pinned
            || pinned_ids.contains(&child.id)
            || locked
            || child.updated_at_ms >= cutoff_ms
            || has_protected_descendant(
                store,
                &child.id,
                pinned_ids,
                active_lock_names,
                cutoff_ms,
                depth + 1,
            )
        {
            return true;
        }
    }
    false
}

fn collect_descendant_ids(
    store: &dyn SessionStore,
    session_id: &str,
    out: &mut Vec<String>,
    depth: usize,
) {
    if depth >= MAX_CHILD_DEPTH {
        return;
    }
    if let Ok(children) = store.list_children(session_id) {
        for child in children {
            let cid = child.id;
            collect_descendant_ids(store, &cid, out, depth + 1);
            out.push(cid);
        }
    }
}

/// Compute which top-level sessions are protected by pinning, active locks, protected child
/// sessions, global/per-cwd recency, or the `retain_days` age window.
fn compute_protected_ids(
    store: &dyn SessionStore,
    sessions: &[SessionMeta],
    pinned_ids: &HashSet<String>,
    active_lock_names: &HashSet<String>,
    cfg: &GcConfig,
    now_ms: i64,
) -> HashSet<String> {
    let mut protected = HashSet::new();
    let retain_ms = i64::try_from(cfg.retain_days)
        .unwrap_or(i64::MAX / MILLIS_PER_DAY)
        .saturating_mul(MILLIS_PER_DAY);
    let cutoff_ms = now_ms.saturating_sub(retain_ms);

    for s in sessions {
        let locked = active_lock_names.contains(&lock_file_name(&s.id));
        if s.pinned
            || pinned_ids.contains(&s.id)
            || locked
            || s.updated_at_ms >= cutoff_ms
            || has_protected_descendant(store, &s.id, pinned_ids, active_lock_names, cutoff_ms, 0)
        {
            protected.insert(s.id.clone());
        }
    }

    // Retain newest N globally.
    let mut by_recency: Vec<&SessionMeta> = sessions.iter().collect();
    by_recency.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| b.id.cmp(&a.id))
    });
    for s in by_recency.iter().take(cfg.retain_newest_global) {
        protected.insert(s.id.clone());
    }

    // Retain newest N per normalized cwd.
    let mut per_cwd: HashMap<String, Vec<&SessionMeta>> = HashMap::new();
    for s in sessions {
        per_cwd.entry(normalize_cwd(&s.cwd)).or_default().push(s);
    }
    for group in per_cwd.values_mut() {
        group.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        for s in group.iter().take(cfg.retain_newest_per_cwd) {
            protected.insert(s.id.clone());
        }
    }
    protected
}

fn estimate_session_bytes(store: &dyn SessionStore, session_id: &str) -> u64 {
    let mut bytes = SESSION_ROW_OVERHEAD_BYTES;
    if let Ok(nodes) = store.load_tree(session_id) {
        for node in &nodes {
            let payload_len = serde_json::to_string(&node.payload).map_or(0, |s| s.len());
            bytes = bytes.saturating_add(u64::try_from(payload_len + 96).unwrap_or(u64::MAX));
        }
    }
    if let Ok(children) = store.list_children(session_id) {
        for child in &children {
            bytes = bytes.saturating_add(estimate_session_bytes(store, &child.id));
        }
    }
    bytes
}

/// Run storage garbage collection against `store` and `data_dir`.
///
/// When `apply == false`, performs ZERO database or filesystem writes.
pub(crate) fn run_gc(
    store: &dyn SessionStore,
    data_dir: &Path,
    db_path: Option<&Path>,
    cfg: &GcConfig,
    now_ms: i64,
    apply: bool,
) -> Result<GcReport> {
    let locks = scan_locks(data_dir)?;
    let sessions = store.list_sessions(usize::MAX)?;
    let pinned_ids: HashSet<String> = store.pinned_sessions()?.into_iter().collect();
    let protected = compute_protected_ids(
        store,
        &sessions,
        &pinned_ids,
        &locks.active_names,
        cfg,
        now_ms,
    );

    let mut eligible_sessions = Vec::new();
    let mut surviving_ids = Vec::new();
    let mut estimated_session_bytes = 0u64;
    for s in &sessions {
        if protected.contains(&s.id) {
            surviving_ids.push(s.id.clone());
            collect_descendant_ids(store, &s.id, &mut surviving_ids, 0);
        } else {
            estimated_session_bytes =
                estimated_session_bytes.saturating_add(estimate_session_bytes(store, &s.id));
            eligible_sessions.push(s.id.clone());
        }
    }

    let artifacts = ArtifactStore::new(data_dir.to_path_buf());
    let orphans = artifacts.scan_orphans(&surviving_ids)?;
    let orphan_artifact_bytes: u64 = orphans
        .iter()
        .fold(0u64, |acc, item| acc.saturating_add(item.bytes));
    let stale_lock_bytes: u64 = locks
        .stale_files
        .iter()
        .fold(0u64, |acc, (_, sz)| acc.saturating_add(*sz));

    let mut report = GcReport {
        applied: apply,
        total_sessions: sessions.len(),
        eligible_sessions,
        orphan_artifacts: orphans.len(),
        stale_locks: locks.stale_files.len(),
        active_locks_skipped: locks.active_names.len(),
        estimated_session_bytes,
        orphan_artifact_bytes,
        stale_lock_bytes,
        vacuumed: false,
    };

    if !apply {
        return Ok(report);
    }

    for id in &report.eligible_sessions {
        store.delete_session(id)?;
    }
    report.orphan_artifacts = artifacts.prune_orphans(&surviving_ids)?;

    let mut removed_locks = 0usize;
    if let Some(locks_canon) = locks.locks_canon.as_deref() {
        for (lock_path, _) in &locks.stale_files {
            if remove_lock_confined(locks_canon, lock_path)? {
                removed_locks += 1;
            }
        }
    }
    report.stale_locks = removed_locks;

    let wal_bytes = db_path
        .map(wal_path_for)
        .and_then(|p| fs::symlink_metadata(p).ok())
        .map_or(0, |m| m.len());
    let db_reclaimable = report.estimated_session_bytes.saturating_add(wal_bytes);
    if (!report.eligible_sessions.is_empty() || wal_bytes > 0)
        && db_reclaimable >= cfg.vacuum_reclaim_bytes
    {
        store.vacuum_and_checkpoint()?;
        report.vacuumed = true;
    }

    Ok(report)
}

/// Format a human-readable structured report for `hrn gc [--apply]`.
pub(crate) fn format_report(report: &GcReport) -> String {
    let mode = if report.applied { "applied" } else { "dry-run" };
    let verb = if report.applied {
        "removed"
    } else {
        "eligible"
    };
    let mut text = String::new();
    let _ = writeln!(text, "hrn gc ({mode})");
    let _ = writeln!(
        text,
        "  sessions ({verb}): {} of {} ({} bytes)",
        report.eligible_sessions.len(),
        report.total_sessions,
        report.estimated_session_bytes
    );
    let _ = writeln!(
        text,
        "  orphan artifacts ({verb}): {} ({} bytes)",
        report.orphan_artifacts, report.orphan_artifact_bytes
    );
    let _ = writeln!(
        text,
        "  stale locks ({verb}): {} ({} bytes; {} active skipped)",
        report.stale_locks, report.stale_lock_bytes, report.active_locks_skipped
    );
    let _ = writeln!(
        text,
        "  total {}: {} bytes",
        if report.applied {
            "reclaimed"
        } else {
            "reclaimable"
        },
        report.total_bytes()
    );
    if report.applied {
        let _ = writeln!(
            text,
            "  wal_checkpoint + vacuum: {}",
            if report.vacuumed {
                "ran"
            } else {
                "skipped (below threshold)"
            }
        );
    } else if !report.eligible_sessions.is_empty()
        || report.orphan_artifacts > 0
        || report.stale_locks > 0
    {
        let _ = writeln!(text, "  re-run with `hrn gc --apply` to delete");
    }
    text
}

/// CLI entry point for `hrn gc [--apply]`.
pub(crate) fn run_cli(cfg: &GcConfig, data_dir: &Path, db_path: &Path, apply: bool) -> Result {
    let store = if !apply && !db_path.is_file() {
        SqliteStore::open_in_memory()?
    } else if !apply {
        SqliteStore::open_read_only(db_path)?
    } else {
        SqliteStore::open(db_path)?
    };
    let report = run_gc(&store, data_dir, Some(db_path), cfg, now_ms(), apply)?;
    out(&format_report(&report));
    Ok(())
}

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;
