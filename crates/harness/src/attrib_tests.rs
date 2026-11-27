use super::*;
use crate::testutil::MemStore;
use hrn_kernel::SessionMeta;

fn make_meta(id: &str, cwd: &str, parent: Option<&str>) -> SessionMeta {
    SessionMeta {
        id: id.into(),
        title: id.into(),
        cwd: cwd.into(),
        parent_session_id: parent.map(str::to_owned),
        ..SessionMeta::default()
    }
}

#[test]
fn load_session_attribution_aggregates_root_and_descendant_subagents() {
    let store = MemStore::default();
    store
        .create_session(&make_meta("root-1", "/repo", None))
        .expect("root");
    store
        .create_session(&make_meta("child-1", "/repo", Some("root-1")))
        .expect("child");
    store
        .create_session(&make_meta("grand-1", "/repo", Some("child-1")))
        .expect("grandchild");

    record_edit_trace(&store, "root-1", "src/lib.rs", &[(2, 4)], false).expect("root trace");
    record_edit_trace(&store, "child-1", "src/lib.rs", &[(5, 7)], false).expect("child trace");
    record_edit_trace(&store, "grand-1", "src/new.rs", &[], true).expect("grand trace");

    let attr = load_session_attribution(&store, "root-1").expect("load attribution");
    assert_eq!(attr.files.len(), 2);
    assert_eq!(
        attr.files.get("src/lib.rs"),
        Some(&FileAttribution {
            whole_file: false,
            ranges: vec![(2, 7)],
        })
    );
    assert_eq!(
        attr.files.get("src/new.rs"),
        Some(&FileAttribution {
            whole_file: true,
            ranges: Vec::new(),
        })
    );
}

#[test]
fn filter_diff_keeps_only_attributed_hunks_and_recomputes_offsets() {
    let diff = "\
diff --git a/src/app.rs b/src/app.rs
index 1111111..2222222 100644
--- a/src/app.rs
+++ b/src/app.rs
@@ -1,4 +1,6 @@
 fn top() {
+    let manual_a = 1;
+    let manual_b = 2;
     let x = 1;
 }
@@ -20,4 +22,4 @@
 fn bottom() {
-    old();
+    agent_edit();
 }
diff --git a/src/other.rs b/src/other.rs
index 3333333..4444444 100644
--- a/src/other.rs
+++ b/src/other.rs
@@ -1,2 +1,3 @@
 // untouched by agent
+let human_only = true;
";
    let mut attr = EditAttribution::default();
    attr.record("src/app.rs".into(), false, &[(23, 23)]);

    let filtered = filter_diff_to_attributed(diff, &attr);
    assert_eq!(filtered.files, vec!["src/app.rs"]);
    assert_eq!(filtered.hunks_kept, 1);
    assert_eq!(filtered.hunks_skipped, 2);
    assert!(
        filtered.patch.contains("@@ -20,4 +20,4 @@"),
        "{}",
        filtered.patch
    );
    assert!(filtered.patch.contains("+    agent_edit();"));
    assert!(!filtered.patch.contains("manual_a"));
    assert!(!filtered.patch.contains("src/other.rs"));
}

#[test]
fn record_tool_edit_parses_edit_diff_and_ignores_internal_write_urls() {
    let store = MemStore::default();
    store
        .create_session(&make_meta("s1", "/repo", None))
        .expect("session");

    // Internal `out://` write must NOT be attributed as a workspace file.
    record_tool_edit(
        &store,
        "s1",
        Path::new("/repo"),
        "write",
        &serde_json::json!({ "path": "out://note", "content": "hi" }),
        &ToolOutput::ok("wrote out://note"),
    );
    // Normal `write` attributes whole file.
    record_tool_edit(
        &store,
        "s1",
        Path::new("/repo"),
        "write",
        &serde_json::json!({ "path": "src/created.rs", "content": "fn f() {}\n" }),
        &ToolOutput::ok("wrote src/created.rs"),
    );
    // `edit` extracts changed line ranges from `output.diff`.
    let mut edit_out = ToolOutput::ok("edited");
    edit_out.diff = Some(
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,4 +10,5 @@\n ctx\n-old\n+new1\n+new2\n ctx\n"
            .to_owned(),
    );
    record_tool_edit(
        &store,
        "s1",
        Path::new("/repo"),
        "edit",
        &serde_json::json!({ "path": "src/lib.rs" }),
        &edit_out,
    );

    let attr = load_session_attribution(&store, "s1").expect("load");
    assert!(!attr.files.contains_key("out://note"));
    assert!(
        attr.files
            .get("src/created.rs")
            .is_some_and(|a| a.whole_file)
    );
    assert_eq!(
        attr.files.get("src/lib.rs"),
        Some(&FileAttribution {
            whole_file: false,
            ranges: vec![(11, 12)],
        })
    );
}
