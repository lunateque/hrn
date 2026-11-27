//! `github` (`cargo xtask module remove github`): GitHub through the `gh` CLI.
//!
//! - `github`: read-only repository, file, search, check and Actions operations.
//! - `github_write`: PR / issue creation, comments, merging and PR worktrees, always confirmed.
//! - `issue://` and `pr://`: single issues and pull requests, lists and PR diffs for `read`.
//!
//! Without `gh` on `PATH` nothing is registered. Authentication is `gh`'s own and is checked
//! when an operation runs, so startup spawns nothing.

mod args;
mod checkout;
mod dates;
mod files;
mod gh;
mod render;
mod search;
mod target;
mod tool;
mod urls;
mod watch;
mod write;

use hrn_kernel::{Registry, Result};
use std::sync::Arc;

/// Register the `github` and `github_write` tools and the `issue://` / `pr://` schemes, or
/// nothing when `gh` is not installed.
///
/// # Errors
///
/// When a name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let Some(gh) = gh::Gh::discover() else {
        return Ok(());
    };
    registry.tool(Arc::new(tool::GithubTool { gh: gh.clone() }))?;
    registry.deferrable_tool(Arc::new(write::GithubWriteTool { gh: gh.clone() }))?;
    for kind in [urls::Kind::Issue, urls::Kind::Pr] {
        registry.scheme(Arc::new(urls::GithubScheme {
            gh: gh.clone(),
            kind,
        }))?;
    }
    Ok(())
}
