//! Deterministic unit tests for `hrn-mod-desktop` backed by an in-memory mock duplex bridge.

use crate::win_bridge::DesktopBridge;
use crate::{
    BackgroundFallback, DesktopConfig, DesktopError, DesktopManager, DesktopTool, normalize_role,
    register, role_matches,
};
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, EventSink, Host, PermissionMode, Registry, Shared, Tool,
    ToolApproval, ToolContext, ToolEffect,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

pub(crate) fn test_ctx() -> ToolContext {
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
        is_subagent: false,
    }
}

fn spawn_mock_bridge<F>(mut handler: F) -> (DesktopBridge, tokio::task::JoinHandle<Vec<Value>>)
where
    F: FnMut(&Value) -> Value + Send + 'static,
{
    let (c2s_read, c2s_write) = tokio::io::duplex(128 * 1024);
    let (s2c_read, mut s2c_write) = tokio::io::duplex(128 * 1024);

    let handle = tokio::spawn(async move {
        let mut r = BufReader::new(c2s_read);
        let mut line = String::new();
        let mut requests = Vec::new();
        while r.read_line(&mut line).await.unwrap_or(0) > 0 {
            let req: Value = serde_json::from_str(line.trim()).expect("req");
            line.clear();
            let resp = handler(&req);
            requests.push(req);
            let mut text = resp.to_string();
            text.push('\n');
            s2c_write.write_all(text.as_bytes()).await.expect("write");
            s2c_write.flush().await.expect("flush");
        }
        requests
    });

    (DesktopBridge::from_streams(s2c_read, c2s_write), handle)
}

#[test]
fn desktop_tool_requires_confirmation_and_is_mutating() {
    let tool = DesktopTool::new(Arc::new(DesktopManager::new()));
    assert_eq!(tool.spec().name, "desktop");
    assert_eq!(tool.approval(&json!({})), ToolApproval::exec());
    assert_eq!(tool.effect(), ToolEffect::Mutating);
}

#[test]
fn desktop_approval_splits_read_and_exec() {
    let tool = DesktopTool::new(Arc::new(DesktopManager::new()));
    for read_action in [
        "list_windows",
        "ax",
        "find",
        "el_read",
        "displays",
        "capabilities",
        "screenshot",
        "window_capture",
        "zoom",
        "clipboard_read",
        "apps_list",
    ] {
        assert_eq!(
            tool.approval(&json!({ "action": read_action })),
            ToolApproval::read(),
            "expected Read approval for {read_action}"
        );
    }
    for exec_action in [
        "click",
        "double_click",
        "right_click",
        "move",
        "scroll",
        "type_text",
        "hotkey",
        "set_value",
        "select_text",
        "invoke",
        "focus",
        "el_type",
        "clipboard_write",
        "open_app",
        "close",
        "unknown_op",
    ] {
        assert_eq!(
            tool.approval(&json!({ "action": exec_action })),
            ToolApproval::exec(),
            "expected Exec approval for {exec_action}"
        );
    }
    assert_eq!(tool.approval(&json!({})), ToolApproval::exec());
}

#[test]
fn desktop_config_loads_defaults_and_registers_help_page() {
    let def = DesktopConfig::default();
    assert_eq!(def.display, "active");
    assert_eq!(def.max_width, 3840);
    assert_eq!(def.max_height, 2400);
    assert_eq!(def.background_fallback, BackgroundFallback::Takeover);

    let mut table = BTreeMap::new();
    table.insert(
        "desktop".to_owned(),
        json!({ "background_fallback": "refuse", "max_width": 1920 }),
    );
    let host = Host {
        cwd: PathBuf::from("."),
        data_dir: PathBuf::from("."),
    };
    let reg = Registry::new(host, table, Arc::new(Shared::new()))
        .module("desktop", register)
        .expect("register")
        .finish()
        .expect("finish");
    assert!(reg.help_pages.contains_key("desktop"));
    assert_eq!(reg.deferrable.len(), 1);
}

#[test]
fn role_normalizer_handles_case_separators_and_aliases() {
    assert_eq!(normalize_role("ControlType.MenuBar"), "menubar");
    assert_eq!(normalize_role("menu_bar"), "menubar");
    assert_eq!(normalize_role("Menu-Bar"), "menubar");
    assert!(role_matches("ControlType.Edit", "text_box"));
    assert!(role_matches("CheckBox", "check-box"));
    assert!(role_matches("ComboBox", "dropdown"));
    assert_eq!(
        DesktopError::StaleRef("expired".to_owned()).to_string(),
        "[StaleRef] expired"
    );
}

#[tokio::test]
async fn desktop_tool_scales_coordinates_after_screenshot_and_returns_image() {
    let (bridge, srv) = spawn_mock_bridge(|req| {
        let op = req.get("op").and_then(Value::as_str).unwrap_or("");
        match op {
            "screenshot" => json!({
                "ok": true,
                "origin_x": 0,
                "origin_y": 0,
                "width": 200,
                "height": 100,
                "png_b64": TINY_PNG_B64
            }),
            "list_windows" => json!({
                "ok": true,
                "windows": [{"title": "Terminal", "x": 0, "y": 0, "w": 800, "h": 600, "pid": 42}]
            }),
            _ => json!({"ok": true}),
        }
    });

    let mgr = Arc::new(DesktopManager::new());
    mgr.inject_test_bridge(bridge).await;
    let tool = DesktopTool::new(mgr);
    let ctx = test_ctx();

    let shot = tool
        .execute(&ctx, json!({"action": "screenshot"}))
        .await
        .expect("screenshot");
    assert_eq!(shot.images.len(), 1);
    assert!(
        shot.content.contains("200x100 physical"),
        "{}",
        shot.content
    );

    // TINY_PNG is 1x1, physical is 200x100 -> clicking image (1, 1) maps to physical (200, 100).
    let click = tool
        .execute(&ctx, json!({"action": "click", "x": 1, "y": 1}))
        .await
        .expect("click");
    assert!(click.content.contains("(200, 100)"), "{}", click.content);

    let wins = tool
        .execute(&ctx, json!({"action": "list_windows"}))
        .await
        .expect("windows");
    assert!(wins.content.contains("Terminal"), "{}", wins.content);

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
    let reqs = srv.await.expect("join");
    let click_req = reqs
        .iter()
        .find(|r| r.get("op").and_then(Value::as_str) == Some("click"))
        .expect("click req");
    assert_eq!(click_req["x"], 200);
    assert_eq!(click_req["y"], 100);
}

#[tokio::test]
async fn ax_tree_parses_uia_json() {
    let (bridge, srv) = spawn_mock_bridge(|req| {
        match req.get("op").and_then(Value::as_str).unwrap_or("") {
            "list_windows" => json!({
                "ok": true,
                "windows": [{"hwnd": 1001, "title": "Untitled - Notepad", "x": 10, "y": 20, "w": 800, "h": 600, "pid": 500}]
            }),
            "ax" => json!({
                "ok": true,
                "hwnd": 1001,
                "truncated": false,
                "nodes": [
                    {"role": "ControlType.Window", "name": "Untitled - Notepad", "value": "", "states": [], "runtime_id": [42, 1], "x": 10, "y": 20, "w": 800, "h": 600, "depth": 0},
                    {"role": "ControlType.MenuBar", "name": "Application", "value": "", "states": [], "runtime_id": [42, 2], "x": 10, "y": 50, "w": 800, "h": 24, "depth": 1},
                    {"role": "ControlType.Edit", "name": "Text Editor", "value": "hello world", "states": ["focused"], "runtime_id": [42, 3], "x": 10, "y": 74, "w": 800, "h": 500, "depth": 1}
                ]
            }),
            _ => json!({"ok": true}),
        }
    });

    let mgr = Arc::new(DesktopManager::new());
    mgr.inject_test_bridge(bridge).await;
    let tool = DesktopTool::new(mgr);
    let ctx = test_ctx();

    let wins = tool
        .execute(&ctx, json!({"action": "list_windows"}))
        .await
        .expect("list_windows");
    assert!(wins.content.contains("[id=1]"), "{}", wins.content);

    let ax = tool
        .execute(&ctx, json!({"action": "ax", "window_id": 1}))
        .await
        .expect("ax");
    assert!(
        ax.content
            .contains("- Window \"Untitled - Notepad\" [ref=e1]"),
        "{}",
        ax.content
    );
    assert!(
        ax.content.contains("  - MenuBar \"Application\" [ref=e2]"),
        "{}",
        ax.content
    );
    assert!(
        ax.content
            .contains("  - Edit \"Text Editor\" [ref=e3]: \"hello world\" (focused)"),
        "{}",
        ax.content
    );

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
    let _ = srv.await;
}

#[tokio::test]
async fn ref_expires_after_window_close() {
    let mut list_calls = 0usize;
    let (bridge, srv) = spawn_mock_bridge(move |req| {
        match req.get("op").and_then(Value::as_str).unwrap_or("") {
            "list_windows" => {
                list_calls += 1;
                if list_calls == 1 {
                    json!({
                        "ok": true,
                        "windows": [{"hwnd": 777, "title": "Notepad", "x": 0, "y": 0, "w": 500, "h": 400, "pid": 99}]
                    })
                } else {
                    json!({ "ok": true, "windows": [] })
                }
            }
            "ax" => json!({
                "ok": true,
                "hwnd": 777,
                "nodes": [
                    {"role": "Edit", "name": "Editor", "value": "", "states": [], "runtime_id": [7, 1], "x": 0, "y": 0, "w": 500, "h": 400, "depth": 0}
                ]
            }),
            "el_read" => json!({"ok": true, "field": "value", "value": "still alive"}),
            _ => json!({"ok": true}),
        }
    });

    let mgr = Arc::new(DesktopManager::new());
    mgr.inject_test_bridge(bridge).await;
    let tool = DesktopTool::new(mgr);
    let ctx = test_ctx();

    let _ = tool
        .execute(&ctx, json!({"action": "list_windows"}))
        .await
        .expect("list 1");
    let ax = tool
        .execute(&ctx, json!({"action": "ax", "window_id": 1}))
        .await
        .expect("ax");
    assert!(ax.content.contains("[ref=e1]"), "{}", ax.content);

    let live_read = tool
        .execute(
            &ctx,
            json!({"action": "el_read", "ref": "e1", "field": "value"}),
        )
        .await
        .expect("live read");
    assert!(live_read.content.contains("still alive"));

    // Window closes and disappears on next list_windows -> ref must expire with StaleRef.
    let _ = tool
        .execute(&ctx, json!({"action": "list_windows"}))
        .await
        .expect("list 2");
    let err = tool
        .execute(
            &ctx,
            json!({"action": "el_read", "ref": "e1", "field": "value"}),
        )
        .await
        .expect_err("stale ref error")
        .to_string();
    assert!(err.contains("[StaleRef]"), "{err}");

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
    let _ = srv.await;
}

#[tokio::test]
async fn coordinate_frame_invalid_after_resize() {
    let mut resized = false;
    let (bridge, srv) = spawn_mock_bridge(move |req| {
        match req.get("op").and_then(Value::as_str).unwrap_or("") {
            "list_windows" => json!({
                "ok": true,
                "windows": [{"hwnd": 900, "title": "App", "x": 100, "y": 100, "w": 400, "h": 300, "pid": 12}]
            }),
            "window_capture" => json!({
                "ok": true,
                "hwnd": 900,
                "origin_x": 100,
                "origin_y": 100,
                "width": 400,
                "height": 300,
                "covered": true,
                "png_b64": TINY_PNG_B64
            }),
            "window_rect" => {
                if resized {
                    json!({"ok": true, "hwnd": 900, "x": 100, "y": 100, "w": 640, "h": 480})
                } else {
                    resized = true;
                    json!({"ok": true, "hwnd": 900, "x": 100, "y": 100, "w": 400, "h": 300})
                }
            }
            _ => json!({"ok": true}),
        }
    });

    let mgr = Arc::new(DesktopManager::new());
    mgr.inject_test_bridge(bridge).await;
    let tool = DesktopTool::new(mgr);
    let ctx = test_ctx();

    let _ = tool
        .execute(&ctx, json!({"action": "list_windows"}))
        .await
        .expect("list");
    let cap = tool
        .execute(&ctx, json!({"action": "window_capture", "id": 1}))
        .await
        .expect("capture");
    assert!(cap.content.contains("covered=true"), "{}", cap.content);

    // First click sees matching rect (400x300) and succeeds.
    let ok_click = tool
        .execute(&ctx, json!({"action": "click", "x": 1, "y": 1}))
        .await
        .expect("click before resize");
    assert!(
        ok_click.content.contains("(500, 400)"),
        "{}",
        ok_click.content
    );

    // Second click sees resized rect (640x480) and must fail with InvalidCoordinateFrame.
    let err = tool
        .execute(&ctx, json!({"action": "click", "x": 1, "y": 1}))
        .await
        .expect_err("invalid frame error")
        .to_string();
    assert!(err.contains("[InvalidCoordinateFrame]"), "{err}");

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
    let _ = srv.await;
}

#[tokio::test]
async fn zoom_preserves_active_coordinate_frame() {
    let (bridge, srv) =
        spawn_mock_bridge(
            |req| match req.get("op").and_then(Value::as_str).unwrap_or("") {
                "screenshot" => json!({
                    "ok": true,
                    "origin_x": 10,
                    "origin_y": 20,
                    "width": 200,
                    "height": 100,
                    "png_b64": TINY_PNG_B64
                }),
                "zoom" => json!({
                    "ok": true,
                    "x": 10,
                    "y": 20,
                    "width": 50,
                    "height": 50,
                    "png_b64": TINY_PNG_B64
                }),
                _ => json!({"ok": true}),
            },
        );

    let mgr = Arc::new(DesktopManager::new());
    mgr.inject_test_bridge(bridge).await;
    let tool = DesktopTool::new(mgr);
    let ctx = test_ctx();

    let _ = tool
        .execute(&ctx, json!({"action": "screenshot"}))
        .await
        .expect("shot");
    let zoomed = tool
        .execute(
            &ctx,
            json!({"action": "zoom", "x": 0, "y": 0, "w": 1, "h": 1}),
        )
        .await
        .expect("zoom");
    assert_eq!(zoomed.images.len(), 1);
    assert!(zoomed.content.contains("unchanged"), "{}", zoomed.content);

    // Clicking (1, 1) must still map using the original screenshot frame (10+200, 20+100) = (210, 120).
    let click = tool
        .execute(&ctx, json!({"action": "click", "x": 1, "y": 1}))
        .await
        .expect("click");
    assert!(click.content.contains("(210, 120)"), "{}", click.content);

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
    let _ = srv.await;
}

#[tokio::test]
async fn background_input_falls_back_to_takeover_note() {
    let (bridge, srv) = spawn_mock_bridge(|req| {
        match req.get("op").and_then(Value::as_str).unwrap_or("") {
            "list_windows" => json!({
                "ok": true,
                "windows": [{"hwnd": 555, "title": "Editor", "x": 0, "y": 0, "w": 800, "h": 600, "pid": 77}]
            }),
            "bg_type" => {
                if req.get("takeover").and_then(Value::as_bool) == Some(true) {
                    json!({"ok": true, "takeover": true, "unconfirmed": ""})
                } else {
                    json!({"ok": false, "code": "BackgroundUnavailable", "error": "PostMessage blocked"})
                }
            }
            _ => json!({"ok": true}),
        }
    });

    let mgr = Arc::new(DesktopManager::with_config(DesktopConfig {
        background_fallback: BackgroundFallback::Takeover,
        ..DesktopConfig::default()
    }));
    mgr.inject_test_bridge(bridge).await;
    let tool = DesktopTool::new(mgr);
    let ctx = test_ctx();

    let _ = tool
        .execute(&ctx, json!({"action": "list_windows"}))
        .await
        .expect("list");
    let out = tool
        .execute(
            &ctx,
            json!({"action": "type_text", "window_id": 1, "text": "hello"}),
        )
        .await
        .expect("takeover fallback");
    assert!(
        out.content.to_ascii_lowercase().contains("takeover"),
        "expected takeover note in: {}",
        out.content
    );

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
    let reqs = srv.await.expect("join");
    let bg_calls: Vec<&Value> = reqs
        .iter()
        .filter(|r| r.get("op").and_then(Value::as_str) == Some("bg_type"))
        .collect();
    assert_eq!(bg_calls.len(), 2);
    assert_eq!(bg_calls[0]["takeover"], json!(false));
    assert_eq!(bg_calls[1]["takeover"], json!(true));

    // Also verify `refuse` config surfaces BackgroundUnavailable without retrying.
    let (bridge2, srv2) = spawn_mock_bridge(|req| {
        match req.get("op").and_then(Value::as_str).unwrap_or("") {
            "list_windows" => json!({
                "ok": true,
                "windows": [{"hwnd": 555, "title": "Editor", "x": 0, "y": 0, "w": 800, "h": 600, "pid": 77}]
            }),
            "bg_type" => {
                json!({"ok": false, "code": "BackgroundUnavailable", "error": "PostMessage blocked"})
            }
            _ => json!({"ok": true}),
        }
    });
    let mgr2 = Arc::new(DesktopManager::with_config(DesktopConfig {
        background_fallback: BackgroundFallback::Refuse,
        ..DesktopConfig::default()
    }));
    mgr2.inject_test_bridge(bridge2).await;
    let tool2 = DesktopTool::new(mgr2);
    let _ = tool2
        .execute(&ctx, json!({"action": "list_windows"}))
        .await
        .expect("list");
    let err = tool2
        .execute(
            &ctx,
            json!({"action": "type_text", "window_id": 1, "text": "hello"}),
        )
        .await
        .expect_err("refuse should error")
        .to_string();
    assert!(err.contains("[BackgroundUnavailable]"), "{err}");
    let _ = tool2.execute(&ctx, json!({"action": "close"})).await;
    let _ = srv2.await;
}
