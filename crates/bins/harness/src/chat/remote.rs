//! Live `/remote` hub bridging the active desktop TUI session with both the direct
//! LAN/Tailscale mobile PWA (`MobileWebServer`) and the E2E-encrypted `hrn-relay` room.

use super::remote_async::PendingRemoteFinish;
use crate::collab::CollabHostBridge;
use crate::remote_web::{answers_to_submission, approval_decision_from_mobile, host_display_name};
use hrn_edge_ipc::{
    CreateSessionRequest, DaemonEvent, MobileApproveRequest, MobileAskAnswerRequest,
    MobileHostInfo, MobileWebBackend, MobileWebHandle, SessionSummaryDto, SessionTranscriptDto,
    StartTurnRequest,
};
use hrn_kernel::{
    AgentEvent, ApprovalDecision, ApprovalRequest, AskRequest, BoxFuture, HrnError, Result,
    SessionStore, Submission,
};
use hrn_tui::{RemoteSessionView, UiEvent, UiSender};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::{broadcast, oneshot};

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// First-responder-wins channel shared between the desktop TUI approval dock and mobile clients.
pub(super) type SharedApprovalReply = Arc<Mutex<Option<oneshot::Sender<ApprovalDecision>>>>;
/// First-responder-wins channel shared between the desktop TUI `ask` panel and mobile clients.
pub(super) type SharedAskReply = Arc<Mutex<Option<oneshot::Sender<Option<Submission>>>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActiveTransport {
    None,
    Direct,
    Relay,
}

/// Live hub shared across `ChatBackend`, turn I/O bridges, `MobileWebServer`, and `CollabHost`.
pub(crate) struct RemoteLiveHub {
    tx: UiSender,
    store: Arc<dyn SessionStore>,
    host_name: String,
    models: Mutex<Vec<String>>,
    active_session: Mutex<Option<String>>,
    busy: AtomicBool,
    events_tx: broadcast::Sender<DaemonEvent>,
    seq: AtomicU64,
    pending_approvals: Mutex<HashMap<String, (ApprovalRequest, SharedApprovalReply)>>,
    pending_asks: Mutex<HashMap<String, (AskRequest, SharedAskReply)>>,
    collab_host: Mutex<Option<CollabHostBridge>>,
}

impl RemoteLiveHub {
    pub(super) fn new(tx: UiSender, store: Arc<dyn SessionStore>, models: Vec<String>) -> Self {
        let (events_tx, _) = broadcast::channel(512);
        Self {
            tx,
            store,
            host_name: host_display_name(),
            models: Mutex::new(models),
            active_session: Mutex::new(None),
            busy: AtomicBool::new(false),
            events_tx,
            seq: AtomicU64::new(1),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_asks: Mutex::new(HashMap::new()),
            collab_host: Mutex::new(None),
        }
    }

    pub(super) fn set_active_session(&self, id: Option<String>) {
        *lock(&self.active_session) = id;
    }

    pub(super) fn active_session_id(&self) -> Option<String> {
        lock(&self.active_session).clone()
    }

    pub(super) fn set_models(&self, models: Vec<String>) {
        *lock(&self.models) = models;
    }

    pub(super) fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::Relaxed);
    }

    pub(super) fn set_collab_host(&self, bridge: Option<CollabHostBridge>) {
        *lock(&self.collab_host) = bridge;
    }

    fn broadcast_to_collab(&self, event: &AgentEvent) {
        if let Some(bridge) = lock(&self.collab_host)
            .as_ref()
            .filter(|b| !b.host.is_closed())
            .cloned()
        {
            bridge.host.broadcast_event(event.clone());
        }
    }

    /// Publish a live [`AgentEvent`] to all subscribed SSE mobile clients.
    pub(super) fn publish(&self, session_id: &str, event: AgentEvent) {
        match &event {
            AgentEvent::TurnStart { .. } => self.set_busy(true),
            AgentEvent::TurnDone { .. } | AgentEvent::TurnError { .. } => self.set_busy(false),
            _ => {}
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let _ = self.events_tx.send(DaemonEvent {
            seq,
            session_id: session_id.to_owned(),
            event,
        });
    }

    /// Register a pending approval so mobile clients (LAN SSE + E2E relay) can answer it too.
    pub(super) fn register_approval(&self, request: ApprovalRequest, reply: SharedApprovalReply) {
        let sid = if request.session_id.is_empty() {
            self.active_session_id().unwrap_or_default()
        } else {
            request.session_id.clone()
        };
        lock(&self.pending_approvals).insert(request.id.clone(), (request.clone(), reply));
        let ev = AgentEvent::ApprovalRequested { request };
        self.publish(&sid, ev.clone());
        self.broadcast_to_collab(&ev);
    }

    /// Remove a pending approval after desktop resolution or turn cancellation and notify remotes.
    pub(super) fn clear_approval(&self, call_id: &str, approved: Option<bool>) {
        let removed = lock(&self.pending_approvals).remove(call_id);
        let sid = removed
            .as_ref()
            .map(|(req, _)| req.session_id.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| self.active_session_id())
            .unwrap_or_default();
        if let Some(ok) = approved {
            let ev = AgentEvent::ApprovalResolved {
                id: call_id.to_owned(),
                approved: ok,
            };
            self.publish(&sid, ev.clone());
            self.broadcast_to_collab(&ev);
        }
    }

    /// Answer a pending tool approval from a mobile remote client (LAN PWA or E2E relay)
    /// and dismiss the desktop TUI approval dock immediately.
    pub(super) fn resolve_approval_from_remote(
        &self,
        call_id: &str,
        approved: bool,
        always: bool,
    ) -> Result<()> {
        let pending = lock(&self.pending_approvals).remove(call_id);
        let Some((request, reply)) = pending else {
            return Err(HrnError::Tool(format!("no pending approval `{call_id}`")));
        };
        let decision = approval_decision_from_mobile(&request, approved, always);
        let accepted = decision.is_approved();
        if let Some(tx) = lock(&reply).take() {
            let _ = tx.send(decision);
        }
        let sid = if request.session_id.is_empty() {
            self.active_session_id().unwrap_or_default()
        } else {
            request.session_id
        };
        let ev = AgentEvent::ApprovalResolved {
            id: call_id.to_owned(),
            approved: accepted,
        };
        self.publish(&sid, ev.clone());
        self.broadcast_to_collab(&ev);
        let _ = self.tx.send(UiEvent::RemoteApprovalResolved {
            call_id: call_id.to_owned(),
        });
        Ok(())
    }

    /// Register a pending `ask` form so mobile clients (LAN SSE + E2E relay) can answer it too.
    pub(super) fn register_ask(&self, request: AskRequest, reply: SharedAskReply) {
        let sid = if request.session_id.is_empty() {
            self.active_session_id().unwrap_or_default()
        } else {
            request.session_id.clone()
        };
        lock(&self.pending_asks).insert(request.id.clone(), (request.clone(), reply));
        let ev = AgentEvent::AskRequested { request };
        self.publish(&sid, ev.clone());
        self.broadcast_to_collab(&ev);
    }

    /// Remove a pending `ask` form after desktop submission or cancellation.
    pub(super) fn clear_ask(&self, id: &str) {
        lock(&self.pending_asks).remove(id);
    }

    /// Submit a pending `ask` form from a mobile remote client and dismiss the desktop panel.
    pub(super) fn resolve_ask_from_remote(&self, id: &str, answers: &[String]) -> Result<()> {
        let pending = lock(&self.pending_asks).remove(id);
        let Some((request, reply)) = pending else {
            return Err(HrnError::Tool(format!("no pending ask `{id}`")));
        };
        let submission = answers_to_submission(&request.form, answers);
        if let Some(tx) = lock(&reply).take() {
            let _ = tx.send(Some(submission));
        }
        let _ = self
            .tx
            .send(UiEvent::RemoteAskResolved { id: id.to_owned() });
        Ok(())
    }
}

impl MobileWebBackend for RemoteLiveHub {
    fn host_info(&self) -> MobileHostInfo {
        MobileHostInfo {
            host_name: self.host_name.clone(),
            mode: "live".to_owned(),
            active_session_id: self.active_session_id(),
            models: lock(&self.models).clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    fn list_sessions(&self) -> BoxFuture<Result<Vec<SessionSummaryDto>>> {
        let store = Arc::clone(&self.store);
        let active = self.active_session_id();
        let busy = self.busy.load(Ordering::Relaxed);
        Box::pin(async move {
            let Some(sid) = active else {
                return Ok(Vec::new());
            };
            let Some(meta) = store.get_session(&sid)? else {
                return Ok(Vec::new());
            };
            Ok(vec![SessionSummaryDto {
                id: meta.id,
                title: meta.title,
                cwd: meta.cwd,
                model: meta.model,
                status: meta.status,
                active_turn: busy,
                updated_at_ms: u64::try_from(meta.updated_at_ms).unwrap_or(0),
            }])
        })
    }

    fn create_session(&self, _req: CreateSessionRequest) -> BoxFuture<Result<SessionSummaryDto>> {
        Box::pin(async {
            Err(HrnError::Config(
                "live /remote mirrors the active desktop TUI session · run `hrn remote pair` for multi-session mode"
                    .to_owned(),
            ))
        })
    }

    fn session_transcript(&self, session_id: &str) -> BoxFuture<Result<SessionTranscriptDto>> {
        let store = Arc::clone(&self.store);
        let sid = session_id.to_owned();
        let busy = self.busy.load(Ordering::Relaxed);
        let pending_approvals = self
            .pending_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|(req, _)| req.clone())
            .collect();
        let pending_asks = self
            .pending_asks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|(req, _)| req.clone())
            .collect();
        Box::pin(async move {
            let meta = store
                .get_session(&sid)?
                .ok_or_else(|| HrnError::Store(format!("unknown session `{sid}`")))?;
            let nodes = match meta.head_node_id.as_deref() {
                Some(head) => store.load_branch(&meta.id, head)?,
                None => Vec::new(),
            };
            let summary = SessionSummaryDto {
                id: meta.id,
                title: meta.title,
                cwd: meta.cwd,
                model: meta.model,
                status: meta.status,
                active_turn: busy,
                updated_at_ms: u64::try_from(meta.updated_at_ms).unwrap_or(0),
            };
            Ok(SessionTranscriptDto {
                meta: summary,
                busy,
                thinking: meta.thinking,
                permission_mode: meta.permission_mode,
                nodes,
                pending_approvals,
                pending_asks,
            })
        })
    }

    fn start_turn(&self, _session_id: String, req: StartTurnRequest) -> BoxFuture<Result<()>> {
        let sent = self.tx.send(UiEvent::RemotePrompt { text: req.prompt });
        Box::pin(async move {
            if sent {
                Ok(())
            } else {
                Err(HrnError::Cancelled)
            }
        })
    }

    fn steer(&self, _session_id: String, text: String) -> BoxFuture<Result<()>> {
        let sent = self.tx.send(UiEvent::RemoteSteer { text });
        Box::pin(async move {
            if sent {
                Ok(())
            } else {
                Err(HrnError::Cancelled)
            }
        })
    }

    fn interrupt(&self, _session_id: String) -> BoxFuture<Result<()>> {
        let sent = self.tx.send(UiEvent::RemoteAbort);
        Box::pin(async move {
            if sent {
                Ok(())
            } else {
                Err(HrnError::Cancelled)
            }
        })
    }

    fn approve(&self, _session_id: String, req: MobileApproveRequest) -> BoxFuture<Result<()>> {
        let res = self.resolve_approval_from_remote(&req.call_id, req.approved, req.always);
        Box::pin(async move { res })
    }

    fn answer_ask(
        &self,
        _session_id: String,
        req: MobileAskAnswerRequest,
    ) -> BoxFuture<Result<()>> {
        let res = self.resolve_ask_from_remote(&req.id, &req.answers);
        Box::pin(async move { res })
    }

    fn subscribe(
        &self,
        _session_id: String,
    ) -> BoxFuture<Result<broadcast::Receiver<DaemonEvent>>> {
        let rx = self.events_tx.subscribe();
        Box::pin(async move { Ok(rx) })
    }
}

/// Live remote state owned by [`ChatBackend`](super::ChatBackend).
pub(crate) struct ChatRemoteState {
    pub(super) hub: Arc<RemoteLiveHub>,
    pub(super) web: Option<MobileWebHandle>,
    pub(super) active_transport: ActiveTransport,
    pub(super) relay_owned: bool,
    pub(super) last_view: Option<RemoteSessionView>,
    pub(super) pending_gen: u64,
    pub(super) pending_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) pending_result: Arc<Mutex<Option<PendingRemoteFinish>>>,
}

impl ChatRemoteState {
    pub(super) fn new(tx: UiSender, store: Arc<dyn SessionStore>, models: Vec<String>) -> Self {
        Self {
            hub: Arc::new(RemoteLiveHub::new(tx, store, models)),
            web: None,
            active_transport: ActiveTransport::None,
            relay_owned: false,
            last_view: None,
            pending_gen: 0,
            pending_task: None,
            pending_result: Arc::new(Mutex::new(None)),
        }
    }

    /// True while a background HTTPS tunnel startup task is still running.
    pub(super) fn is_starting(&self) -> bool {
        self.pending_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    /// Abort any in-flight background tunnel startup and invalidate any queued completion.
    pub(super) fn cancel_pending(&mut self) -> bool {
        let was_active = self.is_starting();
        self.pending_gen = self.pending_gen.wrapping_add(1);
        *lock(&self.pending_result) = None;
        if let Some(task) = self.pending_task.take() {
            task.abort();
        }
        was_active
    }
}
