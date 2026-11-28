//! Session lifecycle and turn-spawning helpers split out of `chat.rs` to keep file sizes lean.

use super::ChatBackend;
use super::chat_ui::run_turns;
use crate::term::short;
use hrn_edge_llm::resolve_alias;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{SessionControl, discover_custom_commands};
use hrn_kernel::{CancelFlag, DagNode, HrnError, ImageAttachment, Result, SessionMeta};
use hrn_tui::UiEvent;
use std::path::PathBuf;

impl ChatBackend {
    pub(super) fn set_session(&mut self, id: Option<String>) {
        self.clear_focus_sink();
        self.focused_agent = None;
        self.focused_on_child
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.saved_main_settings = None;
        let changed = self.session != id;
        self.control = SessionControl::default();
        self.control.set_permission_mode(self.settings.mode);
        self.control
            .set_advisor(self.settings.advisor, self.settings.advisor_model.clone());
        if let Ok(mut last) = self.last.lock() {
            last.clone_from(&id);
        }
        self.session = id;
        self.remote.hub.set_active_session(self.session.clone());
        if changed {
            self.close_collab_host("host switched session");
            match self.session.clone() {
                Some(sid) => self.acquire_session_lease(&sid),
                None => self.lease = None,
            }
        }
        // The UI shows the agents of the session now current.
        self.runtime.agents().inbox().wake();
        self.idle.touch();
    }

    /// Active session id for settings/switch operations: focused child session if focused, else main.
    pub(super) fn active_session_id(&self) -> Option<String> {
        if let (Some(root), Some(agent_id)) =
            (self.session.as_deref(), self.focused_agent.as_deref())
            && let Some(rec) = self.runtime.agents().get_in_tree(root, agent_id)
            && let Some(sid) = rec.snapshot().session_id
        {
            return Some(sid);
        }
        self.session.clone()
    }

    /// The current session, created with the current settings if there is none yet.
    pub(super) fn session_for(&mut self, prompt: &str) -> Result<String> {
        if let Some(id) = &self.session {
            return Ok(id.clone());
        }
        let meta = self.runtime.create_session(
            short(prompt, 80),
            &self.cwd,
            Some(self.settings.model.clone()),
            self.settings.thinking,
            self.settings.mode,
        )?;
        self.set_session(Some(meta.id.clone()));
        Ok(meta.id)
    }

    /// Persist model and thinking changes made since the last turn; the turn reads them from the
    /// stored session.
    pub(super) fn sync_settings(&self, id: &str) -> Result {
        let mut meta = self
            .store
            .get_session(id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{id}`")))?;
        let model = Some(self.settings.model.clone());
        if meta.model != model
            || meta.thinking != self.settings.thinking
            || meta.permission_mode != self.settings.mode
        {
            meta.model = model;
            meta.thinking = self.settings.thinking;
            meta.permission_mode = self.settings.mode;
            self.store.update_session(&meta)?;
        }
        Ok(())
    }

    /// Run `prompt` as a turn of `session_id` in the background; the UI hears of its end.
    pub(super) fn start_turn(
        &mut self,
        session_id: String,
        prompt: &str,
        images: Vec<ImageAttachment>,
    ) {
        self.idle.set_busy(true);
        self.remote.hub.set_busy(true);
        let effective_prompt = if !images.is_empty()
            && let Ok(data_dir) = crate::app_data_dir()
            && let Ok(store) = self.background.attachments(&data_dir)
        {
            let root = self.runtime.agents().root_of(&session_id);
            crate::url_adapters::materialize_prompt_images(&store, &root, prompt, &images)
        } else {
            prompt.to_owned()
        };
        let cancel = self.fresh_cancel();
        let store = std::sync::Arc::clone(&self.store);
        let sid_for_title = session_id.clone();
        let collab_host = self
            .collab_host
            .as_ref()
            .filter(|b| b.session_id == session_id && !b.host.is_closed())
            .cloned();
        let remote_hub = std::sync::Arc::clone(&self.remote.hub);
        let task = run_turns(
            self.runtime.clone(),
            session_id,
            effective_prompt,
            images,
            self.control.clone(),
            self.tx.clone(),
            cancel,
            collab_host,
            Some(std::sync::Arc::clone(&remote_hub)),
            std::sync::Arc::clone(&self.store),
            std::sync::Arc::clone(&self.focused_on_child),
        );
        let tx = self.tx.clone();
        let idle = self.idle.clone();
        self.handle.spawn(async move {
            let end = task.await;
            idle.set_busy(false);
            remote_hub.set_busy(false);
            if let Ok(Some(meta)) = store.get_session(&sid_for_title) {
                tx.send(UiEvent::SessionTitle {
                    session_id: sid_for_title,
                    title: meta.title,
                });
            }
            tx.send(UiEvent::Finished(end));
        });
    }

    /// Bare `/rename`: regenerate the active session's title with the title model (`TitleSource::User`).
    pub(super) fn regenerate_title_live(&mut self) -> Result {
        self.ensure_writable()?;
        let session_id = self
            .session
            .clone()
            .ok_or_else(|| HrnError::Store("no active session".to_owned()))?;
        let runtime = self.runtime.clone();
        let tx = self.tx.clone();
        let cancel = CancelFlag::new();
        self.handle.spawn(async move {
            match runtime.regenerate_title(&session_id, &cancel).await {
                Ok(Some(title)) => {
                    tx.send(UiEvent::SessionTitle {
                        session_id,
                        title: title.clone(),
                    });
                    tx.send(UiEvent::Report(format!("session renamed to `{title}`")));
                }
                Ok(None) => {
                    tx.send(UiEvent::Report(
                        "title generator returned no title (session unchanged)".to_owned(),
                    ));
                }
                Err(err) => {
                    tx.send(UiEvent::Report(format!("rename failed: {err}")));
                }
            }
        });
        Ok(())
    }

    /// Import one external Claude/Codex session selected from the `/sessions` picker and resume it.
    pub(super) fn import_external_session_live(
        &mut self,
        source: &str,
        id: &str,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        let imported_id = crate::import_cmd::import_for_tui(&self.sqlite, source, id)?;
        self.resume_session_live(&imported_id)
    }

    /// Resume a stored session, repairing any interrupted tail without making an LLM request.
    pub(super) fn resume_session_live(
        &mut self,
        session_id: &str,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        let _ = self.runtime.repair_interrupted(session_id)?;
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

    /// Directory storing the active session tree's `local://` artifacts (`Alt+G`), if any.
    pub(super) fn local_artifacts_dir_live(&self) -> Option<PathBuf> {
        let sid = self.session.as_deref()?;
        let safe: String = self
            .runtime
            .agents()
            .root_of(sid)
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Some(crate::app_data_dir().ok()?.join("scratch").join(safe))
    }

    pub(super) fn fresh_cancel(&mut self) -> CancelFlag {
        let cancel = CancelFlag::new();
        self.cancel = Some(cancel.clone());
        cancel
    }

    /// The current session's branch, for read-only views.
    pub(super) fn session_branch(&self) -> hrn_kernel::Result<Option<Vec<hrn_kernel::DagNode>>> {
        let Some(id) = self.active_session_id() else {
            return Ok(None);
        };
        let Some(meta) = self.store.get_session(&id)? else {
            return Ok(None);
        };
        match &meta.head_node_id {
            Some(head) => Ok(Some(self.store.load_branch(&id, head)?)),
            None => Ok(Some(Vec::new())),
        }
    }
}
