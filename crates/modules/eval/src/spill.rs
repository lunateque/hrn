//! Output windowing (50 KiB head+tail inline window, 4 KiB per-line cap), `local://N.log`
//! artifact spill with `ToolOutput.meta`, image attachment pass-through, and bridge token redaction.

use hrn_kernel::serde_json::json;
use hrn_kernel::{ImageAttachment, ToolContext, ToolOutput};

/// Maximum inline byte budget kept in the model-facing eval output (50 KiB).
pub(crate) const INLINE_WINDOW_BYTES: usize = 50 * 1024;
/// Maximum inline bytes per individual line before per-line elision.
pub(crate) const MAX_LINE_BYTES: usize = 4 * 1024;

/// Redact every exact occurrence of the per-kernel bearer token so it never leaks into output.
pub(crate) fn redact_token(text: &str, token: Option<&str>) -> String {
    match token.filter(|t| !t.is_empty()) {
        Some(secret) if text.contains(secret) => text.replace(secret, "[REDACTED_EVAL_TOKEN]"),
        _ => text.to_owned(),
    }
}

/// Apply token redaction, 50 KiB head+tail windowing with per-line cap, `local://N.log` spill,
/// structured `ToolOutput.meta`, and image attachment pass-through.
pub(crate) fn apply_spill(
    ctx: &ToolContext,
    full_output: &str,
    images: Vec<ImageAttachment>,
    bridge_token: Option<&str>,
    is_error: bool,
) -> ToolOutput {
    let clean = redact_token(full_output, bridge_token);
    let total_bytes = clean.len();
    let total_lines = clean.lines().count();
    let line_too_long = clean.lines().any(|line| line.len() > MAX_LINE_BYTES);
    let needs_spill = total_bytes > INLINE_WINDOW_BYTES || line_too_long;

    let (content, meta) = if needs_spill {
        let spilled = ctx.urls.spill(ctx, &clean);
        let inline = build_inline_window(&clean);
        let (footer, meta_val) = match spilled {
            Ok(Some(url)) => {
                let note = format!(
                    "[Output truncated: {total_bytes} bytes, {total_lines} lines. Full output saved \
                     to `{url}`; page it with `read`, e.g. `{url}:1-200` or `{url}:-100`.]"
                );
                let meta = json!({
                    "spill": url,
                    "total_bytes": total_bytes,
                    "total_lines": total_lines,
                });
                (note, Some(meta))
            }
            Ok(None) => {
                let note = format!("[Output truncated: {total_bytes} bytes, {total_lines} lines.]");
                let meta = json!({
                    "total_bytes": total_bytes,
                    "total_lines": total_lines,
                    "truncated": true,
                });
                (note, Some(meta))
            }
            Err(err) => {
                let note = format!(
                    "[Output truncated: {total_bytes} bytes, {total_lines} lines; saving full output failed: {err}]"
                );
                let meta = json!({
                    "total_bytes": total_bytes,
                    "total_lines": total_lines,
                    "truncated": true,
                });
                (note, Some(meta))
            }
        };
        (format!("{inline}\n\n{footer}"), meta_val)
    } else {
        (clean, None)
    };

    let mut out = if is_error {
        ToolOutput::err(content)
    } else {
        ToolOutput::ok(content)
    };
    out.meta = meta;
    out.images = images;
    out
}

fn build_inline_window(text: &str) -> String {
    let line_capped = cap_long_lines(text, MAX_LINE_BYTES);
    let Some((head, omitted, tail)) = split_head_tail(&line_capped, INLINE_WINDOW_BYTES) else {
        return line_capped;
    };
    format!("{head}\n... [{omitted} bytes omitted] ...\n{tail}")
}

fn cap_long_lines(text: &str, max_line: usize) -> String {
    if !text.lines().any(|line| line.len() > max_line) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len().min(INLINE_WINDOW_BYTES * 2));
    for (idx, line) in text.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if line.len() <= max_line {
            out.push_str(line);
        } else {
            let cut = line.floor_char_boundary(max_line);
            out.push_str(line.get(..cut).unwrap_or_default());
            out.push_str(" ... [line truncated]");
        }
    }
    out
}

/// Split `text` into ~60% head and ~35% tail of `max_bytes` (UTF-8 safe, snapped to line starts
/// when close), returning `(head, omitted_bytes, tail)` or `None` when it already fits.
fn split_head_tail(text: &str, max_bytes: usize) -> Option<(&str, usize, &str)> {
    if text.len() <= max_bytes {
        return None;
    }
    let mut head_end = text.floor_char_boundary((max_bytes * 60) / 100);
    if let Some(nl) = text.get(..head_end).and_then(|h| h.rfind('\n'))
        && nl >= head_end / 2
    {
        head_end = nl + 1;
    }
    let tail_want = (max_bytes * 35) / 100;
    let mut tail_start = text.ceil_char_boundary(text.len().saturating_sub(tail_want));
    if let Some(nl) = text.get(tail_start..).and_then(|t| t.find('\n'))
        && nl <= tail_want / 2
    {
        tail_start += nl + 1;
    }
    let tail_start = tail_start.max(head_end);
    Some((
        text.get(..head_end).unwrap_or_default(),
        tail_start.saturating_sub(head_end),
        text.get(tail_start..).unwrap_or_default(),
    ))
}
