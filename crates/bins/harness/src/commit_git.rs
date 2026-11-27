//! Safe, zero-shell git subprocess helpers backing [`super`] (`/commit` and `hrn commit`).

use crate::term::sanitize_cli_inline;
use hrn_kernel::{HrnError, Result};
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

/// Maximum untracked file size included verbatim in diff previews/prompts.
pub(super) const MAX_UNTRACKED_BYTES: usize = 256 * 1024;

/// Refuse attributed commits if `.git/index` already has pre-existing staged entries (works on
/// both normal and unborn repositories without `HEAD`) so unrelated staged edits never leak.
pub(super) fn ensure_no_staged_changes(repo_root: &Path) -> Result {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(["status", "--porcelain=v1", "-z"])
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(HrnError::Tool(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    let mut staged = Vec::new();
    let mut parts = out.stdout.split(|b| *b == 0);
    while let Some(rec) = parts.next() {
        if rec.len() < 4 || rec.get(2) != Some(&b' ') {
            continue;
        }
        let x = rec.first().copied().unwrap_or(b' ');
        let y = rec.get(1).copied().unwrap_or(b' ');
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            let _ = parts.next();
        }
        if !matches!(x, b' ' | b'?' | b'!') {
            let raw_path = String::from_utf8_lossy(rec.get(3..).unwrap_or_default());
            staged.push(sanitize_cli_inline(&raw_path));
        }
    }
    if !staged.is_empty() {
        let preview = staged
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let extra = if staged.len() > 5 { ", ..." } else { "" };
        return Err(HrnError::Tool(format!(
            "refusing attributed commit: git index already has {} staged file(s) ({preview}{extra}); commit or unstage them first (e.g. via `/git` or `git restore --staged`)",
            staged.len()
        )));
    }
    Ok(())
}

pub(super) fn list_untracked(repo_root: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(HrnError::Tool(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect())
}

pub(super) fn untracked_preview_diff(repo_root: &Path, rel: &str) -> String {
    let path = repo_root.join(rel);
    let Ok(meta) = fs::symlink_metadata(&path) else {
        return String::new();
    };
    if !meta.file_type().is_file() {
        return String::new();
    }
    let placeholder = || format!("--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,0 @@\n");
    if meta.len() > MAX_UNTRACKED_BYTES as u64 {
        return placeholder();
    }
    let Ok(file) = File::open(&path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    if file
        .take((MAX_UNTRACKED_BYTES as u64) + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return String::new();
    }
    if bytes.len() > MAX_UNTRACKED_BYTES || bytes.contains(&0) {
        return placeholder();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut out = format!("--- /dev/null\n+++ b/{rel}\n");
    let count = text.lines().count();
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("@@ -0,0 +1,{count} @@\n"));
    for line in text.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub(super) fn apply_patch_cached(repo_root: &Path, patch: &str) -> Result {
    let mut child = Command::new("git")
        .current_dir(repo_root)
        .args(["apply", "--cached", "--whitespace=nowarn", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(patch.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(HrnError::Tool(format!(
            "git apply --cached failed: {stderr}"
        )));
    }
    Ok(())
}

pub(super) fn hrn_tui_commit_staged(repo_root: &Path, message: &str) -> Result<String> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return Err(HrnError::Tool("commit message cannot be empty".to_owned()));
    }
    let mut child = Command::new("git")
        .current_dir(repo_root)
        .args(["commit", "-F", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(trimmed.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let msg = if stderr.is_empty() { stdout } else { stderr };
        return Err(HrnError::Tool(format!("git commit failed: {msg}")));
    }
    let sha = run_git(repo_root, &["rev-parse", "--short", "HEAD"])
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let subject = trimmed.lines().next().unwrap_or(trimmed);
    Ok(if sha.is_empty() {
        format!("committed: {subject}")
    } else {
        format!("committed {sha}: {subject}")
    })
}

pub(super) fn has_head(repo_root: &Path) -> bool {
    Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub(super) fn run_git(repo_root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(HrnError::Tool(format!(
            "git {} failed: {stderr}",
            args.first().copied().unwrap_or("")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
