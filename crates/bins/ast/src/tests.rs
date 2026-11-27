//! Engine tests for the sidecar: the behaviors that decide what a user sees (paging, parse
//! issues, staged rewrites, language resolution).

use crate::engine::{rewrite, search};
use crate::lang::Lang;
use hrn_edge_ast::protocol::Rule;
use std::fs;
use std::path::{Path, PathBuf};

struct Tree(PathBuf);

impl Tree {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("hrn-astbin-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp tree");
        Self(dir)
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&path, content).expect("write file");
        path
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.0.join(rel)).expect("read file")
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// The whole tree as a scope expression.
    fn scope(&self) -> String {
        self.0.to_string_lossy().replace('\\', "/")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn finds_calls_and_binds_metavariables() {
    let tree = Tree::new("find");
    tree.write(
        "src/a.rs",
        "fn main() {\n    log(\"one\");\n    log(\"two\");\n}\n",
    );
    let found = search(tree.path(), &tree.scope(), None, "log($A)", 0, 50).expect("search");
    assert_eq!(found.total, 2, "{:?}", found.matches);
    let first = found.matches.first().expect("a match");
    assert_eq!(first.line, 2);
    assert_eq!(first.path, "src/a.rs");
    assert_eq!(first.meta, vec![("A".to_owned(), "\"one\"".to_owned())]);
}

#[test]
fn pages_with_skip_and_reports_the_cut() {
    let tree = Tree::new("paging");
    tree.write("a.rs", "fn m() { p(1); p(2); p(3); }\n");
    let first = search(tree.path(), &tree.scope(), None, "p($A)", 0, 2).expect("first page");
    assert_eq!(first.matches.len(), 2);
    assert!(first.limit_reached);
    let second = search(tree.path(), &tree.scope(), None, "p($A)", 2, 2).expect("second page");
    assert_eq!(second.matches.len(), 1);
    assert!(!second.limit_reached);
}

#[test]
fn multi_capture_rewrites_and_binds() {
    let tree = Tree::new("multi");
    tree.write("m.rs", "fn m() {\n    foo(1, 2);\n    foo();\n}\n");
    let out = rewrite(
        tree.path(),
        &tree.scope(),
        None,
        &[Rule {
            pat: "foo($$$ARGS)".to_owned(),
            out: "bar($$$ARGS)".to_owned(),
        }],
        1000,
        true,
    )
    .expect("rewrite");
    assert_eq!(out.total, 2);
    assert_eq!(out.files_touched, 1);
    assert_eq!(
        tree.read("m.rs"),
        "fn m() {\n    bar(1, 2);\n    bar();\n}\n"
    );
}

#[test]
fn preview_writes_nothing_and_apply_reports_the_same_counts() {
    let tree = Tree::new("preview");
    tree.write("a.rs", "fn m() { old(1); }\n");
    let rules = [Rule {
        pat: "old($A)".to_owned(),
        out: "new($A)".to_owned(),
    }];
    let preview = rewrite(tree.path(), &tree.scope(), None, &rules, 1000, false).expect("preview");
    assert_eq!(tree.read("a.rs"), "fn m() { old(1); }\n");
    let applied = rewrite(tree.path(), &tree.scope(), None, &rules, 1000, true).expect("apply");
    assert_eq!(applied.total, preview.total);
    assert_eq!(applied.per_file, preview.per_file);
    assert_eq!(tree.read("a.rs"), "fn m() { new(1); }\n");
}

#[test]
fn a_file_with_syntax_errors_is_skipped_and_reported() {
    let tree = Tree::new("broken");
    tree.write("good.rs", "fn m() { old(1); }\n");
    tree.write("broken.rs", "fn m( { old(2) \n");
    let out = rewrite(
        tree.path(),
        &tree.scope(),
        None,
        &[Rule {
            pat: "old($A)".to_owned(),
            out: "new($A)".to_owned(),
        }],
        1000,
        true,
    )
    .expect("rewrite");
    assert_eq!(out.total, 1);
    assert!(
        out.parse_errors
            .iter()
            .any(|e| e.contains("broken.rs") && e.contains("error nodes")),
        "{:?}",
        out.parse_errors
    );
    assert_eq!(tree.read("broken.rs"), "fn m( { old(2) \n");
}

#[test]
fn overlapping_rewrites_abort() {
    let tree = Tree::new("overlap");
    tree.write("a.rs", "fn m() { old(old(1)); }\n");
    let err = rewrite(
        tree.path(),
        &tree.scope(),
        None,
        &[
            Rule {
                pat: "old($A)".to_owned(),
                out: "wrap(old($A))".to_owned(),
            },
            Rule {
                pat: "old(old($A))".to_owned(),
                out: "old(1)".to_owned(),
            },
        ],
        1000,
        true,
    )
    .expect_err("overlap must abort");
    assert!(err.contains("overlapping"), "{err}");
}

#[test]
fn mixed_languages_rewrite_only_the_files_a_rule_parses_in() {
    let tree = Tree::new("mixed");
    tree.write("a.rs", "fn m() { go(1); }\n");
    tree.write("b.py", "def m():\n    go(2)\n");
    let out = rewrite(
        tree.path(),
        &tree.scope(),
        None,
        &[Rule {
            pat: "def $F($$$A): $$$B".to_owned(),
            out: "def $F($$$A): pass".to_owned(),
        }],
        1000,
        true,
    )
    .expect("rewrite");
    assert_eq!(out.total, 1);
    assert_eq!(out.per_file.first().expect("a touched file").0, "b.py");
}

#[test]
fn glob_narrows_a_directory_scope() {
    let tree = Tree::new("glob");
    tree.write("src/a.rs", "fn m() { p(1); }\n");
    tree.write("vendor/b.rs", "fn m() { p(2); }\n");
    let scope = format!("{}/src/*.rs", tree.scope());
    let found = search(tree.path(), &scope, None, "p($A)", 0, 50).expect("search");
    assert_eq!(found.matches.len(), 1);
    assert_eq!(found.matches.first().expect("a match").path, "src/a.rs");
}

#[test]
fn lang_override_ignores_extensions() {
    let tree = Tree::new("force-lang");
    tree.write("fragment.txt", "call(x)\n");
    let scope = tree
        .path()
        .join("fragment.txt")
        .to_string_lossy()
        .replace('\\', "/");
    let found = search(tree.path(), &scope, Some("rs"), "call($A)", 0, 50).expect("search");
    assert_eq!(found.matches.len(), 1);
}

#[test]
fn an_empty_scope_is_an_error() {
    let tree = Tree::new("empty");
    tree.write("notes.md", "nothing here\n");
    let err =
        search(tree.path(), &tree.scope(), None, "a", 0, 50).expect_err("no grammars in scope");
    assert!(err.contains("no supported source files"), "{err}");
}

#[test]
fn gitignored_files_are_not_searched() {
    let tree = Tree::new("ignore");
    tree.write(".gitignore", "gen/\n");
    tree.write("gen/a.rs", "fn m() { p(1); }\n");
    tree.write("keep.rs", "fn m() { p(2); }\n");
    let found = search(tree.path(), &tree.scope(), None, "p($A)", 0, 50).expect("search");
    assert_eq!(found.matches.len(), 1);
    assert_eq!(found.matches.first().expect("a match").path, "keep.rs");
}

#[test]
fn language_resolution_follows_the_table() {
    assert!(
        Lang::resolve(Path::new("x.py"), None)
            .expect("table")
            .is_some()
    );
    assert_eq!(
        Lang::resolve(Path::new("x.md"), None).expect("inference"),
        None
    );
    assert!(
        Lang::resolve(Path::new("x.txt"), Some("rs"))
            .expect("override")
            .is_some()
    );
    assert!(Lang::resolve(Path::new("x.txt"), Some("nolang")).is_err());
}

#[test]
fn json_member_fragment_patterns_work() {
    let tree = Tree::new("json");
    tree.write(
        "c.json",
        "{\n  \"keep\": \"one\",\n  \"drop\": \"two\"\n}\n",
    );
    let found = search(tree.path(), &tree.scope(), None, "\"drop\": $V", 0, 50).expect("search");
    assert_eq!(found.matches.len(), 1, "{:?}", found.parse_errors);
}

#[test]
fn check_syntax_reports_clean_broken_and_unsupported() {
    let clean =
        crate::engine::check_syntax("src/lib.rs", None, "fn ok() -> i32 { 1 }\n").expect("clean");
    assert!(clean.supported && clean.error_count == 0 && clean.errors.is_empty());

    let broken = crate::engine::check_syntax("src/lib.rs", None, "fn broken( { let x = ;\n")
        .expect("broken");
    assert!(broken.supported && broken.error_count > 0 && !broken.errors.is_empty());

    let plain = crate::engine::check_syntax("README.txt", None, "not code").expect("plain");
    assert!(!plain.supported && plain.error_count == 0);
}
