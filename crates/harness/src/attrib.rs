//! Session-tree edit attribution and unified-diff hunk filtering.
//!
//! Records which relative workspace paths and post-edit line ranges were modified by `edit`,
//! `write`, and `apply_patch` tool calls in a session and its descendant subagent sessions,
//! persisting lightweight [`TraceRecord`] entries (`kind = "edit"`) in [`SessionStore`].
//! [`filter_diff_to_attributed`] then slices a `git diff` down to ONLY attributed files and hunks
//! so `/commit` and `hrn commit` never stage unrelated human edits.

#[path = "attrib_diff.rs"]
mod diff;

pub use diff::filter_diff_to_attributed;

use diff::{coalesce_ranges, extract_diff_attributions};
use hrn_kernel::{Clock, Result, SessionStore, SystemClock, ToolOutput, TraceRecord, UrlRouter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;

/// [`TraceRecord::kind`] used for edit attribution records.
pub const EDIT_TRACE_KIND: &str = "edit";

/// Maximum descendant sessions visited when aggregating attribution for one root session.
const MAX_ATTRIBUTION_SESSIONS: usize = 256;

/// JSON payload stored inside [`TraceRecord::payload`] for `kind = "edit"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditTracePayload {
    /// Repo/workspace-relative path normalized with forward slashes.
    pub path: String,
    /// Inclusive 1-based changed line ranges `[start, end]` in post-edit file coordinates.
    #[serde(default)]
    pub ranges: Vec<[usize; 2]>,
    /// `true` for whole-file writes, creations, or deletions.
    #[serde(default)]
    pub whole_file: bool,
    /// Session ID (root or child subagent) that performed the edit.
    pub session_id: String,
}

/// Attributed edits recorded for a single workspace-relative path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileAttribution {
    /// `true` when the entire file is attributed (e.g. `write`, new file, or deletion).
    pub whole_file: bool,
    /// Coalesced 1-based inclusive line ranges `(start, end)` touched by the agent.
    pub ranges: Vec<(usize, usize)>,
}

impl FileAttribution {
    /// Merge `other` into this file attribution, coalescing overlapping/adjacent ranges.
    pub fn merge(&mut self, other: &Self) {
        self.whole_file |= other.whole_file;
        if self.whole_file {
            self.ranges.clear();
            return;
        }
        self.ranges.extend(other.ranges.iter().copied());
        coalesce_ranges(&mut self.ranges);
    }

    /// Return `true` when any changed span in `changed` overlaps this file's attributed edits.
    pub fn matches_changes(&self, changed: &[(usize, usize)]) -> bool {
        if self.whole_file {
            return true;
        }
        changed
            .iter()
            .any(|&(a, b)| self.ranges.iter().any(|&(c, d)| a <= d && c <= b))
    }
}

/// Aggregated edit attribution across a root session and all descendant subagents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditAttribution {
    /// Map of normalized forward-slash relative path -> attributed spans.
    pub files: BTreeMap<String, FileAttribution>,
}

impl EditAttribution {
    /// Whether no edits have been attributed.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Insert or merge one path's attribution.
    pub fn record(&mut self, path: String, whole_file: bool, ranges: &[(usize, usize)]) {
        let mut incoming = FileAttribution {
            whole_file,
            ranges: ranges.to_vec(),
        };
        coalesce_ranges(&mut incoming.ranges);
        self.files
            .entry(path)
            .and_modify(|existing| existing.merge(&incoming))
            .or_insert(incoming);
    }

    /// Re-root every path under `prefix` (used when session `cwd` is a subdirectory of the git
    /// repository root).
    pub fn with_prefix(&self, prefix: &str) -> Self {
        let clean = prefix.trim().replace('\\', "/");
        let clean = clean.trim_matches('/');
        if clean.is_empty() {
            return self.clone();
        }
        let mut out = Self::default();
        for (path, attr) in &self.files {
            out.files.insert(format!("{clean}/{path}"), attr.clone());
        }
        out
    }
}

/// Result of filtering a `git diff` to attributed files and hunks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilteredDiff {
    /// Unified diff patch containing ONLY attributed files and hunks, ready for `git apply --cached`.
    pub patch: String,
    /// Normalized paths present in `patch` (deduplicated, in diff order).
    pub files: Vec<String>,
    /// Number of hunks kept.
    pub hunks_kept: usize,
    /// Number of hunks skipped because they did not overlap any attributed edit.
    pub hunks_skipped: usize,
}

/// Record an explicit edit attribution trace for `session_id`.
pub fn record_edit_trace(
    store: &dyn SessionStore,
    session_id: &str,
    path: &str,
    ranges: &[(usize, usize)],
    whole_file: bool,
) -> Result {
    let mut normalized_ranges = ranges.to_vec();
    coalesce_ranges(&mut normalized_ranges);
    let payload = EditTracePayload {
        path: path.to_owned(),
        ranges: normalized_ranges.into_iter().map(|(a, b)| [a, b]).collect(),
        whole_file,
        session_id: session_id.to_owned(),
    };
    let value = serde_json::to_value(payload)
        .map_err(|e| hrn_kernel::HrnError::Store(format!("serialize edit trace: {e}")))?;
    store.record_trace(&TraceRecord {
        session_id: session_id.to_owned(),
        node_id: None,
        kind: EDIT_TRACE_KIND.to_owned(),
        started_ms: SystemClock.now_ms(),
        dur_ms: 0,
        payload: value,
    })
}

/// Inspect a successful tool execution and persist edit attribution traces when `tool_name` is
/// `edit`, `write`, or `apply_patch`. Failures to persist are intentionally ignored so trace
/// recording never breaks tool execution.
pub(crate) fn record_tool_edit(
    store: &dyn SessionStore,
    session_id: &str,
    cwd: &Path,
    tool_name: &str,
    args: &Value,
    output: &ToolOutput,
) {
    if output.is_error || !matches!(tool_name, "edit" | "write" | "apply_patch") {
        return;
    }
    if tool_name == "write" {
        if let Some(raw_path) = args.get("path").and_then(Value::as_str)
            && let Some(rel) = normalize_rel_path(cwd, raw_path)
        {
            let _ = record_edit_trace(store, session_id, &rel, &[], true);
        }
        return;
    }
    if let Some(diff) = output.diff.as_deref() {
        let extracted = extract_diff_attributions(cwd, diff);
        if !extracted.is_empty() {
            for (rel, attr) in extracted {
                let _ = record_edit_trace(store, session_id, &rel, &attr.ranges, attr.whole_file);
            }
            return;
        }
    }
    if let Some(raw_path) = args.get("path").and_then(Value::as_str)
        && let Some(rel) = normalize_rel_path(cwd, raw_path)
    {
        let _ = record_edit_trace(store, session_id, &rel, &[], true);
    }
}

/// Aggregate all edit attribution traces across `root_session_id` and its entire descendant
/// subagent session tree.
pub fn load_session_attribution(
    store: &dyn SessionStore,
    root_session_id: &str,
) -> Result<EditAttribution> {
    let mut attribution = EditAttribution::default();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    visited.insert(root_session_id.to_owned());
    queue.push_back(root_session_id.to_owned());

    while let Some(sid) = queue.pop_front() {
        for trace in store.list_traces(&sid)? {
            if trace.kind != EDIT_TRACE_KIND {
                continue;
            }
            let Ok(payload) = serde_json::from_value::<EditTracePayload>(trace.payload) else {
                continue;
            };
            let clean = normalize_lexical_rel(&payload.path);
            if clean.is_empty() {
                continue;
            }
            let ranges: Vec<(usize, usize)> = payload
                .ranges
                .into_iter()
                .map(|[a, b]| (a.min(b).max(1), a.max(b).max(1)))
                .collect();
            attribution.record(clean, payload.whole_file, &ranges);
        }
        if visited.len() >= MAX_ATTRIBUTION_SESSIONS {
            continue;
        }
        for child in store.list_children(&sid)? {
            if visited.len() >= MAX_ATTRIBUTION_SESSIONS {
                break;
            }
            if visited.insert(child.id.clone()) {
                queue.push_back(child.id);
            }
        }
    }
    Ok(attribution)
}

/// Normalize a tool path argument or diff header path relative to `cwd` using forward slashes.
/// Returns `None` for internal URL schemes (`cfg://`, `out://`, ...) or paths escaping `cwd`.
pub fn normalize_rel_path(cwd: &Path, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || UrlRouter::split(trimmed).is_some() {
        return None;
    }
    let path = Path::new(trimmed);
    let rel_str = if path.is_absolute() {
        strip_cwd_prefix(cwd, path)?
    } else {
        trimmed.to_owned()
    };
    let clean = normalize_lexical_rel(&rel_str);
    (!clean.is_empty()).then_some(clean)
}

fn strip_cwd_prefix(cwd: &Path, path: &Path) -> Option<String> {
    if let Ok(rel) = path.strip_prefix(cwd) {
        return Some(rel.to_string_lossy().into_owned());
    }
    let norm_cwd = strip_verbatim(cwd).replace('\\', "/");
    let norm_cwd = norm_cwd.trim_end_matches('/');
    let norm_path = strip_verbatim(path).replace('\\', "/");
    if cfg!(windows) {
        let low_cwd = norm_cwd.to_ascii_lowercase();
        let low_path = norm_path.to_ascii_lowercase();
        let rest = low_path.strip_prefix(&low_cwd)?;
        if !rest.starts_with('/') {
            return None;
        }
        return norm_path.get(norm_cwd.len() + 1..).map(str::to_owned);
    }
    let rest = norm_path.strip_prefix(norm_cwd)?;
    rest.strip_prefix('/').map(str::to_owned)
}

fn strip_verbatim(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
}

pub(super) fn normalize_lexical_rel(raw: &str) -> String {
    let unified = raw.trim().replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for seg in unified.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return String::new();
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[cfg(test)]
#[path = "attrib_tests.rs"]
mod tests;
