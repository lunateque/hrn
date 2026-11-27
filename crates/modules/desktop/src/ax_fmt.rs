//! Compact UI Automation tree formatting, JSON parsing, and role-alias normalization.

use crate::session::{AxNode, RawWindow};
use hrn_kernel::serde_json::Value;
use std::fmt::Write as _;

/// Maximum displayed value characters in compact AX tree output.
const MAX_VALUE_CHARS: usize = 120;

/// Normalize a UIA role name or alias for case-, underscore-, and hyphen-insensitive matching.
pub fn normalize_role(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    let without_prefix = lower.strip_prefix("controltype.").unwrap_or(&lower);
    let collapsed: String = without_prefix
        .chars()
        .filter(|ch| !matches!(ch, '_' | '-' | ' '))
        .collect();
    match collapsed.as_str() {
        "textbox" | "textfield" | "textarea" | "input" | "edit" => "edit".to_owned(),
        "button" | "pushbutton" | "btn" => "button".to_owned(),
        "checkbox" | "check" => "checkbox".to_owned(),
        "radiobutton" | "radio" => "radiobutton".to_owned(),
        "combobox" | "dropdown" | "select" => "combobox".to_owned(),
        "hyperlink" | "link" => "hyperlink".to_owned(),
        "text" | "statictext" | "label" => "text".to_owned(),
        "tab" | "tablist" => "tab".to_owned(),
        "image" | "img" => "image".to_owned(),
        _ => collapsed,
    }
}

/// Return `true` when `actual` UIA role matches `filter` after alias normalization.
pub fn role_matches(actual: &str, filter: &str) -> bool {
    normalize_role(actual) == normalize_role(filter)
}

/// Parse an array of UIA JSON nodes emitted by the bridge (`ax` or `find`).
pub fn parse_ax_nodes(value: &Value) -> Vec<AxNode> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter().map(parse_single_ax_node).collect()
}

/// Parse one UIA JSON node emitted by the bridge.
pub(crate) fn parse_single_ax_node(item: &Value) -> AxNode {
    let role_raw = item
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("Unknown");
    let role = role_raw
        .strip_prefix("ControlType.")
        .unwrap_or(role_raw)
        .to_owned();
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let value = item
        .get("value")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let states = item
        .get("states")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let runtime_id = item
        .get("runtime_id")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_i64)
                .filter_map(|num| i32::try_from(num).ok())
                .collect()
        })
        .unwrap_or_default();
    let left = item.get("x").and_then(Value::as_i64).unwrap_or(0);
    let top = item.get("y").and_then(Value::as_i64).unwrap_or(0);
    let width = item
        .get("w")
        .and_then(Value::as_u64)
        .and_then(|num| u32::try_from(num).ok())
        .unwrap_or(0);
    let height = item
        .get("h")
        .and_then(Value::as_u64)
        .and_then(|num| u32::try_from(num).ok())
        .unwrap_or(0);
    let depth = item
        .get("depth")
        .and_then(Value::as_u64)
        .and_then(|num| usize::try_from(num).ok())
        .unwrap_or(0);
    AxNode {
        role,
        name,
        value,
        states,
        runtime_id,
        bounds: (left, top, width, height),
        depth,
    }
}

/// Parse raw window objects from a `list_windows` bridge response.
pub(crate) fn parse_raw_windows(value: &Value) -> Vec<RawWindow> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter().map(parse_single_window).collect()
}

fn parse_single_window(item: &Value) -> RawWindow {
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let pid = item
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|num| u32::try_from(num).ok())
        .unwrap_or(0);
    let hwnd = item
        .get("hwnd")
        .and_then(parse_hwnd_value)
        .unwrap_or_else(|| synthetic_hwnd(pid, &title));
    let left = item.get("x").and_then(Value::as_i64).unwrap_or(0);
    let top = item.get("y").and_then(Value::as_i64).unwrap_or(0);
    let width = item
        .get("w")
        .and_then(Value::as_u64)
        .and_then(|num| u32::try_from(num).ok())
        .unwrap_or(0);
    let height = item
        .get("h")
        .and_then(Value::as_u64)
        .and_then(|num| u32::try_from(num).ok())
        .unwrap_or(0);
    RawWindow {
        hwnd,
        pid,
        title,
        x: left,
        y: top,
        w: width,
        h: height,
    }
}

/// Parse a native Win32 `HWND` encoded as JSON integer, decimal string, or `0x...` hex string.
pub(crate) fn parse_hwnd_value(val: &Value) -> Option<i64> {
    if let Some(num) = val.as_i64() {
        return Some(num);
    }
    let raw = val.as_str()?.trim();
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else {
        raw.parse::<i64>().ok()
    }
}

fn synthetic_hwnd(pid: u32, title: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325 ^ u64::from(pid);
    for byte in title.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let positive = i64::try_from((hash & 0x7FFF_FFFF_FFFF_FFFE) | 1).unwrap_or(1);
    -positive
}

/// Append one compact indented UIA tree line: `- role "label" [ref=eN]: "value" (states)`.
pub(crate) fn format_ax_line(out: &mut String, node: &AxNode, ref_id: &str) {
    for _ in 0..node.depth {
        out.push_str("  ");
    }
    let label = escape_inline(&node.name, MAX_VALUE_CHARS);
    let _ = write!(out, "- {} \"{}\" [ref={ref_id}]", node.role, label);
    if let Some(val) = &node.value {
        let escaped = escape_inline(val, MAX_VALUE_CHARS);
        let _ = write!(out, ": \"{escaped}\"");
    }
    if !node.states.is_empty() {
        let _ = write!(out, " ({})", node.states.join(", "));
    }
    out.push('\n');
}

fn escape_inline(raw: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_chars));
    let mut truncated = false;
    for (count, ch) in raw.chars().enumerate() {
        if count >= max_chars {
            truncated = true;
            break;
        }
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => {}
            '\t' => out.push(' '),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    if truncated {
        out.push('…');
    }
    out
}
