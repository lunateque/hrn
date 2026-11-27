use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static REPO_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempRepo(PathBuf);

impl TempRepo {
    async fn init(tag: &str) -> Self {
        let n = REPO_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hrn-ipc-wt-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        run_git(&dir, &["init".into()]).await.expect("git init");
        run_git(&dir, &["config".into(), "user.name".into(), "test".into()])
            .await
            .expect("user.name");
        run_git(
            &dir,
            &["config".into(), "user.email".into(), "test@local".into()],
        )
        .await
        .expect("user.email");
        run_git(
            &dir,
            &["config".into(), "core.autocrlf".into(), "false".into()],
        )
        .await
        .expect("core.autocrlf");
        run_git(&dir, &["config".into(), "core.eol".into(), "lf".into()])
            .await
            .expect("core.eol");
        std::fs::write(dir.join("hello.txt"), "initial\n").expect("write");
        run_git(&dir, &["add".into(), "hello.txt".into()])
            .await
            .expect("add");
        run_git(&dir, &["commit".into(), "-m".into(), "init".into()])
            .await
            .expect("commit");
        Self(dir)
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn onedrive_paths_are_rejected() {
    assert!(is_onedrive_path(Path::new(
        r"C:\Users\alice\OneDrive\Documents\repo"
    )));
    assert!(is_onedrive_path(Path::new(
        r"C:\Users\alice\OneDrive - Company\repo"
    )));
    assert!(!is_onedrive_path(Path::new(r"C:\Users\alice\harness")));
}

#[test]
fn porcelain_parsers_work() {
    let wt = "worktree C:/repo\nHEAD abcdef\nbranch refs/heads/main\n\nworktree C:/repo/.harness/wt/ui\nHEAD 123456\nbranch refs/heads/agent/ui\n";
    let list = parse_worktree_porcelain(wt);
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].branch.as_deref(), Some("main"));
    assert_eq!(list[1].branch.as_deref(), Some("agent/ui"));

    let st =
        " M crates/front/tui/src/app.rs\n?? Cargo.toml\nR  old.rs -> crates/front/tui/src/new.rs\n";
    let changed = parse_status_porcelain(st);
    assert_eq!(
        changed,
        vec![
            "crates/front/tui/src/app.rs",
            "Cargo.toml",
            "crates/front/tui/src/new.rs"
        ]
    );
}

#[test]
fn slug_and_branch_validation_blocks_traversal() {
    assert!(validate_slug("tui-redesign_1").is_ok());
    assert!(validate_slug("../escape").is_err());
    assert!(validate_slug("-flag").is_err());
    assert!(validate_slug("").is_err());
    assert!(validate_branch_name("hrn/task/worker-1").is_ok());
    assert!(validate_branch_name("-bad").is_err());
    assert!(validate_branch_name("a..b").is_err());
}

#[tokio::test]
async fn worktree_branch_patch_capture_commit_and_apply() {
    let repo = TempRepo::init("patch-apply").await;
    let info = add_worktree_on_branch(&repo.0, "task-alpha", "hrn/task/alpha", Some("HEAD"))
        .await
        .expect("add worktree");
    assert_eq!(info.branch.as_deref(), Some("hrn/task/alpha"));
    assert!(info.path.ends_with(Path::new(".harness/wt/task-alpha")));

    std::fs::write(info.path.join("hello.txt"), "updated\n").expect("edit tracked");
    std::fs::write(info.path.join("new.txt"), "created\n").expect("add untracked");

    let diff = capture_patch(&info.path).await.expect("capture patch");
    assert!(diff.contains("updated") && diff.contains("new.txt") && diff.contains("created"));

    let sha = commit_worktree_branch(&info.path, "task alpha changes")
        .await
        .expect("commit");
    assert!(!sha.is_empty());
    assert!(changed_files(&info.path).await.unwrap().is_empty());

    apply_patch(&repo.0, &diff).await.expect("apply patch");
    assert_eq!(
        std::fs::read_to_string(repo.0.join("hello.txt")).unwrap(),
        "updated\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.0.join("new.txt")).unwrap(),
        "created\n"
    );

    remove_worktree(&repo.0, "task-alpha", true)
        .await
        .expect("remove");
}
