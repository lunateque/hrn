//! Pure JSONL record parsers for OMP (`~/.omp/agent/sessions/**`) v3 transcripts.

use super::io::{MAX_IMPORT_META_CHARS, MAX_IMPORT_TITLE_CHARS, sanitize_meta, sanitize_title};
use hrn_kernel::{FinishReason, Msg, ThinkingBlock, TitleSource, ToolCall, Usage};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// Normalize an OMP `stopReason` wire string to [`FinishReason`].
#[must_use]
pub fn map_stop_reason(raw: &str) -> FinishReason {
    match raw.trim().to_ascii_lowercase().as_str() {
        "stop" | "end_turn" | "pause_turn" => FinishReason::Stop,
        "tooluse" | "tool_use" | "tool_calls" => FinishReason::ToolCalls,
        "length" | "max_tokens" => FinishReason::Length,
        "aborted" | "cancelled" | "canceled" => FinishReason::Aborted,
        "error" => FinishReason::Error,
        other => FinishReason::from_wire(other),
    }
}

/// Derive a fallback session identifier from an OMP transcript filename (`<ts>_<id>.jsonl`).
pub(super) fn filename_session_id(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("omp-session");
    let candidate = stem
        .rsplit_once('_')
        .map(|(_, suffix)| suffix)
        .filter(|s| !s.is_empty())
        .unwrap_or(stem);
    let clean = sanitize_meta(candidate, MAX_IMPORT_META_CHARS);
    if clean.is_empty() {
        "omp-session".to_owned()
    } else {
        clean
    }
}

/// Extract an OMP record identifier (`id`), falling back to synthetic line index.
pub(super) fn entry_id(val: &Value, idx: usize) -> String {
    val.get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| format!("omp_line_{idx}"), str::to_owned)
}

/// Extract a title update from `{"type":"title"|"title_change",...}`.
pub(super) fn extract_title_record(val: &Value) -> Option<(String, TitleSource)> {
    let kind = val.get("type").and_then(Value::as_str)?;
    if !matches!(kind, "title" | "title_change") {
        return None;
    }
    let text = val
        .get("title")
        .and_then(Value::as_str)
        .map(|s| sanitize_title(s, MAX_IMPORT_TITLE_CHARS))
        .filter(|s| !s.is_empty())?;
    let source = match val
        .get("source")
        .and_then(Value::as_str)
        .map_or("auto", str::trim)
    {
        "user" => TitleSource::User,
        _ => TitleSource::Auto,
    };
    Some((text, source))
}

/// Format an OMP model selector (`provider/model` when `provider` is present and `model` lacks `/`).
pub(super) fn extract_model_selector(obj: &Value) -> Option<String> {
    let model = obj
        .get("model")
        .or_else(|| obj.get("modelId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    if model.contains('/') {
        return Some(sanitize_meta(model, MAX_IMPORT_META_CHARS));
    }
    let provider = obj
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let combined = match provider {
        Some(p) => format!("{p}/{model}"),
        None => model.to_owned(),
    };
    Some(sanitize_meta(&combined, MAX_IMPORT_META_CHARS))
}

/// Extract plain user text from an OMP message's `content` (string or array of `{type:"text"}`).
pub(super) fn extract_text_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") | None => b.get("text").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) if !other.is_null() => serde_json::to_string(other).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Pre-scan an OMP record for assistant tool-call IDs -> names (so toolResult can resolve names).
pub(super) fn collect_tool_names(val: &Value, names: &mut HashMap<String, String>) {
    let Some(blocks) = val
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(kind, "toolCall" | "tool_use")
            && let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            )
            && !id.is_empty()
        {
            names.insert(id.to_owned(), name.to_owned());
        }
    }
}

/// Parse an OMP assistant message `content` field into text, reasoning, thinking blocks, and tool calls.
pub(super) fn parse_assistant_content(
    content: Option<&Value>,
    model: &str,
    tool_names: &mut HashMap<String, String>,
) -> (String, Option<String>, Vec<ThinkingBlock>, Vec<ToolCall>) {
    let mut texts = Vec::new();
    let mut thoughts = Vec::new();
    let mut blocks_out = Vec::new();
    let mut calls = Vec::new();
    match content {
        Some(Value::String(s)) if !s.is_empty() => texts.push(s.clone()),
        Some(Value::Array(items)) => {
            for item in items {
                parse_assistant_block(
                    item,
                    model,
                    tool_names,
                    &mut texts,
                    &mut thoughts,
                    &mut blocks_out,
                    &mut calls,
                );
            }
        }
        _ => {}
    }
    let reasoning = (!thoughts.is_empty()).then(|| thoughts.join("\n"));
    (texts.join("\n"), reasoning, blocks_out, calls)
}

fn parse_assistant_block(
    item: &Value,
    model: &str,
    tool_names: &mut HashMap<String, String>,
    texts: &mut Vec<String>,
    thoughts: &mut Vec<String>,
    blocks_out: &mut Vec<ThinkingBlock>,
    calls: &mut Vec<ToolCall>,
) {
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => {
            if let Some(t) = item.get("text").and_then(Value::as_str)
                && !t.is_empty()
            {
                texts.push(t.to_owned());
            }
        }
        "thinking" => {
            let text = item
                .get("thinking")
                .or_else(|| item.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let signature = item
                .get("thinkingSignature")
                .or_else(|| item.get("signature"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if !text.is_empty() {
                thoughts.push(text.clone());
            }
            if !text.is_empty() || signature.is_some() {
                blocks_out.push(ThinkingBlock {
                    model: model.to_owned(),
                    text,
                    signature,
                    redacted: None,
                });
            }
        }
        "redacted_thinking" => {
            if let Some(data) = item.get("data").and_then(Value::as_str) {
                blocks_out.push(ThinkingBlock {
                    model: model.to_owned(),
                    text: String::new(),
                    signature: None,
                    redacted: Some(data.to_owned()),
                });
            }
        }
        "toolCall" | "tool_use" => {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            let arguments = match item.get("arguments").or_else(|| item.get("input")) {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_owned()),
                None => "{}".to_owned(),
            };
            let signature = item
                .get("thoughtSignature")
                .or_else(|| item.get("signature"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if !id.is_empty() {
                tool_names.insert(id.clone(), name.clone());
            }
            calls.push(ToolCall {
                id,
                name,
                arguments,
                signature,
            });
        }
        _ => {}
    }
}

/// Convert an OMP `toolResult` message into a canonical [`Msg`].
pub(super) fn parse_tool_result(message: &Value, tool_names: &HashMap<String, String>) -> Msg {
    let call_id = message
        .get("toolCallId")
        .or_else(|| message.get("tool_call_id"))
        .or_else(|| message.get("tool_use_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let name = message
        .get("toolName")
        .or_else(|| message.get("tool_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| tool_names.get(&call_id).cloned())
        .unwrap_or_else(|| "tool".to_owned());
    let body = extract_text_content(message.get("content"));
    let mut msg = Msg::tool_result(call_id, name, body);
    msg.is_error = message
        .get("isError")
        .or_else(|| message.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    msg
}

/// Parse OMP assistant `usage` (`{input, output, cacheRead, cacheWrite, totalTokens, cost:{total}}`).
pub(super) fn parse_omp_usage(raw: Option<&Value>) -> Option<Usage> {
    let obj = raw?.as_object()?;
    let input = obj
        .get("input")
        .or_else(|| obj.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = obj
        .get("output")
        .or_else(|| obj.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = obj
        .get("cacheRead")
        .or_else(|| obj.get("cache_read_input_tokens"))
        .and_then(Value::as_u64);
    let cache_write = obj
        .get("cacheWrite")
        .or_else(|| obj.get("cache_creation_input_tokens"))
        .and_then(Value::as_u64);
    let prompt_tokens = input
        .saturating_add(cache_read.unwrap_or(0))
        .saturating_add(cache_write.unwrap_or(0));
    let total_tokens = obj
        .get("totalTokens")
        .and_then(Value::as_u64)
        .filter(|&t| t > 0)
        .unwrap_or_else(|| prompt_tokens.saturating_add(output));
    let cost_micros = obj
        .get("cost")
        .and_then(|c| c.get("total"))
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|usd| {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "non-negative finite USD cost converted to microdollars"
            )]
            let micros = (usd * 1_000_000.0).round() as u64;
            micros
        });
    if total_tokens == 0 && cache_read.is_none() && cache_write.is_none() && cost_micros.is_none() {
        return None;
    }
    Some(Usage {
        prompt_tokens,
        completion_tokens: output,
        total_tokens,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: None,
        cost_micros,
    })
}

#[derive(Default)]
pub(super) struct PrefixInfo {
    pub(super) session_id: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) title: Option<String>,
    pub(super) updated_at_ms: Option<i64>,
}

/// Strictly bounded 64 KiB prefix scan for OMP session metadata during discovery.
pub(super) fn scan_prefix(path: &std::path::Path) -> PrefixInfo {
    use super::io::{
        MAX_IMPORT_META_CHARS, PREFIX_SCAN_BYTES, clip_title, parse_timestamp_ms,
        read_bounded_prefix_omp, sanitize_meta,
    };

    let mut info = PrefixInfo::default();
    let Ok(prefix) = read_bounded_prefix_omp(path, PREFIX_SCAN_BYTES) else {
        return info;
    };
    let mut user_title: Option<String> = None;
    let mut auto_title: Option<String> = None;
    let mut first_user: Option<String> = None;
    for line in prefix.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "session" {
            if info.session_id.is_none()
                && let Some(id) = val
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|s| sanitize_meta(s, MAX_IMPORT_META_CHARS))
                    .filter(|s| !s.is_empty())
            {
                info.session_id = Some(id);
            }
            if info.cwd.is_none()
                && let Some(c) = val
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(|s| sanitize_meta(s, MAX_IMPORT_META_CHARS))
                    .filter(|s| !s.is_empty())
            {
                info.cwd = Some(c);
            }
        }
        if let Some(ts) = val
            .get("timestamp")
            .or_else(|| val.get("updatedAt"))
            .and_then(parse_timestamp_ms)
        {
            info.updated_at_ms = Some(info.updated_at_ms.map_or(ts, |prev| prev.max(ts)));
        }
        if let Some((t, src)) = extract_title_record(&val) {
            match src {
                TitleSource::User => user_title = Some(t),
                TitleSource::Auto => auto_title = Some(t),
            }
        }
        if first_user.is_none()
            && kind == "message"
            && let Some(msg) = val.get("message")
            && msg.get("role").and_then(Value::as_str) == Some("user")
        {
            let text = extract_text_content(msg.get("content"));
            if !text.trim().is_empty() {
                first_user = Some(clip_title(&text, 80));
            }
        }
    }
    info.title = user_title.or(auto_title).or(first_user);
    info
}
