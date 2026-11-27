//! `/wt [branch]` (alias `/worktree`): safe branch/slug validation, `add_worktree_on_branch`,
//! carrying tracked (`git diff HEAD --binary`) and untracked non-ignored files into the new
//! worktree, and capturing the previous checkout state.

use hrn_edge_ipc::add_worktree_on_branch;
use hrn_kernel::{HrnError, Result};
use hrn_tui::WorktreeOutcome;
use std::path::{Component, Path};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Create a worktree from `old_cwd`, carry uncommitted tracked and untracked changes into it,
/// and return the relocation summary.
pub(super) async fn relocate_to_worktree(
    old_cwd: &Path,
    requested_branch: Option<&str>,
    session_id: Option<&str>,
) -> Result<WorktreeOutcome> {
    let (slug, branch) = resolve_slug_and_branch(requested_branch, session_id)?;
    let previous_checkout = describe_checkout(old_cwd).await;
    let info = add_worktree_on_branch(old_cwd, &slug, &branch, Some("HEAD")).await?;
    let mut warnings = Vec::new();
    let carried_tracked = carry_tracked_diff(old_cwd, &info.path).await?;
    let carried_untracked = carry_untracked_files(old_cwd, &info.path, &mut warnings).await?;
    Ok(WorktreeOutcome {
        new_cwd: info.path,
        branch: info.branch.unwrap_or(branch),
        carried_tracked,
        carried_untracked,
        previous_checkout,
        warnings,
    })
}

/// Validate an explicit branch name or synthesize a safe default `(slug, branch)`.
pub(super) fn resolve_slug_and_branch(
    requested: Option<&str>,
    session_id: Option<&str>,
) -> Result<(String, String)> {
    let Some(raw) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let suffix = session_id
            .map(|id| {
                id.chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(8)
                    .collect::<String>()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| secs.to_string());
        let slug = format!("wt-{suffix}-{secs}");
        let branch = format!("wt/{slug}");
        return Ok((slug, branch));
    };
    validate_requested_branch(raw)?;
    let slug = slug_from_branch(raw)?;
    Ok((slug, raw.to_owned()))
}

fn validate_requested_branch(branch: &str) -> Result {
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains('\0')
        || !branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(HrnError::Config(format!(
            "invalid worktree branch name `{branch}`: use ASCII letters, digits, `.`, `_`, `-`, `/` without leading `-` or `..`"
        )));
    }
    Ok(())
}

fn slug_from_branch(branch: &str) -> Result<String> {
    let slug: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = slug.trim_matches(['-', '.']);
    if trimmed.is_empty() {
        return Err(HrnError::Config(format!(
            "cannot derive worktree directory slug from branch `{branch}`"
        )));
    }
    Ok(trimmed.to_owned())
}

async fn describe_checkout(cwd: &Path) -> String {
    let branch_out = git_output(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let label = match branch_out {
        Ok(b) if b.trim() == "HEAD" => git_output(cwd, &["rev-parse", "--short", "HEAD"])
            .await
            .map_or_else(
                |_| "detached".to_owned(),
                |s| format!("detached@{}", s.trim()),
            ),
        Ok(b) if !b.trim().is_empty() => b.trim().to_owned(),
        _ => "unknown".to_owned(),
    };
    let dirty_count = git_output(cwd, &["status", "--porcelain"])
        .await
        .map_or(0, |s| s.lines().filter(|l| !l.trim().is_empty()).count());
    let state = if dirty_count == 0 {
        "clean".to_owned()
    } else {
        format!("{dirty_count} uncommitted path(s) remain in original checkout")
    };
    format!("{} ({label}; {state})", cwd.display())
}

async fn carry_tracked_diff(old_cwd: &Path, wt_path: &Path) -> Result<usize> {
    let names = git_output(old_cwd, &["diff", "HEAD", "--name-only"]).await?;
    let count = names.lines().filter(|l| !l.trim().is_empty()).count();
    if count == 0 {
        return Ok(0);
    }
    let diff = git_bytes(old_cwd, &["diff", "HEAD", "--binary"]).await?;
    if diff.is_empty() {
        return Ok(0);
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(wt_path)
        .args(["apply", "--whitespace=nowarn"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| HrnError::Io(format!("spawn `git apply`: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&diff)
            .await
            .map_err(|e| HrnError::Io(format!("write patch to `git apply`: {e}")))?;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| HrnError::Io(format!("wait `git apply`: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(HrnError::Io(format!(
            "carrying tracked diff into worktree failed: {stderr}"
        )));
    }
    Ok(count)
}

async fn carry_untracked_files(
    old_cwd: &Path,
    wt_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<usize> {
    let raw = git_bytes(
        old_cwd,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await?;
    if raw.is_empty() {
        return Ok(0);
    }
    let canonical_root = std::fs::canonicalize(old_cwd).unwrap_or_else(|_| old_cwd.to_path_buf());
    let mut copied = 0usize;
    for entry in raw.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Ok(rel_str) = std::str::from_utf8(entry) else {
            warnings.push("skipped untracked file with non-UTF-8 path".to_owned());
            continue;
        };
        if !is_safe_rel_path(rel_str) {
            continue;
        }
        let rel = Path::new(rel_str);
        let src = old_cwd.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&src) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            let inside =
                std::fs::canonicalize(&src).is_ok_and(|target| target.starts_with(&canonical_root));
            if !inside {
                warnings.push(format!("skipped escaping symlink `{rel_str}`"));
                continue;
            }
        }
        if meta.is_dir() {
            continue;
        }
        let dst = wt_path.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
        copied += 1;
    }
    Ok(copied)
}

fn is_safe_rel_path(rel_str: &str) -> bool {
    if rel_str.is_empty()
        || rel_str == ".git"
        || rel_str.starts_with(".git/")
        || rel_str.starts_with(".git\\")
        || rel_str == ".harness/wt"
        || rel_str.starts_with(".harness/wt/")
        || rel_str.starts_with(".harness\\wt\\")
    {
        return false;
    }
    Path::new(rel_str).components().all(|c| {
        !matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

async fn git_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| HrnError::Io(format!("run `git {}`: {e}", args.join(" "))))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(HrnError::Io(format!(
            "`git {}` failed: {stderr}",
            args.join(" ")
        )));
    }
    Ok(out.stdout)
}

async fn git_output(cwd: &Path, args: &[&str]) -> Result<String> {
    let bytes = git_bytes(cwd, args).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_edge_store::SqliteStore;
    use hrn_edge_tools::StdWorkspaceFs;
    use hrn_harness::{AgentRuntime, HarnessConfig, ToolRegistry};
    use hrn_kernel::{
        CancelFlag, ChunkSink, LlmProvider, LlmRequest, LlmResponse, ModelInfo, PermissionMode,
        ScopedFuture, SessionStore, ThinkingLevel,
    };
    use std::sync::Arc;

    struct DummyLlm;
    impl LlmProvider for DummyLlm {
        fn stream_chat<'a>(
            &'a self,
            _req: &'a LlmRequest,
            _cancel: &'a CancelFlag,
            _sink: &'a dyn ChunkSink,
        ) -> ScopedFuture<'a, Result<LlmResponse>> {
            Box::pin(async { Err(HrnError::Internal("unused".into())) })
        }
        fn models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
    }

    fn run_git_sync(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git command should run");
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn wt_relocates_and_carries_changes() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("hrn-wt-11c-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        run_git_sync(&dir, &["init", "-b", "main"]);
        run_git_sync(&dir, &["config", "user.name", "Tester"]);
        run_git_sync(&dir, &["config", "user.email", "tester@example.com"]);
        run_git_sync(&dir, &["config", "core.autocrlf", "false"]);
        run_git_sync(&dir, &["config", "core.eol", "lf"]);
        std::fs::write(dir.join("tracked.txt"), "v1\n").unwrap();
        run_git_sync(&dir, &["add", "tracked.txt"]);
        run_git_sync(&dir, &["commit", "-m", "initial"]);

        // Make tracked modification + untracked nested file.
        std::fs::write(dir.join("tracked.txt"), "v2\n").unwrap();
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes").join("todo.txt"), "carry me").unwrap();

        // Reject unsafe branch names (flag injection and traversal).
        assert!(resolve_slug_and_branch(Some("-evil"), None).is_err());
        assert!(resolve_slug_and_branch(Some("../escape"), None).is_err());

        let store: Arc<dyn SessionStore> = Arc::new(SqliteStore::open_in_memory().unwrap());
        let rt = AgentRuntime::new(
            Arc::new(DummyLlm),
            Arc::clone(&store),
            ToolRegistry::new(),
            Arc::new(StdWorkspaceFs),
            HarnessConfig::default(),
        );
        let session = rt
            .create_session(
                "wt test",
                &dir,
                Some("dummy".into()),
                ThinkingLevel::Off,
                PermissionMode::Default,
            )
            .unwrap();

        let outcome = relocate_to_worktree(&dir, Some("feat/carry-test"), Some(&session.id))
            .await
            .expect("worktree relocation should succeed");
        let updated = rt
            .relocate_session(&session.id, &outcome.new_cwd, Some(outcome.branch.clone()))
            .expect("session metadata should relocate");

        assert_eq!(outcome.branch, "feat/carry-test");
        assert!(outcome.new_cwd.exists());
        assert_eq!(outcome.carried_tracked, 1);
        assert_eq!(outcome.carried_untracked, 1);
        assert!(outcome.previous_checkout.contains("uncommitted"));
        assert_eq!(
            std::fs::read_to_string(outcome.new_cwd.join("tracked.txt")).unwrap(),
            "v2\n"
        );
        assert_eq!(
            std::fs::read_to_string(outcome.new_cwd.join("notes").join("todo.txt")).unwrap(),
            "carry me"
        );
        assert_eq!(updated.cwd, outcome.new_cwd.to_string_lossy());
        assert_eq!(updated.branch.as_deref(), Some("feat/carry-test"));
        let stored = store.get_session(&session.id).unwrap().unwrap();
        assert_eq!(stored.cwd, outcome.new_cwd.to_string_lossy());
        assert_eq!(stored.branch.as_deref(), Some("feat/carry-test"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
