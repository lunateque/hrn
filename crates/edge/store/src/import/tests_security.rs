//! Security regression tests for transcript import sanitization, UTF-8 read-only `SQLite` URIs,
//! and strict `.jsonl` filtering on Codex `threads` metadata.
//! Uses synthetic temp-directory fixtures ONLY.

use super::*;
use crate::SqliteStore;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static SEC_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

struct SecTempDir(PathBuf);

impl SecTempDir {
    fn new(tag: &str) -> Self {
        let n = SEC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("hrn-imp-sec-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SecTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_jsonl(path: &Path, lines: &[serde_json::Value]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    let body = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{body}\n")).expect("write jsonl");
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<PathBuf, (u64, SystemTime)>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("entry");
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
    let mut map = BTreeMap::new();
    walk(root, root, &mut map);
    map
}

#[test]
fn import_sanitizes_control_chars_and_escapes_in_titles_and_cwd() {
    let tmp = SecTempDir::new("sanitize");
    let claude_dir = tmp.path().join("claude");
    let slug_dir = claude_dir.join("projects").join("demo");
    let long_tail = "x".repeat(260);
    let evil_title =
        format!("\x1b]0;pwn\x07\x1b[31mRed\x1b[0m\u{202E}Title\x00\nLine2 {long_tail}");
    let evil_cwd = "/work/\x1b]52;c;Y21k\x1b\\repo\x07\r\n";
    let evil_branch = "main\x1b[1m\x00\n";

    write_jsonl(
        &slug_dir.join("sess-evil.jsonl"),
        &[
            json!({
                "type": "user",
                "uuid": "u1",
                "parentUuid": null,
                "cwd": evil_cwd,
                "gitBranch": evil_branch,
                "timestamp": "2026-04-03T10:00:00Z",
                "message": {"role": "user", "content": "Hello"}
            }),
            json!({
                "type": "custom-title",
                "customTitle": evil_title
            }),
        ],
    );

    let discovered = discover_claude(&claude_dir).expect("discover claude");
    assert_eq!(discovered.len(), 1);
    let item = &discovered[0];
    assert!(
        !item.title.chars().any(char::is_control),
        "discovered title kept control chars: {:?}",
        item.title
    );
    assert!(
        !item.title.contains("pwn") && !item.title.contains('\u{202E}'),
        "discovered title kept OSC/bidi payload: {:?}",
        item.title
    );
    assert!(
        item.title.chars().count() <= 120,
        "discovered title exceeded cap: {}",
        item.title.chars().count()
    );
    assert_eq!(item.cwd, "/work/repo");

    let store = SqliteStore::open_in_memory().expect("store");
    let outcome = import_claude(&store, &claude_dir, Some("sess-evil")).expect("import claude");
    assert!(!outcome.session.title.chars().any(char::is_control));
    assert!(outcome.session.title.starts_with("RedTitle Line2 "));
    assert!(outcome.session.title.chars().count() <= 120);
    assert_eq!(outcome.session.cwd, "/work/repo");
    assert_eq!(outcome.session.branch.as_deref(), Some("main"));

    let display = sanitize_display_text("\x1b[32mok\x1b[0m\tcol\x07\nnext");
    assert_eq!(display, "ok col next");
}

#[test]
fn codex_readonly_uri_and_sqlite_unicode_path_and_jsonl_filter() {
    let tmp = SecTempDir::new("unicode-jsonl");
    // Path containing Cyrillic UTF-8 bytes ("\u{0442}\u{0435}\u{0441}\u{0442}") + space + '#' + '%'.
    let unicode_dir_name = "codex \u{0442}\u{0435}\u{0441}\u{0442} #1 %20";
    let codex_dir = tmp.path().join(unicode_dir_name);
    let ok_rollout = codex_dir.join("sessions").join("rollout-ok.jsonl");
    let upper_rollout = codex_dir.join("sessions").join("rollout-upper.JSONL");
    let non_jsonl = codex_dir.join("sessions").join("notes.txt");
    let auth_file = codex_dir.join("auth.json");

    write_jsonl(
        &ok_rollout,
        &[
            json!({
                "type": "session_meta",
                "timestamp": "2026-04-03T12:00:00Z",
                "payload": {"id": "thr-ok", "cwd": "/work/ok"}
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-04-03T12:00:01Z",
                "payload": {"type": "user_message", "message": "Check unicode path"}
            }),
        ],
    );
    write_jsonl(
        &upper_rollout,
        &[json!({
            "type": "session_meta",
            "timestamp": "2026-04-03T12:00:02Z",
            "payload": {"id": "thr-upper", "cwd": "/work/upper"}
        })],
    );
    fs::write(&non_jsonl, "not a transcript\n").expect("write txt");
    fs::write(&auth_file, r#"{"token":"secret"}"#).expect("write auth");

    let db_path = codex_dir.join("state_5.sqlite");
    let uri = codex::readonly_immutable_uri(&db_path);
    assert!(
        uri.contains("%D1%82%D0%B5%D1%81%D1%82"),
        "expected UTF-8 percent-encoded bytes in URI: {uri}"
    );
    assert!(uri.contains("%23") && uri.contains("%25") && uri.contains("%20"));

    {
        let conn = Connection::open(&db_path).expect("create state_5");
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT,
                title TEXT,
                cwd TEXT,
                updated_at INTEGER,
                archived INTEGER DEFAULT 0
            );",
        )
        .expect("schema");
        for (id, rel, title) in [
            (
                "thr-ok",
                "sessions/rollout-ok.jsonl",
                "\x1b[31mValid\x1b[0m Thread",
            ),
            ("thr-upper", "sessions/rollout-upper.JSONL", "Upper JSONL"),
            ("thr-txt", "sessions/notes.txt", "Should Ignore TXT"),
            ("thr-db", "state_5.sqlite", "Should Ignore SQLite"),
            ("thr-auth", "auth.json", "Should Ignore Auth"),
        ] {
            conn.execute(
                "INSERT INTO threads (id, rollout_path, title, cwd, updated_at, archived)
                 VALUES (?1, ?2, ?3, '/work/u\x07', 1775383202, 0)",
                rusqlite::params![id, rel, title],
            )
            .expect("insert thread");
        }
    }

    let before = snapshot_tree(&codex_dir);
    let found = discover_codex(&codex_dir).expect("discover codex in unicode dir");
    let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids.len(), 2, "only .jsonl/.JSONL rows allowed: {ids:?}");
    assert!(ids.contains(&"thr-ok"));
    assert!(ids.contains(&"thr-upper"));
    let ok_row = found.iter().find(|s| s.id == "thr-ok").expect("thr-ok row");
    assert_eq!(ok_row.title, "Valid Thread");
    assert_eq!(ok_row.cwd, "/work/u");

    let store = SqliteStore::open_in_memory().expect("store");
    let imported = import_codex(&store, &codex_dir, Some("thr-ok")).expect("import codex");
    assert_eq!(imported.session.title, "Valid Thread");
    assert_eq!(imported.session.cwd, "/work/u");

    let after = snapshot_tree(&codex_dir);
    assert_eq!(
        before, after,
        "unicode state_5.sqlite and rollout files must remain untouched"
    );
}
