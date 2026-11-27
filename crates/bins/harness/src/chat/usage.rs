//! `/usage` in the TUI: read the provider limits in the background and print them in the
//! transcript when they arrive.

use super::ChatBackend;
use crate::paths;
use hrn_edge_http::HttpClient;
use hrn_edge_llm::{fetch_usage_with_pool, render_usage};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_kernel::{Result, WorkspaceFs};
use hrn_tui::UiEvent;
use std::sync::Arc;

impl ChatBackend {
    /// Start reading the limits; the rendered report reaches the UI as [`UiEvent::Report`].
    pub(super) fn usage_task(&self) -> Result {
        let auth_dir = paths::auth_dir(StdWorkspaceFs.home_dir().as_deref())?;
        let http = HttpClient::new()?;
        let tx = self.tx.clone();
        let pool = Arc::clone(&self.background.pool);
        pool.clear_all_invalidated();
        self.handle.spawn(async move {
            let report = render_usage(&fetch_usage_with_pool(&http, &auth_dir, &pool).await);
            tx.send(UiEvent::Report(report));
        });
        Ok(())
    }
}
