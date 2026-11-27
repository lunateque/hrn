//! `pr_checkout` and `pr_push`: pull-request branches in git worktrees.
//!
//! A checkout lives at `<repo>/.harness/wt/pr-<N>` on branch `pr-<N>`, whose `remote` / `merge`
//! config points at the PR's head branch; `pr_push` reads that config back.

use crate::gh::{Gh, git, reason};
use crate::urls::repo_flag;
use crate::{
    args,
    render::{field, title},
};
use hrn_edge_ipc::{is_onedrive_path, list_worktrees};
use hrn_kernel::serde_json::Value;
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const PR_FIELDS: &str = "number,title,url,headRefName,headRefOid,headRepository,\
    headRepositoryOwner,isCrossRepository";

/// `host/owner/name` of a git remote URL (https, ssh, scp-like), lower-cased, without `.git`.
fn normalize_remote(url: &str) -> String {
    let url = url.trim().to_lowercase();
    let url = ["https://", "http://", "ssh://", "git://"]
        .iter()
        .find_map(|p| url.strip_prefix(p))
        .unwrap_or(&url);
    let url = url.split_once('@').map_or(url, |(_, rest)| rest);
    let url = url.replacen(':', "/", 1);
    url.trim_end_matches('/')
        .trim_end_matches(".git")
        .to_owned()
}

/// Name of the remote in `git remote -v` output whose fetch URL is `url`.
fn find_remote(listing: &str, url: &str) -> Option<String> {
    let want = normalize_remote(url);
    listing.lines().find_map(|line| {
        let mut cols = line.split_whitespace();
        let (name, remote_url, kind) = (cols.next()?, cols.next()?, cols.next()?);
        (kind == "(fetch)" && normalize_remote(remote_url) == want).then(|| name.to_owned())
    })
}

/// The main repository's root (the parent of `.git`), even when `cwd` is inside a worktree.
async fn main_root(cwd: &Path, cancel: &CancelFlag) -> Result<PathBuf> {
    let common = git(
        cwd,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        cancel,
    )
    .await?;
    let common = PathBuf::from(common);
    match common.parent() {
        Some(parent) if common.file_name().is_some_and(|n| n == ".git") => Ok(parent.to_path_buf()),
        _ => Err(HrnError::Tool(format!(
            "cannot place worktrees for the bare repository {}",
            common.display()
        ))),
    }
}

/// Check out each requested PR into its worktree.
pub(crate) async fn checkout(
    gh: &Gh,
    a: &Value,
    cwd: &Path,
    cancel: &CancelFlag,
) -> Result<String> {
    let mut prs = args::list(a, "pr");
    if prs.is_empty() {
        prs.push(String::new());
    }
    let (repo, force) = (args::opt(a, "repo"), args::flag(a, "force"));
    let mut sections = Vec::new();
    let mut failed = Vec::new();
    for pr in &prs {
        match checkout_one(gh, pr, repo, force, cwd, cancel).await {
            Ok(text) => sections.push(text),
            Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
            Err(e) => failed.push(format!(
                "- {}: {}",
                if pr.is_empty() { "current branch" } else { pr },
                reason(&e)
            )),
        }
    }
    if prs.len() == 1 {
        return match (sections.pop(), failed.pop()) {
            (Some(ok), _) => Ok(ok),
            (None, Some(err)) => Err(HrnError::Tool(err.trim_start_matches("- ").to_owned())),
            (None, None) => Err(HrnError::Tool("no pull request given".to_owned())),
        };
    }
    let mut out = format!(
        "# {}/{} pull requests checked out\n\n{}",
        sections.len(),
        prs.len(),
        sections.join("\n\n---\n\n")
    );
    if !failed.is_empty() {
        let _ = write!(out, "\n\n## Failed\n{}", failed.join("\n"));
    }
    Ok(out)
}

/// The remote that fetches `head_url`, adding a `fork-<owner>` remote when none does.
async fn remote_for(
    root: &Path,
    owner: &str,
    head_url: &str,
    cancel: &CancelFlag,
) -> Result<String> {
    let remotes = git(root, ["remote", "-v"], cancel).await?;
    if let Some(found) = find_remote(&remotes, head_url) {
        return Ok(found);
    }
    let taken = |name: &str| {
        remotes
            .lines()
            .any(|l| l.split_whitespace().next() == Some(name))
    };
    let mut remote = format!("fork-{owner}");
    let mut n = 2;
    while taken(&remote) {
        remote = format!("fork-{owner}-{n}");
        n += 1;
    }
    git(root, ["remote", "add", &remote, head_url], cancel).await?;
    Ok(remote)
}

/// The worktree of `branch` (reused when it exists, else created from `tracking`).
async fn worktree_for(
    root: &Path,
    branch: &str,
    tracking: &str,
    force: bool,
    cancel: &CancelFlag,
) -> Result<(PathBuf, bool)> {
    let existing = list_worktrees(root)
        .await?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch));
    if let Some(w) = existing {
        return Ok((w.path, true));
    }
    if is_onedrive_path(root) {
        return Err(HrnError::Tool(format!(
            "refusing to create a worktree inside OneDrive: {}",
            root.display()
        )));
    }
    let path = root.join(".harness").join("wt").join(branch);
    if path.exists() {
        return Err(HrnError::Tool(format!(
            "{} exists but is not a worktree of `{branch}`",
            path.display()
        )));
    }
    let path_str = path.to_string_lossy().into_owned();
    let local_ref = format!("refs/heads/{branch}");
    let local = git(
        root,
        ["rev-parse", "--verify", "--quiet", &local_ref],
        cancel,
    )
    .await
    .ok();
    let fetched = git(root, ["rev-parse", tracking], cancel).await?;
    let mut add = vec!["-c", "core.longpaths=true", "worktree", "add"];
    match local {
        Some(sha) if sha == fetched => add.extend([path_str.as_str(), branch]),
        Some(_) if force => add.extend(["-B", branch, path_str.as_str(), tracking]),
        Some(_) => {
            return Err(HrnError::Tool(format!(
                "branch `{branch}` already exists at a different commit; pass `force` to reset it"
            )));
        }
        None => add.extend(["-b", branch, path_str.as_str(), tracking]),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git(root, add, cancel).await?;
    Ok((path, false))
}

async fn checkout_one(
    gh: &Gh,
    pr: &str,
    repo: Option<&str>,
    force: bool,
    cwd: &Path,
    cancel: &CancelFlag,
) -> Result<String> {
    let mut cmd = vec!["pr".to_owned(), "view".to_owned()];
    if !pr.is_empty() {
        cmd.push(args::no_flag(pr, "pr")?.to_owned());
    }
    // A full PR URL names its repository itself.
    if !pr.starts_with("http") {
        cmd.extend(repo_flag(repo));
    }
    cmd.extend(["--json".to_owned(), PR_FIELDS.to_owned()]);
    let v = gh.json(cwd, &cmd, cancel).await?;
    let number = v.get("number").and_then(Value::as_u64).unwrap_or(0);
    let head_ref = field(&v, "headRefName");
    let owner = v
        .pointer("/headRepositoryOwner/login")
        .and_then(Value::as_str);
    let name = v.pointer("/headRepository/name").and_then(Value::as_str);
    let (Some(owner), Some(name)) = (owner, name) else {
        return Err(HrnError::Tool(format!(
            "PR #{number}: the head repository was deleted"
        )));
    };
    let pr_url = field(&v, "url");
    let host = pr_url
        .strip_prefix("https://")
        .and_then(|r| r.split('/').next())
        .unwrap_or("github.com");
    let head_url = format!("https://{host}/{owner}/{name}.git");

    let root = main_root(cwd, cancel).await?;
    let remote = remote_for(&root, owner, &head_url, cancel).await?;
    let tracking = format!("refs/remotes/{remote}/{head_ref}");
    let refspec = format!("+refs/heads/{head_ref}:{tracking}");
    git(&root, ["fetch", &remote, &refspec], cancel).await?;
    let branch = format!("pr-{number}");
    let (path, reused) = worktree_for(&root, &branch, &tracking, force, cancel).await?;
    let remote_key = format!("branch.{branch}.remote");
    git(&root, ["config", &remote_key, &remote], cancel).await?;
    let merge_key = format!("branch.{branch}.merge");
    git(
        &root,
        ["config", &merge_key, &format!("refs/heads/{head_ref}")],
        cancel,
    )
    .await?;
    Ok(format!(
        "# {} PR #{number}: {}\n{pr_url}\nBranch: {branch} (tracks {remote}/{head_ref})\nWorktree: {}",
        if reused {
            "Reused worktree for"
        } else {
            "Checked out"
        },
        title(&v),
        path.display()
    ))
}

/// Push the checked-out PR branch to its head branch.
pub(crate) async fn push(a: &Value, cwd: &Path, cancel: &CancelFlag) -> Result<String> {
    let branch = match args::opt(a, "branch") {
        Some(b) => args::no_flag(b, "branch")?.to_owned(),
        None => git(cwd, ["branch", "--show-current"], cancel).await?,
    };
    if branch.is_empty() {
        return Err(HrnError::Tool("HEAD is detached; pass `branch`".to_owned()));
    }
    let config = |key: &str| {
        let key = format!("branch.{branch}.{key}");
        async move {
            git(cwd, ["config", "--get", &key], cancel)
                .await
                .ok()
                .filter(|v| !v.is_empty())
        }
    };
    let (Some(remote), Some(merge)) = (config("remote").await, config("merge").await) else {
        return Err(HrnError::Tool(format!(
            "`{branch}` is not a PR branch from `pr_checkout` (no remote/merge config)"
        )));
    };
    let spec = format!("{branch}:{merge}");
    let mut cmd = vec!["push"];
    if args::flag(a, "forceWithLease") {
        cmd.push("--force-with-lease");
    }
    cmd.extend([remote.as_str(), spec.as_str()]);
    git(cwd, cmd, cancel).await?;
    Ok(format!(
        "# Pushed PR branch\nLocal: {branch}\nRemote: {remote} {merge}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_match_across_url_styles() {
        let listing = "origin\thttps://github.com/Cli/cli.git (fetch)\norigin\thttps://github.com/Cli/cli.git (push)\nme\tgit@github.com:me/cli.git (fetch)\n";
        assert_eq!(
            find_remote(listing, "https://github.com/cli/cli.git").as_deref(),
            Some("origin")
        );
        assert_eq!(
            find_remote(listing, "https://github.com/me/cli.git").as_deref(),
            Some("me")
        );
        assert_eq!(
            find_remote(listing, "https://github.com/other/cli.git"),
            None
        );
        assert_eq!(
            normalize_remote("ssh://git@ghe.x:22/o/r.git/"),
            "ghe.x/22/o/r"
        );
    }
}
