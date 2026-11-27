//! End-to-end loopback HTTP+SSE daemon & security enforcement tests.

use hrn_edge_ipc::{
    ApproveRequest, CreateSessionRequest, DaemonBackend, DaemonEvent, DaemonServer, IpcClient,
    SessionSummaryDto, StartTurnRequest, generate_token, sha256_hex,
};
use hrn_kernel::{
    AgentEvent, AgentStatus, BoxFuture, HrnError, PermissionMode, Result, ThinkingLevel,
};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

struct FakeBackend {
    sessions: Mutex<BTreeMap<String, SessionSummaryDto>>,
    tx: broadcast::Sender<DaemonEvent>,
    seq: AtomicU64,
    steers: Mutex<Vec<(String, String)>>,
    approvals: Mutex<Vec<(String, String, bool)>>,
}

impl FakeBackend {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(32);
        Self {
            sessions: Mutex::new(BTreeMap::new()),
            tx,
            seq: AtomicU64::new(1),
            steers: Mutex::new(Vec::new()),
            approvals: Mutex::new(Vec::new()),
        }
    }
}

impl DaemonBackend for FakeBackend {
    fn list_sessions(&self) -> BoxFuture<Result<Vec<SessionSummaryDto>>> {
        let list: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        Box::pin(async move { Ok(list) })
    }

    fn create_session(&self, req: CreateSessionRequest) -> BoxFuture<Result<SessionSummaryDto>> {
        let id = format!("sess_{}", self.seq.fetch_add(1, Ordering::Relaxed));
        let dto = SessionSummaryDto {
            id: id.clone(),
            title: req.title,
            cwd: req.cwd,
            model: req.model,
            status: AgentStatus::Idle,
            active_turn: false,
            updated_at_ms: 1000,
        };
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, dto.clone());
        Box::pin(async move { Ok(dto) })
    }

    fn start_turn(&self, session_id: String, req: StartTurnRequest) -> BoxFuture<Result<()>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(DaemonEvent {
            seq,
            session_id,
            event: AgentEvent::TextDelta { delta: req.prompt },
        });
        Box::pin(async { Ok(()) })
    }

    fn steer(&self, session_id: String, text: String) -> BoxFuture<Result<()>> {
        self.steers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((session_id, text));
        Box::pin(async { Ok(()) })
    }

    fn interrupt(&self, _session_id: String) -> BoxFuture<Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn approve(&self, session_id: String, req: ApproveRequest) -> BoxFuture<Result<()>> {
        self.approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((session_id, req.call_id, req.approved));
        Box::pin(async { Ok(()) })
    }

    fn subscribe(
        &self,
        _session_id: String,
    ) -> BoxFuture<Result<broadcast::Receiver<DaemonEvent>>> {
        let rx = self.tx.subscribe();
        Box::pin(async move { Ok(rx) })
    }
}

#[tokio::test]
async fn daemon_http_and_sse_roundtrip_with_security_checks() {
    let backend = Arc::new(FakeBackend::new());
    let token = generate_token().expect("token");
    let host_key = format!("SHA256:{}", sha256_hex(b"daemon-test-identity"));

    // Non-loopback bind must be refused.
    let bad_bind: SocketAddr = "0.0.0.0:0".parse().expect("addr");
    assert!(matches!(
        DaemonServer::bind(bad_bind, token.clone(), host_key.clone(), backend.clone()).await,
        Err(HrnError::Auth(_))
    ));

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let handle = DaemonServer::bind(bind_addr, token.clone(), host_key.clone(), backend.clone())
        .await
        .expect("bind");
    let addr = handle.addr();

    // 1. Valid client round-trip.
    let client = IpcClient::new(addr, token.clone(), Some(host_key.clone())).expect("client");
    let health = client.health().await.expect("health");
    assert_eq!(health.status, "ok");
    assert_eq!(health.host_key, host_key);

    let sess = client
        .create_session(&CreateSessionRequest {
            title: "demo".to_owned(),
            cwd: ".".to_owned(),
            model: Some("default".to_owned()),
            thinking: ThinkingLevel::Medium,
            permission_mode: PermissionMode::Default,
        })
        .await
        .expect("create");
    assert_eq!(client.list_sessions().await.expect("list").len(), 1);

    let mut sse = client.subscribe_events(&sess.id).await.expect("sse");
    client
        .start_turn(
            &sess.id,
            &StartTurnRequest {
                prompt: "hello-sse".to_owned(),
                auto_approve: true,
            },
        )
        .await
        .expect("start_turn");
    let ev = tokio::time::timeout(std::time::Duration::from_secs(3), sse.next_event())
        .await
        .expect("sse timeout")
        .expect("sse ok")
        .expect("event present");
    assert_eq!(ev.session_id, sess.id);
    assert!(matches!(ev.event, AgentEvent::TextDelta { ref delta } if delta == "hello-sse"));

    client
        .steer(&sess.id, "focus on tests".to_owned())
        .await
        .expect("steer");
    client
        .approve(&sess.id, "call_1".to_owned(), true)
        .await
        .expect("approve");
    client.interrupt(&sess.id).await.expect("interrupt");
    assert_eq!(backend.steers.lock().expect("lock").len(), 1);
    assert_eq!(backend.approvals.lock().expect("lock").len(), 1);

    // 2. Wrong bearer token rejected with Auth error.
    let bad_tok = IpcClient::new(addr, "wrong".to_owned(), Some(host_key.clone())).expect("c");
    assert!(matches!(bad_tok.health().await, Err(HrnError::Auth(_))));

    // 3. Wrong pinned host-key rejected loudly with Auth error.
    let bad_key = IpcClient::new(
        addr,
        token.clone(),
        Some(format!("SHA256:{}", "0".repeat(64))),
    )
    .expect("c");
    assert!(matches!(bad_key.health().await, Err(HrnError::Auth(_))));

    // 4. Raw HTTP request with Origin header -> 403 Forbidden.
    let origin_resp = raw_http(
        addr,
        &format!(
            "GET /v1/health HTTP/1.1\r\nHost: {addr}\r\nOrigin: https://evil.example\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
    .expect("raw http");
    assert!(origin_resp.starts_with("HTTP/1.1 403"), "{origin_resp}");

    // 5. Raw HTTP request with non-loopback Host header -> 403 Forbidden.
    let host_resp = raw_http(
        addr,
        &format!(
            "GET /v1/health HTTP/1.1\r\nHost: evil.example\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
    .expect("raw http");
    assert!(host_resp.starts_with("HTTP/1.1 403"), "{host_resp}");

    handle.shutdown().await;
}

async fn raw_http(addr: SocketAddr, req: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
