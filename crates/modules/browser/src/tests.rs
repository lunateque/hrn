use super::*;
use crate::tabs::{BackendTarget, resolve_backend};
use hrn_edge_ipc::{read_native_message, write_native_message};
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{AgentEvent, BoxFuture, CancelFlag, EventSink, PermissionMode, ToolContext};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Minimal valid 1x1 red PNG encoded as base64.
const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

fn make_ctx(is_subagent: bool) -> ToolContext {
    ToolContext {
        session_id: "s".to_owned(),
        turn_id: "t".to_owned(),
        call_id: "c".to_owned(),
        cwd: std::env::temp_dir(),
        permission_mode: PermissionMode::Yolo,
        cancel: CancelFlag::new(),
        events: Arc::new(NullSink),
        ask: None,
        approval: None,
        urls: Arc::default(),
        root_session_id: String::new(),
        inbox: Arc::default(),
        caller: None,
        todos: Arc::new(hrn_kernel::todo::TodoBoards::new()),
        yield_data: Arc::default(),
        is_subagent,
    }
}

async fn read_ws_frame(sock: &mut tokio::net::TcpStream) -> Option<String> {
    let mut hdr = [0u8; 2];
    sock.read_exact(&mut hdr).await.ok()?;
    let [b0, b1] = hdr;
    if (b0 & 0x0F) == 0x8 {
        return None;
    }
    let len7 = b1 & 0x7F;
    let len = match len7 {
        126 => {
            let mut b = [0u8; 2];
            sock.read_exact(&mut b).await.ok()?;
            usize::from(u16::from_be_bytes(b))
        }
        127 => {
            let mut b = [0u8; 8];
            sock.read_exact(&mut b).await.ok()?;
            usize::try_from(u64::from_be_bytes(b)).ok()?
        }
        n => usize::from(n),
    };
    let mut mask = [0u8; 4];
    sock.read_exact(&mut mask).await.ok()?;
    let mut data = vec![0u8; len];
    sock.read_exact(&mut data).await.ok()?;
    for (i, b) in data.iter_mut().enumerate() {
        *b ^= mask.get(i & 3).copied().unwrap_or(0);
    }
    String::from_utf8(data).ok()
}

async fn write_ws_text(sock: &mut tokio::net::TcpStream, text: &str) {
    let data = text.as_bytes();
    let mut out = vec![0x81];
    if data.len() < 126 {
        out.push(u8::try_from(data.len()).unwrap_or(0));
    } else {
        out.push(126);
        out.extend_from_slice(&u16::try_from(data.len()).unwrap_or(0).to_be_bytes());
    }
    out.extend_from_slice(data);
    let _ = sock.write_all(&out).await;
}

fn mock_cdp_result(method: &str) -> Value {
    match method {
        "Accessibility.getFullAXTree" => json!({
            "nodes": [
                { "nodeId": "1", "role": {"value": "RootWebArea"}, "name": {"value": "Demo"}, "childIds": ["2"] },
                { "nodeId": "2", "role": {"value": "button"}, "name": {"value": "Submit"}, "backendDOMNodeId": 42 }
            ]
        }),
        "DOM.resolveNode" => json!({ "object": { "objectId": "obj-42" } }),
        "Runtime.evaluate" => json!({
            "result": { "value": "{\"url\":\"https://example.test\",\"title\":\"Demo\"}" }
        }),
        "Page.captureScreenshot" => json!({ "data": TINY_PNG_B64 }),
        _ => json!({}),
    }
}

/// Spawn a stub Chrome extension speaking Chrome Native Messaging over duplex streams,
/// recording every command received so tests can assert exact relay translation and ownership.
fn spawn_stub_extension(
    mut from_host: tokio::io::DuplexStream,
    mut to_host: tokio::io::DuplexStream,
    existing_tabs: Vec<Value>,
) -> (Arc<StdMutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let log: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
    let log_ref = Arc::clone(&log);
    let task = tokio::spawn(async move {
        let mut next_tab_id = 100i64;
        while let Ok(Some(msg)) = read_native_message(&mut from_host).await {
            log_ref.lock().expect("log").push(msg.clone());
            let id = msg.get("id").and_then(Value::as_i64).unwrap_or(0);
            let cmd = msg.get("cmd").and_then(Value::as_str).unwrap_or("");
            let reply = match cmd {
                "open_tab" => {
                    let tab_id = next_tab_id;
                    next_tab_id += 1;
                    let url = msg
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("about:blank");
                    json!({ "id": id, "ok": true, "result": { "tabId": tab_id, "url": url, "title": "Demo" } })
                }
                "list_tabs" => json!({ "id": id, "ok": true, "result": { "tabs": existing_tabs } }),
                "attach_tab" => {
                    json!({ "id": id, "ok": true, "result": { "tabId": msg.get("tabId") } })
                }
                "detach_tab" | "close_tab" => json!({ "id": id, "ok": true, "result": {} }),
                "cdp" => {
                    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                    json!({ "id": id, "ok": true, "result": mock_cdp_result(method) })
                }
                other => {
                    json!({ "id": id, "ok": false, "error": format!("unexpected cmd `{other}`") })
                }
            };
            if write_native_message(&mut to_host, &reply).await.is_err() {
                break;
            }
        }
    });
    (log, task)
}

#[test]
fn browser_config_reads_defaults_json() {
    let cfg = BrowserConfig::default();
    assert!(!cfg.relay);
    assert!(cfg.headless);
    assert!(cfg.cdp_url.is_none());
    assert!(cfg.binary.is_none());
    assert!(cfg.screenshot_dir.is_none());
}

#[tokio::test]
async fn missing_relay_suggests_install_command() {
    let dir = std::env::temp_dir().join(format!("hrn-missing-relay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let tool = browser_tool(BrowserConfig {
        relay: true,
        data_dir: dir.clone(),
        ..BrowserConfig::default()
    });
    let err = tool
        .execute(&make_ctx(false), json!({ "action": "snapshot" }))
        .await
        .expect_err("must fail without relay");
    assert!(
        err.to_string().contains("run `hrn browser-relay install`"),
        "{err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn relay_translates_open_and_capture() {
    let dir = std::env::temp_dir().join(format!("hrn-relay-open-cap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let (host_in, ext_out) = tokio::io::duplex(64 * 1024);
    let (ext_in, host_out) = tokio::io::duplex(64 * 1024);
    let (log, ext_task) = spawn_stub_extension(ext_in, ext_out, Vec::new());

    let cancel = CancelFlag::new();
    let handle = start_relay_over_io(&dir, host_in, host_out, cancel)
        .await
        .expect("start relay");

    let tool = browser_tool(BrowserConfig {
        relay: true,
        data_dir: dir.clone(),
        ..BrowserConfig::default()
    });
    let ctx = make_ctx(false);

    let open = tool
        .execute(
            &ctx,
            json!({ "action": "open", "name": "work", "url": "https://example.test", "wait_ms": 0 }),
        )
        .await
        .expect("relay open");
    assert!(
        open.content.contains("[button] \"Submit\" @e1"),
        "{}",
        open.content
    );

    let shot = tool
        .execute(&ctx, json!({ "action": "screenshot", "name": "work" }))
        .await
        .expect("relay screenshot");
    assert_eq!(shot.images.len(), 1);
    assert_eq!(
        shot.images.first().map(|i| (i.width, i.height)),
        Some((Some(1), Some(1)))
    );

    let recorded = log.lock().expect("log").clone();
    assert!(
        recorded
            .iter()
            .any(|m| m["cmd"] == "open_tab" && m["url"] == "https://example.test"),
        "expected open_tab command: {recorded:?}"
    );
    assert!(
        recorded
            .iter()
            .any(|m| m["cmd"] == "cdp" && m["method"] == "Page.captureScreenshot"),
        "expected Page.captureScreenshot over relay: {recorded:?}"
    );
    assert!(
        !recorded
            .iter()
            .any(|m| m["method"] == "Target.activateTarget"),
        "screenshots must never activate tabs: {recorded:?}"
    );

    handle.shutdown().await;
    ext_task.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn borrowed_tab_survives_close() {
    let dir = std::env::temp_dir().join(format!("hrn-relay-borrow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let (host_in, ext_out) = tokio::io::duplex(64 * 1024);
    let (ext_in, host_out) = tokio::io::duplex(64 * 1024);
    let existing = vec![json!({
        "tabId": 77,
        "url": "https://mail.example.test/inbox",
        "title": "Inbox - Example Mail",
        "active": true
    })];
    let (log, ext_task) = spawn_stub_extension(ext_in, ext_out, existing);

    let cancel = CancelFlag::new();
    let handle = start_relay_over_io(&dir, host_in, host_out, cancel)
        .await
        .expect("start relay");

    let tool = browser_tool(BrowserConfig {
        relay: true,
        data_dir: dir.clone(),
        ..BrowserConfig::default()
    });
    let ctx = make_ctx(false);

    // Borrow the existing user tab by title substring.
    let snap = tool
        .execute(
            &ctx,
            json!({ "action": "snapshot", "name": "mail", "target": "Inbox", "app": "relay" }),
        )
        .await
        .expect("borrow snapshot");
    assert!(snap.content.contains("Submit"), "{}", snap.content);

    // Closing a borrowed tab detaches without closing the tab in Chrome.
    let close_borrowed = tool
        .execute(&ctx, json!({ "action": "close", "name": "mail" }))
        .await
        .expect("close borrowed");
    assert!(
        close_borrowed.content.contains("Detached"),
        "{}",
        close_borrowed.content
    );

    // Opening a brand-new owned tab and closing it DOES send `close_tab`.
    let _ = tool
        .execute(
            &ctx,
            json!({ "action": "open", "name": "scratch", "url": "https://example.test", "wait_ms": 0 }),
        )
        .await
        .expect("open owned");
    let close_owned = tool
        .execute(&ctx, json!({ "action": "close", "name": "scratch" }))
        .await
        .expect("close owned");
    assert!(
        close_owned.content.contains("Closed"),
        "{}",
        close_owned.content
    );

    let recorded = log.lock().expect("log").clone();
    assert!(
        recorded
            .iter()
            .any(|m| m["cmd"] == "detach_tab" && m["tabId"] == 77),
        "borrowed tab must be detached: {recorded:?}"
    );
    assert!(
        !recorded
            .iter()
            .any(|m| m["cmd"] == "close_tab" && m["tabId"] == 77),
        "borrowed tab must NEVER be closed: {recorded:?}"
    );
    assert!(
        recorded
            .iter()
            .any(|m| m["cmd"] == "close_tab" && m["tabId"] == 100),
        "owned tab must be closed: {recorded:?}"
    );

    handle.shutdown().await;
    ext_task.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn subagent_defaults_skip_relay() {
    // Policy check: `[browser] relay = true` routes root agents to Relay, subagents to CDP/Managed.
    let cfg_relay_only = BrowserConfig {
        relay: true,
        cdp_url: None,
        ..BrowserConfig::default()
    };
    assert_eq!(
        resolve_backend(None, &cfg_relay_only, false),
        BackendTarget::Relay
    );
    assert_eq!(
        resolve_backend(None, &cfg_relay_only, true),
        BackendTarget::Managed(None)
    );

    // End-to-end check: with both relay and direct CDP configured, a subagent skips relay and uses direct CDP.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let srv = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 2048];
        let _ = sock.read(&mut buf).await;
        let _ = sock
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await;
        while let Some(raw) = read_ws_frame(&mut sock).await {
            let Ok(req) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let id = req.get("id").and_then(Value::as_i64).unwrap_or(0);
            let method = req.get("method").and_then(Value::as_str).unwrap_or("");
            let resp = json!({ "id": id, "result": mock_cdp_result(method) });
            write_ws_text(&mut sock, &resp.to_string()).await;
        }
    });

    let empty_dir = std::env::temp_dir().join(format!("hrn-subagent-skip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&empty_dir);
    std::fs::create_dir_all(&empty_dir).expect("mkdir");

    let tool = browser_tool(BrowserConfig {
        relay: true,
        cdp_url: Some(format!("ws://127.0.0.1:{port}/devtools/page/1")),
        data_dir: empty_dir.clone(),
        ..BrowserConfig::default()
    });
    // Subagent skips relay (which isn't even running in `empty_dir`) and succeeds against direct CDP.
    let snap = tool
        .execute(&make_ctx(true), json!({ "action": "snapshot" }))
        .await
        .expect("subagent should bypass relay and hit direct cdp");
    assert!(snap.content.contains("Submit"), "{}", snap.content);

    let _ = tool
        .execute(&make_ctx(true), json!({ "action": "close" }))
        .await;
    srv.abort();
    let _ = std::fs::remove_dir_all(&empty_dir);
}

#[tokio::test]
async fn browser_tool_runs_snapshot_click_console_and_screenshot_over_cdp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let srv = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 2048];
        let _ = sock.read(&mut buf).await;
        let _ = sock
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await;
        write_ws_text(
            &mut sock,
            &json!({
                "method": "Runtime.consoleAPICalled",
                "params": { "type": "warn", "args": [{ "value": "deprecated api" }] }
            })
            .to_string(),
        )
        .await;

        while let Some(raw) = read_ws_frame(&mut sock).await {
            let Ok(req) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let id = req.get("id").and_then(Value::as_i64).unwrap_or(0);
            let method = req.get("method").and_then(Value::as_str).unwrap_or("");
            let resp = json!({ "id": id, "result": mock_cdp_result(method) });
            write_ws_text(&mut sock, &resp.to_string()).await;
        }
    });

    let tool = browser_tool(BrowserConfig {
        cdp_url: Some(format!("ws://127.0.0.1:{port}/devtools/page/1")),
        ..BrowserConfig::default()
    });
    let ctx = make_ctx(false);

    let open = tool
        .execute(
            &ctx,
            json!({"action": "open", "url": "https://example.test", "wait_ms": 0}),
        )
        .await
        .expect("open");
    assert!(
        open.content.contains("[button] \"Submit\" @e1"),
        "{}",
        open.content
    );

    let click = tool
        .execute(&ctx, json!({"action": "click", "ref": "@e1"}))
        .await
        .expect("click");
    assert!(click.content.contains("Clicked"), "{}", click.content);

    let console = tool
        .execute(&ctx, json!({"action": "console"}))
        .await
        .expect("console");
    assert!(
        console.content.contains("deprecated api"),
        "{}",
        console.content
    );

    let shot = tool
        .execute(&ctx, json!({"action": "screenshot"}))
        .await
        .expect("screenshot");
    assert_eq!(shot.images.len(), 1);
    assert_eq!(
        shot.images.first().map(|i| (i.width, i.height)),
        Some((Some(1), Some(1)))
    );

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
    srv.abort();
}

#[tokio::test]
#[ignore = "launches real headless Chromium/Edge via CDP"]
async fn live_browser_launches_headless_cdp_and_captures_snapshot_and_screenshot() {
    let tool = browser_tool(BrowserConfig::from_env());
    let ctx = make_ctx(false);

    let open = tool
        .execute(
            &ctx,
            json!({
                "action": "open",
                "url": "data:text/html,<title>HrnSmoke</title><button id='b'>PressMe</button>",
                "wait_ms": 300
            }),
        )
        .await
        .expect("open");
    assert!(open.content.contains("HrnSmoke"), "{}", open.content);
    assert!(open.content.contains("PressMe"), "{}", open.content);
    assert!(open.content.contains("@e1"), "{}", open.content);

    let eval = tool
        .execute(&ctx, json!({"action": "eval", "expression": "2 + 40"}))
        .await
        .expect("eval");
    assert_eq!(eval.content.trim(), "42");

    let shot = tool
        .execute(&ctx, json!({"action": "screenshot"}))
        .await
        .expect("screenshot");
    assert_eq!(shot.images.len(), 1);
    let img = shot.images.first().expect("img");
    assert!(img.width.unwrap_or(0) > 0);
    assert!(img.height.unwrap_or(0) > 0);

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
}
