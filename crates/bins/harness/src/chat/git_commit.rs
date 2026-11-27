//! `/commit` and `/git` commit-message backend methods on [`ChatBackend`].

use super::ChatBackend;
use crate::commit::{
    apply_commit_plan, ensure_commit_enabled, generate_commit_message, prepare_commit_plan,
};
use hrn_kernel::Result;
use hrn_tui::CommitPreview;
use std::sync::Arc;

impl ChatBackend {
    pub(super) fn prepare_commit_preview_live(&mut self) -> Result<CommitPreview> {
        ensure_commit_enabled(&self.cfg.commit)?;
        let sid = self.session.as_deref();
        let plan = prepare_commit_plan(&self.cwd, self.store.as_ref(), sid)?;
        let llm = Arc::clone(self.runtime.llm());
        let llm_cfg = self.llm.clone();
        let diff_preview = plan.diff_preview.clone();
        let suggested = self.handle.block_on(async move {
            generate_commit_message(llm.as_ref(), &llm_cfg, &diff_preview).await
        })?;
        let changelog_enabled = self.cfg.commit.changelog;
        Ok(plan.into_tui_preview(suggested, changelog_enabled))
    }

    pub(super) fn execute_commit_live(&mut self, message: &str, changelog: bool) -> Result<String> {
        ensure_commit_enabled(&self.cfg.commit)?;
        let sid = self.session.as_deref();
        let plan = prepare_commit_plan(&self.cwd, self.store.as_ref(), sid)?;
        apply_commit_plan(&plan, message, changelog)
    }

    pub(super) fn generate_commit_message_for_staged_live(
        &mut self,
        staged_diff: &str,
    ) -> Result<String> {
        let llm = Arc::clone(self.runtime.llm());
        let llm_cfg = self.llm.clone();
        let diff = staged_diff.to_owned();
        self.handle
            .block_on(async move { generate_commit_message(llm.as_ref(), &llm_cfg, &diff).await })
    }
}
