//! Unit and integration tests for `hrn-mod-agy` worktree isolation and diff-scope gates.

use super::*;
use std::process::Command;

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(tag: &str) -> PathBuf {
    let n = WORKTREE_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hrn-agy-wt-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir");
    git(&dir, &["init"]);
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    git(&dir, &["config", "core.autocrlf", "false"]);
    git(&dir, &["config", "core.eol", "lf"]);
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() -> i32 { 1 }\n").expect("write");
    std::fs::write(dir.join("README.md"), "hello\n").expect("write");
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "init"]);
    dir
}

#[test]
fn scope_glob_and_denied_precedence() {
    let files = vec![
        "src/auth/login.rs".to_owned(),
        "src/auth/secrets.rs".to_owned(),
        "README.md".to_owned(),
    ];
    let allowed = vec!["src/**".to_owned()];
    let denied = vec!["**/secrets.rs".to_owned()];
    let violations = find_scope_violations(&files, &allowed, &denied);
    assert_eq!(
        violations,
        vec!["src/auth/secrets.rs".to_owned(), "README.md".to_owned()]
    );
    assert!(path_matches("src/a/b.rs", "src"));
    assert!(path_matches("src/a/b.rs", "src/**"));
    assert!(!path_matches("src2/a.rs", "src"));
}

#[test]
fn parses_porcelain_renames_and_untracked() {
    let raw = " M src/lib.rs\n?? src/new.rs\nR  old.rs -> \"src/renamed.rs\"\n";
    assert_eq!(
        parse_porcelain_paths(raw),
        vec!["src/lib.rs", "src/new.rs", "src/renamed.rs"]
    );
}

#[hrn_edge_rt::tokio::test]
async fn worktree_applies_in_scope_edits_and_cleans_up() {
    let repo = init_repo("apply");
    let wt = TempWorktree::create(&repo).await.expect("create wt");
    assert!(wt.path.is_dir());

    std::fs::write(wt.path.join("src/lib.rs"), "pub fn a() -> i32 { 2 }\n").expect("edit");
    std::fs::write(wt.path.join("src/extra.rs"), "pub fn b() {}\n").expect("add");

    let out = wt
        .finalize(&["src/**".to_owned()], &[], false)
        .await
        .expect("finalize");
    assert!(out.applied);
    assert!(out.worktree_path.is_none());
    assert_eq!(out.changed_files, vec!["src/extra.rs", "src/lib.rs"]);
    assert!(!wt.path.exists(), "temp worktree should be removed");
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).expect("read"),
        "pub fn a() -> i32 { 2 }\n"
    );
    assert!(repo.join("src/extra.rs").is_file());
    let _ = std::fs::remove_dir_all(&repo);
}

#[hrn_edge_rt::tokio::test]
async fn worktree_rolls_back_and_removes_on_scope_violation() {
    let repo = init_repo("rollback");
    let wt = TempWorktree::create(&repo).await.expect("create wt");

    std::fs::write(wt.path.join("src/lib.rs"), "pub fn a() -> i32 { 99 }\n").expect("edit");
    std::fs::write(wt.path.join("README.md"), "hacked\n").expect("out of scope");

    let err = wt
        .finalize(&["src/**".to_owned()], &[], false)
        .await
        .expect_err("must reject out-of-scope edit");
    let msg = err.to_string();
    assert!(msg.contains("README.md"), "{msg}");
    assert!(msg.contains("rolled back"), "{msg}");
    assert!(!wt.path.exists(), "violating worktree must be cleaned up");
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).expect("unchanged"),
        "pub fn a() -> i32 { 1 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).expect("unchanged"),
        "hello\n"
    );
    let _ = std::fs::remove_dir_all(&repo);
}
