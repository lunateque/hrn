//! Git worktree lifecycle (`<repo>/.harness/wt/<slug>`), patch capture/apply, branch commit,
//! and post-run diff-scope verification.
//!
//! Enforces Windows safety defaults: refuses OneDrive-synced directories and passes
//! `-c core.longpaths=true` when adding worktrees.

#[path = "worktree_patch.rs"]
mod patch;

use patch::run_git;
pub use patch::{apply_patch, capture_patch, commit_worktree_branch};

use hrn_kernel::{HrnError, Result};
use std::path::{Path, PathBuf};

/// Metadata for one git worktree returned by `git worktree list --porcelain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Absolute worktree directory path.
    pub path: PathBuf,
    /// Checked-out branch name (stripped of `refs/heads/`), if attached.
    pub branch: Option<String>,
    /// Current `HEAD` commit hash, if available.
    pub head: Option<String>,
    /// Whether this entry is the bare repository root.
    pub bare: bool,
}

/// Returns `true` if `path` appears to live inside a `OneDrive`-synced directory.
///
/// Git worktrees inside `OneDrive` corrupt `.git` lockfiles and trigger sync storms.
pub fn is_onedrive_path(path: &Path) -> bool {
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

/// Create a new git worktree at `<repo_root>/.harness/wt/<slug>` on branch `agent/<slug>`.
pub async fn add_worktree(
    repo_cwd: &Path,
    slug: &str,
    base_ref: Option<&str>,
) -> Result<WorktreeInfo> {
    let branch = format!("agent/{slug}");
    add_worktree_on_branch(repo_cwd, slug, &branch, base_ref).await
}

/// Create a new git worktree at `<repo_root>/.harness/wt/<slug>` checked out on `branch_name`
/// (e.g. `hrn/task/<id>`) starting from optional `base_ref` (such as `Some("HEAD")`).
pub async fn add_worktree_on_branch(
    repo_cwd: &Path,
    slug: &str,
    branch_name: &str,
    base_ref: Option<&str>,
) -> Result<WorktreeInfo> {
    validate_slug(slug)?;
    validate_branch_name(branch_name)?;
    let root = repo_root(repo_cwd).await?;
    if is_onedrive_path(&root) {
        return Err(HrnError::Config(format!(
            "refusing to create git worktree inside OneDrive directory `{}` (move the repository outside OneDrive)",
            root.display()
        )));
    }
    let wt_dir = root.join(".harness").join("wt").join(slug);
    if wt_dir.exists() {
        return Err(HrnError::Config(format!(
            "worktree directory already exists: {}",
            wt_dir.display()
        )));
    }
    if let Some(parent) = wt_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut args = vec![
        "-c".to_owned(),
        "core.longpaths=true".to_owned(),
        "worktree".to_owned(),
        "add".to_owned(),
        "-b".to_owned(),
        branch_name.to_owned(),
        wt_dir.to_string_lossy().into_owned(),
    ];
    if let Some(base) = base_ref.map(str::trim).filter(|s| !s.is_empty()) {
        args.push(base.to_owned());
    }
    run_git(&root, &args).await?;
    let head = run_git(&wt_dir, &["rev-parse".to_owned(), "HEAD".to_owned()])
        .await
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    Ok(WorktreeInfo {
        path: wt_dir,
        branch: Some(branch_name.to_owned()),
        head,
        bare: false,
    })
}

/// List all worktrees of the git repository containing `repo_cwd`.
pub async fn list_worktrees(repo_cwd: &Path) -> Result<Vec<WorktreeInfo>> {
    let root = repo_root(repo_cwd).await?;
    let out = run_git(
        &root,
        &[
            "worktree".to_owned(),
            "list".to_owned(),
            "--porcelain".to_owned(),
        ],
    )
    .await?;
    Ok(parse_worktree_porcelain(&out))
}

/// Remove a worktree by `<slug>` (under `<repo_root>/.harness/wt/<slug>`) or explicit path.
pub async fn remove_worktree(repo_cwd: &Path, slug_or_path: &str, force: bool) -> Result<()> {
    let root = repo_root(repo_cwd).await?;
    let candidate = PathBuf::from(slug_or_path);
    let target = if candidate.is_absolute() || slug_or_path.contains(['/', '\\']) {
        candidate
    } else {
        validate_slug(slug_or_path)?;
        root.join(".harness").join("wt").join(slug_or_path)
    };
    let mut args = vec!["worktree".to_owned(), "remove".to_owned()];
    if force {
        args.push("--force".to_owned());
    }
    args.push(target.to_string_lossy().into_owned());
    run_git(&root, &args).await?;
    Ok(())
}

/// Return slash-normalized relative paths modified, added, or untracked in `worktree_path`.
pub async fn changed_files(worktree_path: &Path) -> Result<Vec<String>> {
    let out = run_git(
        worktree_path,
        &[
            "status".to_owned(),
            "--porcelain".to_owned(),
            "-uall".to_owned(),
        ],
    )
    .await?;
    Ok(parse_status_porcelain(&out))
}

/// Verify every changed file in `worktree_path` against `allowed_paths` and `denied_paths`.
///
/// Returns the list of violating relative paths (empty means all changes are within scope).
pub async fn check_diff_scope(
    worktree_path: &Path,
    allowed_paths: &[String],
    denied_paths: &[String],
) -> Result<Vec<String>> {
    let files = changed_files(worktree_path).await?;
    let mut violations = Vec::new();
    for file in files {
        let denied = denied_paths.iter().any(|p| path_matches(&file, p));
        let allowed =
            allowed_paths.is_empty() || allowed_paths.iter().any(|p| path_matches(&file, p));
        if denied || !allowed {
            violations.push(file);
        }
    }
    Ok(violations)
}

async fn repo_root(cwd: &Path) -> Result<PathBuf> {
    let out = run_git(cwd, &["rev-parse".to_owned(), "--show-toplevel".to_owned()]).await?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err(HrnError::Config(format!(
            "`{}` is not inside a git repository",
            cwd.display()
        )));
    }
    Ok(PathBuf::from(trimmed))
}

fn validate_slug(slug: &str) -> Result<()> {
    let trimmed = slug.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.contains("..")
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(HrnError::Config(format!(
            "invalid worktree slug `{slug}` (use ASCII letters, digits, `-`, `_`, `.`)"
        )));
    }
    Ok(())
}

fn validate_branch_name(branch: &str) -> Result<()> {
    let trimmed = branch.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.starts_with('/')
        || trimmed.ends_with('/')
        || trimmed.ends_with('.')
        || trimmed.contains("..")
        || trimmed.contains("//")
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        return Err(HrnError::Config(format!(
            "invalid git branch name `{branch}`"
        )));
    }
    Ok(())
}

fn parse_worktree_porcelain(raw: &str) -> Vec<WorktreeInfo> {
    let mut out = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(info) = current.take() {
                out.push(info);
            }
            continue;
        }
        if let Some(path) = trimmed.strip_prefix("worktree ") {
            if let Some(info) = current.take() {
                out.push(info);
            }
            current = Some(WorktreeInfo {
                path: PathBuf::from(path.trim()),
                branch: None,
                head: None,
                bare: false,
            });
        } else if let Some(info) = current.as_mut() {
            if let Some(head) = trimmed.strip_prefix("HEAD ") {
                info.head = Some(head.trim().to_owned());
            } else if let Some(branch) = trimmed.strip_prefix("branch ") {
                let clean = branch
                    .trim()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch.trim());
                info.branch = Some(clean.to_owned());
            } else if trimmed == "bare" {
                info.bare = true;
            }
        }
    }
    if let Some(info) = current {
        out.push(info);
    }
    out
}

fn parse_status_porcelain(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.len() < 4 {
            continue;
        }
        let path_part = line.get(3..).unwrap_or("").trim();
        let target = path_part.rsplit(" -> ").next().unwrap_or(path_part);
        let unquoted = target.trim_matches('"').replace('\\', "/");
        if !unquoted.is_empty() {
            out.push(unquoted);
        }
    }
    out
}

fn path_matches(rel_path: &str, pattern: &str) -> bool {
    let p_norm = rel_path.replace('\\', "/");
    let p_trim = p_norm.trim_start_matches("./").trim_matches('/');
    let g_norm = pattern.replace('\\', "/");
    let g_trim = g_norm.trim().trim_start_matches("./").trim_matches('/');
    if g_trim.is_empty() {
        return false;
    }
    let p_segs: Vec<&str> = p_trim.split('/').filter(|s| !s.is_empty()).collect();
    let g_segs: Vec<&str> = g_trim.split('/').filter(|s| !s.is_empty()).collect();
    if match_segs(&p_segs, &g_segs) {
        return true;
    }
    if !g_trim.contains(['*', '?']) && p_segs.len() > g_segs.len() {
        return g_segs.iter().zip(p_segs.iter()).all(|(g, s)| eq_seg(g, s));
    }
    false
}

fn eq_seg(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn match_segs(path: &[&str], glob: &[&str]) -> bool {
    match (glob.first(), path.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            let rest_glob = glob.get(1..).unwrap_or_default();
            if match_segs(path, rest_glob) {
                return true;
            }
            path.get(1..).is_some_and(|rest| match_segs(rest, glob))
        }
        (Some(g), Some(s)) => {
            glob_seg(s, g)
                && match_segs(
                    path.get(1..).unwrap_or_default(),
                    glob.get(1..).unwrap_or_default(),
                )
        }
        _ => false,
    }
}

fn glob_seg(text: &str, pat: &str) -> bool {
    let norm = |s: &str| -> Vec<char> {
        if cfg!(windows) {
            s.to_ascii_lowercase().chars().collect()
        } else {
            s.chars().collect()
        }
    };
    let (t, p) = (norm(text), norm(pat));
    let (mut ti, mut pi, mut sp, mut st) = (0usize, 0usize, None, 0usize);
    while ti < t.len() {
        let (tc, pc) = (t.get(ti).copied(), p.get(pi).copied());
        if pc == Some('*') {
            sp = Some(pi);
            st = ti;
            pi += 1;
        } else if pc == Some('?') || (pc.is_some() && pc == tc) {
            ti += 1;
            pi += 1;
        } else if let Some(star) = sp {
            pi = star + 1;
            st += 1;
            ti = st;
        } else {
            return false;
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
#[path = "worktree_tests.rs"]
mod tests;
