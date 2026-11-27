//! Pointer, keyboard, and background/takeover input actions with coordinate frame validation.

use crate::actions::BRIDGE_TIMEOUT;
use crate::actions_ax::{WindowTarget, resolve_window_target};
use crate::coords::parse_hotkey_vks;
use crate::error::DesktopError;
use crate::session::{DesktopSession, FrameTarget};
use crate::tool::{BackgroundFallback, DesktopConfig};
use crate::win_bridge::DesktopBridge;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{HrnError, Result, ToolOutput};

pub(crate) async fn action_move(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
) -> Result<ToolOutput> {
    let target = resolve_window_target(session, args)?;
    let (px, py) = resolve_validated_xy(bridge, session, args, &target).await?;
    let _ = bridge
        .call(&json!({ "op": "move", "x": px, "y": py }), BRIDGE_TIMEOUT)
        .await?;
    Ok(ToolOutput::ok(format!(
        "Moved cursor to physical ({px}, {py})."
    )))
}

pub(crate) async fn action_click(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    config: &DesktopConfig,
    action: &str,
    args: &Value,
) -> Result<ToolOutput> {
    let target = resolve_window_target(session, args)?;
    let (px, py) = resolve_validated_xy(bridge, session, args, &target).await?;
    let kind = match action {
        "double_click" => "double",
        "right_click" => "right",
        _ => "left",
    };

    if let Some((bg_hwnd, bg_title)) = background_target(session, args, &target)? {
        let explicit_takeover = args
            .get("takeover")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let req = json!({
            "op": "bg_click",
            "hwnd": bg_hwnd,
            "window": bg_title,
            "x": px,
            "y": py,
            "kind": kind
        });
        let (res, took_over) =
            call_bg_with_fallback(bridge, config, &req, explicit_takeover).await?;
        let note = takeover_note(&res, took_over);
        return Ok(ToolOutput::ok(format!(
            "Performed {action} at physical ({px}, {py}) (background).{note}"
        )));
    }

    let _ = bridge
        .call(
            &json!({ "op": "click", "x": px, "y": py, "kind": kind }),
            BRIDGE_TIMEOUT,
        )
        .await?;
    Ok(ToolOutput::ok(format!(
        "Performed {action} at physical ({px}, {py})."
    )))
}

pub(crate) async fn action_scroll(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
) -> Result<ToolOutput> {
    let delta = req_i64(args, "delta_y", "scroll")?;
    let (has_pos, px, py) = if args.get("x").is_some() && args.get("y").is_some() {
        let target = resolve_window_target(session, args)?;
        let (x, y) = resolve_validated_xy(bridge, session, args, &target).await?;
        (true, x, y)
    } else {
        (false, 0, 0)
    };
    let _ = bridge
        .call(
            &json!({ "op": "scroll", "delta": delta, "has_pos": has_pos, "x": px, "y": py }),
            BRIDGE_TIMEOUT,
        )
        .await?;
    Ok(ToolOutput::ok(format!("Scrolled delta_y={delta}.")))
}

pub(crate) async fn action_type_text(
    bridge: &mut DesktopBridge,
    session: &DesktopSession,
    config: &DesktopConfig,
    args: &Value,
) -> Result<ToolOutput> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Tool("`desktop` type_text: missing `text`".to_owned()))?;
    let target = resolve_window_target(session, args)?;
    let count = text.chars().count();

    if let Some((bg_hwnd, bg_title)) = background_target(session, args, &target)? {
        let explicit_takeover = args
            .get("takeover")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let req = json!({
            "op": "bg_type",
            "hwnd": bg_hwnd,
            "window": bg_title,
            "text": text
        });
        let (res, took_over) =
            call_bg_with_fallback(bridge, config, &req, explicit_takeover).await?;
        let note = takeover_note(&res, took_over);
        return Ok(ToolOutput::ok(format!(
            "Typed {count} character(s) (background).{note}"
        )));
    }

    let _ = bridge
        .call(&json!({ "op": "type_text", "text": text }), BRIDGE_TIMEOUT)
        .await?;
    Ok(ToolOutput::ok(format!("Typed {count} character(s).")))
}

pub(crate) async fn action_hotkey(
    bridge: &mut DesktopBridge,
    session: &DesktopSession,
    config: &DesktopConfig,
    args: &Value,
) -> Result<ToolOutput> {
    let (vks, label) = parse_hotkey_vks(args)?;
    let target = resolve_window_target(session, args)?;

    if let Some((bg_hwnd, bg_title)) = background_target(session, args, &target)? {
        let explicit_takeover = args
            .get("takeover")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let req = json!({
            "op": "bg_hotkey",
            "hwnd": bg_hwnd,
            "window": bg_title,
            "vks": vks
        });
        let (res, took_over) =
            call_bg_with_fallback(bridge, config, &req, explicit_takeover).await?;
        let note = takeover_note(&res, took_over);
        return Ok(ToolOutput::ok(format!(
            "Sent hotkey `{label}` (background).{note}"
        )));
    }

    let _ = bridge
        .call(&json!({ "op": "hotkey", "vks": vks }), BRIDGE_TIMEOUT)
        .await?;
    Ok(ToolOutput::ok(format!("Sent hotkey `{label}`.")))
}

fn background_target(
    session: &DesktopSession,
    args: &Value,
    target: &WindowTarget,
) -> Result<Option<(i64, String)>> {
    let want_bg = args
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || args
            .get("takeover")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || target.explicit;
    if !want_bg {
        return Ok(None);
    }
    if target.explicit {
        return Ok(Some((target.hwnd, target.title.clone())));
    }
    if let Some(frame) = session.active_frame()
        && let FrameTarget::Window { hwnd, .. } = frame.target
    {
        return Ok(Some((hwnd, String::new())));
    }
    Err(DesktopError::WindowNotFound(
        "background input requires `window_id`/`window` or an active `window_capture` frame"
            .to_owned(),
    )
    .into())
}

async fn call_bg_with_fallback(
    bridge: &mut DesktopBridge,
    config: &DesktopConfig,
    base_req: &Value,
    explicit_takeover: bool,
) -> Result<(Value, bool)> {
    let mut req = base_req.clone();
    if let Some(obj) = req.as_object_mut() {
        obj.insert("takeover".to_owned(), json!(explicit_takeover));
    }
    match bridge.call(&req, BRIDGE_TIMEOUT).await {
        Ok(val) => {
            let took_over = explicit_takeover
                || val
                    .get("takeover")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            Ok((val, took_over))
        }
        Err(DesktopError::BackgroundUnavailable(msg)) => {
            if !explicit_takeover && config.background_fallback == BackgroundFallback::Takeover {
                let mut retry = base_req.clone();
                if let Some(obj) = retry.as_object_mut() {
                    obj.insert("takeover".to_owned(), json!(true));
                }
                let val = bridge.call(&retry, BRIDGE_TIMEOUT).await?;
                Ok((val, true))
            } else {
                Err(DesktopError::BackgroundUnavailable(msg).into())
            }
        }
        Err(err) => Err(err.into()),
    }
}

fn takeover_note(res: &Value, took_over: bool) -> String {
    if !took_over {
        return String::new();
    }
    let unconfirmed = res
        .get("unconfirmed")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match unconfirmed {
        Some(text) => format!(" [takeover fallback used; focus restored; unconfirmed: {text}]"),
        None => " [takeover fallback used; focus restored]".to_owned(),
    }
}

async fn resolve_validated_xy(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    args: &Value,
    target: &WindowTarget,
) -> Result<(i64, i64)> {
    let x = req_i64(args, "x", "coordinate action")?;
    let y = req_i64(args, "y", "coordinate action")?;

    let Some(frame) = session.active_frame().cloned() else {
        return Ok((x, y));
    };

    match &frame.target {
        FrameTarget::Window { hwnd, .. } => {
            if target.explicit && target.hwnd != 0 && target.hwnd != *hwnd {
                return Err(DesktopError::InvalidCoordinateFrame(format!(
                    "target HWND {} does not match active capture frame HWND {hwnd}",
                    target.hwnd
                ))
                .into());
            }
            if let Some(info) = session.window_by_hwnd(*hwnd) {
                session.validate_window_rect(*hwnd, info.rect())?;
            }
            if *hwnd != 0 {
                validate_live_window_rect(bridge, session, *hwnd).await?;
            }
            Ok(frame.scale.to_physical(x, y))
        }
        FrameTarget::Desktop { .. } => {
            if target.explicit {
                return Err(DesktopError::InvalidCoordinateFrame(
                    "active coordinate frame is a desktop capture, not the targeted window; run `window_capture` first"
                        .to_owned(),
                )
                .into());
            }
            Ok(frame.scale.to_physical(x, y))
        }
    }
}

async fn validate_live_window_rect(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    hwnd: i64,
) -> Result {
    let rect_res = bridge
        .call(
            &json!({ "op": "window_rect", "hwnd": hwnd }),
            BRIDGE_TIMEOUT,
        )
        .await;
    match rect_res {
        Ok(val) => {
            if let (Some(rx), Some(ry), Some(rw), Some(rh)) = (
                val.get("x").and_then(Value::as_i64),
                val.get("y").and_then(Value::as_i64),
                val.get("w")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
                val.get("h")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
            ) {
                session.validate_window_rect(hwnd, (rx, ry, rw, rh))?;
            }
            Ok(())
        }
        Err(DesktopError::WindowNotFound(msg)) => {
            session.mark_window_closed(hwnd);
            Err(DesktopError::InvalidCoordinateFrame(msg).into())
        }
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn req_i64(args: &Value, key: &str, ctx: &str) -> Result<i64> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| HrnError::Tool(format!("`desktop` {ctx}: missing integer `{key}`")))
}
