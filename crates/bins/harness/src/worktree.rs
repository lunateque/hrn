//! `hrn worktree add | list | rm`: git worktree management with `OneDrive` guard.

use crate::term::out;
use hrn_edge_ipc::{add_worktree, list_worktrees, remove_worktree};
use hrn_kernel::Result;
use std::path::Path;

/// `hrn worktree add <slug> [--base REF]`.
pub(crate) async fn add(cwd: &Path, slug: &str, base: Option<&str>) -> Result<()> {
    let info = add_worktree(cwd, slug, base).await?;
    out(&format!(
        "created worktree `{}` on branch `{}`\n",
        info.path.display(),
        info.branch.as_deref().unwrap_or("-")
    ));
    Ok(())
}

/// `hrn worktree list`.
pub(crate) async fn list(cwd: &Path) -> Result<()> {
    let entries = list_worktrees(cwd).await?;
    if entries.is_empty() {
        out("no worktrees found\n");
        return Ok(());
    }
    out("BRANCH\tHEAD\tPATH\n");
    for entry in entries {
        let branch = if entry.bare {
            "(bare)"
        } else {
            entry.branch.as_deref().unwrap_or("(detached)")
        };
        let head = entry
            .head
            .as_deref()
            .and_then(|h| h.get(..8))
            .unwrap_or("-");
        out(&format!("{branch}\t{head}\t{}\n", entry.path.display()));
    }
    Ok(())
}

/// `hrn worktree rm <slug> [--force]`.
pub(crate) async fn remove(cwd: &Path, target: &str, force: bool) -> Result<()> {
    remove_worktree(cwd, target, force).await?;
    out(&format!("removed worktree `{target}`\n"));
    Ok(())
}
