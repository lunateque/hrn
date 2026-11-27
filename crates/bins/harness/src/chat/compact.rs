//! `/compact [instructions]`, `/shake`, and compaction summary lookup on [`ChatBackend`].

use super::ChatBackend;
use super::chat_ui::UiEvents;
use hrn_kernel::{DagPayload, EventSink, HrnError};
use hrn_tui::{TaskEnd, UiEvent};
use std::sync::Arc;

impl ChatBackend {
    /// `/compact [instructions]`: run the compaction method walk in the background.
    pub(super) fn run_compact(&mut self, instructions: Option<&str>) -> bool {
        let Some(session_id) = self.session.clone() else {
            return false;
        };
        self.idle.set_busy(true);
        let cancel = self.fresh_cancel();
        let runtime = self.runtime.clone();
        let tx = self.tx.clone();
        let idle = self.idle.clone();
        let instructions = instructions.map(str::to_owned);
        self.handle.spawn(async move {
            let events: Arc<dyn EventSink> = Arc::new(UiEvents::tui_only(tx.clone()));
            let result = match runtime
                .compact_now(&session_id, instructions.as_deref(), &cancel, &events)
                .await
            {
                Ok(covered) => Ok(covered.is_some()),
                Err(HrnError::Cancelled) => Err("interrupted".to_owned()),
                Err(err) => Err(err.to_string()),
            };
            idle.set_busy(false);
            tx.send(UiEvent::Finished(TaskEnd::Compact(result)));
        });
        true
    }

    /// `/shake`: run aggressive zero-LLM tool-output elision in the background.
    pub(super) fn run_shake(&mut self) -> bool {
        let Some(session_id) = self.session.clone() else {
            return false;
        };
        self.idle.set_busy(true);
        let cancel = self.fresh_cancel();
        let runtime = self.runtime.clone();
        let tx = self.tx.clone();
        let idle = self.idle.clone();
        self.handle.spawn(async move {
            let events: Arc<dyn EventSink> = Arc::new(UiEvents::tui_only(tx.clone()));
            let result = match runtime.shake_now(&session_id, &cancel, &events).await {
                Ok(covered) => Ok(covered.is_some()),
                Err(HrnError::Cancelled) => Err("interrupted".to_owned()),
                Err(err) => Err(err.to_string()),
            };
            idle.set_busy(false);
            tx.send(UiEvent::Finished(TaskEnd::Compact(result)));
        });
        true
    }

    /// Latest persisted compaction summary text on the active branch (for `Ctrl+O` expansion).
    pub(super) fn latest_compaction_text(&self) -> Option<String> {
        let branch = self.session_branch().ok().flatten()?;
        branch.iter().rev().find_map(|node| match &node.payload {
            DagPayload::Summary { text, .. } => Some(text.clone()),
            _ => None,
        })
    }
}
