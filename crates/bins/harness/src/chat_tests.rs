use super::*;
use crate::chat::chat_ui::{UiAsk, UiGate};
use crate::cli::ChatArgs;
use crate::resume::find_session;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hrn_edge_store::SqliteStore;
use hrn_kernel::{
    AgentStatus, ApprovalGate, ApprovalRequest, AskHandler, AskRequest, HrnError, PermissionMode,
    SessionMeta, ThinkingLevel,
};
use hrn_tui::{App, ApprovalAsk, Inbound, UiEvent, UiReceiver};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

fn meta(id: &str) -> SessionMeta {
    SessionMeta {
        pinned: false,
        id: id.into(),
        title: id.into(),
        cwd: ".".into(),
        branch: None,
        head_node_id: None,
        model: None,
        thinking: ThinkingLevel::Off,
        goal: None,
        permission_mode: PermissionMode::Default,
        parent_session_id: None,
        parent_node_id: None,
        status: AgentStatus::Idle,
        title_source: None,
        exit_ms: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

#[test]
fn sessions_resolve_by_id_or_unique_prefix() {
    let store = SqliteStore::open_in_memory().unwrap();
    for id in ["sess_ab_1", "sess_ab_2", "sess_cd_1"] {
        store.create_session(&meta(id)).unwrap();
    }
    assert_eq!(find_session(&store, "sess_ab_1").unwrap(), "sess_ab_1");
    assert_eq!(find_session(&store, "sess_cd").unwrap(), "sess_cd_1");
    let ambiguous = find_session(&store, "sess_ab").unwrap_err().to_string();
    assert!(ambiguous.contains("2 sessions"), "{ambiguous}");
    assert!(find_session(&store, "nope").is_err());
}

fn approval(id: &str) -> ApprovalRequest {
    ApprovalRequest {
        id: id.into(),
        session_id: "s".into(),
        tool_name: "bash".into(),
        summary: "ls".into(),
        diff: None,
        reason: None,
        targets: Vec::new(),
        choices: Vec::new(),
        tier: None,
    }
}

fn next_ask(rx: &UiReceiver) -> ApprovalAsk {
    match rx
        .recv(Some(Duration::from_secs(5)))
        .and_then(Inbound::into_event)
    {
        Some(UiEvent::Approval(ask)) => ask,
        other => panic!("expected an approval, got {other:?}"),
    }
}

#[tokio::test]
async fn the_gate_relays_answers_and_cancellation() {
    let (tx, rx) = hrn_tui::channel();
    let gate = UiGate::new(tx, None);
    let pending = gate.await_decision(approval("1"), CancelFlag::new());
    next_ask(&rx).answer(hrn_kernel::ApprovalDecision::once());
    assert!(pending.await.is_ok_and(|d| d.is_approved()));

    let pending = gate.await_decision(approval("2"), CancelFlag::new());
    drop(next_ask(&rx));
    assert!(
        matches!(pending.await, Err(HrnError::Cancelled)),
        "a dropped ask cancels"
    );

    let cancel = CancelFlag::new();
    let pending = gate.await_decision(approval("3"), cancel.clone());
    let _unanswered = next_ask(&rx);
    cancel.cancel();
    assert!(matches!(pending.await, Err(HrnError::Cancelled)));
}

#[tokio::test]
async fn the_ask_handler_relays_answers_and_cancellation() {
    use hrn_kernel::{Form, Submission};

    let (tx, rx) = hrn_tui::channel();
    let handler = UiAsk::new(tx, None);
    let make_req = || AskRequest {
        id: "ask-1".into(),
        session_id: "s".into(),
        form: Form::parse("<form><input type=radio name=env value=prod> prod</form>")
            .expect("form"),
    };

    let pending = handler.ask(make_req(), CancelFlag::new());
    match rx
        .recv(Some(Duration::from_secs(5)))
        .and_then(Inbound::into_event)
    {
        Some(UiEvent::Ask(prompt)) => prompt.answer(Some(Submission {
            values: serde_json::json!({"env": "prod"}),
            timed_out: false,
        })),
        other => panic!("expected AskPrompt, got {other:?}"),
    }
    let done = pending.await.unwrap();
    assert_eq!(done.values["env"], "prod");
    assert!(!done.timed_out);

    let pending = handler.ask(make_req(), CancelFlag::new());
    match rx
        .recv(Some(Duration::from_secs(5)))
        .and_then(Inbound::into_event)
    {
        Some(UiEvent::Ask(prompt)) => prompt.answer(None),
        other => panic!("expected AskPrompt, got {other:?}"),
    }
    assert!(matches!(pending.await, Err(HrnError::Tool(_))));
}

#[tokio::test]
async fn remote_live_hub_syncs_approvals_both_ways() {
    use crate::chat::remote::RemoteLiveHub;
    use hrn_edge_ipc::MobileWebBackend;
    use hrn_kernel::{AgentEvent, ApprovalDecision, ApprovalGrantScope};

    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let (tx, rx) = hrn_tui::channel();
    let hub = Arc::new(RemoteLiveHub::new(tx.clone(), store, vec!["flash".into()]));
    hub.set_active_session(Some("s".into()));
    let mut sse = hub.subscribe("s".into()).await.unwrap();
    let gate = UiGate::new(tx, Some(Arc::clone(&hub)));

    // Case 1: phone answers first -> gate resolves, desktop gets dismissal, SSE gets resolved.
    let mut req_a = approval("call-a");
    req_a.choices = vec![ApprovalGrantScope::Once, ApprovalGrantScope::ToolSession];
    let pending_a = gate.await_decision(req_a, CancelFlag::new());
    let desktop_a = next_ask(&rx);
    let ev1 = sse.recv().await.unwrap();
    assert!(matches!(ev1.event, AgentEvent::ApprovalRequested { .. }));

    hub.resolve_approval_from_remote("call-a", true, true)
        .unwrap();
    assert_eq!(pending_a.await.unwrap(), ApprovalDecision::tool_session());
    match rx
        .recv(Some(Duration::from_secs(5)))
        .and_then(Inbound::into_event)
    {
        Some(UiEvent::RemoteApprovalResolved { call_id }) => assert_eq!(call_id, "call-a"),
        other => panic!("expected RemoteApprovalResolved, got {other:?}"),
    }
    let ev2 = sse.recv().await.unwrap();
    assert!(matches!(
        ev2.event,
        AgentEvent::ApprovalResolved { id, approved: true } if id == "call-a"
    ));
    // Late desktop answer is a harmless no-op.
    desktop_a.answer(ApprovalDecision::Deny);

    // Case 2: desktop answers first -> gate resolves, SSE gets resolved, late remote gets error.
    let pending_b = gate.await_decision(approval("call-b"), CancelFlag::new());
    let desktop_b = next_ask(&rx);
    let _ = sse.recv().await.unwrap();
    desktop_b.answer(ApprovalDecision::Deny);
    assert_eq!(pending_b.await.unwrap(), ApprovalDecision::Deny);
    let ev3 = sse.recv().await.unwrap();
    assert!(matches!(
        ev3.event,
        AgentEvent::ApprovalResolved { id, approved: false } if id == "call-b"
    ));
    assert!(
        hub.resolve_approval_from_remote("call-b", true, false)
            .is_err()
    );
}

#[tokio::test]
async fn remote_live_hub_syncs_ask_from_phone() {
    use crate::chat::remote::RemoteLiveHub;
    use hrn_edge_ipc::MobileWebBackend;
    use hrn_kernel::{AgentEvent, Form};

    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let (tx, rx) = hrn_tui::channel();
    let hub = Arc::new(RemoteLiveHub::new(tx.clone(), store, Vec::new()));
    hub.set_active_session(Some("s".into()));
    let mut sse = hub.subscribe("s".into()).await.unwrap();
    let handler = UiAsk::new(tx, Some(Arc::clone(&hub)));

    let req = AskRequest {
        id: "ask-remote".into(),
        session_id: "s".into(),
        form: Form::parse("<form><input type=radio name=env value=prod> prod</form>")
            .expect("form"),
    };
    let pending = handler.ask(req, CancelFlag::new());
    let _desktop_prompt = match rx
        .recv(Some(Duration::from_secs(5)))
        .and_then(Inbound::into_event)
    {
        Some(UiEvent::Ask(p)) => p,
        other => panic!("expected AskPrompt, got {other:?}"),
    };
    let ev = sse.recv().await.unwrap();
    assert!(matches!(ev.event, AgentEvent::AskRequested { .. }));

    hub.resolve_ask_from_remote("ask-remote", &["prod".to_owned()])
        .unwrap();
    let submitted = pending.await.unwrap();
    assert_eq!(submitted.values["env"], "prod");
    match rx
        .recv(Some(Duration::from_secs(5)))
        .and_then(Inbound::into_event)
    {
        Some(UiEvent::RemoteAskResolved { id }) => assert_eq!(id, "ask-remote"),
        other => panic!("expected RemoteAskResolved, got {other:?}"),
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Feed backend events into the app until `done` holds or `limit` passes.
fn drive(app: &mut App, rx: &UiReceiver, limit: Duration, done: impl Fn(&App) -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        app.pump(rx, Duration::from_millis(50));
        if done(app) {
            return true;
        }
    }
    false
}

fn screen(app: &mut App) -> String {
    let (width, height) = (100, 32);
    let frame = app.paint(width, height);
    let buffer = &frame.live;
    let history = frame.scrollback.iter().map(ToString::to_string);
    let rows = (0..buffer.area.height).map(|y| {
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        row.trim_end().to_owned()
    });
    history.chain(rows).collect::<Vec<_>>().join("\n")
}

fn dump(app: &mut App, log: &mut String, title: &str) -> String {
    let shown = screen(app);
    writeln!(log, "===== {title} =====\n{shown}\n").unwrap();
    shown
}

/// The acceptance check against the configured model: a markdown turn, a tool call answered at
/// the approval prompt, and an `esc` interrupt. Screens land in `target/live-tui/screens.txt`.
#[test]
#[ignore = "talks to the configured model; run with --ignored"]
fn live_chat_against_the_configured_model() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let cfg_path = crate::global_config_path().unwrap();
    let cfg = Config::load(&cfg_path).unwrap();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/live-tui");
    let work = dir.join("work");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&work).unwrap();
    let work = std::path::absolute(work).unwrap();
    let (tx, rx) = hrn_tui::channel();
    let db = dir.join("sessions.db");
    let (mut backend, mcp_manager, _warnings) = runtime
        .block_on(ChatBackend::open(
            &cfg,
            cfg_path,
            &db,
            work.clone(),
            &ChatArgs::default(),
            tx,
            runtime.handle().clone(),
            None,
        ))
        .unwrap();
    let options = backend
        .options_for_startup(&crate::resume::StartupSession::Fresh)
        .unwrap();
    let mut app = App::new(Box::new(backend), options);
    let mut log = String::new();
    let turn = Duration::from_mins(4);

    app.paste(
        "Without using any tools, answer in markdown: a level-2 heading, two bullet points, \
         one **bold** word, inline `code`, and a 2x2 table.",
    );
    app.press(key(KeyCode::Enter));
    assert!(
        drive(&mut app, &rx, turn, |a| !a.is_busy()),
        "markdown turn timed out"
    );
    let shown = dump(&mut app, &mut log, "1. markdown turn");
    let rendered = shown.contains('•') && shown.contains('╭') && !shown.contains("## ");
    assert!(rendered, "markdown is rendered\n{shown}");

    app.paste("Use the write tool to create hello.txt containing exactly the text: hi");
    app.press(key(KeyCode::Enter));
    let asked = drive(&mut app, &rx, turn, |a| {
        a.awaiting_approval() || !a.is_busy()
    });
    let shown = dump(&mut app, &mut log, "2a. approval prompt");
    assert!(
        asked && app.awaiting_approval(),
        "no approval prompt\n{shown}"
    );
    // The model may follow up with more gated calls (say, a check via bash); approve each.
    let mut finished = false;
    for _ in 0..5 {
        app.press(key(KeyCode::Char('y')));
        finished = drive(&mut app, &rx, turn, |a| {
            a.awaiting_approval() || !a.is_busy()
        }) && !app.is_busy();
        if finished {
            break;
        }
    }
    let shown = dump(&mut app, &mut log, "2b. after approving");
    assert!(finished, "the approved turn did not finish\n{shown}");
    let written = std::fs::read_to_string(work.join("hello.txt")).unwrap_or_default();
    assert_eq!(written.trim(), "hi");

    app.paste("Count from 1 to 3000, one number per line, and nothing else.");
    app.press(key(KeyCode::Enter));
    drive(&mut app, &rx, Duration::from_secs(6), |_| false);
    dump(&mut app, &mut log, "3a. streaming before esc");
    assert!(app.is_busy(), "the counting turn ended before esc");
    app.press(key(KeyCode::Esc));
    let stopped = drive(&mut app, &rx, Duration::from_secs(30), |a| !a.is_busy());
    let shown = dump(&mut app, &mut log, "3b. after esc");
    std::fs::write(dir.join("screens.txt"), &log).unwrap();
    println!("{log}");
    runtime.block_on(mcp_manager.shutdown());
    assert!(
        stopped && shown.contains("interrupted"),
        "esc did not interrupt\n{shown}"
    );
}

#[test]
fn fresh_session_auto_relay_connects_on_first_try_and_supports_lan_override() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let root = std::env::temp_dir().join(format!("hrn-remote-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let auth_dir = home.join(".harness").join("auth");
    let work = root.join("work");
    std::fs::create_dir_all(&auth_dir).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let cfg_toml = "[[providers]]\n\
        name = \"seed\"\n\
        kind = \"openai_chat\"\n\
        base_url = \"http://127.0.0.1:9/v1\"\n\
        [models.seed]\n\
        provider = \"seed\"\n\
        id = \"seed-model\"\n\
        context_window = 8192\n\
        [roles]\n\
        default = \"seed\"\n";
    let cfg_path = home.join(".harness").join("config.toml");
    std::fs::write(&cfg_path, cfg_toml).unwrap();
    let cfg = Config::parse(cfg_toml).unwrap();
    let (tx, _rx) = hrn_tui::channel();
    let db = root.join("sessions.db");
    let (mut backend, mcp_manager, _warnings) = runtime
        .block_on(ChatBackend::open(
            &cfg,
            cfg_path,
            &db,
            work,
            &ChatArgs::default(),
            tx,
            runtime.handle().clone(),
            None,
        ))
        .unwrap();

    // Regression test for Bug #1 (`os error 10061` on first `/remote relay` / `/collab` in a
    // brand-new session where `self.session` is still `None`): creating the session MUST happen
    // before `AutoRelay` starts so `set_session` never drops the embedded relay listener.
    assert!(backend.session.is_none());
    let msg = backend
        .start_collab_host_sync(false, false)
        .expect("first-try collab host must connect without os error 10061");
    assert!(msg.contains("collab room"), "{msg}");
    assert!(backend.session.is_some());
    assert!(backend.auto_relay.is_some());
    assert!(backend.collab_host.is_some());

    // Opening `/remote relay` reuses the already-open room immediately.
    let relay_out = backend.remote_command_live(Some("relay")).unwrap();
    assert!(matches!(
        relay_out,
        hrn_tui::RemoteCommandOutcome::Open(ref v) if v.can_control && v.url.contains("/r/")
    ));

    // Explicit `/remote lan` switches to direct LAN/Tailscale mode immediately.
    let lan_out = backend.remote_command_live(Some("lan")).unwrap();
    assert!(matches!(
        lan_out,
        hrn_tui::RemoteCommandOutcome::Open(ref v) if v.transport_label.contains("LAN / Tailscale")
    ));

    let stop_msg = backend.remote_stop_live();
    assert!(stop_msg.contains("remote stopped"), "{stop_msg}");
    drop(backend);
    runtime.block_on(mcp_manager.shutdown());
    let _ = std::fs::remove_dir_all(&root);
}

async fn read_http_post_body(sock: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_len = 0usize;
    loop {
        let n = sock.read(&mut chunk).await.expect("read http");
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
        if header_end.is_none()
            && let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n")
        {
            header_end = Some(pos + 4);
            let head = String::from_utf8_lossy(&raw[..pos]);
            for line in head.lines() {
                if let Some((k, v)) = line.split_once(':')
                    && k.trim().eq_ignore_ascii_case("content-length")
                {
                    content_len = v.trim().parse().unwrap_or(0);
                }
            }
        }
        if let Some(end) = header_end
            && raw.len() >= end + content_len
        {
            break;
        }
    }
    let body = header_end.map_or(&raw[..], |end| &raw[end..]);
    String::from_utf8_lossy(body).into_owned()
}

fn spawn_loopback_openai_server(
    rt: &tokio::runtime::Runtime,
) -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::AsyncWriteExt;

    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_srv = std::sync::Arc::clone(&seen);

    let handle = rt.spawn(async move {
        for turn in 0..2 {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let body = read_http_post_body(&mut sock).await;
            seen_srv.lock().expect("seen").push(body);
            let sse = if turn == 0 {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Plan: write e2e.txt\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_e2e_1\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"e2e.txt\\\",\\\"content\\\":\\\"ok-from-loopback\\\"}\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":32,\"completion_tokens\":14}}\n\n",
                    "data: [DONE]\n\n"
                )
            } else {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"## Done\\n\\n- wrote `e2e.txt`\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":48,\"completion_tokens\":9}}\n\n",
                    "data: [DONE]\n\n"
                )
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
            sock.write_all(resp.as_bytes()).await.expect("write sse");
        }
    });
    (addr, seen, handle)
}

#[test]
fn loopback_openai_chat_e2e_streams_reasoning_tool_approval_and_writes_file() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (addr, seen, server) = spawn_loopback_openai_server(&runtime);

    let root = std::env::temp_dir().join(format!("hrn-chat-loopback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(home.join(".harness")).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let cfg_toml = format!(
        "[tools]\napproval_mode = \"default\"\n[session]\nauto_title = false\n\
         [[providers]]\nname = \"loopback\"\nkind = \"openai_chat\"\nbase_url = \"http://{addr}/v1\"\n\
         [models.mock]\nprovider = \"loopback\"\nid = \"mock-qwen\"\ncontext_window = 32768\nreasoning = true\n\
         [roles]\ndefault = \"mock\"\n"
    );
    let cfg_path = home.join(".harness").join("config.toml");
    std::fs::write(&cfg_path, &cfg_toml).unwrap();
    let cfg = Config::parse(&cfg_toml).unwrap();

    let (tx, rx) = hrn_tui::channel();
    let db = root.join("sessions.db");
    let (mut backend, mcp_manager, _warnings) = runtime
        .block_on(ChatBackend::open(
            &cfg,
            cfg_path,
            &db,
            work.clone(),
            &ChatArgs::default(),
            tx,
            runtime.handle().clone(),
            None,
        ))
        .unwrap();
    // When `fs` is compiled out during removability checks, skip gracefully.
    if backend.runtime.tools().get("write").is_none() {
        server.abort();
        drop(backend);
        runtime.block_on(mcp_manager.shutdown());
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    let options = backend
        .options_for_startup(&crate::resume::StartupSession::Fresh)
        .unwrap();
    let mut app = App::new(Box::new(backend), options);

    app.paste("Create e2e.txt");
    app.press(key(KeyCode::Enter));
    let asked = drive(&mut app, &rx, Duration::from_secs(15), |a| {
        a.awaiting_approval() || !a.is_busy()
    });
    assert!(
        asked && app.awaiting_approval(),
        "expected approval prompt (asked={asked}, busy={}, seen={}):\n{}",
        app.is_busy(),
        seen.lock().unwrap().len(),
        screen(&mut app)
    );

    // Wait past the 500ms accidental-press guard window before approving with 'y'.
    std::thread::sleep(Duration::from_millis(550));
    app.press(key(KeyCode::Char('y')));
    let finished = drive(&mut app, &rx, Duration::from_secs(15), |a| !a.is_busy());
    let shown = screen(&mut app);
    assert!(finished, "loopback turn did not finish:\n{shown}");
    assert!(
        shown.contains("Done") && shown.contains("e2e.txt"),
        "{shown}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("e2e.txt")).unwrap_or_default(),
        "ok-from-loopback"
    );

    runtime.block_on(server).unwrap();
    let bodies = seen.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("mock-qwen"), "{}", bodies[0]);
    assert!(bodies[1].contains("call_e2e_1"), "{}", bodies[1]);

    drop(app);
    runtime.block_on(mcp_manager.shutdown());
    let _ = std::fs::remove_dir_all(&root);
}
