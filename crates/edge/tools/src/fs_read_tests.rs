use super::*;
use crate::testutil::{TempDir, test_ctx};

fn read(dir: &TempDir, raw: &str) -> String {
    read_sync(
        &FileReadTracker::new(),
        &test_ctx(dir.path().to_path_buf()),
        raw,
    )
    .unwrap()
    .content
}

fn numbered(n: usize) -> String {
    (1..=n).fold(String::new(), |mut s, i| {
        let _ = writeln!(s, "line {i}");
        s
    })
}

#[test]
fn selectors_pick_lines_with_context_and_prefixes() {
    let dir = TempDir::new("read-sel");
    dir.write("f.txt", &numbered(50));
    let one = read(&dir, "f.txt:10");
    assert!(
        one.starts_with(&format!(
            "[f.txt#{}] (lines 9-13 of 50)\n9:line 9\n",
            crate::hashline::tag_of(&numbered(50))
        )),
        "{one}"
    );
    assert!(one.ends_with("13:line 13\n"), "{one}");
    let multi = read(&dir, "f.txt:2-3,40-41");
    assert!(multi.contains("(lines 2-3,40-41 of 50)"), "{multi}");
    assert!(!multi.contains("20:"), "{multi}");
    let tail = read(&dir, "f.txt:-2");
    assert!(tail.contains("49:line 49\n50:line 50"), "{tail}");
    assert_eq!(read(&dir, "f.txt:raw:4-5"), "line 4\nline 5");
    assert_eq!(read(&dir, "f.txt:2+2:raw"), "line 2\nline 3");
}

#[test]
fn bare_reads_page_and_name_the_continuation() {
    let dir = TempDir::new("read-page");
    dir.write("big.txt", &numbered(400));
    let first = read(&dir, "big.txt");
    assert!(first.contains("(lines 1-300 of 400)"), "{first}");
    assert!(first.contains("Use `big.txt:301-` to continue"), "{first}");
    let rest = read(&dir, "big.txt:301-");
    assert!(rest.contains("(lines 301-400 of 400)"), "{rest}");
    assert!(!rest.contains("continue"), "{rest}");
}

#[test]
fn out_of_range_is_text_and_zero_is_an_error() {
    let dir = TempDir::new("read-edge");
    dir.write("f.txt", &numbered(3));
    assert!(read(&dir, "f.txt:99").contains("past the end"));
    let err = read_sync(
        &FileReadTracker::new(),
        &test_ctx(dir.path().to_path_buf()),
        "f.txt:0",
    );
    assert!(err.is_err());
}

#[test]
fn directories_render_as_a_tree_and_page() {
    let dir = TempDir::new("read-dir");
    dir.write("a/x.rs", "x");
    dir.write("b.rs", "y");
    let tree = read(&dir, ".");
    assert!(tree.contains("a/") && tree.contains("b.rs"), "{tree}");
    let page = read(&dir, ".:1-1");
    assert_eq!(page.lines().next(), tree.lines().next());
    assert!(page.contains("Use `"), "{page}");
}

#[test]
fn read_without_hashline_anchors_drops_tag_and_colon_prefix() {
    let dir = TempDir::new("read-no-anchors");
    dir.write("f.txt", "alpha\nbeta\n");
    let out = read_sync_with_anchors(
        &FileReadTracker::new(),
        &test_ctx(dir.path().to_path_buf()),
        "f.txt",
        false,
    )
    .unwrap()
    .content;
    assert!(out.starts_with("File: f.txt (lines 1-2 of 2)\n"), "{out}");
    assert!(out.contains("1|alpha\n2|beta"), "{out}");
    assert!(!out.contains("[f.txt#"), "{out}");
}
