//! Isolated git worktree execution and post-run diff-scope gate for `hrn-mod-agy`.
//!
//! Uses only `hrn-edge-proc` (`git` subprocesses) and `std` so `hrn-mod-agy` obeys the strict
//! architecture dependency fence in `xtask/src/graph.rs`.

use hrn_edge_proc::{RunSpec, run_streaming};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_GIT_OUTPUT: usize = 256 * 1024;
const MAX_PATCH_OUTPUT: usize = 4 * 1024 * 1024;

static WORKTREE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Result of finalizing an isolated `agy` worktree after the subprocess completes cleanly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktreeOutcome {
    /// Slash-normalized relative paths touched inside the worktree.
    pub(crate) changed_files: Vec<String>,
    /// Preserved worktree directory when `keep_worktree` was requested.
    pub(crate) worktree_path: Option<String>,
    /// Whether non-empty changes were applied back to the parent repository.
    pub(crate) applied: bool,
}

/// Temporary detached git worktree under `<repo_root>/.harness/wt/agy-<pid>-<seq>`.
pub(crate) struct TempWorktree {
    repo_root: PathBuf,
    path: PathBuf,
    subdir: PathBuf,
}

impl TempWorktree {
    /// Create a detached temporary worktree rooted at the git repo containing `cwd`.
    pub(crate) async fn create(cwd: &Path) -> Result<Self> {
        let root_raw = run_git(cwd, &["rev-parse".to_owned(), "--show-toplevel".to_owned()])
            .await
            .map_err(|err| {
                HrnError::Tool(format!(
                    "agy worktree isolation requires a git repository at `{}`: {err}",
                    cwd.display()
                ))
            })?;
        let repo_root = PathBuf::from(root_raw.trim());
        if is_onedrive_path(&repo_root) {
            return Err(HrnError::Config(format!(
                "refusing to create agy git worktree inside OneDrive directory `{}`",
                repo_root.display()
            )));
        }
        let subdir = relative_subdir(&repo_root, cwd);
        let seq = WORKTREE_SEQ.fetch_add(1, Ordering::Relaxed);
        let slug = format!("agy-{}-{seq}", std::process::id());
        let wt_dir = repo_root.join(".harness").join("wt").join(slug);
        if let Some(parent) = wt_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let args = vec![
            "-c".to_owned(),
            "core.longpaths=true".to_owned(),
            "worktree".to_owned(),
            "add".to_owned(),
            "--detach".to_owned(),
            wt_dir.to_string_lossy().into_owned(),
            "HEAD".to_owned(),
        ];
        run_git(&repo_root, &args).await?;
        Ok(Self {
            repo_root,
            path: wt_dir,
            subdir,
        })
    }

    /// Directory inside the worktree corresponding to the caller's original `cwd`.
    pub(crate) fn run_cwd(&self) -> PathBuf {
        if self.subdir.as_os_str().is_empty() {
            self.path.clone()
        } else {
            let joined = self.path.join(&self.subdir);
            if joined.is_dir() {
                joined
            } else {
                self.path.clone()
            }
        }
    }

    /// Best-effort forced removal of the temporary worktree.
    pub(crate) async fn remove(&self) {
        let _ = run_git(
            &self.repo_root,
            &[
                "worktree".to_owned(),
                "remove".to_owned(),
                "--force".to_owned(),
                self.path.to_string_lossy().into_owned(),
            ],
        )
        .await;
        if self.path.exists() {
            let _ = std::fs::remove_dir_all(&self.path);
            let _ = run_git(
                &self.repo_root,
                &["worktree".to_owned(), "prune".to_owned()],
            )
            .await;
        }
    }

    /// Verify diff scope (`allowed_paths` / `denied_paths`), apply clean changes back to the parent
    /// repository, and clean up (or roll back on violation).
    pub(crate) async fn finalize(
        &self,
        allowed_paths: &[String],
        denied_paths: &[String],
        keep_worktree: bool,
    ) -> Result<WorktreeOutcome> {
        let files = changed_files(&self.path).await?;
        let violations = find_scope_violations(&files, allowed_paths, denied_paths);
        if !violations.is_empty() {
            self.remove().await;
            return Err(HrnError::Tool(format!(
                "agy diff-scope violation — changes rolled back and worktree removed; violating path(s): {}",
                violations.join(", ")
            )));
        }
        if files.is_empty() {
            if !keep_worktree {
                self.remove().await;
            }
            return Ok(WorktreeOutcome {
                changed_files: files,
                worktree_path: keep_worktree.then(|| self.path.display().to_string()),
                applied: false,
            });
        }
        let patch = capture_patch(&self.path).await?;
        if let Err(err) = apply_patch(&self.repo_root, &patch).await {
            return Err(HrnError::Tool(format!(
                "agy changes passed diff-scope check ({}) but failed to apply cleanly to `{}` \
                 (worktree preserved at `{}`): {err}",
                files.join(", "),
                self.repo_root.display(),
                self.path.display()
            )));
        }
        if !keep_worktree {
            self.remove().await;
        }
        Ok(WorktreeOutcome {
            changed_files: files,
            worktree_path: keep_worktree.then(|| self.path.display().to_string()),
            applied: true,
        })
    }
}

struct TempFileGuard(PathBuf);

impl TempFileGuard {
    fn unique_path(tag: &str) -> Self {
        let n = WORKTREE_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hrn-agy-{tag}-{}-{n}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn is_onedrive_path(path: &Path) -> bool {
    let norm = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if norm.split('/').any(|seg| seg.contains("onedrive")) {
        return true;
    }
    for key in ["OneDrive", "OneDriveCommercial", "OneDriveConsumer"] {
        if let Ok(val) = std::env::var(key)
            && !val.trim().is_empty()
        {
            let env_norm = val.replace('\\', "/").to_ascii_lowercase();
            let env_trimmed = env_norm.trim_end_matches('/');
            if !env_trimmed.is_empty()
                && (norm == env_trimmed || norm.starts_with(&format!("{env_trimmed}/")))
            {
                return true;
            }
        }
    }
    false
}

fn relative_subdir(repo_root: &Path, cwd: &Path) -> PathBuf {
    let norm = |p: &Path| {
        let s = p
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('\\', "/");
        s.trim_end_matches('/').to_owned()
    };
    let root_s = norm(repo_root);
    let cwd_s = norm(cwd);
    if cfg!(windows) {
        let root_lo = root_s.to_ascii_lowercase();
        let cwd_lo = cwd_s.to_ascii_lowercase();
        let prefix = format!("{root_lo}/");
        if cwd_lo.starts_with(&prefix) {
            return cwd_s
                .get(prefix.len()..)
                .map_or_else(PathBuf::new, PathBuf::from);
        }
        return PathBuf::new();
    }
    let prefix = format!("{root_s}/");
    cwd_s
        .strip_prefix(&prefix)
        .map_or_else(PathBuf::new, PathBuf::from)
}

async fn changed_files(worktree_path: &Path) -> Result<Vec<String>> {
    let out = run_git(
        worktree_path,
        &[
            "status".to_owned(),
            "--porcelain".to_owned(),
            "-uall".to_owned(),
        ],
    )
    .await?;
    Ok(parse_porcelain_paths(&out))
}

pub(crate) fn parse_porcelain_paths(output: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in output.lines() {
        let Some(rest) = line.get(3..) else {
            continue;
        };
        let path_part = rest.rsplit_once(" -> ").map_or(rest, |(_, dst)| dst);
        let unquoted = path_part.trim().trim_matches('"');
        let norm = normalize_rel_path(unquoted);
        if !norm.is_empty() {
            files.push(norm);
        }
    }
    files.sort();
    files.dedup();
    files
}

async fn capture_patch(worktree_path: &Path) -> Result<String> {
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
    run_git_env(
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
    .await
}

async fn apply_patch(repo_root: &Path, patch: &str) -> Result<()> {
    if patch.trim().is_empty() {
        return Ok(());
    }
    let tmp_patch = TempFileGuard::unique_path("patch");
    std::fs::write(&tmp_patch.0, patch)?;
    let patch_arg = tmp_patch.0.to_string_lossy().into_owned();
    run_git(
        repo_root,
        &["apply".to_owned(), "--check".to_owned(), patch_arg.clone()],
    )
    .await?;
    run_git(repo_root, &["apply".to_owned(), patch_arg]).await?;
    Ok(())
}

/// Return all paths in `files` that violate `allowed_paths` or match `denied_paths`.
/// `denied_paths` always wins over `allowed_paths`.
pub(crate) fn find_scope_violations(
    files: &[String],
    allowed_paths: &[String],
    denied_paths: &[String],
) -> Vec<String> {
    files
        .iter()
        .filter(|path| {
            let norm = normalize_rel_path(path);
            if denied_paths.iter().any(|pat| path_matches(&norm, pat)) {
                return true;
            }
            if !allowed_paths.is_empty()
                && !allowed_paths.iter().any(|pat| path_matches(&norm, pat))
            {
                return true;
            }
            false
        })
        .cloned()
        .collect()
}

fn normalize_rel_path(raw: &str) -> String {
    let mut s = raw.trim().replace('\\', "/");
    while let Some(rest) = s.strip_prefix("./") {
        s = rest.to_owned();
    }
    s.trim_start_matches('/').trim_end_matches('/').to_owned()
}

pub(crate) fn path_matches(path: &str, pattern: &str) -> bool {
    let norm_path = normalize_rel_path(path);
    let norm_pat = normalize_rel_path(pattern);
    if norm_pat.is_empty() {
        return false;
    }
    let (p_str, pat_str) = if cfg!(windows) {
        (
            norm_path.to_ascii_lowercase(),
            norm_pat.to_ascii_lowercase(),
        )
    } else {
        (norm_path, norm_pat)
    };
    if !pat_str.contains(['*', '?']) {
        return p_str == pat_str
            || p_str
                .strip_prefix(&pat_str)
                .is_some_and(|rest| rest.starts_with('/'));
    }
    let file_parts: Vec<&str> = p_str.split('/').collect();
    let glob_parts: Vec<&str> = pat_str.split('/').collect();
    match_segments(&glob_parts, &file_parts)
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    match (pat.first(), path.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            let rest_pat = pat.get(1..).unwrap_or_default();
            if match_segments(rest_pat, path) {
                return true;
            }
            path.get(1..)
                .is_some_and(|rest_path| match_segments(pat, rest_path))
        }
        (Some(p), Some(s)) => {
            wildcard_segment_match(p, s)
                && match_segments(
                    pat.get(1..).unwrap_or_default(),
                    path.get(1..).unwrap_or_default(),
                )
        }
        _ => false,
    }
}

fn wildcard_segment_match(pat: &str, text: &str) -> bool {
    let pattern_chars: Vec<char> = pat.chars().collect();
    let text_chars: Vec<char> = text.chars().collect();
    let (mut pat_idx, mut text_idx) = (0usize, 0usize);
    let (mut star_pat, mut star_text) = (None, 0usize);
    while text_idx < text_chars.len() {
        let pat_ch = pattern_chars.get(pat_idx).copied();
        let text_ch = text_chars.get(text_idx).copied();
        if pat_ch == Some('?') || (pat_ch.is_some() && pat_ch == text_ch) {
            pat_idx += 1;
            text_idx += 1;
        } else if pat_ch == Some('*') {
            star_pat = Some(pat_idx);
            star_text = text_idx;
            pat_idx += 1;
        } else if let Some(saved_pat) = star_pat {
            pat_idx = saved_pat + 1;
            star_text += 1;
            text_idx = star_text;
        } else {
            return false;
        }
    }
    while pattern_chars.get(pat_idx).copied() == Some('*') {
        pat_idx += 1;
    }
    pat_idx == pattern_chars.len()
}

async fn run_git(cwd: &Path, args: &[String]) -> Result<String> {
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
        return Err(HrnError::Tool(format!("git {} timed out", args.join(" "))));
    }
    if out.exit_code != Some(0) {
        let detail = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        return Err(HrnError::Tool(format!(
            "git {} failed: {detail}",
            args.join(" ")
        )));
    }
    Ok(out.stdout)
}

#[cfg(test)]
#[path = "worktree_tests.rs"]
mod tests;
