// The `json!` parameter schema in `tool.rs` nests deeper than the default 128 recursion limit.
#![recursion_limit = "512"]

//! Windows desktop UI Automation and computer-use module (`hrn-mod-desktop`).
//!
//! Zero-`unsafe` Rust module communicating with a persistent DPI-aware Windows PowerShell/C#
//! bridge (`UIAutomationClient`, `PrintWindow`, background `PostMessage` + focus-restoring
//! takeover), guarded by a Win32 Job Object. Inspection actions use [`hrn_kernel::ToolApproval::read`]
//! and input/mutation actions use [`hrn_kernel::ToolApproval::exec`].
//!
//! Removable with `cargo xtask module remove desktop`.

mod actions;
mod actions_ax;
mod actions_input;
mod ax_fmt;
mod coords;
mod error;
mod session;
#[cfg(test)]
mod tests;
#[cfg(all(test, windows))]
mod tests_live;
mod tool;
mod win_bridge;

pub use ax_fmt::{normalize_role, parse_ax_nodes, role_matches};
pub use coords::ScreenScale;
pub use error::DesktopError;
pub use session::{
    AxNode, AxRef, CoordFrame, DEFAULT_MAX_DEPTH, FrameTarget, MAX_AX_NODES, MAX_FIND_LIMIT,
    WindowInfo,
};
pub use tool::{BackgroundFallback, DesktopConfig, DesktopManager, DesktopTool};

use hrn_kernel::{Registry, Result, Tool};
use std::sync::Arc;

/// Build the `desktop` tool backed by `config`.
fn desktop_tool(config: DesktopConfig) -> Arc<dyn Tool> {
    Arc::new(DesktopTool::new(Arc::new(DesktopManager::with_config(
        config,
    ))))
}

/// Register the `desktop` tool, `[desktop]` config table, and `help://desktop` reference page.
///
/// # Errors
///
/// When `[desktop]` configuration is invalid or the tool name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let config: DesktopConfig = registry.config()?;
    config.validate()?;
    let hints = registry
        .shared()
        .get_or_init(|| Ok(hrn_edge_tools::EvalRuntimeHints::default()))?;
    hints.set_desktop_eval_prelude(config.eval_prelude);
    registry.help_page("desktop", tool::HELP);
    registry.deferrable_tool(desktop_tool(config))
}
