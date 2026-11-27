//! `write` targets inside containers: one zip / tar member, one `SQLite` row.

use super::tests::{put, read, tar_bytes, zip_bytes};
use super::write::write_to;
use crate::testutil::TempDir;
use hrn_kernel::ToolOutput;

fn write(dir: &TempDir, raw: &str, content: &str) -> String {
    match write_to(dir.path(), raw, content) {
        Ok(Some(ToolOutput { content, .. })) => content,
        Ok(None) => panic!("`{raw}` was not recognized as a container target"),
        Err(e) => panic!("write `{raw}`: {e}"),
    }
}

fn write_err(dir: &TempDir, raw: &str, content: &str) -> String {
    match write_to(dir.path(), raw, content) {
        Ok(out) => panic!("`{raw}` accepted {content:?} ({out:?})"),
        Err(e) => e.to_string(),
    }
}

fn fall_through(dir: &TempDir, raw: &str) -> bool {
    matches!(write_to(dir.path(), raw, "x"), Ok(None))
}

/// The stored text of a member, with no line numbering (trailing newline not asserted).
fn member(dir: &TempDir, addr: &str) -> String {
    read(dir, &format!("{addr}:raw"))
        .trim_end_matches('\n')
        .to_owned()
}

fn db(dir: &TempDir) {
    let conn = rusqlite::Connection::open(dir.path().join("t.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, score REAL, active INTEGER);
         CREATE TABLE pair(a INTEGER, b INTEGER, PRIMARY KEY (a, b));",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO users(name, score, active) VALUES ('u1', 1.5, 1)",
        [],
    )
    .unwrap();
}

#[test]
fn zip_members_replace_and_add_without_losing_the_rest() {
    let dir = TempDir::new("write-zip");
    put(
        &dir,
        "a.zip",
        &zip_bytes(&[
            ("readme.md", b"old".as_slice()),
            ("docs/guide.md", b"g".as_slice()),
        ]),
    );
    let receipt = write(&dir, "a.zip:readme.md", "new text\n");
    assert!(
        receipt.contains("Successfully wrote 9 bytes to a.zip:readme.md (2 entries"),
        "{receipt}"
    );
    assert_eq!(member(&dir, "a.zip:readme.md"), "new text");
    assert_eq!(member(&dir, "a.zip:docs/guide.md"), "g");

    write(&dir, "a.zip:docs/new.txt", "added\n");
    // The listing shows one level; the new member lives under `docs/`.
    assert_eq!(member(&dir, "a.zip:docs/new.txt"), "added");
    assert!(
        read(&dir, "a.zip:docs").contains("new.txt"),
        "{}",
        read(&dir, "a.zip:docs")
    );

    // A missing archive is created; a directory or `..` address is refused.
    write(&dir, "fresh.tar:inner/x.txt", "x\n");
    assert_eq!(member(&dir, "fresh.tar:inner/x.txt"), "x");
    assert!(write_err(&dir, "a.zip:", "x").contains("address a member"));
    assert!(write_err(&dir, "a.zip:../escape", "x").contains("may not contain `..`"));
    assert!(
        write_err(&dir, "a.zip", "x").contains("address a member"),
        "the archive itself is not a text target"
    );
}

#[test]
fn tar_members_round_trip_including_gzip() {
    let dir = TempDir::new("write-tar");
    put(
        &dir,
        "a.tar",
        &tar_bytes(&[
            ("one.txt", b"1".as_slice(), b'0'),
            ("dir/", b"".as_slice(), b'5'),
        ]),
    );
    write(&dir, "a.tar:one.txt", "replaced\n");
    assert_eq!(member(&dir, "a.tar:one.txt"), "replaced");
    assert!(
        read(&dir, "a.tar").contains("dir/"),
        "the directory entry survives"
    );

    put(&dir, "a.tar.gz", &[0x1f, 0x8b]);
    write(&dir, "a.tar.gz:new.txt", "gzipped\n");
    assert_eq!(member(&dir, "a.tar.gz:new.txt"), "gzipped");
}

#[test]
fn sqlite_rows_insert_update_and_delete() {
    let dir = TempDir::new("write-sqlite");
    db(&dir);

    let inserted = write(&dir, "t.sqlite:users", "{ name: 'u2', score: 2.5 }");
    assert!(inserted.contains("Inserted row into `users`"), "{inserted}");
    assert!(inserted.contains("name = u2"), "{inserted}");

    let updated = write(
        &dir,
        "t.sqlite:users:1",
        "{'name': 'renamed', active: false}",
    );
    assert!(
        updated.contains("Updated row '1' (id) in `users`"),
        "{updated}"
    );
    assert!(
        updated.contains("name = renamed") && updated.contains("active = 0"),
        "{updated}"
    );

    let deleted = write(&dir, "t.sqlite:users:1", "");
    assert!(deleted.contains("Deleted row '1' (id)"), "{deleted}");
    assert!(
        write(&dir, "t.sqlite:users:99", "").contains("No row with id = '99'"),
        "a miss is reported, not an error"
    );

    // Bad addresses and bodies name the problem.
    assert!(write_err(&dir, "nope.sqlite:users", "{}").contains("not found"));
    assert!(write_err(&dir, "t.sqlite:nosuch", "{}").contains("no table `nosuch`"));
    assert!(write_err(&dir, "t.sqlite:users", "{ nope: 1 }").contains("no column(s) `nope`"));
    assert!(write_err(&dir, "t.sqlite:users", "{ name: {a: 1} }").contains("is not a scalar"));
    assert!(write_err(&dir, "t.sqlite:pair:1", "{ a: 1 }").contains("composite primary key"));
    assert!(write_err(&dir, "t.sqlite", "{ a: 1 }").contains("address a table"));
    assert!(write_err(&dir, "t.sqlite:users?limit=2", "{ a: 1 }").contains("not a row"));
    assert!(
        write_err(&dir, "t.sqlite", "{ a: 1 }").contains("address a table"),
        "the database itself is not a text target"
    );
}

#[test]
fn ordinary_paths_fall_through_to_the_file_writer() {
    let dir = TempDir::new("write-fall");
    put(&dir, "notes.txt", b"a\n");
    put(&dir, "fake.zip", b"not a zip at all");
    assert!(fall_through(&dir, "notes.txt"));
    // A text file that only borrows an archive extension stays a plain file.
    assert!(fall_through(&dir, "fake.zip"));
    assert!(fall_through(&dir, "out.md"));
}
