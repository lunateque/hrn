use super::*;
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

static N: AtomicU32 = AtomicU32::new(0);

/// A fresh work directory (not a git repository) and checkpoint root under the temp dir.
fn dirs() -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "hrn-ckpt-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let work = base.join("work");
    fs::create_dir_all(&work).expect("work dir");
    (work, base.join("shadow"))
}

fn open(work: &Path, root: &Path, excludes: &[&str]) -> Checkpoints {
    let excludes: Vec<String> = excludes.iter().map(|s| (*s).to_owned()).collect();
    Checkpoints::open(root, work, Some(&excludes), Duration::from_mins(1)).expect("open")
}

#[test]
fn restore_puts_files_back_removes_new_ones_and_is_undoable() {
    let (work, root) = dirs();
    let ckpt = open(&work, &root, &[]);
    fs::write(work.join("a.txt"), "one\r\ntwo").expect("write");
    fs::create_dir_all(work.join("sub")).expect("mkdir");
    fs::write(work.join("sub").join("c.txt"), "keep").expect("write");
    let first = ckpt
        .snapshot(
            Kind::Explicit,
            "first\nstate",
            Some("sess_1"),
            Some("turn_1"),
        )
        .expect("snapshot");

    fs::write(work.join("a.txt"), "changed").expect("write");
    fs::write(work.join("b.txt"), "new").expect("write");
    fs::remove_dir_all(work.join("sub")).expect("rmdir");
    let second = ckpt.snapshot(Kind::Auto, "", None, None).expect("snapshot");

    let restored = ckpt.restore(&first, Some("sess_1"), None).expect("restore");
    assert_eq!(fs::read(work.join("a.txt")).expect("read"), b"one\r\ntwo");
    assert!(!work.join("b.txt").exists());
    assert_eq!(
        fs::read_to_string(work.join("sub").join("c.txt")).expect("read"),
        "keep"
    );
    let mut changes: Vec<String> = restored
        .changes
        .iter()
        .map(|c| format!("{} {}", c.status, c.path))
        .collect();
    changes.sort();
    assert_eq!(changes, ["A sub/c.txt", "D b.txt", "M a.txt"]);

    // The restore itself was snapshotted, so it can be taken back.
    ckpt.restore(&restored.undo, None, None).expect("undo");
    assert_eq!(
        fs::read_to_string(work.join("a.txt")).expect("read"),
        "changed"
    );
    assert!(work.join("b.txt").exists() && !work.join("sub").exists());

    let listed = ckpt.list(Some(10)).expect("list");
    let kinds: Vec<Kind> = listed.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        [Kind::Restore, Kind::Restore, Kind::Auto, Kind::Explicit]
    );
    let oldest = listed.last().expect("oldest");
    assert_eq!(oldest.id, first);
    assert_eq!(
        (
            oldest.label.as_str(),
            oldest.session.as_deref(),
            oldest.turn.as_deref()
        ),
        ("first state", Some("sess_1"), Some("turn_1"))
    );
    assert_eq!(listed.get(2).map(|s| s.id.as_str()), Some(second.as_str()));
}

#[test]
fn ignored_and_excluded_files_survive_a_restore() {
    let (work, root) = dirs();
    let ckpt = open(&work, &root, &["cache/"]);
    fs::write(work.join(".gitignore"), "ignored.log\n").expect("write");
    fs::write(work.join("code.rs"), "v1").expect("write");
    let id = ckpt
        .snapshot(Kind::Auto, "start", None, None)
        .expect("snapshot");

    fs::write(work.join("code.rs"), "v2").expect("write");
    fs::write(work.join("ignored.log"), "log").expect("write");
    fs::create_dir_all(work.join("cache")).expect("mkdir");
    fs::write(work.join("cache").join("blob"), "x").expect("write");
    ckpt.restore(&id, None, None).expect("restore");

    assert_eq!(
        fs::read_to_string(work.join("code.rs")).expect("read"),
        "v1"
    );
    assert!(work.join("ignored.log").exists());
    assert!(work.join("cache").join("blob").exists());
}

#[test]
fn the_active_checkpoint_is_per_session_and_ids_resolve_by_prefix() {
    let (work, root) = dirs();
    let ckpt = open(&work, &root, &[]);
    fs::write(work.join("f"), "1").expect("write");
    let id = ckpt
        .snapshot(Kind::Explicit, "goal", Some("s1"), None)
        .expect("snapshot");

    assert_eq!(ckpt.active("s1").expect("active"), None);
    ckpt.set_active("s1", &id).expect("set");
    assert_eq!(ckpt.active("s1").expect("active"), Some(id.clone()));
    assert_eq!(ckpt.active("s2").expect("active"), None);
    ckpt.clear_active("s1").expect("clear");
    assert_eq!(ckpt.active("s1").expect("active"), None);

    assert_eq!(ckpt.find(&id[..6]).expect("prefix").id, id);
    assert!(ckpt.find("beef").is_err());
    assert!(ckpt.find("--all").is_err());
    assert!(ckpt.active("../x").is_err());
}

#[test]
fn reading_a_directory_without_snapshots_creates_nothing() {
    let (work, root) = dirs();
    let ckpt = Checkpoints::open(&root, &work, None, Duration::from_mins(1)).expect("open");
    assert!(ckpt.list(None).expect("list").is_empty());
    assert!(ckpt.find("abcd").is_err());
    assert!(!root.exists());
}
