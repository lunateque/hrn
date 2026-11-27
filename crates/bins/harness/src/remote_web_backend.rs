//! Multi-session [`MobileWebBackend`] backed directly by `AgentRuntime` + `SessionStore`,
//! powering `hrn remote pair` (`hrn remote web`).

use super::{answers_to_submission, approval_decision_from_mobile, host_display_name};
use hrn_edge_ipc::{
    CreateSessionRequest, DaemonEvent, MobileApproveRequest, MobileAskAnswerRequest,
    MobileHostInfo, MobileWebBackend, SessionSummaryDto, SessionTranscriptDto, StartTurnRequest,
};
use hrn_harness::{AgentRuntime, SessionControl, TurnIo};
use hrn_kernel::{
    AgentEvent, ApprovalDecision, ApprovalGate, ApprovalGrantScope, ApprovalRequest, AskHandler,
    AskRequest, BoxFuture, CancelFlag, EventSink, HrnError, Result, SessionStore, Submission,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};

type PendingApprovalMap = HashMap<String, (ApprovalRequest, oneshot::Sender<ApprovalDecision>)>;
type PendingAskMap = HashMap<String, (AskRequest, oneshot::Sender<Option<Submission>>)>;

struct MobileSessionChan {
    control: SessionControl,
    tx: broadcast::Sender<DaemonEvent>,
    seq: AtomicU64,
    pending_approvals: Mutex<PendingApprovalMap>,
    pending_asks: Mutex<PendingAskMap>,
}

impl MobileSessionChan {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            control: SessionControl::default(),
            tx,
            seq: AtomicU64::new(1),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_asks: Mutex::new(HashMap::new()),
        }
    }

    fn publish(&self, session_id: &str, event: AgentEvent) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(DaemonEvent {
            seq,
            session_id: session_id.to_owned(),
            event,
        });
    }
}

struct MobileEventSink {
    session_id: String,
    chan: Arc<MobileSessionChan>,
}

impl EventSink for MobileEventSink {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        self.chan.publish(&self.session_id, event);
        Box::pin(async { true })
    }
}

struct MobileApprovalGate {
    session_id: String,
    auto_approve: bool,
    chan: Arc<MobileSessionChan>,
}

impl ApprovalGate for MobileApprovalGate {
    fn await_decision(
        &self,
        request: ApprovalRequest,
        cancel: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        let must_ask =
            request.choices.as_slice() == [ApprovalGrantScope::Once] || request.reason.is_some();
        if self.auto_approve && !must_ask {
            return Box::pin(async { Ok(ApprovalDecision::once()) });
        }
        let approval_id = request.id.clone();
        let (tx, rx) = oneshot::channel();
        self.chan
            .pending_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(approval_id.clone(), (request.clone(), tx));
        self.chan
            .publish(&self.session_id, AgentEvent::ApprovalRequested { request });
        let chan = Arc::clone(&self.chan);
        Box::pin(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            tokio::pin!(rx);
            loop {
                tokio::select! {
                    res = &mut rx => {
                        return Ok(res.unwrap_or(ApprovalDecision::Deny));
                    }
                    _ = ticker.tick() => {
                        if cancel.is_cancelled() {
                            chan.pending_approvals
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .remove(&approval_id);
                            return Err(HrnError::Cancelled);
                        }
                    }
                }
            }
        })
    }
}

struct MobileAskHandler {
    session_id: String,
    chan: Arc<MobileSessionChan>,
}

impl AskHandler for MobileAskHandler {
    fn ask(&self, request: AskRequest, cancel: CancelFlag) -> BoxFuture<Result<Submission>> {
        let ask_id = request.id.clone();
        let (tx, rx) = oneshot::channel();
        self.chan
            .pending_asks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ask_id.clone(), (request.clone(), tx));
        self.chan
            .publish(&self.session_id, AgentEvent::AskRequested { request });
        let chan = Arc::clone(&self.chan);
        Box::pin(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            tokio::pin!(rx);
            loop {
                tokio::select! {
                    res = &mut rx => {
                        let maybe = res.map_err(|_| HrnError::Cancelled)?;
                        return maybe.ok_or_else(|| {
                            HrnError::Tool("the user cancelled the form".to_owned())
                        });
                    }
                    _ = ticker.tick() => {
                        if cancel.is_cancelled() {
                            chan.pending_asks
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .remove(&ask_id);
                            return Err(HrnError::Cancelled);
                        }
                    }
                }
            }
        })
    }
}

/// Multi-session [`MobileWebBackend`] backed directly by `AgentRuntime` + `SessionStore`.
pub(crate) struct RuntimeMobileBackend {
    runtime: AgentRuntime,
    store: Arc<dyn SessionStore>,
    default_cwd: PathBuf,
    models: Vec<String>,
    host_name: String,
    channels: Mutex<HashMap<String, Arc<MobileSessionChan>>>,
}

impl RuntimeMobileBackend {
    pub(crate) fn new(
        runtime: AgentRuntime,
        store: Arc<dyn SessionStore>,
        default_cwd: PathBuf,
        models: Vec<String>,
    ) -> Self {
        Self {
            runtime,
            store,
            default_cwd,
            models,
            host_name: host_display_name(),
            channels: Mutex::new(HashMap::new()),
        }
    }

    fn channel(&self, session_id: &str) -> Arc<MobileSessionChan> {
        let mut map = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            map.entry(session_id.to_owned())
                .or_insert_with(|| Arc::new(MobileSessionChan::new())),
        )
    }
}

impl MobileWebBackend for RuntimeMobileBackend {
    fn host_info(&self) -> MobileHostInfo {
        MobileHostInfo {
            host_name: self.host_name.clone(),
            mode: "daemon".to_owned(),
            active_session_id: None,
            models: self.models.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    fn list_sessions(&self) -> BoxFuture<Result<Vec<SessionSummaryDto>>> {
        let store = Arc::clone(&self.store);
        let active: HashSet<String> = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, c)| c.control.is_running())
            .map(|(id, _)| id.clone())
            .collect();
        Box::pin(async move {
            let metas = store.list_sessions(200)?;
            Ok(metas
                .into_iter()
                .map(|m| SessionSummaryDto {
                    active_turn: active.contains(&m.id),
                    id: m.id,
                    title: m.title,
                    cwd: m.cwd,
                    model: m.model,
                    status: m.status,
                    updated_at_ms: u64::try_from(m.updated_at_ms).unwrap_or(0),
                })
                .collect())
        })
    }

    fn create_session(&self, req: CreateSessionRequest) -> BoxFuture<Result<SessionSummaryDto>> {
        let runtime = self.runtime.clone();
        let default_cwd = self.default_cwd.clone();
        Box::pin(async move {
            let raw_cwd = req.cwd.trim();
            let cwd = if raw_cwd.is_empty() || raw_cwd == "." {
                default_cwd
            } else {
                PathBuf::from(raw_cwd)
            };
            if !cwd.is_dir() {
                return Err(HrnError::Config(format!(
                    "`cwd` is not a directory: {}",
                    cwd.display()
                )));
            }
            let title = if req.title.trim().is_empty() {
                "mobile session".to_owned()
            } else {
                req.title
            };
            let meta = runtime.create_session(
                title,
                &cwd,
                req.model,
                req.thinking,
                req.permission_mode,
            )?;
            Ok(SessionSummaryDto {
                id: meta.id,
                title: meta.title,
                cwd: meta.cwd,
                model: meta.model,
                status: meta.status,
                active_turn: false,
                updated_at_ms: u64::try_from(meta.updated_at_ms).unwrap_or(0),
            })
        })
    }

    fn session_transcript(&self, session_id: &str) -> BoxFuture<Result<SessionTranscriptDto>> {
        let store = Arc::clone(&self.store);
        let chan = self.channel(session_id);
        let sid = session_id.to_owned();
        Box::pin(async move {
            let meta = store
                .get_session(&sid)?
                .ok_or_else(|| HrnError::Store(format!("unknown session `{sid}`")))?;
            let nodes = match meta.head_node_id.as_deref() {
                Some(head) => store.load_branch(&meta.id, head)?,
                None => Vec::new(),
            };
            let busy = chan.control.is_running();
            let pending_approvals = chan
                .pending_approvals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .map(|(req, _)| req.clone())
                .collect();
            let pending_asks = chan
                .pending_asks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .map(|(req, _)| req.clone())
                .collect();
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

    fn start_turn(&self, session_id: String, req: StartTurnRequest) -> BoxFuture<Result<()>> {
        let runtime = self.runtime.clone();
        let store = Arc::clone(&self.store);
        let chan = self.channel(&session_id);
        Box::pin(async move {
            if store.get_session(&session_id)?.is_none() {
                return Err(HrnError::Store(format!("unknown session `{session_id}`")));
            }
            let io = TurnIo {
                cancel: CancelFlag::new(),
                events: Arc::new(MobileEventSink {
                    session_id: session_id.clone(),
                    chan: Arc::clone(&chan),
                }),
                approval: Some(Arc::new(MobileApprovalGate {
                    session_id: session_id.clone(),
                    auto_approve: req.auto_approve,
                    chan: Arc::clone(&chan),
                })),
                ask: Some(Arc::new(MobileAskHandler {
                    session_id: session_id.clone(),
                    chan: Arc::clone(&chan),
                })),
            };
            tokio::spawn(async move {
                let _ = runtime
                    .run_turn(&session_id, &req.prompt, &chan.control, io)
                    .await;
            });
            Ok(())
        })
    }

    fn steer(&self, session_id: String, text: String) -> BoxFuture<Result<()>> {
        let chan = self.channel(&session_id);
        chan.control.push_steer(text);
        Box::pin(async { Ok(()) })
    }

    fn interrupt(&self, session_id: String) -> BoxFuture<Result<()>> {
        let chan = self.channel(&session_id);
        chan.control.interrupt();
        Box::pin(async { Ok(()) })
    }

    fn approve(&self, session_id: String, req: MobileApproveRequest) -> BoxFuture<Result<()>> {
        let chan = self.channel(&session_id);
        let pending = chan
            .pending_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&req.call_id);
        match pending {
            Some((approval_req, tx)) => {
                let decision =
                    approval_decision_from_mobile(&approval_req, req.approved, req.always);
                let approved = decision.is_approved();
                let _ = tx.send(decision);
                chan.publish(
                    &session_id,
                    AgentEvent::ApprovalResolved {
                        id: req.call_id,
                        approved,
                    },
                );
                Box::pin(async { Ok(()) })
            }
            None => Box::pin(async move {
                Err(HrnError::Tool(format!(
                    "no pending approval `{}` for session `{session_id}`",
                    req.call_id
                )))
            }),
        }
    }

    fn answer_ask(&self, session_id: String, req: MobileAskAnswerRequest) -> BoxFuture<Result<()>> {
        let chan = self.channel(&session_id);
        let pending = chan
            .pending_asks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&req.id);
        match pending {
            Some((ask_req, tx)) => {
                let submission = answers_to_submission(&ask_req.form, &req.answers);
                let _ = tx.send(Some(submission));
                Box::pin(async { Ok(()) })
            }
            None => Box::pin(async move {
                Err(HrnError::Tool(format!(
                    "no pending ask `{}` for session `{session_id}`",
                    req.id
                )))
            }),
        }
    }

    fn subscribe(&self, session_id: String) -> BoxFuture<Result<broadcast::Receiver<DaemonEvent>>> {
        let store = Arc::clone(&self.store);
        let chan = self.channel(&session_id);
        Box::pin(async move {
            if store.get_session(&session_id)?.is_none() {
                return Err(HrnError::Store(format!("unknown session `{session_id}`")));
            }
            Ok(chan.tx.subscribe())
        })
    }
}
