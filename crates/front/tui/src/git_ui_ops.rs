//! Safe, zero-shell git operations backing `/git [revision]` and `hrn git [revision]`.
//!
//! Security invariants:
//! - Every path argument is passed strictly after `--` to prevent flag injection.
//! - Every revision argument is validated by [`validate_revision`] before invoking `git` (no
//!   leading `-`, no whitespace, no ASCII control characters or NUL bytes).
//! - Commit messages are supplied on stdin via `git commit -F -` (never interpolated through a
//!   shell).

use crate::diff;
use crate::wrap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Maximum bytes read from an untracked file when rendering a synthetic diff preview.
const MAX_UNTRACKED_PREVIEW_BYTES: usize = 512 * 1024;

/// One row in the `/git` status or revision file sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitFileEntry {
    /// Repository-relative path (forward slashes).
    pub path: String,
    /// Original path when renamed/copied.
    pub orig_path: Option<String>,
    /// Index (staged) status column (`' '`, `'M'`, `'A'`, `'D'`, `'R'`, `'C'`, `'?'`, etc.).
    pub index_status: char,
    /// Worktree (unstaged) status column (`' '`, `'M'`, `'D'`, `'?'`, etc.).
    pub worktree_status: char,
}

impl GitFileEntry {
    /// Whether this entry currently has staged changes in the index.
    #[must_use]
    pub fn is_staged(&self) -> bool {
        !matches!(self.index_status, ' ' | '?' | '!')
    }

    /// Whether this entry has unstaged worktree changes (including untracked files).
    #[must_use]
    pub fn is_unstaged(&self) -> bool {
        !matches!(self.worktree_status, ' ' | '!')
    }

    /// Whether this entry is an untracked worktree file (`??`).
    #[must_use]
    pub fn is_untracked(&self) -> bool {
        self.index_status == '?' && self.worktree_status == '?'
    }

    /// Two-character porcelain status badge (`M `, ` M`, `MM`, `A `, `??`).
    #[must_use]
    pub fn badge(&self) -> String {
        let mut s = String::with_capacity(2);
        s.push(self.index_status);
        s.push(self.worktree_status);
        s
    }
}

/// Validate a user-supplied git revision spec before passing it to `git`.
///
/// Rejects empty strings, leading `-` (flag injection such as `--exec=...` or `--output=...`),
/// whitespace, NUL bytes, and ASCII control characters.
///
/// # Errors
/// Returns a human-readable rejection reason if `rev` is unsafe or empty.
pub fn validate_revision(rev: &str) -> Result<(), String> {
    if rev.is_empty() {
        return Err("git revision must not be empty".to_owned());
    }
    if rev.starts_with('-') {
        return Err(format!(
            "rejected git revision `{rev}`: revisions must not start with `-`"
        ));
    }
    if rev
        .chars()
        .any(|c| c == '\0' || c.is_control() || c.is_whitespace())
    {
        return Err(
            "rejected git revision: whitespace and control characters are not allowed".to_owned(),
        );
    }
    Ok(())
}

/// Validate a repository-relative path before handing it to `git`.
fn validate_path_arg(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("git path must not be empty".to_owned());
    }
    if path.contains('\0') {
        return Err("git path contains NUL byte".to_owned());
    }
    Ok(())
}

/// Locate the git repository root containing `cwd`.
///
/// # Errors
/// Returns an error if `cwd` is not inside a git worktree or `git` cannot be executed.
pub fn find_repo_root(cwd: &Path) -> Result<PathBuf, String> {
    let out = run_git(cwd, &["rev-parse", "--show-toplevel"])?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err("not inside a git repository".to_owned());
    }
    Ok(PathBuf::from(trimmed))
}

/// Current branch name (or short HEAD hash / `"unborn"`) sanitized for display.
#[must_use]
pub(crate) fn branch_label(repo: &Path) -> String {
    if let Ok(branch) = run_git(repo, &["symbolic-ref", "--short", "HEAD"]) {
        let b = branch.trim();
        if !b.is_empty() {
            return wrap::sanitize(b).into_owned();
        }
    }
    if let Ok(sha) = run_git(repo, &["rev-parse", "--short", "HEAD"]) {
        let s = sha.trim();
        if !s.is_empty() {
            return format!("detached@{}", wrap::sanitize(s));
        }
    }
    "unborn".to_owned()
}

/// List working-tree and staged file statuses via `git status --porcelain=v1 -z`.
///
/// # Errors
/// Returns an error if `git status` fails.
pub(crate) fn list_status(repo: &Path) -> Result<Vec<GitFileEntry>, String> {
    let raw = run_git_bytes(repo, &["status", "--porcelain=v1", "-z", "-uall"])?;
    Ok(parse_porcelain_z(&raw))
}

/// Parse NUL-delimited `git status --porcelain=v1 -z` output.
pub(crate) fn parse_porcelain_z(raw: &[u8]) -> Vec<GitFileEntry> {
    let mut entries = Vec::new();
    let mut parts = raw.split(|b| *b == 0);
    while let Some(rec) = parts.next() {
        if rec.len() < 4 {
            continue;
        }
        let s = String::from_utf8_lossy(rec);
        let mut chars = s.chars();
        let Some(index_status) = chars.next() else {
            continue;
        };
        let Some(worktree_status) = chars.next() else {
            continue;
        };
        let Some(sep) = chars.next() else {
            continue;
        };
        if sep != ' ' {
            continue;
        }
        let path = chars.as_str().to_owned();
        let orig_path = if matches!(index_status, 'R' | 'C') || matches!(worktree_status, 'R' | 'C')
        {
            parts
                .next()
                .map(|p| String::from_utf8_lossy(p).into_owned())
        } else {
            None
        };
        entries.push(GitFileEntry {
            path,
            orig_path,
            index_status,
            worktree_status,
        });
    }
    entries
}

/// List files modified in a specific git revision (`git diff-tree --name-status -r -z --root`).
///
/// # Errors
/// Returns an error if `rev` fails validation or `git diff-tree` fails.
pub(crate) fn list_revision_files(repo: &Path, rev: &str) -> Result<Vec<GitFileEntry>, String> {
    validate_revision(rev)?;
    let raw = run_git_bytes(
        repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-z",
            "--root",
            rev,
            "--",
        ],
    )?;
    let mut entries = Vec::new();
    let mut parts = raw.split(|b| *b == 0);
    while let Some(status_bytes) = parts.next() {
        if status_bytes.is_empty() {
            continue;
        }
        let status = String::from_utf8_lossy(status_bytes);
        let code = status.chars().next().unwrap_or('M');
        let Some(path_bytes) = parts.next() else {
            break;
        };
        let first = String::from_utf8_lossy(path_bytes).into_owned();
        let (path, orig_path) = if matches!(code, 'R' | 'C') {
            match parts.next() {
                Some(new_bytes) => (String::from_utf8_lossy(new_bytes).into_owned(), Some(first)),
                None => (first, None),
            }
        } else {
            (first, None)
        };
        entries.push(GitFileEntry {
            path,
            orig_path,
            index_status: code,
            worktree_status: ' ',
        });
    }
    Ok(entries)
}

/// Stage `path` using `git add -- <path>`.
///
/// # Errors
/// Returns an error if `path` is invalid or `git add` fails.
pub(crate) fn stage_path(repo: &Path, path: &str) -> Result<(), String> {
    validate_path_arg(path)?;
    let _ = run_git(repo, &["add", "--", path])?;
    Ok(())
}

/// Unstage `path` using `git restore --staged -- <path>`, falling back to
/// `git rm --cached -- <path>` for unborn repositories without `HEAD`.
///
/// # Errors
/// Returns an error if `path` is invalid or both git commands fail.
pub(crate) fn unstage_path(repo: &Path, path: &str) -> Result<(), String> {
    validate_path_arg(path)?;
    if has_head(repo) {
        let _ = run_git(repo, &["restore", "--staged", "--", path])?;
    } else {
        let _ = run_git(repo, &["rm", "--cached", "--", path])?;
    }
    Ok(())
}

/// Return the unified diff currently staged in the index (`git diff --cached --no-color`).
///
/// # Errors
/// Returns an error if `git diff --cached` fails.
pub(crate) fn staged_diff(repo: &Path) -> Result<String, String> {
    run_git(repo, &["diff", "--cached", "--no-color"])
}

/// Compute unified diff text for `entry` (either in `revision` mode or live working tree).
#[must_use]
pub(crate) fn diff_for_entry(repo: &Path, entry: &GitFileEntry, revision: Option<&str>) -> String {
    if let Some(rev) = revision {
        if let Err(err) = validate_revision(rev) {
            return format!("invalid revision: {err}");
        }
        return run_git(
            repo,
            &["show", "--no-color", "--format=", rev, "--", &entry.path],
        )
        .unwrap_or_else(|e| format!("unable to load revision diff: {e}"));
    }
    if entry.is_untracked() {
        return untracked_file_diff(repo, &entry.path);
    }
    let staged = if entry.is_staged() {
        run_git(repo, &["diff", "--cached", "--no-color", "--", &entry.path]).unwrap_or_default()
    } else {
        String::new()
    };
    let unstaged = if entry.is_unstaged() {
        run_git(repo, &["diff", "--no-color", "--", &entry.path]).unwrap_or_default()
    } else {
        String::new()
    };
    match (staged.trim().is_empty(), unstaged.trim().is_empty()) {
        (false, false) => format!("{staged}\n{unstaged}"),
        (false, true) => staged,
        (true, false) => unstaged,
        (true, true) => String::new(),
    }
}

/// Commit staged changes using `git commit -F -` (stdin, no shell interpolation).
///
/// # Errors
/// Returns an error if `message` is blank or `git commit` exits non-zero.
pub(crate) fn commit_staged(repo: &Path, message: &str) -> Result<String, String> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return Err("commit message cannot be empty".to_owned());
    }
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["commit", "-F", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn git commit: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(trimmed.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .map_err(|e| format!("failed to write commit message: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait on git commit: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(if detail.is_empty() {
            "git commit failed".to_owned()
        } else {
            detail
        });
    }
    let sha = run_git(repo, &["rev-parse", "--short", "HEAD"])
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let subject = trimmed.lines().next().unwrap_or(trimmed);
    Ok(if sha.is_empty() {
        format!("committed: {subject}")
    } else {
        format!("committed {sha}: {subject}")
    })
}

fn untracked_file_diff(repo: &Path, rel: &str) -> String {
    let full = repo.join(rel);
    let Ok(meta) = fs::symlink_metadata(&full) else {
        return String::new();
    };
    if !meta.file_type().is_file() {
        return format!("--- /dev/null\n+++ b/{rel}\n(non-regular file)\n");
    }
    if meta.len() > MAX_UNTRACKED_PREVIEW_BYTES as u64 {
        return format!(
            "--- /dev/null\n+++ b/{rel}\n(untracked file too large to preview: {} bytes)\n",
            meta.len()
        );
    }
    let Ok(file) = fs::File::open(&full) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    if std::io::Read::take(file, (MAX_UNTRACKED_PREVIEW_BYTES as u64) + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return String::new();
    }
    if bytes.len() > MAX_UNTRACKED_PREVIEW_BYTES {
        return format!(
            "--- /dev/null\n+++ b/{rel}\n(untracked file too large to preview: >{MAX_UNTRACKED_PREVIEW_BYTES} bytes)\n"
        );
    }
    if bytes.contains(&0) {
        return format!("--- /dev/null\n+++ b/{rel}\n(binary untracked file)\n");
    }
    let text = String::from_utf8_lossy(&bytes);
    let body = diff::unified("", &text);
    format!("--- /dev/null\n+++ b/{rel}\n{body}")
}

fn has_head(repo: &Path) -> bool {
    Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn run_git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let bytes = run_git_bytes(repo, args)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn run_git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let msg = if stderr.is_empty() { stdout } else { stderr };
        return Err(if msg.is_empty() {
            format!("git {} failed", args.first().copied().unwrap_or(""))
        } else {
            msg
        });
    }
    Ok(out.stdout)
}
