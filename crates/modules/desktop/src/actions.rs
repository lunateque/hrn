//! Desktop action dispatch, capture/zoom, discovery, clipboard, and app launchers.

use crate::actions_ax::{
    action_ax, action_el_read, action_el_type, action_find, action_focus, action_invoke,
    action_select_text, action_set_value, resolve_window_target,
};
use crate::actions_input::{
    action_click, action_hotkey, action_move, action_scroll, action_type_text, req_i64,
};
use crate::ax_fmt::{parse_hwnd_value, parse_raw_windows};
use crate::coords::ScreenScale;
use crate::error::DesktopError;
use crate::session::{DesktopSession, FrameTarget, RawWindow};
use crate::tool::{DesktopConfig, DesktopManager};
use crate::win_bridge::DesktopBridge;
use hrn_edge_tools::img::load_image_from_base64;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{HrnError, Result, ToolOutput};
use std::fmt::Write as _;
use std::time::Duration;

pub(crate) const BRIDGE_TIMEOUT: Duration = Duration::from_secs(20);

/// Dispatch a `desktop` tool call under the manager's serialized bridge lock.
pub(crate) async fn run_action(mgr: &DesktopManager, args: &Value) -> Result<ToolOutput> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Tool("`desktop`: missing `action`".to_owned()))?;
    let config = mgr.config().clone();
    let mut state = mgr.lock_state().await;

    if action == "close" {
        state.bridge.take();
        state.session.clear();
        return Ok(ToolOutput::ok("Desktop bridge closed."));
    }
    if state.bridge.is_none() {
        state.bridge = Some(DesktopBridge::spawn()?);
    }

    let (bridge, session) = state.split_mut()?;
    dispatch_action(bridge, session, &config, action, args).await
}

async fn dispatch_action(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    config: &DesktopConfig,
    action: &str,
    args: &Value,
) -> Result<ToolOutput> {
    match action {
        "screenshot" => action_screenshot(bridge, session, config, args).await,
        "window_capture" => action_window_capture(bridge, session, config, args).await,
        "zoom" => action_zoom(bridge, session, config, args).await,
        "list_windows" => action_list_windows(bridge, session).await,
        "displays" => action_displays(bridge).await,
        "capabilities" => action_capabilities(bridge).await,
        "apps_list" => action_apps_list(bridge).await,
        "open_app" => action_open_app(bridge, args).await,
        "clipboard_read" => action_clipboard_read(bridge).await,
        "clipboard_write" => action_clipboard_write(bridge, args).await,
        "ax" => action_ax(bridge, session, args, BRIDGE_TIMEOUT).await,
        "find" => action_find(bridge, session, args, BRIDGE_TIMEOUT).await,
        "el_read" => action_el_read(bridge, session, args, BRIDGE_TIMEOUT).await,
        "set_value" => action_set_value(bridge, session, args, BRIDGE_TIMEOUT).await,
        "select_text" => action_select_text(bridge, session, args, BRIDGE_TIMEOUT).await,
        "invoke" | "press" => action_invoke(bridge, session, args, BRIDGE_TIMEOUT).await,
        "focus" => action_focus(bridge, session, args, BRIDGE_TIMEOUT).await,
        "el_type" => action_el_type(bridge, session, args, BRIDGE_TIMEOUT).await,
        "move" => action_move(bridge, session, args).await,
        "click" | "double_click" | "right_click" => {
            action_click(bridge, session, config, action, args).await
        }
        "scroll" => action_scroll(bridge, session, args).await,
        "type_text" => action_type_text(bridge, session, config, args).await,
        "hotkey" => action_hotkey(bridge, session, config, args).await,
        other => Err(HrnError::Tool(format!(
            "`desktop`: unknown action `{other}`"
        ))),
    }
}

async fn action_screenshot(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    config: &DesktopConfig,
    args: &Value,
) -> Result<ToolOutput> {
    let target = resolve_window_target(session, args)?;
    let all_monitors = args
        .get("all_monitors")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let display = args
        .get("display")
        .and_then(Value::as_str)
        .unwrap_or(&config.display);

    let res = bridge
        .call(
            &json!({
                "op": "screenshot",
                "all_monitors": all_monitors,
                "window": target.title,
                "hwnd": target.hwnd,
                "display": display,
                "max_width": config.max_width,
                "max_height": config.max_height
            }),
            BRIDGE_TIMEOUT,
        )
        .await?;

    let (att, scale) = decode_capture_image(&res, "desktop-screenshot.png", config)?;
    let hwnd = res
        .get("hwnd")
        .and_then(parse_hwnd_value)
        .unwrap_or(target.hwnd);
    let frame_target = if target.explicit || hwnd != 0 {
        let wid = target
            .window_id
            .or_else(|| session.window_by_hwnd(hwnd).map(|w| w.id));
        FrameTarget::Window {
            window_id: wid,
            hwnd,
        }
    } else {
        FrameTarget::Desktop {
            display: if all_monitors {
                "all".to_owned()
            } else {
                display.to_owned()
            },
        }
    };
    let rect = (scale.origin_x, scale.origin_y, scale.phys_w, scale.phys_h);
    session.set_frame(frame_target, rect, scale);

    let summary = format!(
        "Captured desktop screenshot ({}x{} image -> {}x{} physical origin=({},{})). \
         Pass click/move coordinates in image space (0..{}, 0..{}).",
        scale.img_w,
        scale.img_h,
        scale.phys_w,
        scale.phys_h,
        scale.origin_x,
        scale.origin_y,
        scale.img_w,
        scale.img_h
    );
    let mut out = ToolOutput::ok(summary);
    out.images.push(att);
    Ok(out)
}

async fn action_window_capture(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
    config: &DesktopConfig,
    args: &Value,
) -> Result<ToolOutput> {
    let target = resolve_window_target(session, args)?;
    let res = bridge
        .call(
            &json!({
                "op": "window_capture",
                "hwnd": target.hwnd,
                "window": target.title,
                "max_width": config.max_width,
                "max_height": config.max_height
            }),
            BRIDGE_TIMEOUT,
        )
        .await?;

    let (att, scale) = decode_capture_image(&res, "desktop-window.png", config)?;
    let hwnd = res
        .get("hwnd")
        .and_then(parse_hwnd_value)
        .unwrap_or(target.hwnd);
    let covered = res.get("covered").and_then(Value::as_bool).unwrap_or(false);
    let rect = (scale.origin_x, scale.origin_y, scale.phys_w, scale.phys_h);
    let wid = target.window_id.or_else(|| {
        session.window_by_hwnd(hwnd).map(|w| w.id).or_else(|| {
            if hwnd != 0 {
                let info = session.upsert_window(&RawWindow {
                    hwnd,
                    pid: 0,
                    title: target.title.clone(),
                    x: scale.origin_x,
                    y: scale.origin_y,
                    w: scale.phys_w,
                    h: scale.phys_h,
                });
                Some(info.id)
            } else {
                None
            }
        })
    });
    session.set_frame(
        FrameTarget::Window {
            window_id: wid,
            hwnd,
        },
        rect,
        scale,
    );

    let id_note = wid.map_or_else(String::new, |id| format!(" id={id}"));
    let summary = format!(
        "Captured window{id_note} ({}x{} image -> {}x{} physical origin=({},{}) covered={covered}). \
         Active click frame updated.",
        scale.img_w, scale.img_h, scale.phys_w, scale.phys_h, scale.origin_x, scale.origin_y
    );
    let mut out = ToolOutput::ok(summary);
    out.images.push(att);
    Ok(out)
}

async fn action_zoom(
    bridge: &mut DesktopBridge,
    session: &DesktopSession,
    config: &DesktopConfig,
    args: &Value,
) -> Result<ToolOutput> {
    let x = req_i64(args, "x", "zoom")?;
    let y = req_i64(args, "y", "zoom")?;
    let w = args
        .get("w")
        .or_else(|| args.get("width"))
        .and_then(Value::as_i64)
        .ok_or_else(|| HrnError::Tool("`desktop` zoom: missing integer `w`".to_owned()))?;
    let h = args
        .get("h")
        .or_else(|| args.get("height"))
        .and_then(Value::as_i64)
        .ok_or_else(|| HrnError::Tool("`desktop` zoom: missing integer `h`".to_owned()))?;
    let target = resolve_window_target(session, args)?;

    let (px, py, pw, ph, hwnd, title) = if let Some(frame) = session.active_frame() {
        let (x1, y1) = frame.scale.to_physical(x, y);
        let (x2, y2) = frame.scale.to_physical(x + w.max(1), y + h.max(1));
        let (fh, ft) = if target.explicit {
            (target.hwnd, target.title)
        } else if let FrameTarget::Window { hwnd, .. } = frame.target {
            (hwnd, String::new())
        } else {
            (0, String::new())
        };
        (x1, y1, (x2 - x1).max(1), (y2 - y1).max(1), fh, ft)
    } else {
        (x, y, w.max(1), h.max(1), target.hwnd, target.title)
    };

    let res = bridge
        .call(
            &json!({
                "op": "zoom",
                "x": px,
                "y": py,
                "w": pw,
                "h": ph,
                "hwnd": hwnd,
                "window": title,
                "max_width": config.max_width,
                "max_height": config.max_height
            }),
            BRIDGE_TIMEOUT,
        )
        .await?;

    let b64 = res
        .get("png_b64")
        .and_then(Value::as_str)
        .ok_or_else(|| DesktopError::CaptureFailed("zoom returned no image".to_owned()))?;
    let att = load_image_from_base64(
        b64,
        Some("desktop-zoom.png".to_owned()),
        config.max_image_dim(),
    )?;
    let iw = att.width.unwrap_or(0);
    let ih = att.height.unwrap_or(0);
    let mut out = ToolOutput::ok(format!(
        "Zoomed region physical ({px},{py} {pw}x{ph}) -> {iw}x{ih} image. Active coordinate frame unchanged."
    ));
    out.images.push(att);
    Ok(out)
}

async fn action_list_windows(
    bridge: &mut DesktopBridge,
    session: &mut DesktopSession,
) -> Result<ToolOutput> {
    let res = bridge
        .call(&json!({ "op": "list_windows" }), BRIDGE_TIMEOUT)
        .await?;
    let raw = res
        .get("windows")
        .map(parse_raw_windows)
        .unwrap_or_default();
    let tracked = session.sync_windows(&raw);
    if tracked.is_empty() {
        return Ok(ToolOutput::ok("No visible windows found."));
    }
    let mut out = String::new();
    for win in tracked.iter().take(60) {
        let _ = writeln!(
            out,
            "[id={}] [pid={}] ({},{} {}x{}) {}",
            win.id, win.pid, win.x, win.y, win.w, win.h, win.title
        );
    }
    Ok(ToolOutput::ok(out.trim_end().to_owned()))
}

async fn action_displays(bridge: &mut DesktopBridge) -> Result<ToolOutput> {
    let res = bridge
        .call(&json!({ "op": "displays" }), BRIDGE_TIMEOUT)
        .await?;
    let Some(arr) = res.get("displays").and_then(Value::as_array) else {
        return Ok(ToolOutput::ok("No displays reported."));
    };
    let mut out = String::new();
    for d in arr {
        let id = d.get("id").and_then(Value::as_i64).unwrap_or(0);
        let name = d.get("name").and_then(Value::as_str).unwrap_or("");
        let primary = d.get("primary").and_then(Value::as_bool).unwrap_or(false);
        let x = d.get("x").and_then(Value::as_i64).unwrap_or(0);
        let y = d.get("y").and_then(Value::as_i64).unwrap_or(0);
        let w = d.get("width").and_then(Value::as_i64).unwrap_or(0);
        let h = d.get("height").and_then(Value::as_i64).unwrap_or(0);
        let tag = if primary { " [primary]" } else { "" };
        let _ = writeln!(out, "display {id}: {name}{tag} ({x},{y} {w}x{h})");
    }
    Ok(ToolOutput::ok(if out.is_empty() {
        "No displays reported.".to_owned()
    } else {
        out.trim_end().to_owned()
    }))
}

async fn action_capabilities(bridge: &mut DesktopBridge) -> Result<ToolOutput> {
    let res = bridge
        .call(&json!({ "op": "capabilities" }), BRIDGE_TIMEOUT)
        .await?;
    let locked = res
        .get("screenLocked")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let uia = res.get("uia").and_then(Value::as_bool).unwrap_or(false);
    let bg = res
        .get("backgroundInput")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ToolOutput::ok(format!(
        "Capabilities: uia={uia}, backgroundInput={bg}, screenLocked={locked}"
    )))
}

async fn action_apps_list(bridge: &mut DesktopBridge) -> Result<ToolOutput> {
    let res = bridge
        .call(&json!({ "op": "apps_list" }), BRIDGE_TIMEOUT)
        .await?;
    let Some(arr) = res.get("apps").and_then(Value::as_array) else {
        return Ok(ToolOutput::ok("No GUI applications found."));
    };
    let mut out = String::new();
    for app in arr.iter().take(80) {
        let name = app.get("name").and_then(Value::as_str).unwrap_or("");
        let pid = app.get("pid").and_then(Value::as_u64).unwrap_or(0);
        let count = app.get("window_count").and_then(Value::as_u64).unwrap_or(0);
        let _ = writeln!(out, "{name} [pid={pid}] ({count} window(s))");
    }
    Ok(ToolOutput::ok(if out.is_empty() {
        "No GUI applications found.".to_owned()
    } else {
        out.trim_end().to_owned()
    }))
}

async fn action_open_app(bridge: &mut DesktopBridge, args: &Value) -> Result<ToolOutput> {
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| HrnError::Tool("`desktop` open_app: missing `target`".to_owned()))?;
    let launch_args: Vec<&str> = args
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let res = bridge
        .call(
            &json!({ "op": "open_app", "target": target, "args": launch_args }),
            BRIDGE_TIMEOUT,
        )
        .await?;
    let pid = res.get("pid").and_then(Value::as_u64).unwrap_or(0);
    Ok(ToolOutput::ok(format!("Launched `{target}` (pid={pid}).")))
}

async fn action_clipboard_read(bridge: &mut DesktopBridge) -> Result<ToolOutput> {
    let res = bridge
        .call(&json!({ "op": "clipboard_read" }), BRIDGE_TIMEOUT)
        .await?;
    let has_text = res.get("has_text").and_then(Value::as_bool).unwrap_or(true);
    let text = res.get("text").and_then(Value::as_str).unwrap_or("");
    if !has_text {
        return Ok(ToolOutput::ok("Clipboard has no text."));
    }
    Ok(ToolOutput::ok(text.to_owned()))
}

async fn action_clipboard_write(bridge: &mut DesktopBridge, args: &Value) -> Result<ToolOutput> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Tool("`desktop` clipboard_write: missing `text`".to_owned()))?;
    let _ = bridge
        .call(
            &json!({ "op": "clipboard_write", "text": text }),
            BRIDGE_TIMEOUT,
        )
        .await?;
    Ok(ToolOutput::ok(format!(
        "Wrote {} character(s) to clipboard.",
        text.chars().count()
    )))
}

fn decode_capture_image(
    res: &Value,
    label: &str,
    config: &DesktopConfig,
) -> Result<(hrn_kernel::ImageAttachment, ScreenScale)> {
    let b64 = res
        .get("png_b64")
        .and_then(Value::as_str)
        .ok_or_else(|| DesktopError::CaptureFailed("capture returned no image".to_owned()))?;
    let origin_x = res.get("origin_x").and_then(Value::as_i64).unwrap_or(0);
    let origin_y = res.get("origin_y").and_then(Value::as_i64).unwrap_or(0);
    let phys_w = res
        .get("width")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1);
    let phys_h = res
        .get("height")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1);

    let att = load_image_from_base64(b64, Some(label.to_owned()), config.max_image_dim())?;
    let img_w = att.width.unwrap_or(phys_w);
    let img_h = att.height.unwrap_or(phys_h);
    let scale = ScreenScale {
        origin_x,
        origin_y,
        phys_w,
        phys_h,
        img_w,
        img_h,
    };
    Ok((att, scale))
}
