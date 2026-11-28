//! Session-inspection half of the TUI backend (split out of `chat.rs`): the listings, usage
//! rollups and exports the front end asks for between turns.

use super::{ChatBackend, SESSION_LIST};
use hrn_kernel::{HrnError, Result, SessionMeta, Usage};
use std::path::Path;

impl ChatBackend {
    pub(super) fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        self.store.list_sessions(SESSION_LIST)
    }

    pub(super) fn rollup_usage(&self) -> Result<Usage> {
        self.active_session_id()
            .as_deref()
            .map_or_else(|| Ok(Usage::default()), |id| self.store.session_usage(id))
    }

    pub(super) fn last_prompt_tokens_live(&self) -> u64 {
        let Some(id) = self.active_session_id() else {
            return 0;
        };
        self.store
            .latest_prompt_tokens(&id)
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    pub(super) fn run_usage_task(&mut self) -> Result {
        self.usage_task()
    }

    pub(super) fn stats_report(&self) -> Result<String> {
        crate::stats::report(
            self.store.as_ref(),
            self.session.as_deref(),
            &self.model_names,
        )
    }

    pub(super) fn export_to(&self, path: Option<&str>) -> Result<String> {
        let id = self
            .session
            .as_deref()
            .ok_or_else(|| HrnError::Store("nothing to export: no session yet".to_owned()))?;
        let written = crate::export::write_file(
            self.store.as_ref(),
            id,
            path.map(Path::new),
            None,
            &self.cwd,
        )?;
        Ok(written.display().to_string())
    }
}
