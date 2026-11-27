//! Concrete [`WorktreeIsolator`] backed by `hrn-edge-ipc::worktree` git worktrees:
//! prepares `<repo>/.harness/wt/task-<id>` on branch `hrn/task/<id>` from `HEAD`, verifies diff
//! scope on settle, saves `<id>.patch` into [`ArtifactStore`], optionally commits the branch
//! (`merge == "branch"`) and/or applies the patch to the parent working tree (`apply == true`).

use hrn_edge_ipc::{
    add_worktree_on_branch, apply_patch, capture_patch, check_diff_scope, commit_worktree_branch,
    remove_worktree,
};
use hrn_harness::{
    ArtifactStore, IsolatedWorktree, IsolationFinalize, IsolationOutcome, WorktreeIsolator,
};
use hrn_kernel::{BoxFuture, Result};
use std::path::Path;
use std::sync::Arc;

/// Git-worktree implementation of [`WorktreeIsolator`], wired into [`hrn_harness::AgentRuntime`].
pub(crate) struct GitWorktreeIsolator {
    fallback_artifacts: Option<Arc<ArtifactStore>>,
}

impl GitWorktreeIsolator {
    /// Create an isolator with an optional fallback [`ArtifactStore`] (used when a finalize call
    /// does not supply one).
    pub(crate) fn new(fallback_artifacts: Option<Arc<ArtifactStore>>) -> Self {
        Self { fallback_artifacts }
    }
}

fn sanitize_slug(agent_id: &str) -> String {
    let mut out = String::with_capacity(agent_id.len());
    for ch in agent_id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let collapsed = out.replace("..", "-");
    let trimmed = collapsed.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "agent".to_owned()
    } else {
        trimmed.to_owned()
    }
}

impl WorktreeIsolator for GitWorktreeIsolator {
    fn prepare(&self, repo_cwd: &Path, agent_id: &str) -> BoxFuture<Result<IsolatedWorktree>> {
        let repo = repo_cwd.to_path_buf();
        let clean = sanitize_slug(agent_id);
        let slug = format!("task-{clean}");
        let branch = format!("hrn/task/{clean}");
        Box::pin(async move {
            let info = add_worktree_on_branch(&repo, &slug, &branch, Some("HEAD")).await?;
            Ok(IsolatedWorktree {
                path: info.path,
                branch,
                repo_cwd: repo,
            })
        })
    }

    fn finalize(
        &self,
        worktree: &IsolatedWorktree,
        req: IsolationFinalize,
    ) -> BoxFuture<Result<IsolationOutcome>> {
        let wt = worktree.clone();
        let fallback = self.fallback_artifacts.clone();
        Box::pin(async move { finalize_inner(&wt, req, fallback.as_deref()).await })
    }

    fn cleanup(&self, worktree: &IsolatedWorktree) -> BoxFuture<Result<()>> {
        let repo = worktree.repo_cwd.clone();
        let target = worktree.path.to_string_lossy().into_owned();
        Box::pin(async move { remove_worktree(&repo, &target, true).await })
    }
}

async fn finalize_inner(
    wt: &IsolatedWorktree,
    req: IsolationFinalize,
    fallback_artifacts: Option<&ArtifactStore>,
) -> Result<IsolationOutcome> {
    let mut outcome = IsolationOutcome::default();
    let violations = check_diff_scope(&wt.path, &req.allowed_paths, &req.denied_paths).await?;
    if !violations.is_empty() {
        outcome.notes.push(format!(
            "diff scope violation (changes outside allowed paths): {}",
            violations.join(", ")
        ));
    }
    let patch = capture_patch(&wt.path).await?;
    if patch.trim().is_empty() {
        return Ok(outcome);
    }

    let artifacts = req.artifacts.as_deref().or(fallback_artifacts);
    if let Some(store) = artifacts {
        let patch_file = store.write_patch(&req.root_session_id, &req.agent_id, &patch)?;
        outcome.patch_path = Some(patch_file);
    }

    if req.config.merge == "branch" {
        if req.child_succeeded && violations.is_empty() {
            match commit_worktree_branch(&wt.path, &format!("hrn task {}", req.agent_id)).await {
                Ok(_) => outcome.branch = Some(wt.branch.clone()),
                Err(err) => outcome
                    .notes
                    .push(format!("branch commit failed ({err}); patch kept")),
            }
        } else {
            outcome.notes.push(
                "skipped branch commit because child failed or violated diff scope; patch kept"
                    .to_owned(),
            );
        }
    }

    if req.config.apply {
        if !req.child_succeeded {
            outcome
                .notes
                .push("child failed; skipped patch apply (patch kept)".to_owned());
        } else if !violations.is_empty() {
            outcome
                .notes
                .push("skipped patch apply due to diff scope violation (patch kept)".to_owned());
        } else {
            match apply_patch(&wt.repo_cwd, &patch).await {
                Ok(()) => outcome.applied = true,
                Err(err) => {
                    let kept = outcome
                        .patch_path
                        .as_ref()
                        .map_or_else(|| "artifact store".to_owned(), |p| p.display().to_string());
                    outcome
                        .notes
                        .push(format!("patch apply failed ({err}); kept patch at {kept}"));
                }
            }
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_harness::TaskIsolationConfig;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("hrn-bin-iso-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(tag: &str) -> TempDir {
        let dir = TempDir::new(tag);
        git(&dir.0, &["init"]);
        git(&dir.0, &["config", "user.name", "tester"]);
        git(&dir.0, &["config", "user.email", "tester@local"]);
        git(&dir.0, &["config", "core.autocrlf", "false"]);
        git(&dir.0, &["config", "core.eol", "lf"]);
        std::fs::write(dir.0.join("a.txt"), "hello\n").expect("write");
        git(&dir.0, &["add", "a.txt"]);
        git(&dir.0, &["commit", "-m", "initial"]);
        dir
    }

    #[tokio::test]
    async fn isolator_captures_patch_applies_and_keeps_patch_on_conflict() {
        let repo = init_repo("apply");
        let data = TempDir::new("artifacts");
        let artifacts = Arc::new(ArtifactStore::new(data.0.clone()));
        let isolator = GitWorktreeIsolator::new(Some(Arc::clone(&artifacts)));

        let wt = isolator
            .prepare(&repo.0, "worker-1")
            .await
            .expect("prepare");
        assert!(wt.path.ends_with(Path::new(".harness/wt/task-worker-1")));
        assert_eq!(wt.branch, "hrn/task/worker-1");

        std::fs::write(wt.path.join("a.txt"), "hello world\n").expect("edit");
        let outcome = isolator
            .finalize(
                &wt,
                IsolationFinalize {
                    root_session_id: "root".into(),
                    agent_id: "worker-1".into(),
                    allowed_paths: vec![],
                    denied_paths: vec![],
                    config: TaskIsolationConfig {
                        enabled: true,
                        merge: "patch".into(),
                        apply: true,
                    },
                    child_succeeded: true,
                    artifacts: Some(Arc::clone(&artifacts)),
                },
            )
            .await
            .expect("finalize");
        assert!(outcome.applied);
        assert!(outcome.patch_path.as_ref().is_some_and(|p| p.is_file()));
        assert_eq!(
            std::fs::read_to_string(repo.0.join("a.txt")).unwrap(),
            "hello world\n"
        );

        // Conflicting parent edit keeps the patch file and records a failure note.
        let wt2 = isolator
            .prepare(&repo.0, "worker-2")
            .await
            .expect("prepare2");
        std::fs::write(wt2.path.join("a.txt"), "branch edit\n").expect("edit wt2");
        std::fs::write(repo.0.join("a.txt"), "conflicting parent\n").expect("edit parent");
        let conflict = isolator
            .finalize(
                &wt2,
                IsolationFinalize {
                    root_session_id: "root".into(),
                    agent_id: "worker-2".into(),
                    allowed_paths: vec![],
                    denied_paths: vec![],
                    config: TaskIsolationConfig {
                        enabled: true,
                        merge: "branch".into(),
                        apply: true,
                    },
                    child_succeeded: true,
                    artifacts: Some(Arc::clone(&artifacts)),
                },
            )
            .await
            .expect("finalize conflict");
        assert!(!conflict.applied);
        assert_eq!(conflict.branch.as_deref(), Some("hrn/task/worker-2"));
        assert!(conflict.patch_path.as_ref().is_some_and(|p| p.is_file()));
        assert!(
            conflict
                .notes
                .iter()
                .any(|n| n.contains("patch apply failed")),
            "{:?}",
            conflict.notes
        );

        isolator.cleanup(&wt).await.expect("cleanup 1");
        isolator.cleanup(&wt2).await.expect("cleanup 2");
    }
}
