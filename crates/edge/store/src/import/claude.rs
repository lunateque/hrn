//! Read-only Claude Code transcript discovery and DAG importer (`~/.claude/projects/<slug>/<uuid>.jsonl`).

use super::claude_parse::{
    collect_tool_names, entry_uuid, extract_title_record, extract_tool_result_content,
    extract_user_text, is_assistant_entry, is_skipped, is_user_entry, load_claude_json_projects,
    load_custom_titles, merge_assistant_msg, parse_assistant_content, parse_claude_usage,
};
use super::io::{
    MAX_IMPORT_META_CHARS, MAX_IMPORT_TITLE_CHARS, PREFIX_SCAN_BYTES, canonicalize_source_root,
    clip_title, ensure_confined_dir, ensure_confined_file, file_mtime_ms, now_ms,
    parse_timestamp_ms, read_bounded_prefix, read_jsonl_values, sanitize_meta, sanitize_title,
};
use super::{ExternalSession, ImportSource, ImportedNode, ImportedTranscript, ImportedUsage};
use hrn_kernel::{
    AgentStatus, DagPayload, ExitReason, FinishReason, HrnError, Msg, Result, Role, TitleSource,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub use super::claude_parse::map_stop_reason;

/// Discover Claude Code sessions under `<claude_dir>/projects/<slug>/<uuid>.jsonl` (newest first).
pub(crate) fn discover(claude_dir: &Path) -> Result<Vec<ExternalSession>> {
    if !claude_dir.exists() {
        return Ok(Vec::new());
    }
    let root_canon = canonicalize_source_root(claude_dir)?;
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        return Ok(Vec::new());
    }
    let Ok(projects_canon) = ensure_confined_dir(&root_canon, &projects_dir) else {
        return Ok(Vec::new());
    };
    let project_cwds = load_claude_json_projects(claude_dir);
    let mut out = Vec::new();
    let entries = fs::read_dir(&projects_canon)
        .map_err(|e| HrnError::Io(format!("list `{}`: {e}", projects_canon.display())))?;
    for slug_entry in entries.flatten() {
        scan_slug_dir(
            &root_canon,
            claude_dir,
            &projects_canon,
            &slug_entry.path(),
            &project_cwds,
            &mut out,
        );
    }
    out.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(out)
}

fn scan_slug_dir(
    root_canon: &Path,
    claude_dir: &Path,
    projects_canon: &Path,
    slug_path: &Path,
    project_cwds: &HashMap<String, String>,
    out: &mut Vec<ExternalSession>,
) {
    let Ok(slug_canon) = ensure_confined_dir(projects_canon, slug_path) else {
        return;
    };
    let Some(slug) = slug_canon.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let custom_titles = load_custom_titles(root_canon, claude_dir, &slug_canon);
    let Ok(files) = fs::read_dir(&slug_canon) else {
        return;
    };
    for file_entry in files.flatten() {
        let file_path = file_entry.path();
        if file_path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file_canon) = ensure_confined_file(&slug_canon, &file_path) else {
            continue;
        };
        let Some(stem) = file_canon.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let prefix = scan_prefix(&file_canon);
        let cwd = prefix
            .cwd
            .or_else(|| project_cwds.get(slug).cloned())
            .map(|c| sanitize_meta(&c, MAX_IMPORT_META_CHARS))
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| ".".to_owned());
        let title = custom_titles
            .get(stem)
            .cloned()
            .or(prefix.title)
            .map(|t| sanitize_title(&t, MAX_IMPORT_TITLE_CHARS))
            .unwrap_or_default();
        let updated_at_ms = prefix
            .updated_at_ms
            .unwrap_or_else(|| file_mtime_ms(&file_canon));
        out.push(ExternalSession {
            source: ImportSource::Claude,
            id: sanitize_meta(stem, MAX_IMPORT_META_CHARS),
            title,
            cwd,
            updated_at_ms,
            path: file_canon,
            archived: false,
        });
    }
}

#[derive(Default)]
struct PrefixInfo {
    cwd: Option<String>,
    title: Option<String>,
    updated_at_ms: Option<i64>,
}

/// Strictly bounded 64 KiB prefix scan for `cwd`, title hints, and timestamps during discovery.
fn scan_prefix(path: &Path) -> PrefixInfo {
    let mut info = PrefixInfo::default();
    let Ok(prefix) = read_bounded_prefix(path, PREFIX_SCAN_BYTES) else {
        return info;
    };
    let mut first_user: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;
    for line in prefix.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if info.cwd.is_none()
            && let Some(c) = val
                .get("cwd")
                .and_then(Value::as_str)
                .map(|s| sanitize_meta(s, MAX_IMPORT_META_CHARS))
                .filter(|s| !s.is_empty())
        {
            info.cwd = Some(c);
        }
        if let Some(ts) = val.get("timestamp").and_then(parse_timestamp_ms) {
            info.updated_at_ms = Some(info.updated_at_ms.map_or(ts, |prev| prev.max(ts)));
        }
        if let Some((t, src)) = extract_title_record(&val) {
            match src {
                TitleSource::User => custom_title = Some(t),
                TitleSource::Auto => ai_title = Some(t),
            }
        }
        if first_user.is_none() && !is_skipped(&val) && is_user_entry(&val) {
            let text = extract_user_text(&val);
            if !text.is_empty() {
                first_user = Some(clip_title(&text, 80));
            }
        }
    }
    info.title = custom_title.or(ai_title).or(first_user);
    info
}

/// Parse one Claude Code JSONL file into a full DAG [`ImportedTranscript`].
pub(crate) fn parse_session(claude_dir: &Path, jsonl_path: &Path) -> Result<ImportedTranscript> {
    let root_canon = canonicalize_source_root(claude_dir)?;
    let file_canon = ensure_confined_file(&root_canon, jsonl_path)?;
    let source_id = file_canon
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("claude-session")
        .to_owned();
    let slug_dir = file_canon.parent().unwrap_or(&root_canon);
    let slug = slug_dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let project_cwds = load_claude_json_projects(claude_dir);
    let custom_titles = load_custom_titles(&root_canon, claude_dir, slug_dir);
    let records = read_jsonl_values(&file_canon)?;

    let user_title = custom_titles.get(&source_id).cloned();
    let mut state = ClaudeParseState::new(
        source_id,
        user_title,
        project_cwds.get(slug).cloned(),
        file_mtime_ms(&file_canon),
    );
    state.ingest(&records);
    Ok(state.finish())
}

struct ClaudeParseState {
    source_id: String,
    user_title: Option<String>,
    ai_title: Option<String>,
    fallback_title: Option<String>,
    cwd: Option<String>,
    fallback_cwd: Option<String>,
    branch: Option<String>,
    model: Option<String>,
    created_at_ms: Option<i64>,
    updated_at_ms: i64,
    last_finish_reason: Option<FinishReason>,
    nodes: Vec<ImportedNode>,
    usage: Vec<ImportedUsage>,
    raw_parents: HashMap<String, Option<String>>,
    uuid_tail: HashMap<String, String>,
    tool_names: HashMap<String, String>,
    node_msg_ids: HashMap<String, String>,
    best_head: Option<(i64, usize, String)>,
    turn_seq: usize,
    node_seq: usize,
}

impl ClaudeParseState {
    fn new(
        source_id: String,
        user_title: Option<String>,
        fallback_cwd: Option<String>,
        mtime_ms: i64,
    ) -> Self {
        Self {
            source_id,
            user_title,
            ai_title: None,
            fallback_title: None,
            cwd: None,
            fallback_cwd,
            branch: None,
            model: None,
            created_at_ms: None,
            updated_at_ms: mtime_ms,
            last_finish_reason: None,
            nodes: Vec::new(),
            usage: Vec::new(),
            raw_parents: HashMap::new(),
            uuid_tail: HashMap::new(),
            tool_names: HashMap::new(),
            node_msg_ids: HashMap::new(),
            best_head: None,
            turn_seq: 0,
            node_seq: 0,
        }
    }

    fn ingest(&mut self, records: &[Value]) {
        let mut child_counts: HashMap<String, usize> = HashMap::new();
        for (idx, val) in records.iter().enumerate() {
            let uuid = entry_uuid(val, idx);
            let parent = val
                .get("parentUuid")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if let Some(ref p) = parent {
                *child_counts.entry(p.clone()).or_insert(0) += 1;
            }
            self.raw_parents.insert(uuid, parent);
            collect_tool_names(val, &mut self.tool_names);
        }

        for (idx, val) in records.iter().enumerate() {
            self.capture_metadata(val);
            if is_skipped(val) {
                continue;
            }
            let uuid = entry_uuid(val, idx);
            let ts = val
                .get("timestamp")
                .and_then(parse_timestamp_ms)
                .unwrap_or_else(|| self.fallback_ts());
            let parent_local = self.resolve_parent_local(&uuid);

            if is_user_entry(val) {
                self.ingest_user(val, &uuid, parent_local, ts);
            } else if is_assistant_entry(val) {
                self.ingest_assistant(val, &uuid, parent_local, ts, &child_counts);
            }
        }
    }

    fn capture_metadata(&mut self, val: &Value) {
        if self.cwd.is_none()
            && let Some(c) = val
                .get("cwd")
                .and_then(Value::as_str)
                .map(|s| sanitize_meta(s, MAX_IMPORT_META_CHARS))
                .filter(|s| !s.is_empty())
        {
            self.cwd = Some(c);
        }
        if self.branch.is_none()
            && let Some(b) = val
                .get("gitBranch")
                .and_then(Value::as_str)
                .map(|s| sanitize_meta(s, MAX_IMPORT_META_CHARS))
                .filter(|s| !s.is_empty())
        {
            self.branch = Some(b);
        }
        if let Some(ts) = val.get("timestamp").and_then(parse_timestamp_ms) {
            self.created_at_ms = Some(self.created_at_ms.map_or(ts, |prev| prev.min(ts)));
            self.updated_at_ms = self.updated_at_ms.max(ts);
        }
        if let Some((title, src)) = extract_title_record(val) {
            match src {
                TitleSource::User => self.user_title = Some(title),
                TitleSource::Auto => self.ai_title = Some(title),
            }
        }
    }

    fn resolve_parent_local(&self, uuid: &str) -> Option<String> {
        let mut cursor = self.raw_parents.get(uuid).and_then(Clone::clone);
        let mut hops = 0usize;
        while let Some(parent_uuid) = cursor {
            if hops > 1024 {
                break;
            }
            hops += 1;
            if let Some(local) = self.uuid_tail.get(&parent_uuid) {
                return Some(local.clone());
            }
            cursor = self.raw_parents.get(&parent_uuid).and_then(Clone::clone);
        }
        None
    }

    fn next_local_id(&mut self) -> String {
        self.node_seq += 1;
        format!("c_{}", self.node_seq)
    }

    fn fallback_ts(&self) -> i64 {
        self.updated_at_ms.max(now_ms())
    }

    fn push_node(
        &mut self,
        parent: Option<String>,
        created_at_ms: i64,
        payload: DagPayload,
    ) -> String {
        let local_id = self.next_local_id();
        let order = self.nodes.len();
        self.nodes.push(ImportedNode {
            local_id: local_id.clone(),
            parent_local_id: parent,
            created_at_ms,
            payload,
        });
        let candidate = (created_at_ms, order, local_id.clone());
        let replace = self
            .best_head
            .as_ref()
            .is_none_or(|best| (candidate.0, candidate.1) >= (best.0, best.1));
        if replace {
            self.best_head = Some(candidate);
        }
        local_id
    }

    fn ingest_user(&mut self, val: &Value, uuid: &str, mut parent: Option<String>, ts: i64) {
        let (tool_results, text_parts) = self.extract_user_parts(val);
        if tool_results.is_empty() && text_parts.is_empty() {
            return;
        }
        if !text_parts.is_empty() {
            self.turn_seq += 1;
        }
        let turn_id = format!("turn_{}", self.turn_seq.max(1));
        let mut tail = None;
        for msg in tool_results {
            let id = self.push_node(
                parent,
                ts,
                DagPayload::Message {
                    turn_id: turn_id.clone(),
                    msg,
                },
            );
            parent = Some(id.clone());
            tail = Some(id);
        }
        if !text_parts.is_empty() {
            let text = text_parts.join("\n");
            if self.fallback_title.is_none() {
                self.fallback_title = Some(clip_title(&text, 80));
            }
            let id = self.push_node(
                parent,
                ts,
                DagPayload::Message {
                    turn_id,
                    msg: Msg::user(text),
                },
            );
            tail = Some(id);
        }
        if let Some(last) = tail {
            self.uuid_tail.insert(uuid.to_owned(), last);
        }
    }

    fn extract_user_parts(&self, val: &Value) -> (Vec<Msg>, Vec<String>) {
        let content = val.get("message").and_then(|m| m.get("content"));
        let mut tool_results = Vec::new();
        let mut text_parts = Vec::new();
        match content {
            Some(Value::String(s)) if !s.trim().is_empty() => text_parts.push(s.clone()),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                    if kind == "tool_result" {
                        let call_id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let name = self
                            .tool_names
                            .get(&call_id)
                            .cloned()
                            .unwrap_or_else(|| "tool".to_owned());
                        let body = extract_tool_result_content(block.get("content"));
                        let mut msg = Msg::tool_result(call_id, name, body);
                        msg.is_error = block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        tool_results.push(msg);
                    } else if kind == "text"
                        && let Some(t) = block.get("text").and_then(Value::as_str)
                        && !t.trim().is_empty()
                    {
                        text_parts.push(t.to_owned());
                    }
                }
            }
            _ => {}
        }
        (tool_results, text_parts)
    }

    fn ingest_assistant(
        &mut self,
        val: &Value,
        uuid: &str,
        parent: Option<String>,
        ts: i64,
        child_counts: &HashMap<String, usize>,
    ) {
        let Some(message) = val.get("message") else {
            return;
        };
        let model_id = message
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(ref m) = model_id {
            self.model = Some(m.clone());
        }
        let model_for_thinking = model_id
            .or_else(|| self.model.clone())
            .unwrap_or_else(|| "claude".to_owned());
        if let Some(raw_stop) = message.get("stop_reason").and_then(Value::as_str) {
            self.last_finish_reason = Some(map_stop_reason(raw_stop));
        }
        if let Some(usage) = parse_claude_usage(message.get("usage")) {
            self.usage.push(ImportedUsage {
                at_ms: ts,
                model: model_for_thinking.clone(),
                usage,
                after_node_count: self.nodes.len(),
            });
        }

        let (text, reasoning, thinking_blocks, tool_calls) = parse_assistant_content(
            message.get("content"),
            &model_for_thinking,
            &mut self.tool_names,
        );
        if text.is_empty()
            && reasoning.is_none()
            && thinking_blocks.is_empty()
            && tool_calls.is_empty()
        {
            return;
        }

        let msg_id = message
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let parent_raw_uuid = self.raw_parents.get(uuid).and_then(Clone::clone);
        let parent_unforked = parent_raw_uuid
            .as_deref()
            .is_some_and(|p| child_counts.get(p).copied().unwrap_or(0) <= 1);
        if let (Some(mid), Some(parent_id)) = (msg_id, parent.as_deref())
            && parent_unforked
            && self.node_msg_ids.get(parent_id).map(String::as_str) == Some(mid)
            && let Some(prev) = self.nodes.iter_mut().find(|n| n.local_id == parent_id)
            && let DagPayload::Message { msg, .. } = &mut prev.payload
            && msg.role == Role::Assistant
        {
            merge_assistant_msg(msg, &text, reasoning, thinking_blocks, tool_calls);
            self.uuid_tail.insert(uuid.to_owned(), parent_id.to_owned());
            return;
        }

        let mut msg = Msg::assistant_tool_calls(text, reasoning, tool_calls);
        msg.thinking_blocks = thinking_blocks;
        let turn_id = format!("turn_{}", self.turn_seq.max(1));
        let local_id = self.push_node(parent, ts, DagPayload::Message { turn_id, msg });
        if let Some(mid) = msg_id {
            self.node_msg_ids.insert(local_id.clone(), mid.to_owned());
        }
        self.uuid_tail.insert(uuid.to_owned(), local_id);
    }

    fn finish(mut self) -> ImportedTranscript {
        let mut status = AgentStatus::Idle;
        if let Some(reason) = self.last_finish_reason {
            match reason {
                FinishReason::Error => status = AgentStatus::Error,
                FinishReason::Aborted => {
                    if let Some((_, _, head)) = self.best_head.clone() {
                        self.push_node(
                            Some(head),
                            self.updated_at_ms,
                            DagPayload::SessionExit {
                                reason: ExitReason::Interrupted,
                                pending: Vec::new(),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        let (title, title_source) = if let Some(t) = self.user_title {
            (t, Some(TitleSource::User))
        } else if let Some(t) = self.ai_title {
            (t, Some(TitleSource::Auto))
        } else {
            (self.fallback_title.unwrap_or_default(), None)
        };
        let created_at_ms = self.created_at_ms.unwrap_or(self.updated_at_ms);
        let cwd = self
            .cwd
            .or(self.fallback_cwd)
            .unwrap_or_else(|| ".".to_owned());
        ImportedTranscript {
            source: ImportSource::Claude,
            source_id: self.source_id,
            title,
            title_source,
            cwd,
            branch: self.branch,
            model: self.model,
            status,
            created_at_ms,
            updated_at_ms: self.updated_at_ms.max(created_at_ms),
            head_local_id: self.best_head.map(|(_, _, id)| id),
            nodes: self.nodes,
            usage: self.usage,
            last_finish_reason: self.last_finish_reason,
        }
    }
}
