//! UI Automation inspection and control actions (`ax`, `find`, `el_read`, `set_value`,
//! `select_text`, `invoke`, `focus`, `el_type`).

use crate::ax_fmt::{parse_ax_nodes, parse_hwnd_value, parse_single_ax_node};
use crate::error::DesktopError;
use crate::session::{AxNode, DEFAULT_MAX_DEPTH, DesktopSession, MAX_AX_NODES, MAX_FIND_LIMIT};
use crate::win_bridge::DesktopBridge;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{HrnError, Result, ToolOutput};
use std::fmt::Write as _;
use std::time::Duration;

/// Resolved window selector ready to send to the Windows bridge.
#[derive(Clone, Debug, Default)]
pub(crate) struct WindowTarget {
    pub window_id: Option<u32>,
    pub hwnd: i64,
    pub title: String,
    pub explicit: bool,
}

/// Parse `window_id` / `id` / `window` arguments against the session's live window registry.
pub(crate) fn resolve_window_target(
    session: &DesktopSession,
    args: &Value,
) -> std::result::Result<WindowTarget, DesktopError> {
    if let Some(id) =
        parse_window_id_arg(args, "window_id").or_else(|| parse_window_id_arg(args, "id"))
    {
        let info = session.window_by_id(id).ok_or_else(|| {
            DesktopError::WindowNotFound(format!(
                "window id `{id}` not found; call `list_windows` first"
            ))
        })?;
        return Ok(WindowTarget {
            window_id: Some(info.id),
            hwnd: info.hwnd,
            title: String::new(),
            explicit: true,
        });
    }
    if let Some(raw) = args.get("window").and_then(Value::as_str).map(str::trim)
        && !raw.is_empty()
    {
        if let Some(id) = parse_id_str(raw)
            && let Some(info) = session.window_by_id(id)
        {
            return Ok(WindowTarget {
                window_id: Some(info.id),
                hwnd: info.hwnd,
                title: String::new(),
                explicit: true,
            });
        }
        if let Some(info) = session.find_window_by_title(raw) {
            return Ok(WindowTarget {
                window_id: Some(info.id),
                hwnd: info.hwnd,
                title: String::new(),
                explicit: true,
            });
        }
        return Ok(WindowTarget {
            window_id: None,
            hwnd: 0,
            title: raw.to_owned(),
            explicit: true,
        });
    }
    Ok(WindowTarget::default())
}

fn parse_window_id_arg(args: &Value, key: &str) -> Option<u32> {
    let v = args.get(key)?;
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    v.as_str().and_then(parse_id_str)
}

fn parse_id_str(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix('w').unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok()
}

pub(crate) async fn action_ax(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
    timeout: Duration,
) -> Result<ToolOutput> {
    let target = resolve_window_target(session, args)?;
    let max_depth = args
        .get("max_depth")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_MAX_DEPTH);
    let max_nodes = args
        .get("max_nodes")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .filter(|&v| v > 0)
        .unwrap_or(MAX_AX_NODES)
        .min(MAX_AX_NODES);

    let res = bridge
        .call(
            &json!({
                "op": "ax",
                "hwnd": target.hwnd,
                "window": target.title,
                "max_depth": max_depth,
                "max_nodes": max_nodes
            }),
            timeout,
        )
        .await?;

    let hwnd = res
        .get("hwnd")
        .and_then(parse_hwnd_value)
        .unwrap_or(target.hwnd);
    let window_id = target
        .window_id
        .or_else(|| session.window_by_hwnd(hwnd).map(|w| w.id));
    let nodes = res.get("nodes").map(parse_ax_nodes).unwrap_or_default();
    let bridge_truncated = res
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (tree, count, local_truncated) =
        session.record_ax_snapshot(hwnd, window_id, &nodes, max_depth, max_nodes);
    let truncated = bridge_truncated || local_truncated;

    if tree.is_empty() {
        return Ok(ToolOutput::ok("AX tree is empty."));
    }
    let header = match window_id {
        Some(id) => format!(
            "AX tree for window id={id} ({count} node(s){}):\n{tree}",
            trunc_note(truncated)
        ),
        None => format!(
            "AX tree ({count} node(s){}):\n{tree}",
            trunc_note(truncated)
        ),
    };
    Ok(ToolOutput::ok(header))
}

fn trunc_note(truncated: bool) -> &'static str {
    if truncated { ", truncated" } else { "" }
}

pub(crate) async fn action_find(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
    timeout: Duration,
) -> Result<ToolOutput> {
    let target = resolve_window_target(session, args)?;
    let role = args.get("role").and_then(Value::as_str).unwrap_or("");
    let title = args.get("title").and_then(Value::as_str).unwrap_or("");
    let value = args.get("value").and_then(Value::as_str).unwrap_or("");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .filter(|&v| v > 0)
        .unwrap_or(100)
        .min(MAX_FIND_LIMIT);

    let res = bridge
        .call(
            &json!({
                "op": "find",
                "hwnd": target.hwnd,
                "window": target.title,
                "role": role,
                "title": title,
                "value": value,
                "limit": limit
            }),
            timeout,
        )
        .await?;

    let hwnd = res
        .get("hwnd")
        .and_then(parse_hwnd_value)
        .unwrap_or(target.hwnd);
    let window_id = target
        .window_id
        .or_else(|| session.window_by_hwnd(hwnd).map(|w| w.id));
    let nodes = res.get("matches").map(parse_ax_nodes).unwrap_or_default();
    let registered = session.register_find_matches(hwnd, window_id, &nodes);

    if registered.is_empty() {
        return Ok(ToolOutput::ok("No matching UIA elements found."));
    }
    let mut out = format!("Found {} matching element(s):\n", registered.len());
    for (ref_id, node) in &registered {
        format_match_line(&mut out, ref_id, node);
    }
    Ok(ToolOutput::ok(out.trim_end().to_owned()))
}

fn format_match_line(out: &mut String, ref_id: &str, node: &AxNode) {
    let (x, y, w, h) = node.bounds;
    let _ = write!(
        out,
        "- {} \"{}\" [ref={ref_id}] ({x},{y} {w}x{h})",
        node.role, node.name
    );
    if let Some(val) = &node.value {
        let _ = write!(out, ": \"{val}\"");
    }
    if !node.states.is_empty() {
        let _ = write!(out, " ({})", node.states.join(", "));
    }
    out.push('\n');
}

pub(crate) async fn action_el_read(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
    timeout: Duration,
) -> Result<ToolOutput> {
    let ax_ref = require_ref(session, args)?;
    let field = args.get("field").and_then(Value::as_str).unwrap_or("value");
    let res = call_ref_op(
        bridge,
        session,
        ax_ref.hwnd,
        &json!({
            "op": "el_read",
            "hwnd": ax_ref.hwnd,
            "runtime_id": ax_ref.runtime_id,
            "field": field
        }),
        timeout,
    )
    .await?;

    match field.trim().to_ascii_lowercase().as_str() {
        "value" => {
            let val = res.get("value").and_then(Value::as_str).unwrap_or("");
            Ok(ToolOutput::ok(format!(
                "[ref={}] value: {val}",
                ax_ref.ref_id
            )))
        }
        "bounds" => {
            let x = res.get("x").and_then(Value::as_i64).unwrap_or(0);
            let y = res.get("y").and_then(Value::as_i64).unwrap_or(0);
            let w = res.get("w").and_then(Value::as_i64).unwrap_or(0);
            let h = res.get("h").and_then(Value::as_i64).unwrap_or(0);
            Ok(ToolOutput::ok(format!(
                "[ref={}] bounds: ({x},{y} {w}x{h})",
                ax_ref.ref_id
            )))
        }
        "actions" => {
            let acts: Vec<&str> = res
                .get("actions")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            Ok(ToolOutput::ok(format!(
                "[ref={}] actions: {}",
                ax_ref.ref_id,
                if acts.is_empty() {
                    "(none)".to_owned()
                } else {
                    acts.join(", ")
                }
            )))
        }
        "parent" => {
            let Some(p) = res.get("parent").filter(|v| !v.is_null()) else {
                return Ok(ToolOutput::ok(format!(
                    "[ref={}] has no parent.",
                    ax_ref.ref_id
                )));
            };
            let node = parse_single_ax_node(p);
            let registered = session.register_find_matches(ax_ref.hwnd, ax_ref.window_id, &[node]);
            let mut out = String::new();
            for (rid, n) in &registered {
                format_match_line(&mut out, rid, n);
            }
            Ok(ToolOutput::ok(out.trim_end().to_owned()))
        }
        "children" => {
            let nodes = res.get("children").map(parse_ax_nodes).unwrap_or_default();
            if nodes.is_empty() {
                return Ok(ToolOutput::ok(format!(
                    "[ref={}] has no children.",
                    ax_ref.ref_id
                )));
            }
            let registered = session.register_find_matches(ax_ref.hwnd, ax_ref.window_id, &nodes);
            let mut out = String::new();
            for (rid, n) in &registered {
                format_match_line(&mut out, rid, n);
            }
            Ok(ToolOutput::ok(out.trim_end().to_owned()))
        }
        other => Err(DesktopError::AxFailed(format!("unknown el_read field `{other}`")).into()),
    }
}

pub(crate) async fn action_set_value(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
    timeout: Duration,
) -> Result<ToolOutput> {
    let ax_ref = require_ref(session, args)?;
    let value = args
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Tool("`desktop` set_value: missing string `value`".to_owned()))?;
    let res = call_ref_op(
        bridge,
        session,
        ax_ref.hwnd,
        &json!({
            "op": "set_value",
            "hwnd": ax_ref.hwnd,
            "runtime_id": ax_ref.runtime_id,
            "value": value
        }),
        timeout,
    )
    .await?;
    let confirmed = res.get("value").and_then(Value::as_str).unwrap_or(value);
    Ok(ToolOutput::ok(format!(
        "Set [ref={}] value ({} char(s), verified).",
        ax_ref.ref_id,
        confirmed.chars().count()
    )))
}

pub(crate) async fn action_select_text(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
    timeout: Duration,
) -> Result<ToolOutput> {
    let ax_ref = require_ref(session, args)?;
    let mut payload = json!({
        "op": "select_text",
        "hwnd": ax_ref.hwnd,
        "runtime_id": ax_ref.runtime_id
    });
    if let Some(obj) = payload.as_object_mut() {
        if let Some(s) = args.get("start").and_then(Value::as_i64) {
            obj.insert("start".to_owned(), json!(s));
        }
        if let Some(e) = args.get("end").and_then(Value::as_i64) {
            obj.insert("end".to_owned(), json!(e));
        }
    }
    let res = call_ref_op(bridge, session, ax_ref.hwnd, &payload, timeout).await?;
    let selected = res.get("selected").and_then(Value::as_str).unwrap_or("");
    Ok(ToolOutput::ok(format!(
        "Selected text on [ref={}]{}.",
        ax_ref.ref_id,
        if selected.is_empty() {
            String::new()
        } else {
            format!(": \"{selected}\"")
        }
    )))
}

pub(crate) async fn action_invoke(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
    timeout: Duration,
) -> Result<ToolOutput> {
    let ax_ref = require_ref(session, args)?;
    let res = call_ref_op(
        bridge,
        session,
        ax_ref.hwnd,
        &json!({
            "op": "invoke",
            "hwnd": ax_ref.hwnd,
            "runtime_id": ax_ref.runtime_id
        }),
        timeout,
    )
    .await?;
    let pattern = res
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or("invoke");
    let state = res.get("state").and_then(Value::as_str);
    let suffix = state.map_or_else(String::new, |s| format!(" -> {s}"));
    Ok(ToolOutput::ok(format!(
        "Invoked [ref={}] via {pattern}{suffix}.",
        ax_ref.ref_id
    )))
}

pub(crate) async fn action_focus(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
    timeout: Duration,
) -> Result<ToolOutput> {
    let ax_ref = require_ref(session, args)?;
    let _ = call_ref_op(
        bridge,
        session,
        ax_ref.hwnd,
        &json!({
            "op": "focus",
            "hwnd": ax_ref.hwnd,
            "runtime_id": ax_ref.runtime_id
        }),
        timeout,
    )
    .await?;
    Ok(ToolOutput::ok(format!("Focused [ref={}].", ax_ref.ref_id)))
}

pub(crate) async fn action_el_type(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
    timeout: Duration,
) -> Result<ToolOutput> {
    let ax_ref = require_ref(session, args)?;
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Tool("`desktop` el_type: missing string `text`".to_owned()))?;
    let _ = call_ref_op(
        bridge,
        session,
        ax_ref.hwnd,
        &json!({
            "op": "el_type",
            "hwnd": ax_ref.hwnd,
            "runtime_id": ax_ref.runtime_id,
            "text": text
        }),
        timeout,
    )
    .await?;
    Ok(ToolOutput::ok(format!(
        "Typed {} character(s) into [ref={}].",
        text.chars().count(),
        ax_ref.ref_id
    )))
}

fn require_ref(session: &DesktopSession, args: &Value) -> Result<crate::session::AxRef> {
    let raw = args.get("ref").and_then(Value::as_str).ok_or_else(|| {
        HrnError::Tool("`desktop`: missing element `ref` (e.g. \"e1\")".to_owned())
    })?;
    session.resolve_ref(raw).map_err(HrnError::from)
}

async fn call_ref_op(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    hwnd: i64,
    request: &Value,
    timeout: Duration,
) -> Result<Value> {
    match bridge.call(request, timeout).await {
        Ok(val) => Ok(val),
        Err(DesktopError::WindowNotFound(msg)) => {
            session.mark_window_closed(hwnd);
            Err(DesktopError::StaleRef(msg).into())
        }
        Err(err) => Err(err.into()),
    }
}
