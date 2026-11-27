//! Opt-in Mobile Pocket Supervision HTTP/1.1 + SSE server.
//!
//! Unlike the loopback-only [`crate::DaemonServer`], [`MobileWebServer`] explicitly permits
//! binding to `0.0.0.0`, Tailscale, or LAN interfaces when started by `hrn remote pair` /
//! `hrn remote web` / `/remote`. Access is gated by single-use 256-bit pairing codes exchanged
//! for constant-time-verified bearer session tokens with explicit `view` vs `control` RBAC.

use crate::mobile_web_auth::{MobileRole, PairingStore};
use crate::protocol::{
    CreateSessionRequest, DaemonEvent, ErrorResponse, OkResponse, SessionSummaryDto,
    StartTurnRequest, SteerRequest,
};
use bytes::Bytes;
use hrn_kernel::{
    ApprovalRequest, AskRequest, BoxFuture, DagNode, HrnError, PermissionMode, Result,
    ThinkingLevel,
};
use http::{Method, Request, Response, StatusCode, header};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};

/// Single-file Mobile Pocket Supervision PWA HTML, also reusable by `hrn-relay` on `GET /r/<room>`.
pub const MOBILE_PWA_HTML: &str = include_str!("../data/mobile_pwa.html");

const MAX_REQUEST_BODY: usize = 2 * 1024 * 1024;
const MAX_CONCURRENT_CONNS: usize = 128;
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const PAIR_FAIL_WINDOW: Duration = Duration::from_mins(1);
const PAIR_FAIL_LIMIT: usize = 30;

const CSP_HEADER: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data:; manifest-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// PWA web app manifest JSON, also reusable by `hrn-relay` on `GET /manifest.json`.
pub const MOBILE_MANIFEST_JSON: &str = r##"{"name":"hrn remote","short_name":"hrn","start_url":"/","display":"standalone","background_color":"#0b0e14","theme_color":"#0b0e14"}"##;

const MANIFEST_JSON: &str = MOBILE_MANIFEST_JSON;

/// Host metadata exposed to paired mobile clients via `POST /api/pair` and `GET /api/info`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileHostInfo {
    /// Human-readable host name.
    pub host_name: String,
    /// Operating mode (`"daemon"` for multi-session or `"live"` for single-session attach).
    pub mode: String,
    /// Currently active session identifier, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    /// Available model aliases configured on the host.
    pub models: Vec<String>,
    /// `hrn` version string.
    pub version: String,
}

/// Full session snapshot returned by `GET /api/sessions/{id}/transcript`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTranscriptDto {
    /// Session summary metadata.
    pub meta: SessionSummaryDto,
    /// Whether a turn is currently running.
    pub busy: bool,
    /// Active thinking effort level.
    pub thinking: ThinkingLevel,
    /// Active tool permission mode.
    pub permission_mode: PermissionMode,
    /// Chronological DAG nodes for the active branch.
    pub nodes: Vec<DagNode>,
    /// Outstanding approval prompts awaiting user decision.
    #[serde(default)]
    pub pending_approvals: Vec<ApprovalRequest>,
    /// Outstanding structured `ask` forms awaiting user answers.
    #[serde(default)]
    pub pending_asks: Vec<AskRequest>,
}

/// Request body for `POST /api/sessions/{id}/approve`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileApproveRequest {
    /// Tool call or approval prompt identifier.
    pub call_id: String,
    /// `true` to allow the tool call, `false` to deny it.
    pub approved: bool,
    /// When `true` and `approved`, grant persistent/session-wide tool approval where permitted.
    #[serde(default)]
    pub always: bool,
}

/// Request body for `POST /api/sessions/{id}/ask`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileAskAnswerRequest {
    /// Identifier of the pending [`AskRequest`].
    pub id: String,
    /// Ordered answers corresponding to each field of the form.
    pub answers: Vec<String>,
}

/// Request body for `POST /api/pair`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairRequest {
    /// Single-use pairing code extracted from `#pair=<code>` or entered manually.
    pub code: String,
}

/// Response body for `POST /api/pair`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResponse {
    /// Fresh 256-bit bearer token for `Authorization: Bearer <token>`.
    pub token: String,
    /// Authorized role (`"view"` or `"control"`).
    pub role: MobileRole,
    /// Host metadata snapshot.
    pub host: MobileHostInfo,
}

/// Response body for `GET /api/info`, flattening [`MobileHostInfo`] with the caller's role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileInfoResponse {
    /// Host metadata fields.
    #[serde(flatten)]
    pub host: MobileHostInfo,
    /// Caller's authorized role (`"view"` or `"control"`).
    pub role: MobileRole,
}

/// Backend port connecting [`MobileWebServer`] to a local `hrn` daemon or live TUI session.
pub trait MobileWebBackend: Send + Sync + 'static {
    /// Static or dynamic host metadata.
    fn host_info(&self) -> MobileHostInfo;
    /// List known sessions (most recently updated first).
    fn list_sessions(&self) -> BoxFuture<Result<Vec<SessionSummaryDto>>>;
    /// Create a new session on the host.
    fn create_session(&self, req: CreateSessionRequest) -> BoxFuture<Result<SessionSummaryDto>>;
    /// Fetch the full transcript and pending interactive prompts for `session_id`.
    fn session_transcript(&self, session_id: &str) -> BoxFuture<Result<SessionTranscriptDto>>;
    /// Start a new turn on `session_id`.
    fn start_turn(&self, session_id: String, req: StartTurnRequest) -> BoxFuture<Result<()>>;
    /// Inject a steering message into a running turn on `session_id`.
    fn steer(&self, session_id: String, text: String) -> BoxFuture<Result<()>>;
    /// Interrupt an active turn on `session_id`.
    fn interrupt(&self, session_id: String) -> BoxFuture<Result<()>>;
    /// Answer a pending approval request for `session_id`.
    fn approve(&self, session_id: String, req: MobileApproveRequest) -> BoxFuture<Result<()>>;
    /// Answer a pending `ask` form for `session_id`.
    fn answer_ask(&self, session_id: String, req: MobileAskAnswerRequest) -> BoxFuture<Result<()>>;
    /// Subscribe to live [`DaemonEvent`] updates for `session_id`.
    fn subscribe(&self, session_id: String) -> BoxFuture<Result<broadcast::Receiver<DaemonEvent>>>;
}

enum RespBody {
    Once(Option<Bytes>),
    Stream(mpsc::Receiver<Bytes>),
}

impl Body for RespBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        match &mut *self {
            Self::Once(slot) => Poll::Ready(slot.take().map(|b| Ok(Frame::data(b)))),
            Self::Stream(rx) => match rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

struct ServerState {
    backend: Arc<dyn MobileWebBackend>,
    store: PairingStore,
    pair_failures: Mutex<VecDeque<Instant>>,
    shutdown_rx: watch::Receiver<bool>,
}

impl ServerState {
    fn is_pair_rate_limited(&self) -> bool {
        let now = Instant::now();
        let mut guard = self
            .pair_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while guard
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= PAIR_FAIL_WINDOW)
        {
            guard.pop_front();
        }
        guard.len() >= PAIR_FAIL_LIMIT
    }

    fn record_pair_failure(&self) {
        let mut guard = self
            .pair_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push_back(Instant::now());
    }
}

/// Handle returned by [`MobileWebServer::bind`] for pairing code minting, status, and shutdown.
pub struct MobileWebHandle {
    addr: SocketAddr,
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl MobileWebHandle {
    /// Bound socket address of the mobile web server.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Mint a single-use pairing code granting `role` for `ttl`.
    pub fn create_pairing(&self, role: MobileRole, ttl: Duration) -> Result<String> {
        self.state.store.create_pairing(role, ttl)
    }

    /// Number of currently active paired mobile sessions.
    pub fn connected_count(&self) -> usize {
        self.state.store.connected_count()
    }

    /// Revoke all outstanding pairing codes and active mobile sessions immediately.
    pub fn revoke_all(&self) {
        self.state.store.revoke_all();
    }

    /// Trigger graceful shutdown and wait for the accept loop to exit.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.task.await;
    }
}

/// Opt-in Mobile Pocket Supervision HTTP + SSE server.
pub struct MobileWebServer;

impl MobileWebServer {
    /// Bind and start serving on `listen_addr` (allows loopback, Tailscale, LAN, or `0.0.0.0`).
    pub async fn bind(
        listen_addr: SocketAddr,
        backend: Arc<dyn MobileWebBackend>,
    ) -> Result<MobileWebHandle> {
        let listener = TcpListener::bind(listen_addr).await?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let state = Arc::new(ServerState {
            backend,
            store: PairingStore::new(),
            pair_failures: Mutex::new(VecDeque::new()),
            shutdown_rx: shutdown_rx.clone(),
        });
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNS));
        let task_state = Arc::clone(&state);

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else {
                            break;
                        };
                        let Ok(permit) = Arc::clone(&sem).try_acquire_owned() else {
                            drop(stream);
                            continue;
                        };
                        let st = Arc::clone(&task_state);
                        tokio::spawn(async move {
                            let _permit = permit;
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req| {
                                let st = Arc::clone(&st);
                                async move { Ok::<_, Infallible>(handle_request(st, req).await) }
                            });
                            let _ = http1::Builder::new().serve_connection(io, svc).await;
                        });
                    }
                }
            }
        });

        Ok(MobileWebHandle {
            addr,
            state,
            shutdown_tx,
            task,
        })
    }
}

async fn handle_request(state: Arc<ServerState>, req: Request<Incoming>) -> Response<RespBody> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    match (&method, segs.as_slice()) {
        (&Method::GET, []) => html_response(),
        (&Method::GET, ["manifest.json"]) => manifest_response(),
        (&Method::POST, ["api", "pair"]) => handle_pair(&state, req).await,
        _ => route_authenticated(&state, &method, segs.as_slice(), req).await,
    }
}

async fn handle_pair(state: &ServerState, req: Request<Incoming>) -> Response<RespBody> {
    if state.is_pair_rate_limited() {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed pairing attempts; retry later",
        );
    }
    let body = match read_json::<PairRequest>(req).await {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let Some((token, role)) = state.store.redeem_pairing(&body.code) else {
        state.record_pair_failure();
        return json_error(StatusCode::UNAUTHORIZED, "invalid or expired pairing code");
    };
    let host = state.backend.host_info();
    json_ok(StatusCode::OK, &PairResponse { token, role, host })
}

async fn route_authenticated(
    state: &ServerState,
    method: &Method,
    segs: &[&str],
    req: Request<Incoming>,
) -> Response<RespBody> {
    let Some(role) = extract_bearer(req.headers().get(header::AUTHORIZATION))
        .and_then(|tok| state.store.verify_session(tok))
    else {
        return json_error(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    };

    match (method, segs) {
        (&Method::GET, ["api", "info"]) => json_ok(
            StatusCode::OK,
            &MobileInfoResponse {
                host: state.backend.host_info(),
                role,
            },
        ),
        (&Method::GET, ["api", "sessions"]) => match state.backend.list_sessions().await {
            Ok(list) => json_ok(StatusCode::OK, &list),
            Err(err) => err_to_response(&err),
        },
        (&Method::GET, ["api", "sessions", id, "transcript"]) => {
            match state.backend.session_transcript(id).await {
                Ok(tr) => json_ok(StatusCode::OK, &tr),
                Err(err) => err_to_response(&err),
            }
        }
        (&Method::GET, ["api", "sessions", id, "events"]) => {
            match state.backend.subscribe((*id).to_owned()).await {
                Ok(rx) => sse_response(rx, state.shutdown_rx.clone()),
                Err(err) => err_to_response(&err),
            }
        }
        (&Method::POST, ["api", "sessions", ..]) => {
            if !role.can_control() {
                return json_error(StatusCode::FORBIDDEN, "view-only session cannot mutate");
            }
            route_control_post(state, segs, req).await
        }
        _ => json_error(StatusCode::NOT_FOUND, "unknown mobile API route"),
    }
}

async fn route_control_post(
    state: &ServerState,
    segs: &[&str],
    req: Request<Incoming>,
) -> Response<RespBody> {
    match segs {
        ["api", "sessions"] => match read_json::<CreateSessionRequest>(req).await {
            Ok(body) => match state.backend.create_session(body).await {
                Ok(created) => json_ok(StatusCode::CREATED, &created),
                Err(err) => err_to_response(&err),
            },
            Err(resp) => *resp,
        },
        ["api", "sessions", id, "turns"] => match read_json::<StartTurnRequest>(req).await {
            Ok(body) => match state.backend.start_turn((*id).to_owned(), body).await {
                Ok(()) => json_ok(StatusCode::ACCEPTED, &OkResponse { ok: true }),
                Err(err) => err_to_response(&err),
            },
            Err(resp) => *resp,
        },
        ["api", "sessions", id, "steer"] => match read_json::<SteerRequest>(req).await {
            Ok(body) => match state.backend.steer((*id).to_owned(), body.text).await {
                Ok(()) => json_ok(StatusCode::OK, &OkResponse { ok: true }),
                Err(err) => err_to_response(&err),
            },
            Err(resp) => *resp,
        },
        ["api", "sessions", id, "interrupt"] => {
            match state.backend.interrupt((*id).to_owned()).await {
                Ok(()) => json_ok(StatusCode::OK, &OkResponse { ok: true }),
                Err(err) => err_to_response(&err),
            }
        }
        ["api", "sessions", id, "approve"] => match read_json::<MobileApproveRequest>(req).await {
            Ok(body) => match state.backend.approve((*id).to_owned(), body).await {
                Ok(()) => json_ok(StatusCode::OK, &OkResponse { ok: true }),
                Err(err) => err_to_response(&err),
            },
            Err(resp) => *resp,
        },
        ["api", "sessions", id, "ask"] => match read_json::<MobileAskAnswerRequest>(req).await {
            Ok(body) => match state.backend.answer_ask((*id).to_owned(), body).await {
                Ok(()) => json_ok(StatusCode::OK, &OkResponse { ok: true }),
                Err(err) => err_to_response(&err),
            },
            Err(resp) => *resp,
        },
        _ => json_error(StatusCode::NOT_FOUND, "unknown mobile API route"),
    }
}

fn sse_response(
    mut rx: broadcast::Receiver<DaemonEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Response<RespBody> {
    let (tx, body_rx) = mpsc::channel::<Bytes>(64);
    tokio::spawn(async move {
        if tx
            .send(Bytes::from_static(b": connected\n\n"))
            .await
            .is_err()
        {
            return;
        }
        let mut ping = tokio::time::interval(SSE_KEEPALIVE_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = ping.tick() => {
                    if tx.send(Bytes::from_static(b": ping\n\n")).await.is_err() {
                        break;
                    }
                }
                ev = rx.recv() => match ev {
                    Ok(item) => {
                        if let Ok(json) = serde_json::to_string(&item) {
                            let frame = format!("id: {}\nevent: daemon\ndata: {json}\n\n", item.seq);
                            if tx.send(Bytes::from(frame)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let frame = format!(": lagged {n} events\n\n");
                        if tx.send(Bytes::from(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header("x-accel-buffering", "no")
        .body(RespBody::Stream(body_rx))
        .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "sse response error"))
}

fn html_response() -> Response<RespBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(header::X_FRAME_OPTIONS, "DENY")
        .header(header::CONTENT_SECURITY_POLICY, CSP_HEADER)
        .body(RespBody::Once(Some(Bytes::from_static(
            MOBILE_PWA_HTML.as_bytes(),
        ))))
        .unwrap_or_else(|_| Response::new(RespBody::Once(None)))
}

fn manifest_response() -> Response<RespBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "application/manifest+json; charset=utf-8",
        )
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(RespBody::Once(Some(Bytes::from_static(
            MANIFEST_JSON.as_bytes(),
        ))))
        .unwrap_or_else(|_| Response::new(RespBody::Once(None)))
}

async fn read_json<T: DeserializeOwned>(
    req: Request<Incoming>,
) -> std::result::Result<T, Box<Response<RespBody>>> {
    let mut body = req.into_body();
    let mut buf = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| {
            Box::new(json_error(
                StatusCode::BAD_REQUEST,
                &format!("read body: {e}"),
            ))
        })?;
        if let Some(chunk) = frame.data_ref() {
            if buf.len().saturating_add(chunk.len()) > MAX_REQUEST_BODY {
                return Err(Box::new(json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body exceeds limit",
                )));
            }
            buf.extend_from_slice(chunk);
        }
    }
    serde_json::from_slice(&buf).map_err(|e| {
        Box::new(json_error(
            StatusCode::BAD_REQUEST,
            &format!("invalid JSON: {e}"),
        ))
    })
}

fn extract_bearer(auth: Option<&http::HeaderValue>) -> Option<&str> {
    let raw = auth.and_then(|v| v.to_str().ok())?;
    raw.strip_prefix("Bearer ").map(str::trim)
}

fn json_ok<T: Serialize>(status: StatusCode, payload: &T) -> Response<RespBody> {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(RespBody::Once(Some(Bytes::from(body))))
        .unwrap_or_else(|_| Response::new(RespBody::Once(None)))
}

fn json_error(status: StatusCode, msg: &str) -> Response<RespBody> {
    json_ok(
        status,
        &ErrorResponse {
            error: msg.to_owned(),
        },
    )
}

fn err_to_response(err: &HrnError) -> Response<RespBody> {
    let status = match err {
        HrnError::Auth(_) => StatusCode::UNAUTHORIZED,
        HrnError::Config(_) | HrnError::Tool(_) => StatusCode::BAD_REQUEST,
        HrnError::Store(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    json_error(status, &err.to_string())
}

#[cfg(test)]
#[path = "mobile_web_tests.rs"]
mod tests;
