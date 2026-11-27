//! The UI bridges: runtime events, approvals and ask forms forwarded to the TUI (and any active
//! `/remote` mobile supervision clients), plus the turn driver a submitted prompt runs as a task.

use super::CANCEL_POLL;
use super::remote::{RemoteLiveHub, SharedApprovalReply, SharedAskReply};
use crate::collab::{CollabEventSink, CollabHostBridge};
use hrn_harness::{AgentRuntime, SessionControl, TurnIo};
use hrn_kernel::{
    AgentEvent, ApprovalDecision, ApprovalGate, ApprovalRequest, AskHandler, AskRequest, BoxFuture,
    CancelFlag, EventSink, HrnError, ImageAttachment, Result, SessionStore, Submission,
};
use hrn_tui::{ApprovalAsk, AskPrompt, TaskEnd, TurnOutcome, UiEvent, UiSender};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Forwards runtime events to the TUI and optionally to the live `/remote` hub.
pub(crate) struct UiEvents {
    tx: UiSender,
    session_id: String,
    remote: Option<Arc<RemoteLiveHub>>,
}

impl UiEvents {
    pub(crate) fn new(
        tx: UiSender,
        session_id: String,
        remote: Option<Arc<RemoteLiveHub>>,
    ) -> Self {
        Self {
            tx,
            session_id,
            remote,
        }
    }

    pub(crate) fn tui_only(tx: UiSender) -> Self {
        Self {
            tx,
            session_id: String::new(),
            remote: None,
        }
    }
}

impl EventSink for UiEvents {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        if let Some(hub) = &self.remote {
            hub.publish(&self.session_id, event.clone());
        }
        let alive = self.tx.send(UiEvent::Agent(event));
        Box::pin(async move { alive })
    }
}

/// Asks the user through the TUI (and any active `/remote` mobile client — first responder wins).
/// A dropped ask or a tripped cancel flag cancels the turn.
pub(crate) struct UiGate {
    tx: UiSender,
    remote: Option<Arc<RemoteLiveHub>>,
}

impl UiGate {
    pub(crate) fn new(tx: UiSender, remote: Option<Arc<RemoteLiveHub>>) -> Self {
        Self { tx, remote }
    }
}

impl ApprovalGate for UiGate {
    fn await_decision(
        &self,
        request: ApprovalRequest,
        cancel: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        let (reply_tx, answer) = oneshot::channel();
        let shared: SharedApprovalReply = Arc::new(Mutex::new(Some(reply_tx)));
        let call_id = request.id.clone();
        if let Some(hub) = &self.remote {
            hub.register_approval(request.clone(), Arc::clone(&shared));
        }
        let reply_desktop = Arc::clone(&shared);
        let hub_desktop = self.remote.clone();
        let id_desktop = call_id.clone();
        let ask = ApprovalAsk::new(request, move |decision| {
            let approved = decision.is_approved();
            let sent = reply_desktop
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .is_some_and(|tx| tx.send(decision).is_ok());
            if sent && let Some(hub) = hub_desktop {
                hub.clear_approval(&id_desktop, Some(approved));
            }
        });
        let delivered = self.tx.send(UiEvent::Approval(ask));
        let hub_cancel = self.remote.clone();
        Box::pin(async move {
            if !delivered {
                if let Some(hub) = &hub_cancel {
                    hub.clear_approval(&call_id, Some(false));
                }
                return Err(HrnError::Cancelled);
            }
            let res = tokio::select! {
                answer = answer => answer.map_err(|_| HrnError::Cancelled),
                () = cancelled(&cancel) => Err(HrnError::Cancelled),
            };
            if res.is_err()
                && let Some(hub) = &hub_cancel
            {
                hub.clear_approval(&call_id, Some(false));
            }
            res
        })
    }
}

/// Presents `ask` forms inline in the TUI (and to any active `/remote` mobile client).
pub(crate) struct UiAsk {
    tx: UiSender,
    remote: Option<Arc<RemoteLiveHub>>,
}

impl UiAsk {
    pub(crate) fn new(tx: UiSender, remote: Option<Arc<RemoteLiveHub>>) -> Self {
        Self { tx, remote }
    }
}

impl AskHandler for UiAsk {
    fn ask(&self, request: AskRequest, cancel: CancelFlag) -> BoxFuture<Result<Submission>> {
        let (reply_tx, answer) = oneshot::channel();
        let shared: SharedAskReply = Arc::new(Mutex::new(Some(reply_tx)));
        let ask_id = request.id.clone();
        if let Some(hub) = &self.remote {
            hub.register_ask(request.clone(), Arc::clone(&shared));
        }
        let reply_desktop = Arc::clone(&shared);
        let hub_desktop = self.remote.clone();
        let id_desktop = ask_id.clone();
        let prompt = AskPrompt::new(request, move |submission| {
            let sent = reply_desktop
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .is_some_and(|tx| tx.send(submission).is_ok());
            if sent && let Some(hub) = hub_desktop {
                hub.clear_ask(&id_desktop);
            }
        });
        let delivered = self.tx.send(UiEvent::Ask(prompt));
        let hub_cancel = self.remote.clone();
        Box::pin(async move {
            if !delivered {
                if let Some(hub) = &hub_cancel {
                    hub.clear_ask(&ask_id);
                }
                return Err(HrnError::Cancelled);
            }
            let maybe = tokio::select! {
                res = answer => res.map_err(|_| HrnError::Cancelled),
                () = cancelled(&cancel) => Err(HrnError::Cancelled),
            };
            let maybe = match maybe {
                Ok(v) => v,
                Err(err) => {
                    if let Some(hub) = &hub_cancel {
                        hub.clear_ask(&ask_id);
                    }
                    return Err(err);
                }
            };
            maybe.ok_or_else(|| HrnError::Tool("the user cancelled the form".to_owned()))
        })
    }
}

pub(crate) async fn cancelled(flag: &CancelFlag) {
    while !flag.is_cancelled() {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

/// Run `prompt`, then any steering that arrived after the turn's last step as a follow-up turn.
#[expect(
    clippy::too_many_arguments,
    reason = "turn driver threads runtime, channels, cancellation, collab, and remote hub"
)]
pub(crate) async fn run_turns(
    runtime: AgentRuntime,
    session_id: String,
    mut prompt: String,
    mut images: Vec<ImageAttachment>,
    control: SessionControl,
    tx: UiSender,
    cancel: CancelFlag,
    collab_host: Option<CollabHostBridge>,
    remote_hub: Option<Arc<RemoteLiveHub>>,
    store: Arc<dyn SessionStore>,
) -> TaskEnd {
    let ui_events: Arc<dyn EventSink> = Arc::new(UiEvents::new(
        tx.clone(),
        session_id.clone(),
        remote_hub.clone(),
    ));
    let events: Arc<dyn EventSink> = match collab_host.clone() {
        Some(bridge) => Arc::new(CollabEventSink {
            inner: ui_events,
            bridge,
            store: Arc::clone(&store),
        }),
        None => ui_events,
    };
    let io = TurnIo {
        cancel: cancel.clone(),
        events,
        approval: Some(Arc::new(UiGate::new(tx.clone(), remote_hub.clone()))),
        ask: Some(Arc::new(UiAsk::new(tx, remote_hub))),
    };
    loop {
        let turn_images = std::mem::take(&mut images);
        let outcome = match runtime
            .run_turn_with_images(&session_id, &prompt, turn_images, &control, io.clone())
            .await
        {
            Ok(_) => {
                if let Some(bridge) = &collab_host {
                    bridge.sync_new_nodes(store.as_ref());
                }
                let mut late = control.drain_steers();
                if let Some(follow) = control.pop_follow_up() {
                    late.push(follow);
                }
                if late.is_empty() || cancel.is_cancelled() {
                    TurnOutcome::Done
                } else {
                    prompt = late.join("\n\n");
                    continue;
                }
            }
            Err(HrnError::Cancelled) => TurnOutcome::Interrupted,
            Err(err) => TurnOutcome::Failed(err.to_string()),
        };
        if let Some(bridge) = &collab_host {
            bridge.sync_new_nodes(store.as_ref());
        }
        return TaskEnd::Turn {
            outcome,
            unsent_steers: control.drain_steers(),
        };
    }
}

/// Continue an interrupted session turn (`--restore`), then drain any follow-ups that arrive.
pub(crate) async fn run_restore(
    runtime: AgentRuntime,
    session_id: String,
    control: SessionControl,
    tx: UiSender,
    cancel: CancelFlag,
    remote_hub: Option<Arc<RemoteLiveHub>>,
    store: Arc<dyn SessionStore>,
) -> TaskEnd {
    let io = TurnIo {
        cancel: cancel.clone(),
        events: Arc::new(UiEvents::new(
            tx.clone(),
            session_id.clone(),
            remote_hub.clone(),
        )),
        approval: Some(Arc::new(UiGate::new(tx.clone(), remote_hub.clone()))),
        ask: Some(Arc::new(UiAsk::new(tx.clone(), remote_hub.clone()))),
    };
    let outcome = match runtime.restore_turn(&session_id, &control, io).await {
        Ok(_) => {
            let mut late = control.drain_steers();
            if let Some(follow) = control.pop_follow_up() {
                late.push(follow);
            }
            if !late.is_empty() && !cancel.is_cancelled() {
                return run_turns(
                    runtime,
                    session_id,
                    late.join("\n\n"),
                    Vec::new(),
                    control,
                    tx,
                    cancel,
                    None,
                    remote_hub,
                    store,
                )
                .await;
            }
            TurnOutcome::Done
        }
        Err(HrnError::Cancelled) => TurnOutcome::Interrupted,
        Err(err) => TurnOutcome::Failed(err.to_string()),
    };
    TaskEnd::Turn {
        outcome,
        unsent_steers: control.drain_steers(),
    }
}
