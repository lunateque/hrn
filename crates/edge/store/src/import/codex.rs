//! Read-only Codex CLI transcript parser (`turn_context`, `thread_rolled_back`, `compacted`,
//! `response_item`, and `event_msg`).

use super::io::{
    MAX_IMPORT_META_CHARS, MAX_IMPORT_TITLE_CHARS, canonicalize_source_root, clip_title,
    ensure_confined_file, file_mtime_ms, now_ms, parse_timestamp_ms, read_jsonl_values,
    sanitize_meta, sanitize_title,
};
use super::{ImportSource, ImportedNode, ImportedTranscript, ImportedUsage};
use hrn_kernel::{AgentStatus, DagPayload, Msg, Result, Role, ThinkingBlock, ToolCall, Usage};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub(crate) use super::codex_discover::{discover, readonly_immutable_uri};

/// Prefix used for imported Codex compaction summary nodes.
pub const CODEX_COMPACTION_PREFIX: &str = "Imported Codex compaction";

/// Parse one Codex rollout JSONL file into an [`ImportedTranscript`].
pub(crate) fn parse_session(
    codex_dir: &Path,
    jsonl_path: &Path,
    db_title: Option<&str>,
    db_cwd: Option<&str>,
) -> Result<ImportedTranscript> {
    let root_canon = canonicalize_source_root(codex_dir)?;
    let file_canon = ensure_confined_file(&root_canon, jsonl_path)?;
    let fallback_id = file_canon
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("codex-session")
        .to_owned();
    let records = read_jsonl_values(&file_canon)?;

    let mut state = CodexParseState::new(
        sanitize_meta(&fallback_id, MAX_IMPORT_META_CHARS),
        db_title
            .map(|t| sanitize_title(t, MAX_IMPORT_TITLE_CHARS))
            .filter(|t| !t.is_empty()),
        db_cwd
            .map(|c| sanitize_meta(c, MAX_IMPORT_META_CHARS))
            .filter(|c| !c.is_empty()),
        file_mtime_ms(&file_canon),
    );
    for val in &records {
        state.ingest_line(val);
    }
    Ok(state.finish())
}

struct CodexParseState {
    source_id: String,
    title: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    current_model: Option<String>,
    created_at_ms: Option<i64>,
    updated_at_ms: i64,
    nodes: Vec<ImportedNode>,
    usage: Vec<ImportedUsage>,
    turn_starts: Vec<usize>,
    pending_reasoning: Vec<String>,
    tool_names: HashMap<String, String>,
    node_seq: usize,
    turn_seq: usize,
}

impl CodexParseState {
    fn new(source_id: String, title: Option<String>, cwd: Option<String>, mtime_ms: i64) -> Self {
        Self {
            source_id,
            title,
            cwd,
            branch: None,
            current_model: None,
            created_at_ms: None,
            updated_at_ms: mtime_ms,
            nodes: Vec::new(),
            usage: Vec::new(),
            turn_starts: Vec::new(),
            pending_reasoning: Vec::new(),
            tool_names: HashMap::new(),
            node_seq: 0,
            turn_seq: 0,
        }
    }

    fn ingest_line(&mut self, val: &Value) {
        let ts = val
            .get("timestamp")
            .and_then(parse_timestamp_ms)
            .unwrap_or_else(|| self.updated_at_ms.max(now_ms()));
        self.created_at_ms = Some(self.created_at_ms.map_or(ts, |prev| prev.min(ts)));
        self.updated_at_ms = self.updated_at_ms.max(ts);

        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = val.get("payload").unwrap_or(val);
        match kind {
            "session_meta" => self.on_session_meta(payload),
            "turn_context" => self.on_turn_context(payload, ts),
            "compacted" => self.on_compacted(payload, ts),
            "thread_rolled_back" => self.on_rolled_back(payload),
            "response_item" => self.on_response_item(payload, ts),
            "event_msg" => self.on_event_msg(payload, ts),
            _ => {}
        }
    }

    fn on_session_meta(&mut self, payload: &Value) {
        if let Some(id) = str_field(payload, "id")
            .map(|s| sanitize_meta(&s, MAX_IMPORT_META_CHARS))
            .filter(|s| !s.is_empty())
        {
            self.source_id = id;
        }
        if self.cwd.is_none() {
            self.cwd = str_field(payload, "cwd")
                .map(|s| sanitize_meta(&s, MAX_IMPORT_META_CHARS))
                .filter(|s| !s.is_empty());
        }
        if self.branch.is_none() {
            self.branch = payload
                .get("git")
                .and_then(|g| str_field(g, "branch"))
                .or_else(|| str_field(payload, "branch"))
                .map(|s| sanitize_meta(&s, MAX_IMPORT_META_CHARS))
                .filter(|s| !s.is_empty());
        }
        if self.current_model.is_none() {
            self.current_model = str_field(payload, "model");
        }
    }

    fn on_turn_context(&mut self, payload: &Value, ts: i64) {
        if self.cwd.is_none() {
            self.cwd = str_field(payload, "cwd")
                .map(|s| sanitize_meta(&s, MAX_IMPORT_META_CHARS))
                .filter(|s| !s.is_empty());
        }
        if let Some(model) = str_field(payload, "model") {
            let changed = self.current_model.as_deref() != Some(model.as_str());
            let should_emit = changed && (self.current_model.is_some() || !self.nodes.is_empty());
            if should_emit {
                self.push_linear(
                    ts,
                    DagPayload::ModelChange {
                        selector: model.clone(),
                        role: Some("imported".to_owned()),
                    },
                );
            }
            self.current_model = Some(model);
        }
    }

    fn on_compacted(&mut self, payload: &Value, ts: i64) {
        if self
            .nodes
            .last()
            .is_some_and(|n| matches!(n.payload, DagPayload::Summary { .. }))
        {
            return;
        }
        let detail = str_field(payload, "message")
            .or_else(|| str_field(payload, "summary"))
            .or_else(|| str_field(payload, "text"));
        let text = match detail {
            Some(ref d) if d.starts_with(CODEX_COMPACTION_PREFIX) => d.clone(),
            Some(d) => format!("{CODEX_COMPACTION_PREFIX}\n\n{d}"),
            None => CODEX_COMPACTION_PREFIX.to_owned(),
        };
        let covers_node_id = self
            .nodes
            .last()
            .map(|n| n.local_id.clone())
            .unwrap_or_default();
        self.push_linear(
            ts,
            DagPayload::Summary {
                covers_node_id,
                text,
                method: Some("codex_import".to_owned()),
                preserve: None,
            },
        );
    }

    fn on_rolled_back(&mut self, payload: &Value) {
        let num_turns = payload
            .get("num_turns")
            .or_else(|| payload.get("turns"))
            .and_then(Value::as_u64)
            .map_or(1, |n| usize::try_from(n).unwrap_or(usize::MAX));
        if num_turns == 0 {
            return;
        }
        let drop_count = num_turns.min(self.turn_starts.len());
        let mut cut = None;
        for _ in 0..drop_count {
            cut = self.turn_starts.pop();
        }
        if let Some(cut_idx) = cut {
            self.nodes.truncate(cut_idx);
            self.usage.retain(|u| u.after_node_count < cut_idx);
            self.pending_reasoning.clear();
        }
    }

    fn on_response_item(&mut self, payload: &Value, ts: i64) {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => {
                let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                let text = extract_codex_text(payload.get("content"));
                if text.trim().is_empty() {
                    return;
                }
                match role {
                    "user" => self.push_user(text, ts),
                    "assistant" => self.push_assistant_text(text, ts),
                    _ => {}
                }
            }
            "reasoning" => {
                let text = extract_reasoning_text(payload);
                if !text.is_empty() {
                    self.pending_reasoning.push(text);
                }
            }
            "function_call" | "custom_tool_call" => {
                let call_id = str_field(payload, "call_id")
                    .or_else(|| str_field(payload, "id"))
                    .unwrap_or_else(|| format!("call_{}", self.node_seq + 1));
                let name = str_field(payload, "name").unwrap_or_else(|| "tool".to_owned());
                let arguments = match payload.get("arguments").or_else(|| payload.get("input")) {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_owned()),
                    None => "{}".to_owned(),
                };
                self.tool_names.insert(call_id.clone(), name.clone());
                self.push_assistant_call(
                    ToolCall {
                        id: call_id,
                        name,
                        arguments,
                        signature: None,
                    },
                    ts,
                );
            }
            "function_call_output" | "custom_tool_call_output" => {
                let call_id = str_field(payload, "call_id")
                    .or_else(|| str_field(payload, "id"))
                    .unwrap_or_default();
                let name = self
                    .tool_names
                    .get(&call_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_owned());
                let output = match payload.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => serde_json::to_string(v).unwrap_or_default(),
                    None => String::new(),
                };
                let mut msg = Msg::tool_result(call_id, name, output);
                msg.is_error = payload
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let turn_id = format!("turn_{}", self.turn_seq.max(1));
                self.push_linear(ts, DagPayload::Message { turn_id, msg });
            }
            _ => {}
        }
    }

    fn on_event_msg(&mut self, payload: &Value, ts: i64) {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "user_message" => {
                if let Some(msg) = str_field(payload, "message") {
                    self.push_user(msg, ts);
                }
            }
            "agent_message" => {
                if let Some(msg) = str_field(payload, "message") {
                    self.push_assistant_text(msg, ts);
                }
            }
            "thread_rolled_back" => self.on_rolled_back(payload),
            "context_compacted" => self.on_compacted(payload, ts),
            "token_count" => {
                if let Some(usage) = parse_codex_usage(payload) {
                    self.usage.push(ImportedUsage {
                        at_ms: ts,
                        model: self
                            .current_model
                            .clone()
                            .unwrap_or_else(|| "codex".to_owned()),
                        usage,
                        after_node_count: self.nodes.len(),
                    });
                }
            }
            _ => {}
        }
    }

    fn push_user(&mut self, text: String, ts: i64) {
        if self.nodes.last().is_some_and(|n| {
            matches!(&n.payload, DagPayload::Message { msg, .. } if msg.role == Role::User && msg.content == text)
        }) {
            return;
        }
        if self.title.as_deref().is_none_or(|t| t.trim().is_empty()) {
            self.title = Some(clip_title(&text, 80));
        }
        self.turn_seq += 1;
        self.turn_starts.push(self.nodes.len());
        let turn_id = format!("turn_{}", self.turn_seq);
        self.push_linear(
            ts,
            DagPayload::Message {
                turn_id,
                msg: Msg::user(text),
            },
        );
    }

    fn push_assistant_text(&mut self, text: String, ts: i64) {
        if self.nodes.last().is_some_and(|n| {
            matches!(&n.payload, DagPayload::Message { msg, .. } if msg.role == Role::Assistant && msg.content == text && msg.tool_calls.is_empty())
        }) {
            return;
        }
        let (reasoning, thinking_blocks) = self.take_pending_reasoning();
        let mut msg = Msg::assistant(text);
        msg.reasoning = reasoning;
        msg.thinking_blocks = thinking_blocks;
        let turn_id = format!("turn_{}", self.turn_seq.max(1));
        self.push_linear(ts, DagPayload::Message { turn_id, msg });
    }

    fn push_assistant_call(&mut self, call: ToolCall, ts: i64) {
        let (reasoning, thinking_blocks) = self.take_pending_reasoning();
        if let Some(last) = self.nodes.last_mut()
            && let DagPayload::Message { msg, .. } = &mut last.payload
            && msg.role == Role::Assistant
            && !msg.tool_calls.is_empty()
        {
            msg.tool_calls.push(call);
            if let Some(r) = reasoning {
                match &mut msg.reasoning {
                    Some(existing) => {
                        existing.push('\n');
                        existing.push_str(&r);
                    }
                    slot => *slot = Some(r),
                }
            }
            msg.thinking_blocks.extend(thinking_blocks);
            return;
        }
        let mut msg = Msg::assistant_tool_calls(String::new(), reasoning, vec![call]);
        msg.thinking_blocks = thinking_blocks;
        let turn_id = format!("turn_{}", self.turn_seq.max(1));
        self.push_linear(ts, DagPayload::Message { turn_id, msg });
    }

    fn take_pending_reasoning(&mut self) -> (Option<String>, Vec<ThinkingBlock>) {
        if self.pending_reasoning.is_empty() {
            return (None, Vec::new());
        }
        let text = std::mem::take(&mut self.pending_reasoning).join("\n");
        let model = self
            .current_model
            .clone()
            .unwrap_or_else(|| "codex".to_owned());
        let block = ThinkingBlock {
            model,
            text: text.clone(),
            signature: None,
            redacted: None,
        };
        (Some(text), vec![block])
    }

    fn push_linear(&mut self, created_at_ms: i64, payload: DagPayload) {
        self.node_seq += 1;
        let local_id = format!("x_{}", self.node_seq);
        let parent_local_id = self.nodes.last().map(|n| n.local_id.clone());
        self.nodes.push(ImportedNode {
            local_id,
            parent_local_id,
            created_at_ms,
            payload,
        });
    }

    fn finish(self) -> ImportedTranscript {
        let created_at_ms = self.created_at_ms.unwrap_or(self.updated_at_ms);
        let head_local_id = self.nodes.last().map(|n| n.local_id.clone());
        ImportedTranscript {
            source: ImportSource::Codex,
            source_id: self.source_id,
            title: self.title.unwrap_or_default(),
            title_source: None,
            cwd: self.cwd.unwrap_or_else(|| ".".to_owned()),
            branch: self.branch,
            model: self.current_model,
            status: AgentStatus::Idle,
            created_at_ms,
            updated_at_ms: self.updated_at_ms.max(created_at_ms),
            head_local_id,
            nodes: self.nodes,
            usage: self.usage,
            last_finish_reason: None,
        }
    }
}

pub(super) fn str_field(val: &Value, key: &str) -> Option<String> {
    val.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

pub(super) fn extract_codex_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_reasoning_text(payload: &Value) -> String {
    if let Some(summary) = payload.get("summary").and_then(Value::as_array) {
        let parts: Vec<&str> = summary
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect();
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    str_field(payload, "text").unwrap_or_default()
}

fn parse_codex_usage(payload: &Value) -> Option<Usage> {
    let raw = payload
        .get("info")
        .and_then(|i| {
            i.get("last_token_usage")
                .or_else(|| i.get("total_token_usage"))
        })
        .or_else(|| payload.get("usage"))?;
    let input = raw.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = raw
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = raw.get("cached_input_tokens").and_then(Value::as_u64);
    let reasoning = raw.get("reasoning_output_tokens").and_then(Value::as_u64);
    let total = raw
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input.saturating_add(output));
    if total == 0 {
        return None;
    }
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
        cache_read_tokens: cached,
        cache_write_tokens: None,
        reasoning_tokens: reasoning,
        cost_micros: None,
    })
}
