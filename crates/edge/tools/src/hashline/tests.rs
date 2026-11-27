use super::*;
use crate::testutil::{TempDir, test_ctx};
use std::fs;

fn run(
    dir: &TempDir,
    store: &Mutex<HashMap<String, Vec<String>>>,
    input: &str,
) -> Result<ToolOutput> {
    edit_sync(
        &FileReadTracker::new(),
        store,
        &test_ctx(dir.path().to_path_buf()),
        input,
        &HashlineOpts::default(),
    )
}

fn once(dir: &TempDir, input: &str) -> Result<ToolOutput> {
    run(dir, &Mutex::default(), input)
}

fn tag(text: &str) -> String {
    tag_of(text)
}

#[test]
fn tags_ignore_crlf_and_track_content() {
    assert_eq!(tag("a\r\nb\r\n"), tag("a\nb\n"));
    assert_ne!(tag("a\nb\n"), tag("a\nc\n"));
    let t = tag("x");
    assert!(
        t.len() == 4
            && t.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_lowercase())
    );
}

#[test]
fn replace_insert_and_cut_use_original_numbers_and_return_a_fresh_tag() {
    let dir = TempDir::new("hl-basic");
    let text = "one\ntwo\nthree\nfour\nfive\n";
    let path = dir.write("f.txt", text);
    let out = once(
        &dir,
        &format!(
            "*** Begin Patch\n[f.txt#{}]\nPUT 2.=3:\n+TWO\nPUT >5:\n+six\nCUT 1.=1\n*** End Patch",
            tag(text)
        ),
    )
    .unwrap();
    let new = "TWO\nfour\nfive\nsix\n";
    assert_eq!(fs::read_to_string(&path).unwrap(), new);
    assert!(
        out.content.starts_with(&format!("[f.txt#{}]\n", tag(new))),
        "{}",
        out.content
    );
    assert!(out.content.contains("1:TWO"), "{}", out.content);
    let diff = out.diff.unwrap();
    assert!(diff.contains("-one") && diff.contains("+six"), "{diff}");
}

#[test]
fn a_stale_tag_is_refused_with_the_current_tag_and_rows() {
    let dir = TempDir::new("hl-stale");
    let path = dir.write("f.txt", "a\nb\nc\n");
    let err = once(&dir, "[f.txt#0000]\nPUT 2.=2:\n+B")
        .unwrap_err()
        .to_string();
    assert!(err.contains("stale tag"), "{err}");
    assert!(err.contains(&format!("#{}", tag("a\nb\nc\n"))), "{err}");
    assert!(err.contains("2:b"), "{err}");
    let placeholder = once(&dir, "[f.txt#AUTO]\nPUT 2.=2:\n+B")
        .unwrap_err()
        .to_string();
    assert!(placeholder.contains("not a snapshot tag"), "{placeholder}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "a\nb\nc\n");
}

#[test]
fn block_replace_and_after_block_insert() {
    let dir = TempDir::new("hl-block");
    let text = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
    let path = dir.write("f.rs", text);
    let out = once(
        &dir,
        &format!(
            "[f.rs#{}]\nPUT 1*:\n+fn a() {{ 10 }}\nPUT >5*:\n+\n+fn c() {{}}\n",
            tag(text)
        ),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "fn a() { 10 }\n\nfn b() {\n    2\n}\n\nfn c() {}\n"
    );
    assert!(
        out.content.contains("`1*` resolved to lines 1-3"),
        "{}",
        out.content
    );
    let single = once(
        &dir,
        &format!(
            "[f.rs#{}]\nPUT 1*:\n+x\n",
            tag(&fs::read_to_string(&path).unwrap())
        ),
    )
    .unwrap_err()
    .to_string();
    assert!(single.contains("explicit-line"), "{single}");
}

#[test]
fn cut_in_one_file_pastes_into_another_and_registers_persist() {
    let dir = TempDir::new("hl-regs");
    let (a, b) = ("x\nfn f() {\n}\ny\n", "top\n");
    let pa = dir.write("a.rs", a);
    let pb = dir.write("b.rs", b);
    let store = Mutex::default();
    run(
        &dir,
        &store,
        &format!(
            "[a.rs#{}]\nCUT 2* @fn\n[b.rs#{}]\nPUT >$ @fn\n",
            tag(a),
            tag(b)
        ),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&pa).unwrap(), "x\ny\n");
    assert_eq!(fs::read_to_string(&pb).unwrap(), "top\nfn f() {\n}\n");
    // The named register outlives the call.
    let b2 = "top\nfn f() {\n}\n";
    run(&dir, &store, &format!("[b.rs#{}]\nPUT <1 @fn\n", tag(b2))).unwrap();
    assert_eq!(
        fs::read_to_string(&pb).unwrap(),
        "fn f() {\n}\ntop\nfn f() {\n}\n"
    );
}

#[test]
fn multi_file_calls_are_all_or_nothing_and_no_change_is_refused() {
    let dir = TempDir::new("hl-atomic");
    let (a, b) = ("a\n", "b\n");
    let pa = dir.write("a.txt", a);
    let pb = dir.write("b.txt", b);
    let err = once(
        &dir,
        &format!(
            "[a.txt#{}]\nPUT 1.=1:\n+A\n[b.txt#{}]\nPUT 9.=9:\n+B\n",
            tag(a),
            tag(b)
        ),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("b.txt") && err.contains("outside"), "{err}");
    assert_eq!(fs::read_to_string(&pa).unwrap(), a);
    assert_eq!(fs::read_to_string(&pb).unwrap(), b);
    let same = once(&dir, &format!("[a.txt#{}]\nPUT 1.=1:\n+a\n", tag(a)))
        .unwrap_err()
        .to_string();
    assert!(same.contains("no change"), "{same}");
}

#[test]
fn rem_and_mv() {
    let dir = TempDir::new("hl-rem-mv");
    let (a, b) = ("a\nb\n", "gone\n");
    let pa = dir.write("a.txt", a);
    let pb = dir.write("b.txt", b);
    let out = once(
        &dir,
        &format!(
            "[a.txt#{}]\nPUT 2.=2:\n+B\nMV \"sub dir/n.txt\"\n[b.txt#{}]\nREM\n",
            tag(a),
            tag(b)
        ),
    )
    .unwrap();
    assert!(!pa.exists() && !pb.exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("sub dir/n.txt")).unwrap(),
        "a\nB\n"
    );
    assert!(out.content.contains("Moved from `a.txt`") && out.content.contains("Removed `b.txt`"));
}

#[test]
fn crlf_bom_and_a_missing_final_newline_survive() {
    let dir = TempDir::new("hl-codec");
    let raw = "\u{feff}one\r\ntwo\r\nthree";
    let path = dir.write("f.txt", raw);
    let t = tag("one\ntwo\nthree");
    once(
        &dir,
        &format!("[f.txt#{t}]\nPUT 2.=2:\n+TWO\nPUT >3:\n+four"),
    )
    .unwrap();
    assert_eq!(
        fs::read(&path).unwrap(),
        "\u{feff}one\r\nTWO\r\nthree\r\nfour".as_bytes()
    );
}

#[test]
fn empty_files_take_rows_and_headers_without_ops_are_refused() {
    let dir = TempDir::new("hl-empty");
    let path = dir.write("e.txt", "");
    let t = tag("");
    let bare = once(&dir, &format!("[e.txt#{t}]\n"))
        .unwrap_err()
        .to_string();
    assert!(bare.contains("no operations"), "{bare}");
    once(&dir, &format!("[e.txt#{t}]\nPUT >$:\n+hello")).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
}

#[test]
fn unseen_line_refused() {
    let dir = TempDir::new("hl-seen");
    let mut body = String::new();
    for i in 1..=100 {
        let _ = writeln!(body, "let v{i} = {i};");
    }
    let path = dir.write("f.rs", &body);
    let ctx = test_ctx(dir.path().to_path_buf());
    let tracker = Arc::new(FileReadTracker::new());
    let editor = HashlineEditor::with_opts(
        Arc::clone(&tracker),
        HashlineOpts {
            enforce_seen_lines: true,
            ..HashlineOpts::default()
        },
    );
    let t = tag(&body);

    // Editing before any read is refused citing `seen: none` (or `only seen none`).
    let unread = editor
        .edit(&ctx, &format!("[f.rs#{t}]\nPUT 1.=1:\n+let v1 = 99;\n"))
        .unwrap_err()
        .to_string();
    assert!(unread.contains("unseen"), "{unread}");
    assert!(unread.contains("1-1"), "{unread}");

    // Read only lines 1-5 (with context -> 1-8); editing line 90 must be refused citing both ranges.
    crate::fs_read::read_sync(&tracker, &ctx, "f.rs:1-5").unwrap();
    let unseen = editor
        .edit(&ctx, &format!("[f.rs#{t}]\nPUT 90.=90:\n+let v90 = 900;\n"))
        .unwrap_err()
        .to_string();
    assert!(unseen.contains("90-90"), "{unseen}");
    assert!(unseen.contains("1-8"), "{unseen}");
    assert_eq!(fs::read_to_string(&path).unwrap(), body);

    // After reading the full file (or range covering line 90), the edit succeeds.
    crate::fs_read::read_sync(&tracker, &ctx, "f.rs").unwrap();
    editor
        .edit(&ctx, &format!("[f.rs#{t}]\nPUT 90.=90:\n+let v90 = 900;\n"))
        .unwrap();
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("let v90 = 900;")
    );
}

#[test]
fn mv_refuses_unread_or_stale_existing_dest_and_preserves_seen_ranges() {
    let dir = TempDir::new("hl-mv-guard");
    let mut body = String::new();
    for i in 1..=30 {
        let _ = writeln!(body, "row {i}");
    }
    let src = dir.write("src.txt", &body);
    let existing = dir.write("dest.txt", "important\n");
    let ctx = test_ctx(dir.path().to_path_buf());
    let tracker = Arc::new(FileReadTracker::new());
    let editor = HashlineEditor::with_opts(
        Arc::clone(&tracker),
        HashlineOpts {
            enforce_seen_lines: true,
            blocked_globs: vec!["Cargo.lock".to_owned()],
            ..HashlineOpts::default()
        },
    );
    crate::fs_read::read_sync(&tracker, &ctx, "src.txt:1-3").unwrap();
    let t = tag(&body);

    // Overwriting an existing UNREAD destination must be refused.
    let err = editor
        .edit(
            &ctx,
            &format!("[src.txt#{t}]\nPUT 1.=1:\n+ROW 1\nMV dest.txt\n"),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("unseen"), "{err}");
    assert_eq!(fs::read_to_string(&existing).unwrap(), "important\n");
    assert!(src.exists());

    // Moving to a new file transfers only the seen range (1-6), NOT all 30 lines.
    editor
        .edit(
            &ctx,
            &format!("[src.txt#{t}]\nPUT 1.=1:\n+ROW 1\nMV moved.txt\n"),
        )
        .unwrap();
    let moved_path = dir.path().join("moved.txt");
    let moved_text = fs::read_to_string(&moved_path).unwrap();
    let err_unseen = editor
        .edit(
            &ctx,
            &format!("[moved.txt#{}]\nPUT 25.=25:\n+ROW 25\n", tag(&moved_text)),
        )
        .unwrap_err()
        .to_string();
    assert!(
        err_unseen.contains("unseen") && err_unseen.contains("25-25"),
        "{err_unseen}"
    );
}

#[test]
fn hashline_refuses_readonly_url_schemes_including_attachment() {
    use hrn_kernel::{UrlBody, UrlRouter, UrlScheme};
    use std::sync::Arc;

    struct StaticPathScheme(&'static str, std::path::PathBuf);
    impl UrlScheme for StaticPathScheme {
        fn scheme(&self) -> &'static str {
            self.0
        }
        fn about(&self) -> &'static str {
            "test scheme"
        }
        fn read(&self, _ctx: &hrn_kernel::ToolContext, _rest: &str) -> hrn_kernel::Result<UrlBody> {
            Ok(UrlBody::Path(self.1.clone()))
        }
    }

    let dir = TempDir::new("hl-readonly-url");
    let attached = dir.write("1-notes.txt", "original\n");
    let scratch = dir.write("scratch.md", "before\n");
    let mut ctx = test_ctx(dir.path().to_path_buf());
    ctx.urls = Arc::new(
        UrlRouter::new()
            .with_builtin(Arc::new(StaticPathScheme("attachment", attached.clone())))
            .with_builtin(Arc::new(StaticPathScheme("skill", attached.clone())))
            .with_builtin(Arc::new(StaticPathScheme("local", scratch.clone()))),
    );
    let editor = HashlineEditor::new(Arc::new(FileReadTracker::new()));

    // `PUT`, `REM`, and `MV` on `attachment://` and `skill://` must be refused and leave files intact.
    let t = tag("original\n");
    for script in [
        format!("[attachment://1-notes.txt#{t}]\nPUT 1.=1:\n+tampered\n"),
        format!("[attachment://1-notes.txt#{t}]\nREM\n"),
        format!("[attachment://1-notes.txt#{t}]\nMV moved.txt\n"),
        format!("[skill://demo#{t}]\nPUT 1.=1:\n+tampered\n"),
    ] {
        let err = editor
            .edit(&ctx, &script)
            .expect_err("read-only scheme edit must fail")
            .to_string();
        assert!(err.contains("read-only"), "{err}");
    }
    assert_eq!(fs::read_to_string(&attached).unwrap(), "original\n");

    // Writable `local://` continues to work.
    let st = tag("before\n");
    editor
        .edit(
            &ctx,
            &format!("[local://scratch.md#{st}]\nPUT 1.=1:\n+after\n"),
        )
        .expect("local:// edit allowed");
    assert_eq!(fs::read_to_string(&scratch).unwrap(), "after\n");
}
