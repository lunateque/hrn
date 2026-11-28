//! `/handoff`, `/branch` checkout & label persistence, and `/btw` background execution on
//! [`ChatBackend`].

use super::ChatBackend;
use super::chat_ui::UiEvents;
use hrn_kernel::{
    BtwExchange, BtwStatus, BtwTopic, CancelFlag, DagNode, EventSink, HrnError, Result, SessionMeta,
};
use hrn_tui::{TaskEnd, UiEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static BTW_SEQ: AtomicU64 = AtomicU64::new(1);

impl ChatBackend {
    /// `/handoff [focus]`: runs on the background handle like `/compact`; when idle commits right
    /// away, and when a turn is running queues the validated draft for the next tool boundary.
    pub(super) fn run_handoff(&mut self, focus: Option<&str>) -> Result<bool> {
        self.ensure_writable()?;
        let Some(session_id) = self.session.clone() else {
            return Ok(false);
        };
        let focus = focus.map(str::to_owned);
        let runtime = self.runtime.clone();
        let control = self.control.clone();
        let tx = self.tx.clone();
        if control.is_running() {
            let cancel = CancelFlag::new();
            self.handle.spawn(async move {
                let events: Arc<dyn EventSink> = Arc::new(UiEvents::tui_only(tx.clone()));
                match runtime
                    .generate_handoff(&session_id, focus.as_deref(), &cancel)
                    .await
                {
                    Ok(Some(draft)) => {
                        if control.is_running() {
                            control.set_pending_handoff(draft);
                        } else {
                            let res = runtime
                                .commit_handoff(&draft, &events)
                                .await
                                .map_err(|e| e.to_string());
                            tx.send(UiEvent::Finished(TaskEnd::Handoff(res)));
                        }
                    }
                    Ok(None) => {
                        tx.send(UiEvent::Report("nothing to hand off yet".to_owned()));
                    }
                    Err(HrnError::Cancelled) => {}
                    Err(err) => {
                        tx.send(UiEvent::Report(format!("handoff failed: {err}")));
                    }
                }
            });
            return Ok(true);
        }

        let cancel = self.fresh_cancel();
        self.handle.spawn(async move {
            let events: Arc<dyn EventSink> = Arc::new(UiEvents::tui_only(tx.clone()));
            let result = match runtime
                .handoff_now(&session_id, focus.as_deref(), &cancel, &events)
                .await
            {
                Ok(covered) => Ok(covered.is_some()),
                Err(HrnError::Cancelled) => Err("interrupted".to_owned()),
                Err(err) => Err(err.to_string()),
            };
            tx.send(UiEvent::Finished(TaskEnd::Handoff(result)));
        });
        Ok(true)
    }

    /// Checkout `node_id` (or `None` to rewind past root), optionally summarizing abandoned nodes.
    pub(super) fn checkout_optional_live(
        &mut self,
        node_id: Option<&str>,
        summarize: bool,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        self.ensure_writable()?;
        self.close_collab_host("host navigated session tree");
        let id = self
            .session
            .as_deref()
            .ok_or_else(|| HrnError::Store("no active session".to_owned()))?
            .to_owned();
        if !summarize {
            return self.runtime.switch_head_optional(&id, node_id);
        }
        let runtime = self.runtime.clone();
        let target = node_id.map(str::to_owned);
        let cancel = CancelFlag::new();
        self.handle.block_on(async move {
            runtime
                .switch_with_branch_summary(&id, target.as_deref(), true, &cancel)
                .await
        })
    }

    /// Append a `DagPayload::Label` node (`Shift+L` in tree/branch overlay).
    pub(super) fn set_label_live(&mut self, node_id: &str, label: Option<&str>) -> Result {
        self.ensure_writable()?;
        let id = self
            .session
            .as_deref()
            .ok_or_else(|| HrnError::Store("no active session".to_owned()))?;
        self.runtime
            .set_label(id, node_id, label.map(str::to_owned))?;
        Ok(())
    }

    /// Start one `/btw` side request (only one BTW runs at a time).
    pub(super) fn start_btw_live(
        &mut self,
        topic_id: Option<&str>,
        question: &str,
    ) -> Result<String> {
        if self.btw_active.swap(true, Ordering::SeqCst) {
            return Err(HrnError::Internal(
                "a /btw question is already running".to_owned(),
            ));
        }
        let session_id = match self.active_session_id() {
            Some(id) => id,
            None => match self.session_for(question) {
                Ok(id) => id,
                Err(err) => {
                    self.btw_active.store(false, Ordering::SeqCst);
                    return Err(err);
                }
            },
        };
        let topic_id = topic_id.map_or_else(
            || {
                let n = BTW_SEQ.fetch_add(1, Ordering::Relaxed);
                format!("btw_{:x}_{n}", std::process::id())
            },
            str::to_owned,
        );
        let cancel = CancelFlag::new();
        self.btw_cancel = Some(cancel.clone());
        let runtime = self.runtime.clone();
        let tx = self.tx.clone();
        let active = Arc::clone(&self.btw_active);
        let q = question.to_owned();
        let tid = topic_id.clone();
        let delta_tx = tx.clone();
        let delta_tid = tid.clone();
        let on_delta = Arc::new(move |delta: String| {
            let _ = delta_tx.send(UiEvent::BtwDelta {
                topic_id: delta_tid.clone(),
                delta,
            });
        });
        self.handle.spawn(async move {
            let result = runtime
                .run_btw(&session_id, Some(&tid), &q, &cancel, on_delta)
                .await;
            active.store(false, Ordering::SeqCst);
            match result {
                Ok(topic) => {
                    let _ = tx.send(UiEvent::BtwDone(topic));
                }
                Err(err) => {
                    let topic = BtwTopic {
                        id: tid,
                        title: q.chars().take(60).collect(),
                        exchanges: vec![BtwExchange {
                            question: q,
                            answer: String::new(),
                            status: BtwStatus::Error(err.to_string()),
                            injected: false,
                        }],
                        updated_at_ms: 0,
                    };
                    let _ = tx.send(UiEvent::BtwDone(topic));
                }
            }
        });
        Ok(topic_id)
    }

    /// Cancel an active `/btw` stream without interrupting the main turn.
    pub(super) fn cancel_btw_live(&mut self) {
        if let Some(cancel) = &self.btw_cancel {
            cancel.cancel();
        }
    }

    /// Load persisted `/btw` topics for the active session.
    pub(super) fn list_btw_live(&self) -> Result<Vec<BtwTopic>> {
        let Some(id) = self.active_session_id() else {
            return Ok(Vec::new());
        };
        self.runtime.list_btw_topics(&id)
    }

    /// Save an updated `/btw` topic (e.g. after marking `<inject>` as sent).
    pub(super) fn save_btw_live(&self, topic: &BtwTopic) -> Result {
        let Some(id) = self.active_session_id() else {
            return Ok(());
        };
        self.runtime.save_btw_topic(&id, topic)
    }
}
