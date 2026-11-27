//! Browser tool action handlers (`open`, `snapshot`, `click`, `fill`, `press`, `eval`,
//! `screenshot`, `console`, `network`, `tabs`, `close`).

use crate::axtree::{DEFAULT_MAX_NODES, build_snapshot};
use crate::cdp::CdpClient;
use crate::tabs::{BrowserManager, TabSession};
use hrn_edge_rt::tokio;
use hrn_edge_tools::img::{DEFAULT_MAX_IMAGE_DIM, load_image_from_base64};
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{HrnError, Result, ToolOutput};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const CMD_TIMEOUT: Duration = Duration::from_secs(20);
static SHOT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Dispatch a single `browser` tool call through `mgr`.
pub(crate) async fn run_action(
    mgr: &BrowserManager,
    args: &Value,
    is_subagent: bool,
) -> Result<ToolOutput> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Tool("`browser`: missing `action`".to_owned()))?;
    let app = args.get("app").and_then(Value::as_str);
    let explicit_name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let tab_name = explicit_name.unwrap_or("default");
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if action == "close" {
        return Ok(ToolOutput::ok(mgr.close_tab(explicit_name).await));
    }
    if action == "tabs" {
        return Ok(ToolOutput::ok(
            mgr.list_tabs_summary(app, is_subagent).await?,
        ));
    }

    let initial_url = if action == "open" {
        args.get("url").and_then(Value::as_str)
    } else {
        None
    };

    let mut tabs = mgr.lock_tabs().await;
    let sess = mgr
        .ensure_tab(&mut tabs, tab_name, app, target, initial_url, is_subagent)
        .await?;

    match action {
        "open" => action_open(sess, args).await,
        "snapshot" => action_snapshot(sess, args).await,
        "click" => action_click(sess, args).await,
        "fill" => action_fill(sess, args).await,
        "press" => action_press(sess, args).await,
        "eval" => action_eval(sess, args).await,
        "screenshot" => action_screenshot(sess, args, mgr.config().screenshot_dir.as_deref()).await,
        "console" => {
            let clear = args.get("clear").and_then(Value::as_bool).unwrap_or(false);
            let logs = sess.cdp.console_logs(clear).await;
            if logs.is_empty() {
                Ok(ToolOutput::ok("No console messages captured."))
            } else {
                Ok(ToolOutput::ok(logs.join("\n")))
            }
        }
        "network" => {
            let clear = args.get("clear").and_then(Value::as_bool).unwrap_or(false);
            let logs = sess.cdp.network_logs(clear).await;
            if logs.is_empty() {
                Ok(ToolOutput::ok("No network requests captured."))
            } else {
                Ok(ToolOutput::ok(logs.join("\n")))
            }
        }
        other => Err(HrnError::Tool(format!(
            "`browser`: unknown action `{other}`"
        ))),
    }
}

async fn action_open(sess: &mut TabSession, args: &Value) -> Result<ToolOutput> {
    let url = req_str(args, "url")?;
    let wait_ms = args
        .get("wait_ms")
        .and_then(Value::as_u64)
        .unwrap_or(500)
        .min(10_000);
    if sess.initial_nav_done {
        // Relay `open_tab` already opened this background tab at `url`.
        sess.initial_nav_done = false;
    } else {
        let _ = sess
            .cdp
            .call("Page.navigate", json!({ "url": url }), CMD_TIMEOUT)
            .await?;
    }
    if wait_ms > 0 {
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    }
    let meta = page_meta(&sess.cdp).await;
    let snap = capture_snapshot(sess, DEFAULT_MAX_NODES).await?;
    Ok(ToolOutput::ok(format!("{meta}\n\n{snap}")))
}

async fn action_snapshot(sess: &mut TabSession, args: &Value) -> Result<ToolOutput> {
    let max_nodes = args
        .get("max_nodes")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(DEFAULT_MAX_NODES);
    let meta = page_meta(&sess.cdp).await;
    let snap = capture_snapshot(sess, max_nodes).await?;
    Ok(ToolOutput::ok(format!("{meta}\n\n{snap}")))
}

async fn capture_snapshot(sess: &mut TabSession, max_nodes: usize) -> Result<String> {
    let tree = sess
        .cdp
        .call("Accessibility.getFullAXTree", json!({}), CMD_TIMEOUT)
        .await?;
    let snap = build_snapshot(&tree, max_nodes);
    sess.refs = snap.refs;
    Ok(snap.text)
}

async fn action_click(sess: &mut TabSession, args: &Value) -> Result<ToolOutput> {
    let object_id = resolve_target_object(sess, args).await?;
    let _ = sess
        .cdp
        .call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": "function() { this.scrollIntoView({block:'center', inline:'center'}); this.click(); return true; }",
                "returnByValue": true
            }),
            CMD_TIMEOUT,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let meta = page_meta(&sess.cdp).await;
    Ok(ToolOutput::ok(format!("Clicked element.\n{meta}")))
}

async fn action_fill(sess: &mut TabSession, args: &Value) -> Result<ToolOutput> {
    let value = args
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Tool("`browser` fill: missing `value`".to_owned()))?;
    let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
    let object_id = resolve_target_object(sess, args).await?;
    let _ = sess
        .cdp
        .call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": "function(v) { \
                    this.scrollIntoView({block:'center'}); \
                    this.focus(); \
                    const proto = Object.getPrototypeOf(this); \
                    const desc = proto && Object.getOwnPropertyDescriptor(proto, 'value'); \
                    if (desc && desc.set) { desc.set.call(this, v); } else { this.value = v; } \
                    this.dispatchEvent(new Event('input', { bubbles: true })); \
                    this.dispatchEvent(new Event('change', { bubbles: true })); \
                    return true; \
                }",
                "arguments": [{ "value": value }],
                "returnByValue": true
            }),
            CMD_TIMEOUT,
        )
        .await?;
    if submit {
        dispatch_key(&sess.cdp, "Enter").await?;
    }
    Ok(ToolOutput::ok(if submit {
        "Filled element and pressed Enter."
    } else {
        "Filled element."
    }))
}

async fn action_press(sess: &mut TabSession, args: &Value) -> Result<ToolOutput> {
    let key = req_str(args, "key")?;
    dispatch_key(&sess.cdp, key).await?;
    Ok(ToolOutput::ok(format!("Pressed `{key}`.")))
}

async fn dispatch_key(cdp: &CdpClient, key: &str) -> Result<()> {
    let (code, text) = match key {
        "Enter" => ("Enter", "\r"),
        "Tab" => ("Tab", ""),
        "Escape" => ("Escape", ""),
        "Backspace" => ("Backspace", ""),
        other => (other, if other.len() == 1 { other } else { "" }),
    };
    let mut down = json!({ "type": "keyDown", "key": key, "code": code });
    if !text.is_empty()
        && let Some(obj) = down.as_object_mut()
    {
        obj.insert("text".to_owned(), json!(text));
    }
    let _ = cdp
        .call("Input.dispatchKeyEvent", down, CMD_TIMEOUT)
        .await?;
    let _ = cdp
        .call(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "code": code }),
            CMD_TIMEOUT,
        )
        .await?;
    Ok(())
}

async fn action_eval(sess: &mut TabSession, args: &Value) -> Result<ToolOutput> {
    let expr = req_str(args, "expression")?;
    let res = sess
        .cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": expr,
                "awaitPromise": true,
                "returnByValue": true
            }),
            CMD_TIMEOUT,
        )
        .await?;
    if let Some(ex) = res.get("exceptionDetails") {
        let msg = ex
            .pointer("/exception/description")
            .or_else(|| ex.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("JS exception");
        return Ok(ToolOutput::err(format!("JS evaluation error: {msg}")));
    }
    let out = match res.pointer("/result/value") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        None => res
            .pointer("/result/description")
            .or_else(|| res.pointer("/result/type"))
            .and_then(Value::as_str)
            .unwrap_or("undefined")
            .to_owned(),
    };
    Ok(ToolOutput::ok(out))
}

async fn action_screenshot(
    sess: &mut TabSession,
    args: &Value,
    screenshot_dir: Option<&Path>,
) -> Result<ToolOutput> {
    let full_page = args
        .get("full_page")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Capture directly on the target's CDP session without ever activating the tab.
    let res = sess
        .cdp
        .call(
            "Page.captureScreenshot",
            json!({ "format": "png", "captureBeyondViewport": full_page }),
            CMD_TIMEOUT,
        )
        .await?;
    let b64 = res
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Tool("CDP screenshot returned no image data".to_owned()))?;
    let meta = page_meta(&sess.cdp).await;
    let att = load_image_from_base64(
        b64,
        Some("browser-screenshot.png".to_owned()),
        DEFAULT_MAX_IMAGE_DIM,
    )?;

    let mut summary = format!(
        "Captured browser screenshot ({}x{}, {}).\n{meta}",
        att.width.unwrap_or(0),
        att.height.unwrap_or(0),
        att.media_type
    );
    if let Some(dir) = screenshot_dir
        && let Ok(raw) = hrn_edge_ipc::base64_decode(b64)
        && std::fs::create_dir_all(dir).is_ok()
    {
        let seq = SHOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let file = dir.join(format!("browser-{}-{seq}.png", std::process::id()));
        if std::fs::write(&file, raw).is_ok() {
            let _ = write!(summary, "\nSaved: {}", file.display());
        }
    }

    let mut out = ToolOutput::ok(summary);
    out.images.push(att);
    Ok(out)
}

async fn resolve_target_object(sess: &TabSession, args: &Value) -> Result<String> {
    if let Some(r) = args.get("ref").and_then(Value::as_str) {
        let key = if r.starts_with('@') {
            r.to_owned()
        } else {
            format!("@{r}")
        };
        let backend_id = sess.refs.get(&key).copied().ok_or_else(|| {
            HrnError::Tool(format!(
                "unknown element ref `{key}`; run `browser` `snapshot` to refresh refs"
            ))
        })?;
        let res = sess
            .cdp
            .call(
                "DOM.resolveNode",
                json!({ "backendNodeId": backend_id }),
                CMD_TIMEOUT,
            )
            .await?;
        return res
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| HrnError::Tool(format!("failed to resolve ref `{key}` in DOM")));
    }

    let sel = req_str(args, "selector")?;
    let expr = format!(
        "document.querySelector({})",
        serde_json::to_string(sel).unwrap_or_else(|_| "\"\"".to_owned())
    );
    let res = sess
        .cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": expr, "returnByValue": false }),
            CMD_TIMEOUT,
        )
        .await?;
    res.pointer("/result/objectId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| HrnError::Tool(format!("no element matched selector `{sel}`")))
}

async fn page_meta(cdp: &CdpClient) -> String {
    let res = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": "JSON.stringify({url: location.href, title: document.title})",
                "returnByValue": true
            }),
            Duration::from_secs(5),
        )
        .await;
    if let Ok(val) = res
        && let Some(raw) = val.pointer("/result/value").and_then(Value::as_str)
        && let Ok(parsed) = serde_json::from_str::<Value>(raw)
    {
        let url = parsed.get("url").and_then(Value::as_str).unwrap_or("");
        let title = parsed.get("title").and_then(Value::as_str).unwrap_or("");
        return format!("URL: {url}\nTitle: {title}");
    }
    "URL: (unknown)".to_owned()
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| HrnError::Tool(format!("`browser`: missing `{key}`")))
}
