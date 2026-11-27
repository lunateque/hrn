use super::*;
use hrn_edge_http::SseParser;
use hrn_kernel::{AgentEvent, AgentStatus, Choice, DagPayload, Field, Form, Item, Kind, Msg};
use http_body_util::Full;
use hyper::client::conn::http1 as client_http1;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use tokio::net::TcpStream;

#[derive(Default)]
struct MockState {
    sessions: Vec<SessionSummaryDto>,
    turns: Vec<(String, StartTurnRequest)>,
    steers: Vec<(String, String)>,
    interrupts: Vec<String>,
    approvals: Vec<(String, MobileApproveRequest)>,
    asks: Vec<(String, MobileAskAnswerRequest)>,
    channels: HashMap<String, broadcast::Sender<DaemonEvent>>,
}

struct MockMobileBackend {
    state: Mutex<MockState>,
}

impl MockMobileBackend {
    fn new() -> Self {
        let mut s = MockState::default();
        let (tx, _) = broadcast::channel(32);
        s.channels.insert("s1".to_owned(), tx);
        s.sessions.push(SessionSummaryDto {
            id: "s1".to_owned(),
            title: "Build mobile web".to_owned(),
            cwd: "/repo".to_owned(),
            model: Some("sonnet".to_owned()),
            status: AgentStatus::Idle,
            active_turn: false,
            updated_at_ms: 1_700_000_000_000,
        });
        Self {
            state: Mutex::new(s),
        }
    }

    fn emit(&self, session_id: &str, event: DaemonEvent) {
        let guard = self.state.lock().expect("lock");
        if let Some(tx) = guard.channels.get(session_id) {
            let _ = tx.send(event);
        }
    }
}

impl MobileWebBackend for MockMobileBackend {
    fn host_info(&self) -> MobileHostInfo {
        MobileHostInfo {
            host_name: "workstation".to_owned(),
            mode: "daemon".to_owned(),
            active_session_id: Some("s1".to_owned()),
            models: vec!["sonnet".to_owned(), "opus".to_owned()],
            version: "0.1.0".to_owned(),
        }
    }

    fn list_sessions(&self) -> BoxFuture<Result<Vec<SessionSummaryDto>>> {
        let sessions = self.state.lock().expect("lock").sessions.clone();
        Box::pin(async move { Ok(sessions) })
    }

    fn create_session(&self, req: CreateSessionRequest) -> BoxFuture<Result<SessionSummaryDto>> {
        let mut guard = self.state.lock().expect("lock");
        let id = format!("s{}", guard.sessions.len() + 1);
        let dto = SessionSummaryDto {
            id: id.clone(),
            title: req.title,
            cwd: req.cwd,
            model: req.model,
            status: AgentStatus::Idle,
            active_turn: false,
            updated_at_ms: 1_700_000_001_000,
        };
        let (tx, _) = broadcast::channel(32);
        guard.channels.insert(id, tx);
        guard.sessions.push(dto.clone());
        Box::pin(async move { Ok(dto) })
    }

    fn session_transcript(&self, session_id: &str) -> BoxFuture<Result<SessionTranscriptDto>> {
        let guard = self.state.lock().expect("lock");
        let meta = guard
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .cloned()
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")));
        let sid = session_id.to_owned();
        Box::pin(async move {
            let meta = meta?;
            Ok(SessionTranscriptDto {
                meta,
                busy: false,
                thinking: ThinkingLevel::Medium,
                permission_mode: PermissionMode::Default,
                nodes: vec![DagNode {
                    id: "n1".to_owned(),
                    session_id: sid.clone(),
                    parent_id: None,
                    created_at_ms: 1_700_000_000_000,
                    payload: DagPayload::Message {
                        turn_id: "t1".to_owned(),
                        msg: Msg::user("hello mobile"),
                    },
                }],
                pending_approvals: vec![ApprovalRequest {
                    id: "apr-1".to_owned(),
                    session_id: sid.clone(),
                    tool_name: "bash".to_owned(),
                    summary: "cargo test".to_owned(),
                    diff: None,
                    reason: None,
                    targets: Vec::new(),
                    choices: Vec::new(),
                    tier: None,
                }],
                pending_asks: vec![AskRequest {
                    id: "ask-1".to_owned(),
                    session_id: sid,
                    form: Form {
                        items: vec![Item::Field(Field {
                            name: "env".to_owned(),
                            question: "Target environment?".to_owned(),
                            required: true,
                            kind: Kind::One,
                            options: vec![Choice {
                                label: "Staging".to_owned(),
                                value: "staging".to_owned(),
                                checked: true,
                            }],
                            other: false,
                        })],
                    },
                }],
            })
        })
    }

    fn start_turn(&self, session_id: String, req: StartTurnRequest) -> BoxFuture<Result<()>> {
        self.state
            .lock()
            .expect("lock")
            .turns
            .push((session_id, req));
        Box::pin(async { Ok(()) })
    }

    fn steer(&self, session_id: String, text: String) -> BoxFuture<Result<()>> {
        self.state
            .lock()
            .expect("lock")
            .steers
            .push((session_id, text));
        Box::pin(async { Ok(()) })
    }

    fn interrupt(&self, session_id: String) -> BoxFuture<Result<()>> {
        self.state.lock().expect("lock").interrupts.push(session_id);
        Box::pin(async { Ok(()) })
    }

    fn approve(&self, session_id: String, req: MobileApproveRequest) -> BoxFuture<Result<()>> {
        self.state
            .lock()
            .expect("lock")
            .approvals
            .push((session_id, req));
        Box::pin(async { Ok(()) })
    }

    fn answer_ask(&self, session_id: String, req: MobileAskAnswerRequest) -> BoxFuture<Result<()>> {
        self.state
            .lock()
            .expect("lock")
            .asks
            .push((session_id, req));
        Box::pin(async { Ok(()) })
    }

    fn subscribe(&self, session_id: String) -> BoxFuture<Result<broadcast::Receiver<DaemonEvent>>> {
        let rx = self
            .state
            .lock()
            .expect("lock")
            .channels
            .get(&session_id)
            .map(broadcast::Sender::subscribe)
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")));
        Box::pin(async move { rx })
    }
}

async fn send_http(
    addr: SocketAddr,
    method: Method,
    path: &str,
    bearer: Option<&str>,
    body: Option<&str>,
) -> http::Response<Incoming> {
    let stream = TcpStream::connect(addr).await.expect("connect");
    let (mut sender, conn) = client_http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, addr.to_string());
    if let Some(tok) = bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    let bytes = match body {
        Some(raw) => {
            b = b.header(header::CONTENT_TYPE, "application/json");
            Bytes::from(raw.to_owned())
        }
        None => Bytes::new(),
    };
    let req = b.body(Full::new(bytes)).expect("req");
    sender.send_request(req).await.expect("send")
}

async fn body_text(resp: http::Response<Incoming>) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

#[tokio::test]
async fn serves_pwa_html_security_headers_and_manifest() {
    let backend = Arc::new(MockMobileBackend::new());
    let handle =
        MobileWebServer::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), backend)
            .await
            .expect("bind");

    let resp = send_http(handle.addr(), Method::GET, "/", None, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    assert_eq!(
        resp.headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        resp.headers()
            .get(header::REFERRER_POLICY)
            .and_then(|v| v.to_str().ok()),
        Some("no-referrer")
    );
    let csp = resp
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|v| v.to_str().ok())
        .expect("csp");
    assert!(csp.contains("connect-src 'self' ws: wss:"), "{csp}");
    let html = body_text(resp).await;
    assert!(html.contains("hrn remote"));
    assert!(!html.contains("innerHTML"), "PWA must never use innerHTML");

    let manifest = send_http(handle.addr(), Method::GET, "/manifest.json", None, None).await;
    assert_eq!(manifest.status(), StatusCode::OK);
    let mjson: serde_json::Value = serde_json::from_str(&body_text(manifest).await).expect("json");
    assert_eq!(mjson["name"], "hrn remote");
    assert_eq!(mjson["display"], "standalone");

    handle.shutdown().await;
}

async fn pair_as(handle: &MobileWebHandle, role: MobileRole) -> PairResponse {
    let code = handle
        .create_pairing(role, Duration::from_mins(1))
        .expect("code");
    let resp = send_http(
        handle.addr(),
        Method::POST,
        "/api/pair",
        None,
        Some(&format!(r#"{{"code":"{code}"}}"#)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    serde_json::from_str(&body_text(resp).await).expect("pair")
}

#[tokio::test]
async fn pairing_single_use_expiry_and_revoke_work() {
    let backend = Arc::new(MockMobileBackend::new());
    let handle =
        MobileWebServer::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), backend)
            .await
            .expect("bind");
    let addr = handle.addr();

    let unauth = send_http(addr, Method::GET, "/api/info", None, None).await;
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let expired_code = handle
        .create_pairing(MobileRole::Control, Duration::ZERO)
        .expect("expired");
    let exp_resp = send_http(
        addr,
        Method::POST,
        "/api/pair",
        None,
        Some(&format!(r#"{{"code":"{expired_code}"}}"#)),
    )
    .await;
    assert_eq!(exp_resp.status(), StatusCode::UNAUTHORIZED);

    let ctrl_code = handle
        .create_pairing(MobileRole::Control, Duration::from_mins(1))
        .expect("ctrl code");
    let first = send_http(
        addr,
        Method::POST,
        "/api/pair",
        None,
        Some(&format!(r#"{{"code":"{ctrl_code}"}}"#)),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let paired: PairResponse = serde_json::from_str(&body_text(first).await).expect("pair");
    assert_eq!(paired.role, MobileRole::Control);
    assert_eq!(paired.host.host_name, "workstation");
    assert_eq!(handle.connected_count(), 1);

    let replay = send_http(
        addr,
        Method::POST,
        "/api/pair",
        None,
        Some(&format!(r#"{{"code":"{ctrl_code}"}}"#)),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);

    handle.revoke_all();
    assert_eq!(handle.connected_count(), 0);
    let revoked = send_http(
        addr,
        Method::GET,
        "/api/sessions",
        Some(&paired.token),
        None,
    )
    .await;
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);

    handle.shutdown().await;
}

#[tokio::test]
async fn view_rbac_blocks_mutations_and_control_endpoints_succeed() {
    let backend = Arc::new(MockMobileBackend::new());
    let handle = MobileWebServer::bind(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Arc::clone(&backend) as Arc<dyn MobileWebBackend>,
    )
    .await
    .expect("bind");
    let addr = handle.addr();

    let view_pair = pair_as(&handle, MobileRole::View).await;
    let ctrl_pair = pair_as(&handle, MobileRole::Control).await;
    assert_eq!(handle.connected_count(), 2);

    let view_info = send_http(addr, Method::GET, "/api/info", Some(&view_pair.token), None).await;
    assert_eq!(view_info.status(), StatusCode::OK);
    let view_tr = send_http(
        addr,
        Method::GET,
        "/api/sessions/s1/transcript",
        Some(&view_pair.token),
        None,
    )
    .await;
    assert_eq!(view_tr.status(), StatusCode::OK);
    let tr: SessionTranscriptDto = serde_json::from_str(&body_text(view_tr).await).expect("tr");
    assert_eq!(tr.nodes.len(), 1);
    assert_eq!(tr.pending_approvals.len(), 1);
    assert_eq!(tr.pending_asks.len(), 1);

    let mutations = [
        (
            "/api/sessions",
            Some(r#"{"title":"x","cwd":"."}"#),
            StatusCode::CREATED,
        ),
        (
            "/api/sessions/s1/turns",
            Some(r#"{"prompt":"run tests"}"#),
            StatusCode::ACCEPTED,
        ),
        (
            "/api/sessions/s1/steer",
            Some(r#"{"text":"focus on ipc"}"#),
            StatusCode::OK,
        ),
        ("/api/sessions/s1/interrupt", None, StatusCode::OK),
        (
            "/api/sessions/s1/approve",
            Some(r#"{"call_id":"apr-1","approved":true,"always":true}"#),
            StatusCode::OK,
        ),
        (
            "/api/sessions/s1/ask",
            Some(r#"{"id":"ask-1","answers":["staging"]}"#),
            StatusCode::OK,
        ),
    ];

    for &(path, body, expected_ctrl_status) in &mutations {
        let denied_resp = send_http(addr, Method::POST, path, Some(&view_pair.token), body).await;
        assert_eq!(
            denied_resp.status(),
            StatusCode::FORBIDDEN,
            "view must be 403 on {path}"
        );
        let allowed_resp = send_http(addr, Method::POST, path, Some(&ctrl_pair.token), body).await;
        assert_eq!(
            allowed_resp.status(),
            expected_ctrl_status,
            "ctrl failed on {path}"
        );
    }

    {
        let st = backend.state.lock().expect("lock");
        assert_eq!(st.turns.len(), 1);
        assert_eq!(st.steers.len(), 1);
        assert_eq!(st.interrupts, vec!["s1"]);
        assert_eq!(st.approvals.len(), 1);
        assert!(st.approvals[0].1.always);
        assert_eq!(st.asks.len(), 1);
        assert_eq!(st.asks[0].1.answers, vec!["staging"]);
    }

    handle.shutdown().await;
}

#[tokio::test]
async fn sse_streams_daemon_events_over_authenticated_fetch() {
    let backend = Arc::new(MockMobileBackend::new());
    let handle = MobileWebServer::bind(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Arc::clone(&backend) as Arc<dyn MobileWebBackend>,
    )
    .await
    .expect("bind");
    let addr = handle.addr();

    let paired = pair_as(&handle, MobileRole::View).await;
    let resp = send_http(
        addr,
        Method::GET,
        "/api/sessions/s1/events",
        Some(&paired.token),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream; charset=utf-8")
    );

    let mut body = resp.into_body();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("connected timeout")
        .expect("connected frame")
        .expect("ok frame");
    assert_eq!(
        first.data_ref().map(AsRef::as_ref),
        Some(b": connected\n\n".as_slice())
    );

    backend.emit(
        "s1",
        DaemonEvent {
            seq: 42,
            session_id: "s1".to_owned(),
            event: AgentEvent::TextDelta {
                delta: "hello from pocket".to_owned(),
            },
        },
    );

    let mut parser = SseParser::default();
    let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("event timeout")
        .expect("event frame")
        .expect("ok frame");
    let data = frame.data_ref().expect("data");
    let raw_text = std::str::from_utf8(data).expect("utf8");
    assert!(
        raw_text.starts_with("id: 42\nevent: daemon\ndata: "),
        "{raw_text}"
    );
    let events = parser.push(data);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.as_deref(), Some("daemon"));
    let decoded: DaemonEvent = serde_json::from_str(&events[0].data).expect("decode daemon event");
    assert_eq!(decoded.seq, 42);
    assert_eq!(
        decoded.event,
        AgentEvent::TextDelta {
            delta: "hello from pocket".to_owned()
        }
    );

    handle.shutdown().await;
}
