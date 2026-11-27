//! Background work in the TUI: tells the UI which agents, jobs and services run, and starts the
//! follow-up turn that delivers their results once the session is idle.

use super::ChatBackend;
use super::hub::build_hub_snapshot;
use hrn_edge_llm::LlmConfig;
use hrn_harness::AgentRegistry;
use hrn_kernel::{Inbox, ProcTable, Result, SessionStore, format_inbox};
use hrn_tui::{AgentHubSnapshot, UiEvent, UiSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;

impl ChatBackend {
    /// Start a turn from the results waiting for the current session. `None` when there is
    /// nothing to deliver or the session is busy; otherwise a one-line summary for the transcript.
    pub(super) fn inbox_turn(&mut self) -> Result<Option<String>> {
        if self.focused_agent.is_some() {
            return Ok(None);
        }
        let Some(session) = self.session.clone() else {
            return Ok(None);
        };
        let inbox = self.runtime.agents().inbox();
        if self.control.is_running() || inbox.queued(&session) == 0 {
            return Ok(None);
        }
        self.sync_settings(&session)?;
        let items = inbox.take(&session);
        let summary = items
            .iter()
            .map(|item| item.summary.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        self.start_turn(session, &format_inbox(&items), Vec::new());
        Ok(Some(format!("background: {summary}")))
    }
}

/// Keep the UI told about the current session's running work, live Agent Hub snapshots, and
/// waiting results. The task sleeps on the inbox (or ticks every 1 s while subagents run so live
/// tool previews stay current) and ends with the UI.
#[expect(
    clippy::too_many_arguments,
    reason = "watcher coordinates inbox, procs, agent registry, store, llm, current session, and tx"
)]
pub(super) fn watch(
    handle: &Handle,
    inbox: Arc<Inbox>,
    procs: Arc<ProcTable>,
    agents: Arc<AgentRegistry>,
    store: Arc<dyn SessionStore>,
    llm: LlmConfig,
    current: Arc<Mutex<Option<String>>>,
    tx: UiSender,
) {
    handle.spawn(async move {
        let mut shown = Vec::new();
        let mut last_hub = AgentHubSnapshot::default();
        loop {
            let seen = inbox.version();
            let _ = agents.sweep_idle();
            let session = current.lock().ok().and_then(|id| id.clone());
            let (running, queued, hub) = session.as_deref().map_or_else(
                || (Vec::new(), 0, AgentHubSnapshot::default()),
                |id| {
                    (
                        procs.running_ids(id),
                        inbox.queued(id),
                        build_hub_snapshot(agents.as_ref(), store.as_ref(), &llm, id),
                    )
                },
            );
            if running != shown || queued > 0 {
                if !tx.send(UiEvent::Running(running.clone())) {
                    return;
                }
                shown = running;
            }
            if hub != last_hub {
                if !tx.send(UiEvent::AgentHub(hub.clone())) {
                    return;
                }
                last_hub = hub;
            }
            if shown.is_empty() {
                inbox.changed(seen).await;
            } else {
                tokio::select! {
                    () = inbox.changed(seen) => {}
                    () = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
    });
}
