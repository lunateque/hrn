//! Git patch capture (tracked + untracked via a temporary `GIT_INDEX_FILE`), branch commits,
//! and atomic patch application (`git apply --check` then `git apply`) for isolated worktrees.

use super::changed_files;
use hrn_edge_proc::{RunSpec, run_streaming};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_mins(1);
const MAX_GIT_OUTPUT: usize = 256 * 1024;
const MAX_PATCH_OUTPUT: usize = 4 * 1024 * 1024;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Removes a temporary file on drop.
struct TempFileGuard(PathBuf);

impl TempFileGuard {
    fn unique_path(tag: &str) -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hrn-wt-{tag}-{}-{n}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Capture all tracked modifications, deletions, renames, and untracked files in `worktree_path`
/// as one `git apply`-compatible unified diff without mutating the worktree's real index.
pub async fn capture_patch(worktree_path: &Path) -> Result<String> {
    if changed_files(worktree_path).await?.is_empty() {
        return Ok(String::new());
    }
    let tmp_index = TempFileGuard::unique_path("index");
    let mut env = BTreeMap::new();
    env.insert(
        "GIT_INDEX_FILE".to_owned(),
        tmp_index.0.to_string_lossy().into_owned(),
    );
    run_git_env(
        worktree_path,
        &["read-tree".to_owned(), "HEAD".to_owned()],
        &env,
        MAX_GIT_OUTPUT,
    )
    .await?;
    run_git_env(
        worktree_path,
        &["add".to_owned(), "-A".to_owned()],
        &env,
        MAX_GIT_OUTPUT,
    )
    .await?;
    let diff = run_git_env(
        worktree_path,
        &[
            "diff".to_owned(),
            "--cached".to_owned(),
            "--binary".to_owned(),
            "HEAD".to_owned(),
        ],
        &env,
        MAX_PATCH_OUTPUT,
    )
    .await?;
    Ok(diff)
}

/// Stage all tracked and untracked changes in `worktree_path` and create a commit on its checked-out
/// branch, returning the resulting commit SHA. If the worktree is clean, returns current `HEAD`.
pub async fn commit_worktree_branch(worktree_path: &Path, message: &str) -> Result<String> {
    run_git(worktree_path, &["add".to_owned(), "-A".to_owned()]).await?;
    if changed_files(worktree_path).await?.is_empty() {
        let head = run_git(worktree_path, &["rev-parse".to_owned(), "HEAD".to_owned()]).await?;
        return Ok(head.trim().to_owned());
    }
    let mut args = Vec::new();
    if run_git(
        worktree_path,
        &[
            "config".to_owned(),
            "--get".to_owned(),
            "user.name".to_owned(),
        ],
    )
    .await
    .map_or(true, |s| s.trim().is_empty())
    {
        args.push("-c".to_owned());
        args.push("user.name=hrn".to_owned());
    }
    if run_git(
        worktree_path,
        &[
            "config".to_owned(),
            "--get".to_owned(),
            "user.email".to_owned(),
        ],
    )
    .await
    .map_or(true, |s| s.trim().is_empty())
    {
        args.push("-c".to_owned());
        args.push("user.email=hrn@local".to_owned());
    }
    args.push("commit".to_owned());
    args.push("-m".to_owned());
    args.push(message.to_owned());
    run_git(worktree_path, &args).await?;
    let sha = run_git(worktree_path, &["rev-parse".to_owned(), "HEAD".to_owned()]).await?;
    Ok(sha.trim().to_owned())
}

/// Atomically verify (`git apply --check`) and apply unified diff `patch` to `repo_cwd`.
/// Leaves the working tree untouched if `--check` fails.
pub async fn apply_patch(repo_cwd: &Path, patch: &str) -> Result<()> {
    if patch.trim().is_empty() {
        return Ok(());
    }
    let tmp_patch = TempFileGuard::unique_path("patch");
    std::fs::write(&tmp_patch.0, patch)?;
    let patch_arg = tmp_patch.0.to_string_lossy().into_owned();
    run_git(
        repo_cwd,
        &["apply".to_owned(), "--check".to_owned(), patch_arg.clone()],
    )
    .await?;
    run_git(repo_cwd, &["apply".to_owned(), patch_arg]).await?;
    Ok(())
}

pub(super) async fn run_git(cwd: &Path, args: &[String]) -> Result<String> {
    run_git_env(cwd, args, &BTreeMap::new(), MAX_GIT_OUTPUT).await
}

async fn run_git_env(
    cwd: &Path,
    args: &[String],
    extra_env: &BTreeMap<String, String>,
    max_output_bytes: usize,
) -> Result<String> {
    let mut env = BTreeMap::new();
    env.insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());
    env.insert("GIT_PAGER".to_owned(), "cat".to_owned());
    env.extend(extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let spec = RunSpec {
        program: "git".to_owned(),
        args: args.to_vec(),
        cwd: cwd.to_path_buf(),
        env,
        timeout: GIT_TIMEOUT,
        max_output_bytes,
    };
    let out = run_streaming(&spec, &CancelFlag::new(), |_| {}).await?;
    if out.timed_out {
        return Err(HrnError::Tool("git command timed out".to_owned()));
    }
    if out.exit_code != Some(0) {
        let err = out.stderr.trim();
        let detail = if err.is_empty() {
            out.stdout.trim()
        } else {
            err
        };
        return Err(HrnError::Tool(format!(
            "git {} failed (exit {:?}): {detail}",
            args.join(" "),
            out.exit_code
        )));
    }
    if out.truncated {
        return Err(HrnError::Tool(format!(
            "git {} output exceeded {} bytes cap",
            args.join(" "),
            max_output_bytes
        )));
    }
    Ok(out.stdout)
}
