//! Native, token-efficient Chrome `DevTools` Protocol (CDP) and Chrome relay browser module
//! (`hrn-mod-browser`).
//!
//! Connects directly to Chromium/Chrome/Edge/Brave via loopback RFC 6455 WebSocket or drives
//! the user's signed-in Chrome through the native messaging relay (`hrn browser-relay`), exposes
//! compact accessibility tree snapshots with `@eN` refs, captures console/network diagnostics,
//! and returns downscaled multimodal screenshots directly in [`hrn_kernel::ToolOutput::images`].
//!
//! Removable with `cargo xtask module remove browser`.

mod actions;
mod axtree;
mod cdp;
mod launcher;
mod relay;
mod relay_install;
mod tabs;
mod tool;

#[cfg(test)]
mod relay_tests;
#[cfg(test)]
mod tests;

pub use launcher::{BrowserConfig, discover_browser_binary};
pub use relay::{
    RelayServerHandle, RelayState, read_relay_state, relay_state_path, serve_stdio,
    start_relay_over_io,
};
pub use relay_install::{
    DEFAULT_HKCU_KEY, InstallReport, NATIVE_HOST_NAME, bundled_extension_id, derive_extension_id,
    install_relay, uninstall_relay,
};
pub use tabs::BrowserManager;
pub use tool::BrowserTool;

use hrn_kernel::{Command, Registry, Result, Tool};
use std::sync::Arc;

/// Build the `browser` tool backed by `config`.
pub(crate) fn browser_tool(config: BrowserConfig) -> Arc<dyn Tool> {
    Arc::new(BrowserTool::new(Arc::new(BrowserManager::new(config))))
}

/// Register the `browser` tool and `hrn browser-relay` command (nothing launches until used).
///
/// # Errors
///
/// When `[browser]` config fails to parse or the tool/command name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let data_dir = registry.host().data_dir.clone();
    let cfg = registry
        .config::<BrowserConfig>()?
        .with_env_fallbacks()
        .with_data_dir(data_dir.clone());
    registry.deferrable_tool(browser_tool(cfg))?;

    let relay_dir = data_dir;
    registry.command(Command {
        name: "browser-relay",
        about: "Install or serve the native Chrome browser relay",
        run: Arc::new(move |args, cancel| {
            let dir = relay_dir.clone();
            Box::pin(async move { relay::run_cli(&dir, args, cancel).await })
        }),
    })
}
