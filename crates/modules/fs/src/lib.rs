//! `fs`: the file tools (`read`, `write`, `apply_patch`, `grep`, `glob`) and the
//! `local://` scratch and `conflict://` merge-resolution schemes. The engines live in
//! `hrn-edge-tools`; this module is the tool surface.
//!
//! One [`FileReadTracker`] is shared across `read`, `write`, `apply_patch`, and `hrn-mod-edit`
//! via [`hrn_kernel::Shared`], and [`EditModeHint`] coordinates whether `read` / `grep` emit
//! `[path#TAG]` hashline anchors so `fs` and `edit` are independently removable.
//!
//! ```toml
//! [fs]
//! grep_context_before = 0       # context lines grep shows around a match
//! grep_context_after = 0
//! ```

mod conflict;
mod glob;
mod grep;
mod patch;
mod read;
mod write;

use hrn_edge_tools::{EditModeHint, FileReadTracker, LocalScheme, PageFetcher};
use hrn_kernel::serde::Deserialize;
use hrn_kernel::{Registry, Result};
use std::sync::Arc;

/// The `[fs]` config table.
#[derive(Debug, Default, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields, default)]
struct FsConfig {
    /// Deprecated: superseded by `[edit] mode` (kept ignored for user config backwards compatibility).
    #[serde(rename = "edit_mode")]
    _legacy_edit_mode: Option<String>,
    /// Lines of context `grep` shows before each match (omp's `grep.contextBefore`).
    grep_context_before: usize,
    /// Lines of context `grep` shows after each match (omp's `grep.contextAfter`).
    grep_context_after: usize,
}

/// Register the file tools (`read`, `write`, `apply_patch`, `grep`, `glob`) and the `local://`
/// and `conflict://` schemes.
///
/// # Errors
///
/// When `[fs]` is malformed or a name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let config: FsConfig = registry.config()?;
    registry.help_page(
        "read/sources",
        include_str!("../prompts/help-read-sources.md"),
    );
    registry.help_page("read/urls", include_str!("../prompts/help-read-urls.md"));
    let tracker = registry
        .shared()
        .get_or_init(|| Ok(FileReadTracker::new()))?;
    let edit_hint = registry.shared().get_or_init(|| Ok(EditModeHint::new()))?;
    let fetcher = PageFetcher::shared(&registry.shared_arc())?;
    registry.tool(Arc::new(read::ReadTool::new(
        Arc::clone(&tracker),
        fetcher,
        Arc::clone(&edit_hint),
    )))?;
    registry.tool(Arc::new(write::WriteTool::new(
        Arc::clone(&tracker),
        Arc::clone(&edit_hint),
    )))?;
    registry.tool(Arc::new(patch::ApplyPatchTool::new(
        Arc::clone(&tracker),
        Arc::clone(&edit_hint),
    )))?;
    registry.scheme(Arc::new(conflict::ConflictScheme::new(tracker)))?;
    registry.tool(Arc::new(grep::GrepTool::new(
        config.grep_context_before,
        config.grep_context_after,
        edit_hint,
    )))?;
    registry.tool(Arc::new(glob::GlobTool))?;
    registry.scheme(Arc::new(LocalScheme::new(
        registry.host().data_dir.join("scratch"),
    )))
}

#[cfg(test)]
mod tests;
