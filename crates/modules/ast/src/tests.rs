//! Module-surface tests: path parsing, argument validation and the proposal bookkeeping.
//! The sidecar round-trip itself is tested in the `hrn-ast` package (`tests/cli.rs`).

use crate::staged::Staged;
use crate::{Config, Overrides, check_scope, tags_of};
use hrn_edge_ast::protocol::{Change, RewriteData, SearchData};
use hrn_edge_ast::{show_applied, show_search};
use hrn_kernel::serde_json::json;
use std::path::Path;

fn tree(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hrn-ast-mod-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(dir: &Path, rel: &str, text: &str) -> std::path::PathBuf {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    std::fs::write(&path, text).expect("write");
    path
}

fn config() -> Config {
    Config::resolve(Overrides::default()).expect("defaults")
}

#[test]
fn defaults_load_and_zeroes_are_refused() {
    let cfg = config();
    assert_eq!(cfg.max_matches, 50);
    assert_eq!(cfg.max_files, 1000);
    let err = Config::resolve(Overrides {
        max_matches: Some(0),
        max_files: None,
        discover: None,
    })
    .expect_err("zero matches cap");
    assert!(err.to_string().contains("max_matches"), "{err}");
}

#[test]
fn a_scope_expression_is_validated_and_classified() {
    let dir = tree("paths");
    write(&dir, "src/a.rs", "fn m() { p(1); }\n");
    assert!(check_scope(&dir, "src/**/*.rs", "e").expect("glob"));
    assert!(check_scope(&dir, "src", "e").expect("directory"));
    assert!(
        check_scope(&dir, "  ; a.rs", "e").is_err(),
        "blank entries fail"
    );
    assert!(check_scope(&dir, "", "e").is_err(), "an empty scope fails");
    let file = write(&dir, "a.rs", "fn m() { p(1); }\n");
    assert!(
        !check_scope(&dir, file.to_str().expect("utf-8"), "e").expect("single file"),
        "one plain file is not rendered as a tree"
    );
}

#[test]
fn staged_proposals_are_taken_once_per_session() {
    let staged = Staged::default();
    assert!(staged.take("s1").is_err(), "nothing staged at first");
    staged.stage(
        "s1",
        crate::staged::Proposal {
            cwd: "C:/x".to_owned(),
            path: ".".to_owned(),
            rules: Vec::new(),
            lang: None,
            max_files: 1,
            expected: crate::staged::Expected {
                total: 2,
                files: 1,
                per_file: vec![("a.rs".to_owned(), 2)],
            },
        },
    );
    assert!(staged.take("s2").is_err(), "another session has nothing");
    assert!(staged.take("s1").is_ok());
    assert!(staged.take("s1").is_err(), "a proposal is consumed once");
}

#[tokio::test]
async fn an_empty_reason_is_refused_before_any_sidecar_call() {
    let staged = std::sync::Arc::new(Staged::default());
    let ctx = hrn_kernel::testing::tool_context(std::env::temp_dir());
    let tool: std::sync::Arc<dyn hrn_kernel::Tool> =
        std::sync::Arc::new(crate::edit::RejectTool::new(staged));
    let out = tool
        .execute(&ctx, json!({"reason": "  "}))
        .await
        .expect("reject");
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("must be a non-empty string"),
        "{}",
        out.content
    );
}

#[test]
fn rendering_uses_the_omp_shapes() {
    let search = SearchData {
        matches: vec![hrn_edge_ast::protocol::Found {
            path: "src/a.rs".to_owned(),
            line: 2,
            text: "p(1)".to_owned(),
            meta: vec![("A".to_owned(), "1".to_owned())],
        }],
        total: 1,
        files_searched: 1,
        limit_reached: false,
        parse_errors: Vec::new(),
    };
    let tag = |_: &str| Some("1234".to_owned());
    let shown = show_search(&search, true, &tag);
    assert!(shown.text.contains("# src/"), "{}", shown.text);
    assert!(shown.text.contains("## a.rs#1234"), "{}", shown.text);
    assert!(shown.text.contains("*2:p(1)"), "{}", shown.text);
    assert!(shown.text.contains("meta: A=1"), "{}", shown.text);

    let rewrite = RewriteData {
        changes: vec![Change {
            path: "a.rs".to_owned(),
            line: 3,
            before: "old(1)".to_owned(),
            after: "new(1)".to_owned(),
        }],
        per_file: vec![("a.rs".to_owned(), 1)],
        total: 1,
        files_touched: 1,
        files_searched: 1,
        limit_reached: false,
        parse_errors: Vec::new(),
    };
    let applied = show_applied(&rewrite, &tag);
    assert!(
        applied.starts_with("Applied 1 replacement in 1 file."),
        "{applied}"
    );
    assert!(applied.contains("[a.rs#1234]"), "{applied}");
}

#[test]
fn tags_come_from_current_file_content() {
    let dir = tree("tags");
    write(&dir, "a.rs", "fn m() { p(1); }\n");
    let tag = tags_of(&dir);
    let first = tag("a.rs");
    assert!(first.is_some());
    assert_eq!(tag("a.rs"), first);
    assert_eq!(tag("missing.rs"), None);
    write(&dir, "a.rs", "fn m() { p(2); }\n");
    assert_ne!(tag("a.rs"), first, "content changed, tag changed");
}
