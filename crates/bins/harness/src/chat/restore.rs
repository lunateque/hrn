//! Crash restore, single-writer session leases, picker lifecycles, and startup `Options`
//! construction for `ChatBackend`.

use super::ChatBackend;
use super::chat_ui::{UiGate, run_restore};
use crate::resume::{StartupSession, recent_in_cwd};
use crate::session_lock::{SessionLease, acquire_lease};
use hrn_edge_llm::resolve_alias;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{TreeRestoreSummary, discover_custom_commands, session_lifecycle};
use hrn_kernel::{DagNode, HrnError, Result, SessionMeta};
use hrn_tui::{Options, RestoreStart, UiEvent};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Maximum recent sessions listed on the welcome banner.
const LANDING_RECENT: usize = 4;

impl ChatBackend {
    /// Acquire (or refresh) the single-writer lease for `session_id`.
    pub(super) fn acquire_session_lease(&mut self, session_id: &str) {
        self.lease = None;
        let Some(lock_dir) = &self.lock_dir else {
            return;
        };
        match acquire_lease(lock_dir, session_id) {
            Ok(lease) => self.lease = Some(lease),
            Err(err) => {
                crate::term::err_line(&format!("warning: session lock unavailable: {err}"));
            }
        }
    }

    /// Reject mutating turn submission when another process owns the session write lock.
    pub(super) fn ensure_writable(&self) -> Result {
        if let Some(lease) = &self.lease
            && lease.is_read_only()
        {
            return Err(HrnError::Store(lease.warning().unwrap_or_else(|| {
                "session is read-only (another hrn process holds the write lock)".to_owned()
            })));
        }
        Ok(())
    }

    /// Read-only lease warning for the current session, if any.
    pub(super) fn lease_warning_text(&self) -> Option<String> {
        self.lease.as_ref().and_then(SessionLease::warning)
    }

    /// Open a stored session for `--restore` without pre-repairing its tail, so `restore_turn`
    /// can replay an all-cut-off tool batch verbatim.
    fn open_for_restore(&mut self, session_id: &str) -> Result<(SessionMeta, Vec<DagNode>)> {
        let meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let nodes = match &meta.head_node_id {
            Some(head) => self.store.load_branch(&meta.id, head)?,
            None => Vec::new(),
        };
        if let Some(model) = &meta.model {
            self.settings.model =
                resolve_alias(&self.llm, Some(model)).unwrap_or_else(|_| model.clone());
        }
        self.settings.thinking = meta.thinking;
        self.settings.mode = meta.permission_mode;
        self.cwd = PathBuf::from(&meta.cwd);
        self.custom_commands =
            discover_custom_commands(&StdWorkspaceFs, &self.cwd).unwrap_or_default();
        let _ = self
            .runtime
            .agents()
            .resurrect_tree(&meta.id, self.store.as_ref());
        self.set_session(Some(meta.id.clone()));
        Ok((meta, nodes))
    }

    /// Up to 4 most recent non-empty sessions for `self.cwd`, using `SQLite` v4 `list_recent_for`
    /// with a Windows-normalized fallback.
    fn recent_for_landing(&self) -> Vec<SessionMeta> {
        let cwd_str = self.cwd.to_string_lossy();
        let mut recent: Vec<SessionMeta> = self
            .store
            .list_recent_for(&cwd_str, LANDING_RECENT * 2)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.head_node_id.is_some())
            .take(LANDING_RECENT)
            .collect();
        if recent.is_empty()
            && let Ok(all) = self.store.list_sessions(200)
        {
            recent = recent_in_cwd(&all, &self.cwd, LANDING_RECENT);
        }
        recent
    }

    /// Build startup [`Options`] for `startup` (`Fresh`, `Resume`, or `Restore`).
    pub(super) fn options_for_startup(&mut self, startup: &StartupSession) -> Result<Options> {
        let recent_sessions = self.recent_for_landing();
        let mut initial_flash = None;
        let mut initial_notices = Vec::new();
        let mut start_restore = false;

        let resumed = match startup {
            StartupSession::Fresh => None,
            StartupSession::Resume(id) => Some(hrn_tui::Backend::resume(self, id)?),
            StartupSession::Restore(id) => {
                let loaded = self.open_for_restore(id)?;
                let main_interrupted = self.runtime.diagnose_session(id)?.is_interrupted();
                let subagent_count = self.runtime.count_interrupted_subagents(id)?;
                let summary = TreeRestoreSummary {
                    main_interrupted,
                    subagent_count,
                };
                if main_interrupted || subagent_count > 0 {
                    if self.lease.as_ref().is_some_and(SessionLease::is_read_only) {
                        initial_flash = Some("restored: opened read-only (locked)".to_owned());
                    } else {
                        initial_flash = Some(summary.flash_message());
                        start_restore = true;
                    }
                } else {
                    initial_flash = Some(summary.flash_message());
                }
                Some(loaded)
            }
        };

        if let Some(warn) = self.lease_warning_text() {
            initial_notices.push(warn);
        }

        let mut appearance = self.cfg.appearance.clone();
        appearance.themes_dir = crate::app_data_dir().ok().map(|d| d.join("themes"));
        appearance.profile = crate::paths::active_profile();
        appearance.keymap_paths = crate::keybindings_paths();

        Ok(Options {
            settings: self.settings.clone(),
            cwd: self.cwd.clone(),
            resumed,
            recent_sessions,
            ask_timeout: self.cfg.ask_timeout,
            theme: None,
            appearance,
            initial_prompt: None,
            initial_flash,
            initial_notices,
            start_restore,
            branch_summary: self.cfg.branch_summary,
            double_escape_action: self.cfg.double_escape_action,
            btw_inject: self.cfg.btw_inject,
            pinned_agents: self.cfg.pinned_agents,
            subagent_live_preview: self.cfg.subagent_live_preview,
            mouse: self.cfg.mouse,
        })
    }

    /// Lifecycle labels (`complete | interrupted | aborted | error`) for all stored sessions.
    pub(super) fn session_lifecycles_map(&self) -> HashMap<String, String> {
        let Ok(sessions) = self.list_sessions() else {
            return HashMap::new();
        };
        let mut map = HashMap::with_capacity(sessions.len());
        for meta in sessions {
            let branch = match &meta.head_node_id {
                Some(head) => self.store.load_branch(&meta.id, head).unwrap_or_default(),
                None => Vec::new(),
            };
            let label = session_lifecycle(&meta, &branch).as_str().to_owned();
            map.insert(meta.id, label);
        }
        map
    }

    /// Continue interrupted child subagents first, then the active main session's interrupted
    /// turn in the background (`--restore`).
    pub(super) fn restore_active_session(&mut self) -> Result<RestoreStart> {
        self.apply_pending_reload();
        self.ensure_writable()?;
        let Some(session_id) = self.session.clone() else {
            return Ok(RestoreStart::default());
        };
        let gate: Arc<dyn hrn_kernel::ApprovalGate> = Arc::new(UiGate::new(
            self.tx.clone(),
            Some(Arc::clone(&self.remote.hub)),
        ));
        let revived_subagents = self
            .runtime
            .restore_interrupted_subagents(&session_id, Some(&gate))?;
        let main_interrupted = self.runtime.diagnose_session(&session_id)?.is_interrupted();
        if main_interrupted {
            self.sync_settings(&session_id)?;
            let cancel = self.fresh_cancel();
            let remote_hub = Arc::clone(&self.remote.hub);
            remote_hub.set_busy(true);
            let task = run_restore(
                self.runtime.clone(),
                session_id,
                self.control.clone(),
                self.tx.clone(),
                cancel,
                Some(Arc::clone(&remote_hub)),
                Arc::clone(&self.store),
            );
            let tx = self.tx.clone();
            self.handle.spawn(async move {
                let end = task.await;
                remote_hub.set_busy(false);
                tx.send(UiEvent::Finished(end));
            });
        }
        Ok(RestoreStart {
            main_started: main_interrupted,
            subagents_started: revived_subagents.len(),
        })
    }
}
