//! Acceptance and security-confinement tests for `hrn gc [--apply]`.

use super::*;
use crate::config::GcConfig;
use crate::session_lock::acquire_lease;
use hrn_edge_store::SqliteStore;
use hrn_harness::ArtifactStore;
use hrn_kernel::{
    AgentStatus, DagNode, DagPayload, Msg, PermissionMode, SessionMeta, SessionStore, ThinkingLevel,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hrn-gc-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const NOW_MS: i64 = 1_800_000_000_000;
const DAY_MS: i64 = 86_400_000;

fn seed_session(store: &SqliteStore, id: &str, cwd: &str, age_days: i64) {
    let ts = NOW_MS - age_days * DAY_MS;
    let meta = SessionMeta {
        id: id.to_owned(),
        title: format!("Session {id}"),
        cwd: cwd.to_owned(),
        branch: None,
        head_node_id: None,
        model: None,
        thinking: ThinkingLevel::Off,
        goal: None,
        permission_mode: PermissionMode::Default,
        parent_session_id: None,
        parent_node_id: None,
        status: AgentStatus::Idle,
        pinned: false,
        title_source: None,
        exit_ms: None,
        created_at_ms: ts,
        updated_at_ms: ts,
    };
    store.create_session(&meta).expect("create");
    store
        .append_node(&DagNode {
            id: format!("n_{id}"),
            session_id: id.to_owned(),
            parent_id: None,
            created_at_ms: ts,
            payload: DagPayload::Message {
                turn_id: "t1".to_owned(),
                msg: Msg::user(format!("Work for {id}")),
            },
        })
        .expect("append");
}

fn snapshot_dir(root: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<PathBuf, (u64, SystemTime)>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let meta = fs::symlink_metadata(&p).expect("stat");
                if meta.is_dir() {
                    walk(&p, base, out);
                } else {
                    let rel = p.strip_prefix(base).expect("strip").to_path_buf();
                    out.insert(rel, (meta.len(), meta.modified().expect("mtime")));
                }
            }
        }
    }
    let mut map = BTreeMap::new();
    walk(root, root, &mut map);
    map
}

#[test]
fn gc_dry_run_no_writes() {
    let tmp = TempDir::new("dry-run");
    let data_dir = tmp.path().join("data");
    let db_path = data_dir.join("sessions.db");
    {
        let store = SqliteStore::open(&db_path).expect("open");
        seed_session(&store, "old_1", "/work/a", 120);
        seed_session(&store, "old_2", "/work/a", 110);
        // Checkpoint cleanly so WAL is empty before read-only dry-run snapshot.
        store.vacuum_and_checkpoint().expect("checkpoint");
    }
    let artifacts = ArtifactStore::new(data_dir.clone());
    artifacts
        .write_output("orphan_sess", "worker", "# orphan artifact", None)
        .expect("write orphan");
    let locks_dir = data_dir.join("session-locks");
    fs::create_dir_all(&locks_dir).expect("mkdir locks");
    fs::write(
        locks_dir.join("stale_sess.lock"),
        r#"{"pid":4294967294,"started_ms":1,"token":1}"#,
    )
    .expect("write stale lock");

    let before = snapshot_dir(&data_dir);
    let cfg = GcConfig {
        retain_days: 90,
        retain_newest_per_cwd: 1,
        retain_newest_global: 1,
        vacuum_reclaim_bytes: 1,
    };

    let ro_store = SqliteStore::open_read_only(&db_path).expect("open ro");
    let report =
        run_gc(&ro_store, &data_dir, Some(&db_path), &cfg, NOW_MS, false).expect("dry-run gc");
    drop(ro_store);

    assert!(!report.applied);
    assert_eq!(report.eligible_sessions, vec!["old_1"]);
    assert_eq!(report.orphan_artifacts, 1);
    assert_eq!(report.stale_locks, 1);
    assert!(!report.vacuumed);
    assert!(report.total_bytes() > 0);

    let after = snapshot_dir(&data_dir);
    assert_eq!(before, after, "dry-run must perform zero disk or DB writes");
}

#[test]
fn gc_retains_newest_per_cwd() {
    let tmp = TempDir::new("per-cwd");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("mkdir");
    let store = SqliteStore::open_in_memory().expect("store");

    // 4 expired sessions in /repo/alpha, 3 expired sessions in /repo/beta.
    for (id, age) in [("a1", 200), ("a2", 180), ("a3", 150), ("a4", 120)] {
        seed_session(&store, id, "/repo/alpha", age);
    }
    for (id, age) in [("b1", 210), ("b2", 160), ("b3", 130)] {
        seed_session(&store, id, "/repo/beta", age);
    }

    let cfg = GcConfig {
        retain_days: 90,
        retain_newest_per_cwd: 2,
        retain_newest_global: 1,
        vacuum_reclaim_bytes: 1,
    };
    let report = run_gc(&store, &data_dir, None, &cfg, NOW_MS, true).expect("apply gc");
    assert!(report.applied);
    assert!(report.vacuumed);

    let remaining: Vec<String> = store
        .list_sessions(usize::MAX)
        .expect("list")
        .into_iter()
        .map(|s| s.id)
        .collect();
    // Newest 2 per cwd kept: a4, a3 (alpha) and b3, b2 (beta).
    for kept in ["a4", "a3", "b3", "b2"] {
        assert!(
            remaining.contains(&kept.to_owned()),
            "expected `{kept}` retained in {remaining:?}"
        );
    }
    for dropped in ["a1", "a2", "b1"] {
        assert!(
            !remaining.contains(&dropped.to_owned()),
            "expected `{dropped}` deleted from {remaining:?}"
        );
    }
}

#[test]
fn gc_keeps_pinned() {
    let tmp = TempDir::new("pinned");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("mkdir");
    let store = SqliteStore::open_in_memory().expect("store");

    seed_session(&store, "pinned_old", "/repo/x", 365);
    seed_session(&store, "locked_old", "/repo/x", 365);
    seed_session(&store, "unpinned_old", "/repo/x", 365);
    store.set_pinned("pinned_old", true).expect("pin");

    // Hold a real live write lease on `locked_old` so GC skips it too.
    let _lease = acquire_lease(&data_dir.join("session-locks"), "locked_old").expect("lease");

    let cfg = GcConfig {
        retain_days: 1,
        retain_newest_per_cwd: 0,
        retain_newest_global: 0,
        vacuum_reclaim_bytes: 10_000_000,
    };
    let report = run_gc(&store, &data_dir, None, &cfg, NOW_MS, true).expect("apply gc");
    assert_eq!(report.eligible_sessions, vec!["unpinned_old"]);
    assert_eq!(report.active_locks_skipped, 1);
    assert!(store.get_session("pinned_old").expect("get").is_some());
    assert!(store.get_session("locked_old").expect("get").is_some());
    assert!(store.get_session("unpinned_old").expect("get").is_none());
}

#[test]
fn gc_removes_orphan_artifacts() {
    let tmp = TempDir::new("orphans");
    let data_dir = tmp.path().join("data");
    let store = SqliteStore::open_in_memory().expect("store");
    seed_session(&store, "live_sess", "/repo/live", 1);

    let artifacts = ArtifactStore::new(data_dir.clone());
    artifacts
        .write_output("live_sess", "worker", "# kept", None)
        .expect("write live");
    artifacts
        .write_output("dead_sess_a", "worker", "# orphan a", None)
        .expect("write dead a");
    artifacts
        .write_output("dead_sess_b", "worker", "# orphan b", None)
        .expect("write dead b");

    let locks_dir = data_dir.join("session-locks");
    fs::create_dir_all(&locks_dir).expect("mkdir locks");
    let stale_lock = locks_dir.join("dead_sess_a.lock");
    fs::write(
        &stale_lock,
        r#"{"pid":4294967294,"started_ms":1,"token":9}"#,
    )
    .expect("write lock");

    let cfg = GcConfig::default();
    let report = run_gc(&store, &data_dir, None, &cfg, NOW_MS, true).expect("apply gc");
    assert_eq!(report.orphan_artifacts, 2);
    assert_eq!(report.stale_locks, 1);

    assert!(artifacts.artifacts_dir("live_sess").is_dir());
    assert!(!data_dir.join("sessions").join("dead_sess_a").exists());
    assert!(!data_dir.join("sessions").join("dead_sess_b").exists());
    assert!(!stale_lock.exists());
}

fn seed_child_session(store: &SqliteStore, id: &str, parent_id: &str, cwd: &str, age_days: i64) {
    let ts = NOW_MS - age_days * DAY_MS;
    let meta = SessionMeta {
        id: id.to_owned(),
        title: format!("Child {id}"),
        cwd: cwd.to_owned(),
        branch: None,
        head_node_id: None,
        model: None,
        thinking: ThinkingLevel::Off,
        goal: None,
        permission_mode: PermissionMode::Default,
        parent_session_id: Some(parent_id.to_owned()),
        parent_node_id: None,
        status: AgentStatus::Idle,
        pinned: false,
        title_source: None,
        exit_ms: None,
        created_at_ms: ts,
        updated_at_ms: ts,
    };
    store.create_session(&meta).expect("create child");
}

#[test]
fn gc_config_fails_closed_on_malformed_global_or_missing_overlay() {
    let tmp = TempDir::new("gc-cfg-fail-closed");
    let cwd = tmp.path().join("repo");
    fs::create_dir_all(&cwd).expect("mkdir cwd");
    let missing_global = tmp.path().join("does-not-exist.toml");

    // Missing global config + no overlays falls back cleanly to embedded defaults.
    let def = load_gc_config(
        Some(missing_global.clone()),
        &[],
        &cwd,
        None,
        BTreeMap::new(),
    )
    .expect("missing global uses defaults");
    assert_eq!(def, GcConfig::default());

    // Global config with ONLY `[gc]` (no `[roles]` / `[[providers]]`) must succeed.
    let global_cfg = tmp.path().join("global.toml");
    fs::write(&global_cfg, "[gc]\nretain_days = 3650\n").expect("write global");
    let loaded = load_gc_config(Some(global_cfg.clone()), &[], &cwd, None, BTreeMap::new())
        .expect("minimal global [gc]");
    assert_eq!(loaded.retain_days, 3650);
    assert_eq!(loaded.retain_newest_global, 20);

    // Malformed global config MUST return Err instead of failing open to defaults.
    let bad_global = tmp.path().join("bad-global.toml");
    fs::write(&bad_global, "[gc]\nretain_days = \"not-a-number\n").expect("write bad global");
    assert!(
        load_gc_config(Some(bad_global), &[], &cwd, None, BTreeMap::new()).is_err(),
        "malformed global config must fail closed"
    );

    // Unknown field inside `[gc]` MUST return Err.
    let bad_key = tmp.path().join("bad-key.toml");
    fs::write(&bad_key, "[gc]\nunknown_gc_key = 42\n").expect("write bad key");
    assert!(
        load_gc_config(Some(bad_key), &[], &cwd, None, BTreeMap::new()).is_err(),
        "unknown [gc] field must fail closed"
    );

    // Missing explicit `--config` overlay MUST return Err.
    let missing_overlay = tmp.path().join("missing-overlay.toml");
    assert!(
        load_gc_config(
            Some(global_cfg.clone()),
            &[missing_overlay],
            &cwd,
            None,
            BTreeMap::new(),
        )
        .is_err(),
        "missing --config overlay must fail closed"
    );

    // Valid `--config` overlay overrides global `[gc]`.
    let overlay = tmp.path().join("overlay.toml");
    fs::write(&overlay, "[gc]\nretain_days = 180\n").expect("write overlay");
    let overridden = load_gc_config(Some(global_cfg), &[overlay], &cwd, None, BTreeMap::new())
        .expect("overlay overrides global");
    assert_eq!(overridden.retain_days, 180);
}

#[test]
fn gc_config_ignores_malformed_project_config() {
    let tmp = TempDir::new("gc-cfg-ignore-proj");
    let cwd = tmp.path().join("untrusted-repo");
    let proj_dir = cwd.join(".harness");
    fs::create_dir_all(&proj_dir).expect("mkdir proj");

    let global_cfg = tmp.path().join("global.toml");
    fs::write(&global_cfg, "[gc]\nretain_days = 3650\n").expect("write global");

    // Broken project `.harness/config.toml` must NOT break `hrn gc` or reset defaults.
    fs::write(proj_dir.join("config.toml"), "[[[broken toml").expect("write broken proj");
    let loaded = load_gc_config(Some(global_cfg.clone()), &[], &cwd, None, BTreeMap::new())
        .expect("broken project config ignored");
    assert_eq!(loaded.retain_days, 3650);

    // Malicious project `.harness/config.toml` setting `retain_days = 0` must also be ignored.
    fs::write(proj_dir.join("config.toml"), "[gc]\nretain_days = 0\n").expect("write hostile proj");
    let loaded2 = load_gc_config(Some(global_cfg), &[], &cwd, None, BTreeMap::new())
        .expect("hostile project config ignored");
    assert_eq!(loaded2.retain_days, 3650);
}

#[test]
fn gc_dry_run_with_wal_without_shm_writes_nothing() {
    let tmp = TempDir::new("wal-no-shm");
    let data_dir = tmp.path().join("data");
    let db_path = data_dir.join("custom-state.db");
    assert_eq!(wal_path_for(&db_path), data_dir.join("custom-state.db-wal"));

    {
        let store = SqliteStore::open(&db_path).expect("open");
        seed_session(&store, "old_a", "/work/w", 200);
        seed_session(&store, "old_b", "/work/w", 180);
        store.vacuum_and_checkpoint().expect("checkpoint");
    }

    // Create an orphaned `-wal` file with NO `-shm` file alongside the custom DB.
    let wal_path = wal_path_for(&db_path);
    let shm_path = data_dir.join("custom-state.db-shm");
    let _ = fs::remove_file(&shm_path);
    fs::write(&wal_path, vec![0u8; 4096]).expect("write orphan wal");
    assert!(!shm_path.exists());

    let before = snapshot_dir(&data_dir);
    let cfg = GcConfig {
        retain_days: 90,
        retain_newest_per_cwd: 1,
        retain_newest_global: 1,
        vacuum_reclaim_bytes: 1,
    };

    let readonly_store = SqliteStore::open_read_only(&db_path).expect("open ro with wal");
    let report = run_gc(
        &readonly_store,
        &data_dir,
        Some(&db_path),
        &cfg,
        NOW_MS,
        false,
    )
    .expect("dry-run gc");
    drop(readonly_store);

    assert_eq!(report.eligible_sessions, vec!["old_a"]);
    assert!(!shm_path.exists(), "dry-run must NEVER create -shm");
    assert_eq!(
        before,
        snapshot_dir(&data_dir),
        "dry-run with -wal present must perform zero disk writes"
    );

    // Apply mode uses `wal_path_for(db_path)` (custom DB name) to count WAL bytes for vacuum.
    let _ = fs::remove_file(&wal_path);
    let writable_store = SqliteStore::open(&db_path).expect("open rw");
    writable_store
        .append_node(&DagNode {
            id: "n_big_wal".to_owned(),
            session_id: "old_b".to_owned(),
            parent_id: None,
            created_at_ms: NOW_MS,
            payload: DagPayload::Message {
                turn_id: "t_wal".to_owned(),
                msg: Msg::user("w".repeat(8192)),
            },
        })
        .expect("append big node to grow WAL");
    let keep_all_cfg = GcConfig {
        retain_days: 3650,
        retain_newest_per_cwd: 20,
        retain_newest_global: 20,
        vacuum_reclaim_bytes: 1024,
    };
    let applied = run_gc(
        &writable_store,
        &data_dir,
        Some(&db_path),
        &keep_all_cfg,
        NOW_MS,
        true,
    )
    .expect("apply gc vacuum on custom db wal");
    assert!(applied.eligible_sessions.is_empty());
    assert!(
        applied.vacuumed,
        "custom DB WAL bytes must trigger wal_checkpoint + VACUUM"
    );
}

#[test]
fn gc_protects_root_with_pinned_or_locked_child() {
    let tmp = TempDir::new("child-protect");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("mkdir");
    let store = SqliteStore::open_in_memory().expect("store");

    seed_session(&store, "root_pin_child", "/repo/c", 365);
    seed_child_session(&store, "child_pinned", "root_pin_child", "/repo/c", 365);
    store.set_pinned("child_pinned", true).expect("pin child");

    seed_session(&store, "root_lock_child", "/repo/c", 365);
    seed_child_session(&store, "child_locked", "root_lock_child", "/repo/c", 365);
    let _child_lease =
        acquire_lease(&data_dir.join("session-locks"), "child_locked").expect("child lease");

    seed_session(&store, "root_deletable", "/repo/c", 365);
    seed_child_session(&store, "child_expired", "root_deletable", "/repo/c", 365);

    let artifacts = ArtifactStore::new(data_dir.clone());
    artifacts
        .write_output("child_pinned", "worker", "# keep child artifact", None)
        .expect("write child artifact");

    let cfg = GcConfig {
        retain_days: 1,
        retain_newest_per_cwd: 0,
        retain_newest_global: 0,
        vacuum_reclaim_bytes: 10_000_000,
    };
    let report = run_gc(&store, &data_dir, None, &cfg, NOW_MS, true).expect("apply gc");
    assert_eq!(report.eligible_sessions, vec!["root_deletable"]);
    assert!(store.get_session("root_pin_child").expect("get").is_some());
    assert!(store.get_session("child_pinned").expect("get").is_some());
    assert!(store.get_session("root_lock_child").expect("get").is_some());
    assert!(store.get_session("child_locked").expect("get").is_some());
    assert!(store.get_session("root_deletable").expect("get").is_none());
    assert!(artifacts.artifacts_dir("child_pinned").is_dir());
}
