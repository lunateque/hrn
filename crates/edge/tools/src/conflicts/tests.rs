use super::write;
use crate::fs_read::{FileReadTracker, read_sync};
use crate::testutil::{TempDir, test_ctx};
use hrn_kernel::HrnError;
use std::fs;

fn read(dir: &TempDir, tracker: &FileReadTracker, raw: &str) -> String {
    read_sync(tracker, &test_ctx(dir.path().to_path_buf()), raw)
        .unwrap()
        .content
}

fn err(dir: &TempDir, tracker: &FileReadTracker, raw: &str) -> String {
    match read_sync(tracker, &test_ctx(dir.path().to_path_buf()), raw) {
        Ok(out) => out.content,
        Err(HrnError::Tool(msg)) => msg,
        Err(other) => panic!("unexpected {other:?}"),
    }
}

fn resolve(
    dir: &TempDir,
    tracker: &FileReadTracker,
    rest: &str,
    content: &str,
) -> Result<String, String> {
    write(tracker, dir.path(), rest, content)
        .map(|out| out.content)
        .map_err(|e| e.to_string())
}

/// Two hunks, the first with a diff3 base section. Ids are per file, so a second conflicted file
/// also starts at `#1`.
const CONFLICTED: &str = "head\n<<<<<<< HEAD\nmine\n||||||| ancestors\nboth\n=======\nyours\n>>>>>>> feature\nmid\n<<<<<<< HEAD\nonly-ours\n=======\nonly-theirs\n>>>>>>> feature\ntail\n";

#[test]
fn index_and_views_use_file_coordinates() {
    let dir = TempDir::new("conflicts-read");
    dir.write("m.rs", CONFLICTED);
    let tracker = FileReadTracker::new();
    let index = read(&dir, &tracker, "m.rs:conflicts");
    assert!(
        index.contains("#1  L2-L8") && index.contains("1 ours, 1 base, 1 theirs"),
        "{index}"
    );
    assert!(
        index.contains("#2  L10-L14") && index.contains("1 ours, 1 theirs"),
        "{index}"
    );

    let whole = read(&dir, &tracker, "conflict://2");
    assert!(
        whole.starts_with("conflict 2 of `m.rs` at file lines 10-14"),
        "{whole}"
    );
    assert!(
        whole.contains("10:<<<<<<< HEAD") && whole.contains("14:>>>>>>> feature"),
        "{whole}"
    );
    assert_eq!(
        read(&dir, &tracker, "conflict://2/theirs"),
        "conflict 2 of `m.rs` at file lines 10-14\n13:only-theirs"
    );
    assert!(read(&dir, &tracker, "conflict://1/base").ends_with("5:both"));
    assert!(read(&dir, &tracker, "conflict://1/ours:1").ends_with("3:mine"));

    let fresh = FileReadTracker::new();
    assert!(
        err(&dir, &fresh, "conflict://3").contains("was never read"),
        "stale id"
    );
    assert!(err(&dir, &fresh, "conflict://*/ours").contains("write-only"));
    // A bare read registers the hunks too and names the ids.
    assert!(read(&dir, &fresh, "m.rs").contains("merge conflict(s) with ids 1, 2"));
}

#[test]
fn resolving_keeps_other_ids_stable_and_refuses_stale_ones() {
    let dir = TempDir::new("conflicts-one");
    let path = dir.write("m.rs", CONFLICTED);
    let tracker = FileReadTracker::new();
    read(&dir, &tracker, "m.rs:conflicts");

    let receipt = resolve(&dir, &tracker, "1", "@theirs\n").unwrap();
    assert!(
        receipt.contains("Resolved conflict(s) at lines 2-8: 7 -> 1 line(s)"),
        "{receipt}"
    );
    assert!(
        receipt.contains("1 conflict(s) still registered"),
        "{receipt}"
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "head\nyours\nmid\n<<<<<<< HEAD\nonly-ours\n=======\nonly-theirs\n>>>>>>> feature\ntail\n"
    );

    // Id 2 still addresses the other hunk; resolving id 1 twice is refused.
    assert!(
        resolve(&dir, &tracker, "2", "merged\n")
            .unwrap()
            .contains("Resolved")
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "head\nyours\nmid\nmerged\ntail\n"
    );
    assert!(
        resolve(&dir, &tracker, "1", "@ours").is_err(),
        "id 1 is gone"
    );
    assert!(read(&dir, &tracker, "m.rs:conflicts").contains("no merge conflict markers"));
}

#[test]
fn tokens_and_directives_resolve_every_file() {
    let dir = TempDir::new("conflicts-bulk");
    let a = dir.write("a.rs", CONFLICTED);
    let b = dir.write(
        "b.rs",
        "x\n<<<<<<< HEAD\nleft\n=======\nright\n>>>>>>> f\ny\n",
    );
    let tracker = FileReadTracker::new();
    read(&dir, &tracker, "a.rs:conflicts");
    read(&dir, &tracker, "b.rs:conflicts");

    // Tokens mixed with literal text; `@both` is the addressed hunk's two sides.
    assert!(
        resolve(&dir, &tracker, "2", "keep\n@both\nend\n")
            .unwrap()
            .contains("Resolved")
    );
    assert!(
        fs::read_to_string(&a)
            .unwrap()
            .contains("keep\nonly-ours\nonly-theirs\nend\n"),
        "{}",
        fs::read_to_string(&a).unwrap()
    );

    // Per-id directives apply in every registered file holding that id.
    assert!(
        resolve(&dir, &tracker, "*", "1: @ours\n")
            .unwrap()
            .contains("Resolved")
    );
    assert!(
        fs::read_to_string(&a)
            .unwrap()
            .starts_with("head\nmine\nmid\n"),
        "{}",
        fs::read_to_string(&a).unwrap()
    );
    assert_eq!(fs::read_to_string(&b).unwrap(), "x\nleft\ny\n");
    // Nothing is registered any more, so a second bulk write has no target.
    assert!(resolve(&dir, &tracker, "*", "done\n").is_err());
}

#[test]
fn empty_content_and_missing_sides_are_refused() {
    let dir = TempDir::new("conflicts-bad");
    let path = dir.write(
        "m.rs",
        "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> f\nc\n",
    );
    let tracker = FileReadTracker::new();
    read(&dir, &tracker, "m.rs:conflicts");
    assert!(
        resolve(&dir, &tracker, "1", "   ")
            .unwrap_err()
            .contains("empty content")
    );
    assert!(err(&dir, &tracker, "conflict://1/base").contains("no `base` side"));
    assert!(
        resolve(&dir, &tracker, "7", "@ours")
            .unwrap_err()
            .contains("never read")
    );
    assert!(
        resolve(&dir, &tracker, "1/ours", "x")
            .unwrap_err()
            .contains("is a view")
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> f\nc\n"
    );
}
