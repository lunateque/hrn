//! Store round-trip, branch ordering and schema-guard tests.

use super::*;
use hrn_kernel::Msg;

fn session(id: &str, parent: Option<&str>, updated: i64) -> SessionMeta {
    SessionMeta {
        pinned: false,
        title_source: None,
        exit_ms: None,
        id: id.into(),
        title: "test".into(),
        cwd: ".".into(),
        branch: None,
        head_node_id: None,
        model: None,
        thinking: ThinkingLevel::Medium,
        goal: None,
        permission_mode: PermissionMode::Default,
        parent_session_id: parent.map(Into::into),
        parent_node_id: None,
        status: AgentStatus::Idle,
        created_at_ms: 1,
        updated_at_ms: updated,
    }
}

fn node(id: &str, session: &str, parent: Option<&str>, at: i64, text: &str) -> DagNode {
    DagNode {
        id: id.into(),
        session_id: session.into(),
        parent_id: parent.map(Into::into),
        created_at_ms: at,
        payload: DagPayload::Message {
            turn_id: "t".into(),
            msg: Msg::assistant(text),
        },
    }
}

#[test]
fn dag_branch_and_fork_preserve_both_paths() {
    let store = SqliteStore::open_in_memory().unwrap_or_else(|e| unreachable!("{e}"));
    assert!(store.create_session(&session("s1", None, 1)).is_ok());

    let n1 = DagNode {
        id: "n1".into(),
        session_id: "s1".into(),
        parent_id: None,
        created_at_ms: 10,
        payload: DagPayload::Message {
            turn_id: "t1".into(),
            msg: Msg::user("root"),
        },
    };
    let n2 = node("n2", "s1", Some("n1"), 20, "branch A");
    let n3 = node("n3", "s1", Some("n1"), 30, "branch B");
    assert!(store.append_node(&n1).is_ok());
    assert!(store.append_node(&n2).is_ok());
    assert!(store.append_node(&n3).is_ok());

    let branch_a = store.load_branch("s1", "n2").unwrap_or_default();
    let branch_b = store.load_branch("s1", "n3").unwrap_or_default();
    assert_eq!(branch_a.len(), 2);
    assert_eq!(branch_b.len(), 2);
    assert_eq!(branch_a.get(1).map(|n| n.id.as_str()), Some("n2"));
    assert_eq!(branch_b.get(1).map(|n| n.id.as_str()), Some("n3"));
    assert_eq!(store.load_tree("s1").unwrap_or_default().len(), 3);
}

#[test]
fn append_advances_head_and_rejects_foreign_or_missing_parent() {
    let store = SqliteStore::open_in_memory().expect("store");
    store.create_session(&session("s1", None, 1)).expect("s1");
    store.create_session(&session("s2", None, 1)).expect("s2");
    store
        .append_node(&node("a", "s1", None, 5, "a"))
        .expect("a");
    assert_eq!(
        store
            .get_session("s1")
            .expect("get")
            .and_then(|s| s.head_node_id),
        Some("a".to_owned())
    );
    // Parent that does not exist at all, and parent that belongs to another session.
    assert!(
        store
            .append_node(&node("b", "s1", Some("nope"), 6, "b"))
            .is_err()
    );
    assert!(
        store
            .append_node(&node("c", "s2", Some("a"), 6, "c"))
            .is_err()
    );
    // A rejected append must leave no orphan row and keep the head.
    assert_eq!(store.load_tree("s1").expect("tree").len(), 1);
    assert_eq!(store.load_tree("s2").expect("tree").len(), 0);
    assert_eq!(
        store
            .get_session("s2")
            .expect("get")
            .and_then(|s| s.head_node_id),
        None
    );
    // Unknown session is an error, not a silent no-op.
    assert!(
        store
            .append_node(&node("d", "ghost", None, 7, "d"))
            .is_err()
    );
}

#[test]
fn long_chain_is_loaded_completely_in_order() {
    let store = SqliteStore::open_in_memory().expect("store");
    store.create_session(&session("s", None, 1)).expect("s");
    let total = 10_050usize;
    let mut parent: Option<String> = None;
    for i in 0..total {
        let id = format!("n{i}");
        let at = i64::try_from(i).expect("fits");
        store
            .append_node(&node(&id, "s", parent.as_deref(), at, "x"))
            .expect("append");
        parent = Some(id);
    }
    let branch = store
        .load_branch("s", &format!("n{}", total - 1))
        .expect("branch");
    assert_eq!(branch.len(), total, "no silent depth cap");
    assert_eq!(branch.first().map(|n| n.id.as_str()), Some("n0"));
    assert_eq!(branch.last().map(|n| n.id.as_str()), Some("n10049"));
}

#[test]
fn missing_head_and_unknown_session_are_errors() {
    let store = SqliteStore::open_in_memory().expect("store");
    store.create_session(&session("s", None, 1)).expect("s");
    assert!(store.load_branch("s", "missing").is_err());
    assert!(store.update_session(&session("ghost", None, 2)).is_err());
}

#[test]
fn list_sessions_hides_subagents_orders_by_recency_and_honours_huge_limits() {
    let store = SqliteStore::open_in_memory().expect("store");
    store
        .create_session(&session("old", None, 10))
        .expect("old");
    store
        .create_session(&session("new", None, 20))
        .expect("new");
    store
        .create_session(&session("child", Some("new"), 30))
        .expect("child");
    let ids: Vec<String> = store
        .list_sessions(usize::MAX)
        .expect("list")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, ["new", "old"]);
}

#[test]
fn corrupt_enum_column_is_reported_not_defaulted() {
    let store = SqliteStore::open_in_memory().expect("store");
    store.create_session(&session("s", None, 1)).expect("s");
    store
        .lock()
        .execute(
            "UPDATE sessions SET permission_mode = 'bogus' WHERE id = 's'",
            [],
        )
        .expect("corrupt");
    assert!(store.get_session("s").is_err());
}

#[test]
fn file_database_uses_wal_and_reopens() {
    let dir = std::env::temp_dir().join(format!("hrn-store-test-{}", std::process::id()));
    let path = dir.join("sessions.db");
    {
        let store = SqliteStore::open(&path).expect("open");
        store.create_session(&session("s", None, 1)).expect("s");
        let mode: String = store
            .lock()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("mode");
        assert_eq!(mode, "wal");
    }
    let again = SqliteStore::open(&path).expect("reopen");
    assert!(again.get_session("s").expect("get").is_some());
    drop(again);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn usage_is_recorded_summed_per_session_and_reported_by_day_and_model() {
    let store = SqliteStore::open_in_memory().expect("store");
    store.create_session(&session("s1", None, 1)).expect("s1");
    store.create_session(&session("s2", None, 1)).expect("s2");
    store
        .create_session(&session("sub", Some("s1"), 1))
        .expect("sub");
    let call = |prompt, cost| Usage {
        prompt_tokens: prompt,
        completion_tokens: 10,
        total_tokens: prompt + 10,
        cache_read_tokens: Some(1),
        cost_micros: cost,
        ..Usage::default()
    };
    let day1 = 1_700_000_000_000; // 2023-11-14 UTC
    let day2 = day1 + 86_400_000;
    store
        .record_usage("s1", day1, "flash", &call(100, Some(5)))
        .expect("1");
    store
        .record_usage("s1", day1, "flash", &call(200, Some(7)))
        .expect("2");
    store
        .record_usage("sub", day1, "flash", &call(50, Some(1)))
        .expect("3");
    store
        .record_usage("s2", day2, "claude", &call(300, None))
        .expect("4");

    let s1 = store.session_usage("s1").expect("sum");
    assert_eq!(
        (s1.prompt_tokens, s1.completion_tokens, s1.cost_micros),
        (300, 20, Some(12))
    );
    assert_eq!(s1.cache_read_tokens, Some(2));
    assert_eq!(
        store.latest_prompt_tokens("s1").expect("latest s1"),
        Some(200)
    );
    assert_eq!(
        store.latest_prompt_tokens("sub").expect("latest sub"),
        Some(50)
    );
    assert_eq!(
        store.latest_prompt_tokens("nobody").expect("latest nobody"),
        None
    );
    assert_eq!(
        store.session_usage("nobody").expect("empty"),
        Usage::default()
    );

    let rows = store.usage_report().expect("report");
    let [newest, older] = rows.as_slice() else {
        unreachable!("two day/model rows, got {rows:?}");
    };
    assert_eq!(
        (newest.day.as_str(), newest.model.as_str()),
        ("2023-11-15", "claude")
    );
    assert_eq!(
        (newest.sessions, newest.calls, newest.usage.cost_micros),
        (1, 1, None)
    );
    assert_eq!(
        (older.day.as_str(), older.model.as_str()),
        ("2023-11-14", "flash")
    );
    // The subagent's calls add up, but only the top-level session is counted.
    assert_eq!(
        (older.sessions, older.calls, older.usage.prompt_tokens),
        (1, 3, 350)
    );
    assert_eq!(older.usage.cost_micros, Some(13));
}

#[test]
fn deleting_a_session_removes_its_usage() {
    let store = SqliteStore::open_in_memory().expect("store");
    store.create_session(&session("s", None, 1)).expect("s");
    store
        .record_usage(
            "s",
            1,
            "flash",
            &Usage {
                prompt_tokens: 1,
                ..Usage::default()
            },
        )
        .expect("usage");
    store
        .lock()
        .execute("DELETE FROM sessions WHERE id = 's'", [])
        .expect("delete");
    assert!(store.usage_report().expect("report").is_empty());
}

#[test]
fn pinned_sessions_lead_the_list_and_deleting_takes_children_nodes_and_pins_along() {
    let store = SqliteStore::open_in_memory().unwrap_or_else(|e| unreachable!("{e}"));
    for (id, updated) in [("old", 1), ("mid", 5), ("new", 9)] {
        store
            .create_session(&session(id, None, updated))
            .unwrap_or_else(|e| unreachable!("{e}"));
    }
    store
        .create_session(&session("child", Some("old"), 2))
        .unwrap_or_else(|e| unreachable!("{e}"));
    store
        .append_node(&node("n1", "old", None, 3, "hello"))
        .unwrap_or_else(|e| unreachable!("{e}"));
    store
        .append_node(&node("c1", "child", None, 4, "sub"))
        .unwrap_or_else(|e| unreachable!("{e}"));

    store
        .set_pinned("old", true)
        .unwrap_or_else(|e| unreachable!("{e}"));
    let ids = |store: &SqliteStore| -> Vec<String> {
        store
            .list_sessions(10)
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.id)
            .collect()
    };
    assert_eq!(ids(&store), ["old", "new", "mid"]);
    assert_eq!(store.pinned_sessions().unwrap_or_default(), ["old"]);
    assert!(store.set_pinned("nope", true).is_err());

    store
        .rename_session("mid", "renamed", TitleSource::User)
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(
        store.get_session("mid").ok().flatten().map(|m| m.title),
        Some("renamed".to_owned())
    );

    assert_eq!(store.delete_session("old").ok(), Some(true));
    assert_eq!(store.delete_session("old").ok(), Some(false));
    assert!(store.get_session("child").ok().flatten().is_none());
    assert!(store.load_tree("old").unwrap_or_default().is_empty());
    assert!(store.load_tree("child").unwrap_or_default().is_empty());
    assert!(store.pinned_sessions().unwrap_or_default().is_empty());
    assert_eq!(ids(&store), ["new", "mid"]);
}

#[test]
fn goal_and_pin_survive_a_round_trip_through_get_and_list() {
    let store = SqliteStore::open_in_memory().unwrap_or_else(|e| unreachable!("{e}"));
    let goal = hrn_kernel::Goal {
        id: "g1".into(),
        objective: "ship it".into(),
        status: hrn_kernel::GoalStatus::Active,
        token_budget: Some(7000),
        time_budget_secs: None,
        tokens_used: 12,
        time_used_seconds: 3,
        created_at_ms: 1,
        updated_at_ms: 2,
    };
    let mut meta = session("s", None, 1);
    meta.goal = Some(goal.clone());
    store
        .create_session(&meta)
        .unwrap_or_else(|e| unreachable!("{e}"));
    store
        .set_pinned("s", true)
        .unwrap_or_else(|e| unreachable!("{e}"));

    let got = store
        .get_session("s")
        .unwrap_or_else(|e| unreachable!("{e}"))
        .unwrap_or_else(|| unreachable!("session missing"));
    assert_eq!(got.goal, Some(goal.clone()));
    assert!(got.pinned);
    let listed = store
        .list_sessions(10)
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].goal, Some(goal));
    assert!(listed[0].pinned);
}

#[test]
fn store_migrates_v3_to_v4() {
    let dir = std::env::temp_dir().join(format!("hrn-store-v3-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("sessions.db");
    {
        let conn = rusqlite::Connection::open(&path).expect("open raw");
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE sessions (
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 cwd TEXT NOT NULL,
                 branch TEXT,
                 head_node_id TEXT,
                 model TEXT,
                 thinking TEXT NOT NULL DEFAULT 'medium',
                 permission_mode TEXT NOT NULL DEFAULT 'default',
                 parent_session_id TEXT,
                 parent_node_id TEXT,
                 status TEXT NOT NULL DEFAULT 'idle',
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 goal TEXT
             );
             CREATE TABLE dag_nodes (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 parent_id TEXT,
                 created_at_ms INTEGER NOT NULL,
                 payload_json TEXT NOT NULL
             );
             CREATE TABLE usage (
                 id INTEGER PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 at_ms INTEGER NOT NULL,
                 model TEXT NOT NULL,
                 prompt_tokens INTEGER NOT NULL,
                 completion_tokens INTEGER NOT NULL,
                 total_tokens INTEGER NOT NULL,
                 cache_read_tokens INTEGER,
                 cache_write_tokens INTEGER,
                 reasoning_tokens INTEGER,
                 cost_micros INTEGER
             );
             CREATE TABLE session_pins (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                 pinned_at_ms INTEGER NOT NULL
             );
             INSERT INTO sessions (id, title, cwd, created_at_ms, updated_at_ms)
             VALUES ('legacy', 'old title', '/repo', 100, 200);
             PRAGMA user_version = 3;",
        )
        .expect("seed v3");
    }

    let store = SqliteStore::open(&path).expect("migrate v3 -> v4");
    let version: i64 = store
        .lock()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version");
    assert_eq!(version, 4);

    let legacy = store
        .get_session("legacy")
        .expect("get legacy")
        .expect("exists");
    assert_eq!(legacy.title, "old title");
    assert_eq!(legacy.title_source, None);
    assert_eq!(legacy.exit_ms, None);

    // Auto title works when unset, user title wins over auto, and subsequent auto is ignored.
    store
        .rename_session("legacy", "auto title", TitleSource::Auto)
        .expect("auto rename");
    let after_auto = store.get_session("legacy").expect("get").expect("exists");
    assert_eq!(after_auto.title, "auto title");
    assert_eq!(after_auto.title_source, Some(TitleSource::Auto));

    store
        .rename_session("legacy", "user title", TitleSource::User)
        .expect("user rename");
    store
        .rename_session("legacy", "should not overwrite", TitleSource::Auto)
        .expect("auto ignored");
    let after_user = store.get_session("legacy").expect("get").expect("exists");
    assert_eq!(after_user.title, "user title");
    assert_eq!(after_user.title_source, Some(TitleSource::User));

    // Reopening a migrated v4 database is idempotent.
    store.vacuum_and_checkpoint().expect("vacuum");
    drop(store);
    let reopened = SqliteStore::open(&path).expect("reopen v4");
    assert_eq!(
        reopened
            .get_session("legacy")
            .expect("get")
            .map(|s| s.title),
        Some("user title".to_owned())
    );
    drop(reopened);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn v4_tables_roundtrip_and_cascade_on_session_delete() {
    let store = SqliteStore::open_in_memory().expect("store");
    let mut s1 = session("s1", None, 10);
    s1.cwd = "/a".into();
    s1.exit_ms = Some(999);
    let mut s2 = session("s2", None, 20);
    s2.cwd = "/a".into();
    let mut s3 = session("s3", None, 30);
    s3.cwd = "/b".into();
    let child = session("c1", Some("s1"), 15);
    for meta in [&s1, &s2, &s3, &child] {
        store.create_session(meta).expect("create");
    }

    assert_eq!(
        store
            .get_session("s1")
            .expect("get")
            .and_then(|s| s.exit_ms),
        Some(999)
    );
    let recent_a: Vec<String> = store
        .list_recent_for("/a", 10)
        .expect("recent")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(recent_a, ["s2", "s1"]);
    let kids: Vec<String> = store
        .list_children("s1")
        .expect("children")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(kids, ["c1"]);

    store
        .put_btw_topic("s1", "t1", r#"{"q":"hi"}"#, 100)
        .expect("btw 1");
    store
        .put_btw_topic("s1", "t2", r#"{"q":"yo"}"#, 200)
        .expect("btw 2");
    store
        .put_btw_topic("s1", "t1", r#"{"q":"updated"}"#, 300)
        .expect("btw update");
    assert_eq!(
        store.get_btw_topics("s1").expect("btw list"),
        vec![
            ("t1".into(), r#"{"q":"updated"}"#.into(), 300),
            ("t2".into(), r#"{"q":"yo"}"#.into(), 200),
        ]
    );

    let grant = ApprovalGrant {
        key: "/a/file.rs".into(),
        tier: hrn_kernel::ToolTier::Write,
        scope: "file".into(),
        created_ms: 42,
    };
    let dir_grant = ApprovalGrant {
        key: "/a/file.rs".into(),
        tier: hrn_kernel::ToolTier::Read,
        scope: "directory".into(),
        created_ms: 43,
    };
    store.put_approval_grant(&grant).expect("grant");
    store.put_approval_grant(&dir_grant).expect("dir grant");
    assert_eq!(
        store.list_approval_grants().expect("grants"),
        vec![grant, dir_grant]
    );
    store.clear_approval_grants().expect("clear grants");
    assert!(store.list_approval_grants().expect("empty").is_empty());

    let trace = TraceRecord {
        session_id: "s1".into(),
        node_id: None,
        kind: "tool".into(),
        started_ms: 10,
        dur_ms: 5,
        payload: serde_json::json!({"tool": "read"}),
    };
    store.record_trace(&trace).expect("trace");
    assert_eq!(store.list_traces("s1").expect("traces"), vec![trace]);

    store.delete_session("s1").expect("delete");
    assert!(store.get_btw_topics("s1").expect("cascaded btw").is_empty());
    assert!(store.list_traces("s1").expect("cascaded traces").is_empty());
}
