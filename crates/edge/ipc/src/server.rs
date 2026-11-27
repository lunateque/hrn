//! Loopback HTTP/1.1 + SSE daemon server (`127.0.0.1`).
//!
//! Security invariants enforced on every connection:
//! - Binds ONLY loopback interfaces (`127.0.0.1` / `::1`).
//! - Rejects ANY request carrying an `Origin` header (`403 Forbidden`).
//! - Enforces a loopback `Host` header (`127.0.0.1`, `localhost`, `[::1]`).
//! - Requires a valid `Authorization: Bearer <token>` verified in constant time (`401`).

use crate::auth::ct_eq;
use crate::protocol::{
    ApproveRequest, CreateSessionRequest, DaemonEvent, ErrorResponse, HealthResponse, OkResponse,
    RestoreSessionRequest, RestoreSessionResponse, SessionSummaryDto, StartTurnRequest,
    SteerRequest,
};
use bytes::Bytes;
use hrn_kernel::{BoxFuture, HrnError, Result};
use http::{Method, Request, Response, StatusCode, header};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};

const MAX_REQUEST_BODY: usize = 2 * 1024 * 1024;

/// Backend port implemented by the `hrn` binary to connect [`DaemonServer`] to `AgentRuntime`.
pub trait DaemonBackend: Send + Sync + 'static {
    /// List known sessions (most recently updated first).
    fn list_sessions(&self) -> BoxFuture<Result<Vec<SessionSummaryDto>>>;
    /// Create a new session on the daemon host.
    fn create_session(&self, req: CreateSessionRequest) -> BoxFuture<Result<SessionSummaryDto>>;
    /// Start an async agent turn for `session_id`.
    fn start_turn(&self, session_id: String, req: StartTurnRequest) -> BoxFuture<Result<()>>;
    /// Queue a steering message on an active session turn.
    fn steer(&self, session_id: String, text: String) -> BoxFuture<Result<()>>;
    /// Interrupt an active session turn.
    fn interrupt(&self, session_id: String) -> BoxFuture<Result<()>>;
    /// Answer a pending approval request for `session_id`.
    fn approve(&self, session_id: String, req: ApproveRequest) -> BoxFuture<Result<()>>;
    /// Continue an interrupted session turn (`POST /v1/sessions/{id}/restore`).
    fn restore_session(
        &self,
        _session_id: String,
        _req: RestoreSessionRequest,
    ) -> BoxFuture<Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    /// Subscribe to live [`DaemonEvent`] updates for `session_id`.
    fn subscribe(&self, session_id: String) -> BoxFuture<Result<broadcast::Receiver<DaemonEvent>>>;
}

/// Streaming HTTP body backed by an unbounded-free bounded `mpsc` byte channel.
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
    backend: Arc<dyn DaemonBackend>,
    token: String,
    host_key: String,
}

/// Handle returned by [`DaemonServer::bind`] allowing graceful shutdown and address inspection.
pub struct DaemonHandle {
    addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl DaemonHandle {
    /// Bound loopback socket address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Trigger graceful shutdown and wait for the accept loop to exit.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.task.await;
    }
}

/// Loopback HTTP + SSE IPC server.
pub struct DaemonServer;

impl DaemonServer {
    /// Bind and start serving on `listen_addr` (must be loopback, e.g. `127.0.0.1:0`).
    pub async fn bind(
        listen_addr: SocketAddr,
        token: String,
        host_key: String,
        backend: Arc<dyn DaemonBackend>,
    ) -> Result<DaemonHandle> {
        if !listen_addr.ip().is_loopback() {
            return Err(HrnError::Auth(format!(
                "refusing to bind daemon to non-loopback address `{listen_addr}` (use SSH tunneling for remote access)"
            )));
        }
        let listener = TcpListener::bind(listen_addr).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(ServerState {
            backend,
            token,
            host_key,
        });
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

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
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req| {
                                let state = Arc::clone(&state);
                                async move { Ok::<_, Infallible>(handle_request(state, req).await) }
                            });
                            let _ = http1::Builder::new().serve_connection(io, svc).await;
                        });
                    }
                }
            }
        });

        Ok(DaemonHandle {
            addr,
            shutdown_tx,
            task,
        })
    }
}

async fn handle_request(state: Arc<ServerState>, req: Request<Incoming>) -> Response<RespBody> {
    if req.headers().contains_key(header::ORIGIN) {
        return json_error(StatusCode::FORBIDDEN, "Origin header is forbidden");
    }
    if !is_loopback_host(req.headers().get(header::HOST)) {
        return json_error(StatusCode::FORBIDDEN, "non-loopback Host header rejected");
    }
    if !verify_bearer(req.headers().get(header::AUTHORIZATION), &state.token) {
        return json_error(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    }

    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    match (&method, segs.as_slice()) {
        (&Method::GET, ["v1", "health"]) => match state.backend.list_sessions().await {
            Ok(sessions) => json_ok(
                StatusCode::OK,
                &HealthResponse {
                    status: "ok".to_owned(),
                    pid: std::process::id(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    host_key: state.host_key.clone(),
                    sessions: sessions.len(),
                },
            ),
            Err(err) => err_to_response(&err),
        },
        (&Method::GET, ["v1", "sessions"]) => match state.backend.list_sessions().await {
            Ok(list) => json_ok(StatusCode::OK, &list),
            Err(err) => err_to_response(&err),
        },
        (&Method::POST, ["v1", "sessions"]) => match read_json::<CreateSessionRequest>(req).await {
            Ok(body) => match state.backend.create_session(body).await {
                Ok(created) => json_ok(StatusCode::CREATED, &created),
                Err(err) => err_to_response(&err),
            },
            Err(resp) => *resp,
        },
        (&Method::POST, ["v1", "sessions", id, "turns"]) => {
            match read_json::<StartTurnRequest>(req).await {
                Ok(body) => match state.backend.start_turn((*id).to_owned(), body).await {
                    Ok(()) => json_ok(StatusCode::ACCEPTED, &OkResponse { ok: true }),
                    Err(err) => err_to_response(&err),
                },
                Err(resp) => *resp,
            }
        }
        (&Method::POST, ["v1", "sessions", id, "steer"]) => {
            match read_json::<SteerRequest>(req).await {
                Ok(body) => match state.backend.steer((*id).to_owned(), body.text).await {
                    Ok(()) => json_ok(StatusCode::OK, &OkResponse { ok: true }),
                    Err(err) => err_to_response(&err),
                },
                Err(resp) => *resp,
            }
        }
        (&Method::POST, ["v1", "sessions", id, "interrupt"]) => {
            match state.backend.interrupt((*id).to_owned()).await {
                Ok(()) => json_ok(StatusCode::OK, &OkResponse { ok: true }),
                Err(err) => err_to_response(&err),
            }
        }
        (&Method::POST, ["v1", "sessions", id, "approve"]) => {
            match read_json::<ApproveRequest>(req).await {
                Ok(body) => match state.backend.approve((*id).to_owned(), body).await {
                    Ok(()) => json_ok(StatusCode::OK, &OkResponse { ok: true }),
                    Err(err) => err_to_response(&err),
                },
                Err(resp) => *resp,
            }
        }
        (&Method::POST, ["v1", "sessions", id, "restore"]) => {
            match read_json::<RestoreSessionRequest>(req).await {
                Ok(body) => match state.backend.restore_session((*id).to_owned(), body).await {
                    Ok(restored) => {
                        json_ok(StatusCode::ACCEPTED, &RestoreSessionResponse { restored })
                    }
                    Err(err) => err_to_response(&err),
                },
                Err(resp) => *resp,
            }
        }
        (&Method::GET, ["v1", "sessions", id, "events"]) => {
            match state.backend.subscribe((*id).to_owned()).await {
                Ok(rx) => sse_response(rx),
                Err(err) => err_to_response(&err),
            }
        }
        _ => json_error(StatusCode::NOT_FOUND, "unknown daemon route"),
    }
}

fn sse_response(mut rx: broadcast::Receiver<DaemonEvent>) -> Response<RespBody> {
    let (tx, body_rx) = mpsc::channel::<Bytes>(64);
    tokio::spawn(async move {
        if tx
            .send(Bytes::from_static(b": connected\n\n"))
            .await
            .is_err()
        {
            return;
        }
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let Ok(json) = serde_json::to_string(&ev) {
                        let frame = format!("id: {}\nevent: agent\ndata: {json}\n\n", ev.seq);
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
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(RespBody::Stream(body_rx))
        .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "sse response error"))
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

fn is_loopback_host(host: Option<&http::HeaderValue>) -> bool {
    let Some(raw) = host.and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("[::1]") {
        return rest.is_empty() || rest.starts_with(':');
    }
    let name = trimmed.split(':').next().unwrap_or("");
    name.eq_ignore_ascii_case("127.0.0.1") || name.eq_ignore_ascii_case("localhost")
}

fn verify_bearer(auth: Option<&http::HeaderValue>, expected_token: &str) -> bool {
    let Some(raw) = auth.and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(token) = raw.strip_prefix("Bearer ") else {
        return false;
    };
    ct_eq(token.trim().as_bytes(), expected_token.as_bytes())
}

fn json_ok<T: Serialize>(status: StatusCode, payload: &T) -> Response<RespBody> {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
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
