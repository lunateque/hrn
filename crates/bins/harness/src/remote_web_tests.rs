use super::*;
use hrn_edge_ipc::{MobileHostInfo, PairResponse, SessionSummaryDto, SessionTranscriptDto};
use hrn_edge_store::SqliteStore;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{AgentRuntime, HarnessConfig, ToolRegistry};
use hrn_kernel::{
    CancelFlag, ChunkSink, FinishReason, LlmProvider, LlmRequest, LlmResponse, ModelInfo,
    ScopedFuture,
};
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct StubLlm;

impl LlmProvider for StubLlm {
    fn stream_chat<'a>(
        &'a self,
        _request: &'a LlmRequest,
        _cancel: &'a CancelFlag,
        _sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>> {
        Box::pin(async {
            Ok(LlmResponse {
                model_used: "stub".to_owned(),
                fallback_from: None,
                content: "ok".to_owned(),
                reasoning: String::new(),
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                usage: None,
                finish_reason: FinishReason::Stop,
            })
        })
    }

    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
}

fn req(choices: Vec<ApprovalGrantScope>) -> ApprovalRequest {
    ApprovalRequest {
        id: "call-1".to_owned(),
        session_id: "s1".to_owned(),
        tool_name: "bash".to_owned(),
        summary: "git status".to_owned(),
        diff: None,
        reason: None,
        targets: Vec::new(),
        choices,
        tier: None,
    }
}

async fn http_call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let body_bytes = body.unwrap_or("").as_bytes();
    let mut raw = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(tok) = bearer {
        let _ = write!(raw, "Authorization: Bearer {tok}\r\n");
    }
    if body.is_some() {
        raw.push_str("Content-Type: application/json\r\n");
        let _ = write!(raw, "Content-Length: {}\r\n", body_bytes.len());
    }
    raw.push_str("\r\n");
    stream.write_all(raw.as_bytes()).await.expect("write head");
    if !body_bytes.is_empty() {
        stream.write_all(body_bytes).await.expect("write body");
    }
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body_out) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let is_chunked = head.lines().any(|l| {
        let lower = l.to_ascii_lowercase();
        lower.starts_with("transfer-encoding:") && lower.contains("chunked")
    });
    let decoded = if is_chunked {
        decode_chunked(body_out)
    } else {
        body_out.to_owned()
    };
    (status, decoded)
}

fn decode_chunked(mut raw: &str) -> String {
    let mut out = String::new();
    while let Some((size_hex, rest)) = raw.split_once("\r\n") {
        let size = usize::from_str_radix(size_hex.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let chunk = rest.get(..size).unwrap_or(rest);
        out.push_str(chunk);
        raw = rest
            .get(size..)
            .and_then(|s| s.strip_prefix("\r\n"))
            .unwrap_or("");
    }
    out
}

#[test]
fn resolve_bind_addr_applies_port_override_and_rejects_bad_input() {
    let addr = resolve_bind_addr("0.0.0.0:0", Some(8799)).expect("valid");
    assert_eq!(
        addr,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8799)
    );

    let keep = resolve_bind_addr("127.0.0.1:9123", None).expect("valid");
    assert_eq!(keep.port(), 9123);

    assert!(resolve_bind_addr("not-an-addr", None).is_err());
}

#[test]
fn build_display_urls_prefers_public_url_and_preserves_explicit_bind() {
    let unspecified = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8788);
    let (primary, _alts) =
        build_display_urls(unspecified, Some("https://box.tail.ts.net/"), "pair=abc");
    assert_eq!(primary, "https://box.tail.ts.net/#pair=abc");

    let explicit = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 2)), 8788);
    let (primary_ex, alts_ex) = build_display_urls(explicit, None, "pair=xyz");
    assert_eq!(primary_ex, "http://198.51.100.2:8788/#pair=xyz");
    assert!(alts_ex.is_empty());

    let (primary_unspec, _) = build_display_urls(unspecified, None, "pair=lan");
    assert!(primary_unspec.starts_with("http://"));
    assert!(primary_unspec.ends_with(":8788/#pair=lan"));
}

#[test]
fn approval_decision_enforces_plain_only_once_restriction() {
    let denied = approval_decision_from_mobile(&req(Vec::new()), false, true);
    assert!(!denied.is_approved());

    let plain_only = req(vec![ApprovalGrantScope::Once]);
    let granted_once = approval_decision_from_mobile(&plain_only, true, true);
    assert_eq!(granted_once, ApprovalDecision::once());

    let normal = req(vec![
        ApprovalGrantScope::Once,
        ApprovalGrantScope::ToolSession,
    ]);
    let granted_session = approval_decision_from_mobile(&normal, true, true);
    assert_eq!(granted_session, ApprovalDecision::tool_session());

    let single = approval_decision_from_mobile(&normal, true, false);
    assert_eq!(single, ApprovalDecision::once());
}

#[test]
fn answers_to_submission_maps_all_form_field_kinds() {
    let form = Form::parse(
        "<form>\
            <input type=radio name=env value=dev> Dev\
            <input type=radio name=env value=prod> Prod\
            <input type=checkbox name=checks value=fmt> Format\
            <input type=checkbox name=checks value=clippy> Clippy\
            <input type=checkbox name=confirm value=yes> Confirm\
            <input type=text name=note>\
            <input type=hidden name=token value=secret_default>\
         </form>",
    )
    .expect("valid form");

    let sub = answers_to_submission(
        &form,
        &[
            "prod".to_owned(),
            "fmt, Clippy".to_owned(),
            "true".to_owned(),
            "ship it".to_owned(),
            String::new(),
        ],
    );
    assert!(!sub.timed_out);
    assert_eq!(sub.values["env"], "prod");
    assert_eq!(sub.values["checks"], serde_json::json!(["fmt", "clippy"]));
    assert_eq!(sub.values["confirm"], "yes");
    assert_eq!(sub.values["note"], "ship it");
    assert_eq!(sub.values["token"], "secret_default");
}

#[tokio::test]
async fn runtime_mobile_backend_serves_pairing_sessions_and_rbac_over_http() {
    let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
    let runtime = AgentRuntime::new(
        Arc::new(StubLlm),
        Arc::clone(&store) as Arc<dyn hrn_kernel::SessionStore>,
        ToolRegistry::default(),
        Arc::new(StdWorkspaceFs),
        HarnessConfig::default(),
    );
    let cwd = std::env::current_dir().expect("cwd");
    let backend = Arc::new(RuntimeMobileBackend::new(
        runtime,
        store,
        cwd,
        vec!["flash".to_owned(), "opus".to_owned()],
    ));
    let handle =
        MobileWebServer::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), backend)
            .await
            .expect("bind");
    let addr = handle.addr();

    // Pair one control device and one view-only device.
    let ctrl_code = handle
        .create_pairing(MobileRole::Control, Duration::from_mins(1))
        .expect("ctrl code");
    let (st, body) = http_call(
        addr,
        "POST",
        "/api/pair",
        None,
        Some(&format!(r#"{{"code":"{ctrl_code}"}}"#)),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let ctrl: PairResponse = serde_json::from_str(&body).expect("ctrl pair");
    assert_eq!(ctrl.role, MobileRole::Control);

    let view_code = handle
        .create_pairing(MobileRole::View, Duration::from_mins(1))
        .expect("view code");
    let (st, body) = http_call(
        addr,
        "POST",
        "/api/pair",
        None,
        Some(&format!(r#"{{"code":"{view_code}"}}"#)),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let view: PairResponse = serde_json::from_str(&body).expect("view pair");
    assert_eq!(view.role, MobileRole::View);

    // Host info reports daemon mode & configured models.
    let (st, body) = http_call(addr, "GET", "/api/info", Some(&ctrl.token), None).await;
    assert_eq!(st, 200, "{body}");
    let info: MobileHostInfo = serde_json::from_str(&body).expect("info");
    assert_eq!(info.mode, "daemon");
    assert_eq!(info.models, vec!["flash".to_owned(), "opus".to_owned()]);

    // View-only token cannot create sessions (403).
    let (st, _) = http_call(
        addr,
        "POST",
        "/api/sessions",
        Some(&view.token),
        Some(r#"{"title":"blocked","cwd":"."}"#),
    )
    .await;
    assert_eq!(st, 403);

    // Control token creates a session, lists it, and reads its transcript.
    let (st, body) = http_call(
        addr,
        "POST",
        "/api/sessions",
        Some(&ctrl.token),
        Some(r#"{"title":"Pocket Session","cwd":"."}"#),
    )
    .await;
    assert_eq!(st, 201, "{body}");
    let created: SessionSummaryDto = serde_json::from_str(&body).expect("created");
    assert_eq!(created.title, "Pocket Session");

    let (st, body) = http_call(addr, "GET", "/api/sessions", Some(&view.token), None).await;
    assert_eq!(st, 200, "{body}");
    let sessions: Vec<SessionSummaryDto> = serde_json::from_str(&body).expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, created.id);

    let (st, body) = http_call(
        addr,
        "GET",
        &format!("/api/sessions/{}/transcript", created.id),
        Some(&view.token),
        None,
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let tr: SessionTranscriptDto = serde_json::from_str(&body).expect("transcript");
    assert_eq!(tr.meta.id, created.id);
    assert!(!tr.busy);

    // Revoking all sessions immediately invalidates bearer tokens (401).
    handle.revoke_all();
    let (st, _) = http_call(addr, "GET", "/api/info", Some(&ctrl.token), None).await;
    assert_eq!(st, 401);

    handle.shutdown().await;
}
