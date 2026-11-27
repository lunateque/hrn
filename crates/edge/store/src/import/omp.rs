//! Read-only OMP transcript discovery and DAG importer (`~/.omp/agent/sessions/<slug>/*.jsonl`).

use super::io::{
    MAX_IMPORT_META_CHARS, MAX_IMPORT_TITLE_CHARS, canonicalize_omp_root, clip_title,
    ensure_confined_omp_dir, ensure_confined_omp_file, file_mtime_ms, is_forbidden_omp_path,
    now_ms, parse_timestamp_ms, read_jsonl_values_omp, sanitize_meta, sanitize_title,
};
use super::omp_parse::{
    collect_tool_names, entry_id, extract_model_selector, extract_text_content,
    extract_title_record, filename_session_id, parse_assistant_content, parse_omp_usage,
    parse_tool_result, scan_prefix,
};
use super::{ExternalSession, ImportSource, ImportedNode, ImportedTranscript, ImportedUsage};
use hrn_kernel::{
    AgentStatus, DagPayload, ExitReason, FinishReason, HrnError, Msg, Result, TitleSource,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub use super::omp_parse::map_stop_reason;

/// Maximum files scanned for header-id fallback when filename matching finds no session.
const FALLBACK_ID_SCAN_LIMIT: usize = 2_000;

/// Discover OMP sessions under `omp_dir` (`<omp_dir>/sessions/<slug>/*.jsonl`, newest first).
///
/// When `limit` is `Some(n)`, candidates are sorted by file mtime and truncated BEFORE opening
/// any JSONL files so large session directories open rapidly.
pub(crate) fn discover(omp_dir: &Path, limit: Option<usize>) -> Result<Vec<ExternalSession>> {
    if !omp_dir.exists() {
        return Ok(Vec::new());
    }
    let root_canon = canonicalize_omp_root(omp_dir)?;
    let mut candidates = collect_candidates(&root_canon)?;
    if let Some(max) = limit {
        candidates.truncate(max);
    }
    let mut out = Vec::with_capacity(candidates.len());
    for (candidate_path, mtime_ms) in candidates {
        let Ok(file_canon) = ensure_confined_omp_file(&root_canon, &candidate_path) else {
            continue;
        };
        let prefix = scan_prefix(&file_canon);
        let id = prefix
            .session_id
            .unwrap_or_else(|| filename_session_id(&file_canon));
        let cwd = prefix
            .cwd
            .map(|c| sanitize_meta(&c, MAX_IMPORT_META_CHARS))
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| ".".to_owned());
        let title = prefix
            .title
            .map(|t| sanitize_title(&t, MAX_IMPORT_TITLE_CHARS))
            .unwrap_or_default();
        let updated_at_ms = prefix.updated_at_ms.unwrap_or(mtime_ms).max(mtime_ms);
        out.push(ExternalSession {
            source: ImportSource::Omp,
            id: sanitize_meta(&id, MAX_IMPORT_META_CHARS),
            title,
            cwd,
            updated_at_ms,
            path: file_canon,
            archived: false,
        });
    }
    out.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(out)
}

/// Resolve a single OMP session JSONL path (`None` or `"latest"` picks the newest transcript).
///
/// Matches by filename stem/suffix first so importing an older session by ID never reads all
/// session files in large `~/.omp/agent/sessions/` trees.
pub(crate) fn resolve_session_path(omp_dir: &Path, wanted: Option<&str>) -> Result<PathBuf> {
    if !omp_dir.exists() {
        return Err(HrnError::Store(
            "no omp sessions found to import".to_owned(),
        ));
    }
    let root_canon = canonicalize_omp_root(omp_dir)?;
    let candidates = collect_candidates(&root_canon)?;
    if candidates.is_empty() {
        return Err(HrnError::Store(
            "no omp sessions found to import".to_owned(),
        ));
    }
    let raw = wanted
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("latest"));
    let Some(query) = raw else {
        for (path, _) in candidates {
            if let Ok(canon) = ensure_confined_omp_file(&root_canon, &path) {
                return Ok(canon);
            }
        }
        return Err(HrnError::Store(
            "no omp sessions found to import".to_owned(),
        ));
    };

    if let Some(found) = match_by_filename(&root_canon, &candidates, query)? {
        return Ok(found);
    }
    match_by_header_prefix(&root_canon, &candidates, query)
}

fn match_by_filename(
    root_canon: &Path,
    candidates: &[(PathBuf, i64)],
    query: &str,
) -> Result<Option<PathBuf>> {
    let mut prefix_matches = Vec::new();
    for (path, _) in candidates {
        let fid = filename_session_id(path);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if fid == query || stem == query {
            return Ok(Some(ensure_confined_omp_file(root_canon, path)?));
        }
        if fid.starts_with(query) || stem.starts_with(query) {
            prefix_matches.push(path);
        }
    }
    match prefix_matches.as_slice() {
        [one] => Ok(Some(ensure_confined_omp_file(root_canon, one)?)),
        [] => Ok(None),
        many => Err(HrnError::Store(format!(
            "`{query}` matches {} omp sessions; pass a longer id prefix",
            many.len()
        ))),
    }
}

fn match_by_header_prefix(
    root_canon: &Path,
    candidates: &[(PathBuf, i64)],
    query: &str,
) -> Result<PathBuf> {
    let mut prefix_matches = Vec::new();
    for (path, _) in candidates.iter().take(FALLBACK_ID_SCAN_LIMIT) {
        let Ok(canon) = ensure_confined_omp_file(root_canon, path) else {
            continue;
        };
        let Some(sid) = scan_prefix(&canon).session_id else {
            continue;
        };
        if sid == query {
            return Ok(canon);
        }
        if sid.starts_with(query) {
            prefix_matches.push(canon);
        }
    }
    match prefix_matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(HrnError::Store(format!("no omp session matches `{query}`"))),
        many => Err(HrnError::Store(format!(
            "`{query}` matches {} omp sessions; pass a longer id prefix",
            many.len()
        ))),
    }
}

/// Walk `<root>/sessions` (or `<root>` directly if it holds transcripts) without opening files.
fn collect_candidates(root_canon: &Path) -> Result<Vec<(PathBuf, i64)>> {
    let sessions_sub = root_canon.join("sessions");
    let scan_root = if sessions_sub.exists() {
        ensure_confined_omp_dir(root_canon, &sessions_sub)?
    } else {
        root_canon.to_path_buf()
    };
    let mut files = Vec::new();
    collect_dir_jsonl(&scan_root, &mut files, true);
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(files)
}

fn collect_dir_jsonl(dir: &Path, out: &mut Vec<(PathBuf, i64)>, recurse_one: bool) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_forbidden_omp_path(&path) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.file_type().is_dir() && recurse_one {
            if let Ok(sub_canon) = ensure_confined_omp_dir(dir, &path) {
                collect_dir_jsonl(&sub_canon, out, false);
            }
            continue;
        }
        if meta.file_type().is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            out.push((path.clone(), file_mtime_ms(&path)));
        }
    }
}

/// Parse one OMP v3 JSONL file into a full DAG [`ImportedTranscript`].
pub(crate) fn parse_session(omp_dir: &Path, jsonl_path: &Path) -> Result<ImportedTranscript> {
    let root_canon = canonicalize_omp_root(omp_dir)?;
    let file_canon = ensure_confined_omp_file(&root_canon, jsonl_path)?;
    let fallback_id = filename_session_id(&file_canon);
    let records = read_jsonl_values_omp(&file_canon)?;
    let mut state = OmpParseState::new(fallback_id, file_mtime_ms(&file_canon));
    state.ingest(&records);
    Ok(state.finish())
}

struct OmpParseState {
    source_id: String,
    user_title: Option<String>,
    auto_title: Option<String>,
    fallback_title: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    created_at_ms: Option<i64>,
    updated_at_ms: i64,
    last_finish_reason: Option<FinishReason>,
    nodes: Vec<ImportedNode>,
    usage: Vec<ImportedUsage>,
    raw_parents: HashMap<String, Option<String>>,
    id_tail: HashMap<String, String>,
    tool_names: HashMap<String, String>,
    best_head: Option<(i64, usize, String)>,
    turn_seq: usize,
    node_seq: usize,
}

impl OmpParseState {
    fn new(source_id: String, mtime_ms: i64) -> Self {
        Self {
            source_id,
            user_title: None,
            auto_title: None,
            fallback_title: None,
            cwd: None,
            model: None,
            created_at_ms: None,
            updated_at_ms: mtime_ms,
            last_finish_reason: None,
            nodes: Vec::new(),
            usage: Vec::new(),
            raw_parents: HashMap::new(),
            id_tail: HashMap::new(),
            tool_names: HashMap::new(),
            best_head: None,
            turn_seq: 0,
            node_seq: 0,
        }
    }

    fn ingest(&mut self, records: &[Value]) {
        for (idx, val) in records.iter().enumerate() {
            let rid = entry_id(val, idx);
            let parent = val
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            self.raw_parents.insert(rid, parent);
            collect_tool_names(val, &mut self.tool_names);
        }

        for (idx, val) in records.iter().enumerate() {
            self.capture_metadata(val);
            let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
            let rid = entry_id(val, idx);
            let ts = val
                .get("timestamp")
                .or_else(|| val.get("message").and_then(|m| m.get("timestamp")))
                .and_then(parse_timestamp_ms)
                .unwrap_or_else(|| self.fallback_ts());
            let parent = self.resolve_parent_local(&rid);

            match kind {
                "message" => self.ingest_message(val, &rid, parent, ts),
                "model_change" => self.ingest_model_change(val, &rid, parent, ts),
                "compaction" | "branch_summary" => self.ingest_summary(val, kind, &rid, parent, ts),
                _ => {}
            }
        }
    }

    fn capture_metadata(&mut self, val: &Value) {
        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "session" {
            if let Some(id) = val
                .get("id")
                .and_then(Value::as_str)
                .map(|s| sanitize_meta(s, MAX_IMPORT_META_CHARS))
                .filter(|s| !s.is_empty())
            {
                self.source_id = id;
            }
            if self.cwd.is_none()
                && let Some(c) = val
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(|s| sanitize_meta(s, MAX_IMPORT_META_CHARS))
                    .filter(|s| !s.is_empty())
            {
                self.cwd = Some(c);
            }
        }
        if let Some(ts) = val
            .get("timestamp")
            .or_else(|| val.get("updatedAt"))
            .and_then(parse_timestamp_ms)
        {
            self.created_at_ms = Some(self.created_at_ms.map_or(ts, |prev| prev.min(ts)));
            self.updated_at_ms = self.updated_at_ms.max(ts);
        }
        if let Some((title, src)) = extract_title_record(val) {
            match src {
                TitleSource::User => self.user_title = Some(title),
                TitleSource::Auto => self.auto_title = Some(title),
            }
        }
    }

    fn resolve_parent_local(&self, rid: &str) -> Option<String> {
        let mut cursor = self.raw_parents.get(rid).and_then(Clone::clone);
        let mut hops = 0usize;
        while let Some(parent_id) = cursor {
            if hops > 2048 {
                break;
            }
            hops += 1;
            if let Some(local) = self.id_tail.get(&parent_id) {
                return Some(local.clone());
            }
            cursor = self.raw_parents.get(&parent_id).and_then(Clone::clone);
        }
        None
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
        self.node_seq += 1;
        let local_id = format!("o_{}", self.node_seq);
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

    fn ingest_model_change(&mut self, val: &Value, rid: &str, parent: Option<String>, ts: i64) {
        let Some(selector) = extract_model_selector(val) else {
            return;
        };
        let is_switch = self.model.is_some() && parent.is_some();
        self.model = Some(selector.clone());
        if is_switch {
            let local_id = self.push_node(
                parent,
                ts,
                DagPayload::ModelChange {
                    selector,
                    role: None,
                },
            );
            self.id_tail.insert(rid.to_owned(), local_id);
        }
    }

    fn ingest_summary(
        &mut self,
        val: &Value,
        kind: &str,
        rid: &str,
        parent: Option<String>,
        ts: i64,
    ) {
        let Some(text) = val
            .get("summary")
            .or_else(|| val.get("text"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let covers_node_id = parent.clone().unwrap_or_default();
        let local_id = self.push_node(
            parent,
            ts,
            DagPayload::Summary {
                covers_node_id,
                text: text.to_owned(),
                method: Some(kind.to_owned()),
                preserve: None,
            },
        );
        self.id_tail.insert(rid.to_owned(), local_id);
    }

    fn ingest_message(&mut self, val: &Value, rid: &str, parent: Option<String>, ts: i64) {
        let Some(message) = val.get("message") else {
            return;
        };
        match message.get("role").and_then(Value::as_str).unwrap_or("") {
            "user" => {
                let text = extract_text_content(message.get("content"));
                if text.trim().is_empty() {
                    return;
                }
                self.turn_seq += 1;
                if self.fallback_title.is_none() {
                    self.fallback_title = Some(clip_title(&text, 80));
                }
                let turn_id = format!("turn_{}", self.turn_seq);
                let local_id = self.push_node(
                    parent,
                    ts,
                    DagPayload::Message {
                        turn_id,
                        msg: Msg::user(text),
                    },
                );
                self.id_tail.insert(rid.to_owned(), local_id);
            }
            "assistant" => self.ingest_assistant(message, rid, parent, ts),
            "toolResult" | "tool" => {
                let msg = parse_tool_result(message, &self.tool_names);
                let turn_id = format!("turn_{}", self.turn_seq.max(1));
                let local_id = self.push_node(parent, ts, DagPayload::Message { turn_id, msg });
                self.id_tail.insert(rid.to_owned(), local_id);
            }
            _ => {}
        }
    }

    fn ingest_assistant(&mut self, message: &Value, rid: &str, parent: Option<String>, ts: i64) {
        let model_id = extract_model_selector(message);
        if let Some(ref m) = model_id {
            self.model = Some(m.clone());
        }
        let model_for_meta = model_id
            .or_else(|| self.model.clone())
            .unwrap_or_else(|| "omp".to_owned());
        if let Some(raw_stop) = message
            .get("stopReason")
            .or_else(|| message.get("stop_reason"))
            .and_then(Value::as_str)
        {
            self.last_finish_reason = Some(map_stop_reason(raw_stop));
        }
        if let Some(usage) = parse_omp_usage(message.get("usage")) {
            self.usage.push(ImportedUsage {
                at_ms: ts,
                model: model_for_meta.clone(),
                usage,
                after_node_count: self.nodes.len(),
            });
        }
        let (text, reasoning, thinking_blocks, tool_calls) = parse_assistant_content(
            message.get("content"),
            &model_for_meta,
            &mut self.tool_names,
        );
        if text.is_empty()
            && reasoning.is_none()
            && thinking_blocks.is_empty()
            && tool_calls.is_empty()
        {
            return;
        }
        let mut msg = Msg::assistant_tool_calls(text, reasoning, tool_calls);
        msg.thinking_blocks = thinking_blocks;
        let turn_id = format!("turn_{}", self.turn_seq.max(1));
        let local_id = self.push_node(parent, ts, DagPayload::Message { turn_id, msg });
        self.id_tail.insert(rid.to_owned(), local_id);
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
        } else if let Some(t) = self.auto_title {
            (t, Some(TitleSource::Auto))
        } else {
            (self.fallback_title.unwrap_or_default(), None)
        };
        let created_at_ms = self.created_at_ms.unwrap_or(self.updated_at_ms);
        let cwd = self.cwd.unwrap_or_else(|| ".".to_owned());
        ImportedTranscript {
            source: ImportSource::Omp,
            source_id: self.source_id,
            title,
            title_source,
            cwd,
            branch: None,
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
