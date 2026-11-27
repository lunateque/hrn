//! Pure JSONL record parsers and auxiliary metadata loaders for Claude Code transcript import.

use super::io::{
    MAX_IMPORT_META_CHARS, MAX_IMPORT_TITLE_CHARS, MAX_META_JSON_BYTES, ensure_confined_file,
    read_bounded_prefix, sanitize_meta, sanitize_title,
};
use hrn_kernel::{FinishReason, Msg, ThinkingBlock, TitleSource, ToolCall, Usage};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Normalize a Claude `stop_reason` wire string to [`FinishReason`].
#[must_use]
pub fn map_stop_reason(raw: &str) -> FinishReason {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pause_turn" => FinishReason::Stop,
        other => FinishReason::from_wire(other),
    }
}

pub(super) fn merge_assistant_msg(
    dst: &mut Msg,
    text: &str,
    reasoning: Option<String>,
    thinking_blocks: Vec<ThinkingBlock>,
    tool_calls: Vec<ToolCall>,
) {
    if !text.is_empty() {
        if !dst.content.is_empty() {
            dst.content.push('\n');
        }
        dst.content.push_str(text);
    }
    if let Some(r) = reasoning {
        match &mut dst.reasoning {
            Some(existing) if !existing.is_empty() => {
                existing.push('\n');
                existing.push_str(&r);
            }
            slot => *slot = Some(r),
        }
    }
    dst.thinking_blocks.extend(thinking_blocks);
    dst.tool_calls.extend(tool_calls);
}

pub(super) fn entry_uuid(val: &Value, idx: usize) -> String {
    val.get("uuid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| format!("line_{idx}"), str::to_owned)
}

pub(super) fn is_skipped(val: &Value) -> bool {
    val.get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || val.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
}

pub(super) fn is_user_entry(val: &Value) -> bool {
    val.get("type").and_then(Value::as_str) == Some("user")
        || val
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            == Some("user")
}

pub(super) fn is_assistant_entry(val: &Value) -> bool {
    val.get("type").and_then(Value::as_str) == Some("assistant")
        || val
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            == Some("assistant")
}

pub(super) fn extract_title_record(val: &Value) -> Option<(String, TitleSource)> {
    let kind = val.get("type").and_then(Value::as_str)?;
    match kind {
        "custom-title" => {
            let text = val
                .get("customTitle")
                .or_else(|| val.get("title"))
                .and_then(Value::as_str)
                .map(|s| sanitize_title(s, MAX_IMPORT_TITLE_CHARS))
                .filter(|s| !s.is_empty())?;
            Some((text, TitleSource::User))
        }
        "ai-title" => {
            let text = val
                .get("aiTitle")
                .or_else(|| val.get("title"))
                .and_then(Value::as_str)
                .map(|s| sanitize_title(s, MAX_IMPORT_TITLE_CHARS))
                .filter(|s| !s.is_empty())?;
            Some((text, TitleSource::Auto))
        }
        _ => None,
    }
}

pub(super) fn extract_user_text(val: &Value) -> String {
    match val.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(super) fn collect_tool_names(val: &Value, names: &mut HashMap<String, String>) {
    let Some(blocks) = val
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("tool_use")
            && let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            )
        {
            names.insert(id.to_owned(), name.to_owned());
        }
    }
}

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
                .get("signature")
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
        "tool_use" => {
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
            let arguments = match item.get("input") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_owned()),
                None => "{}".to_owned(),
            };
            if !id.is_empty() {
                tool_names.insert(id.clone(), name.clone());
            }
            calls.push(ToolCall {
                id,
                name,
                arguments,
                signature: None,
            });
        }
        _ => {}
    }
}

pub(super) fn extract_tool_result_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| serde_json::to_string(item).ok())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => String::new(),
    }
}

pub(super) fn parse_claude_usage(raw: Option<&Value>) -> Option<Usage> {
    let obj = raw?.as_object()?;
    let input = obj.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = obj
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = obj.get("cache_read_input_tokens").and_then(Value::as_u64);
    let cache_write = obj
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    let prompt_tokens = input
        .saturating_add(cache_read.unwrap_or(0))
        .saturating_add(cache_write.unwrap_or(0));
    let total_tokens = prompt_tokens.saturating_add(output);
    if total_tokens == 0 && cache_read.is_none() && cache_write.is_none() {
        return None;
    }
    Some(Usage {
        prompt_tokens,
        completion_tokens: output,
        total_tokens,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: None,
        cost_micros: None,
    })
}

/// Read ONLY the `projects` keys from `<claude_dir>/.claude.json` or `<claude_dir>/../.claude.json`
/// (strictly bounded, never following symlinks, never extracting credentials/accounts).
pub(super) fn load_claude_json_projects(claude_dir: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut candidates: Vec<PathBuf> = vec![claude_dir.join(".claude.json")];
    if let Some(parent) = claude_dir.parent() {
        candidates.push(parent.join(".claude.json"));
    }
    for path in candidates {
        let Ok(raw) = read_bounded_prefix(&path, MAX_META_JSON_BYTES) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(projects) = val.get("projects").and_then(Value::as_object) {
            for project_path in projects.keys() {
                let clean = sanitize_meta(project_path, MAX_IMPORT_META_CHARS);
                if clean.is_empty() {
                    continue;
                }
                map.insert(encode_project_slug(project_path), clean.clone());
                map.insert(project_path.clone(), clean);
            }
        }
    }
    map
}

fn encode_project_slug(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Load user-assigned custom titles from `custom-title.json` files confined inside `root_canon`.
pub(super) fn load_custom_titles(
    root_canon: &Path,
    claude_dir: &Path,
    slug_canon: &Path,
) -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let candidates = [
        claude_dir.join("custom-title.json"),
        slug_canon.join("custom-title.json"),
    ];
    for candidate in candidates {
        let Ok(canon) = ensure_confined_file(root_canon, &candidate) else {
            continue;
        };
        let Ok(raw) = read_bounded_prefix(&canon, MAX_META_JSON_BYTES) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        parse_custom_title_json(&val, &mut titles);
    }
    titles
}

fn parse_custom_title_json(val: &Value, out: &mut HashMap<String, String>) {
    let Some(obj) = val.as_object() else {
        return;
    };
    if let Some(sid) = obj.get("sessionId").and_then(Value::as_str)
        && let Some(title) = obj
            .get("customTitle")
            .or_else(|| obj.get("title"))
            .and_then(Value::as_str)
            .map(|s| sanitize_title(s, MAX_IMPORT_TITLE_CHARS))
            .filter(|s| !s.is_empty())
    {
        out.insert(sid.to_owned(), title);
        return;
    }
    for (k, v) in obj {
        if let Some(t) = v
            .as_str()
            .map(|s| sanitize_title(s, MAX_IMPORT_TITLE_CHARS))
            .filter(|s| !s.is_empty())
        {
            out.insert(k.clone(), t);
        } else if let Some(t) = v
            .get("customTitle")
            .or_else(|| v.get("title"))
            .and_then(Value::as_str)
            .map(|s| sanitize_title(s, MAX_IMPORT_TITLE_CHARS))
            .filter(|s| !s.is_empty())
        {
            out.insert(k.clone(), t);
        }
    }
}
