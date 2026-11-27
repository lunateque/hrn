//! Pure data model and formatting helpers for the live Agent Hub overlay:
//! roster tree lineage ordering/indentation, substring filtering, header aggregates,
//! Activity tab filtering/scoping, and structured inspector details.

use crate::port::{AgentActivityItem, AgentActivityKind, AgentHubEntry, AgentHubStatus};
use crate::render;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Active top-level tab inside the live Agent Hub overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HubTab {
    #[default]
    Agents,
    Activity,
}

/// Filter cycle on the Activity tab (`f`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ActivityFilter {
    #[default]
    All,
    Errors,
    Responses,
    Tools,
}

impl ActivityFilter {
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::All => Self::Errors,
            Self::Errors => Self::Responses,
            Self::Responses => Self::Tools,
            Self::Tools => Self::All,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Errors => "Errors",
            Self::Responses => "Responses",
            Self::Tools => "Tools",
        }
    }

    pub(crate) const fn matches(self, kind: AgentActivityKind) -> bool {
        match self {
            Self::All => true,
            Self::Errors => matches!(kind, AgentActivityKind::Error),
            Self::Responses => matches!(kind, AgentActivityKind::Response),
            Self::Tools => matches!(kind, AgentActivityKind::Tool),
        }
    }
}

/// Scope cycle on the Activity tab (`s`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ActivityScope {
    #[default]
    All,
    Selected,
    Subtree,
}

impl ActivityScope {
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::All => Self::Selected,
            Self::Selected => Self::Subtree,
            Self::Subtree => Self::All,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Selected => "selected",
            Self::Subtree => "subtree",
        }
    }
}

/// One ordered roster row ready to render, with tree lineage indentation metadata.
#[derive(Clone, Debug)]
pub(crate) struct RosterRow<'a> {
    pub(crate) entry: &'a AgentHubEntry,
    pub(crate) depth: usize,
    pub(crate) tree_prefix: String,
}

/// Header aggregate counters across all agents in the current session tree.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct HubAggregates {
    pub(crate) total: usize,
    pub(crate) running: usize,
    pub(crate) idle: usize,
    pub(crate) parked: usize,
    pub(crate) aborted: usize,
    pub(crate) failed: usize,
    pub(crate) unread: usize,
    pub(crate) requests: u64,
    pub(crate) tool_calls: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cost_usd: Option<f64>,
}

/// Action returned by [`crate::hub::AgentHub::key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentHubAction {
    Stay,
    Close,
    Focus(String),
    Revive(String),
    Abort(String),
    JumpToTranscript {
        agent_id: String,
        session_id: Option<String>,
        node_id: Option<String>,
        summary: String,
    },
}

/// Compute aggregate totals displayed in the Agent Hub header.
#[must_use]
pub(crate) fn compute_aggregates(entries: &[AgentHubEntry]) -> HubAggregates {
    let mut agg = HubAggregates::default();
    let mut cost_sum = 0.0_f64;
    let mut has_cost = false;
    for entry in entries {
        agg.total += 1;
        match entry.status {
            AgentHubStatus::Running => agg.running += 1,
            AgentHubStatus::Idle => agg.idle += 1,
            AgentHubStatus::Parked => agg.parked += 1,
            AgentHubStatus::Aborted => agg.aborted += 1,
            AgentHubStatus::Failed => agg.failed += 1,
        }
        agg.unread = agg.unread.saturating_add(entry.unread);
        agg.requests = agg.requests.saturating_add(entry.requests);
        agg.tool_calls = agg.tool_calls.saturating_add(entry.tool_calls);
        agg.input_tokens = agg.input_tokens.saturating_add(entry.input_tokens);
        agg.output_tokens = agg.output_tokens.saturating_add(entry.output_tokens);
        if let Some(c) = entry.cost_usd {
            cost_sum += c;
            has_cost = true;
        }
    }
    agg.cost_usd = has_cost.then_some(cost_sum);
    agg
}

/// Case-insensitive substring filter over an agent's identity, role, status, parent, model, task,
/// and current activity.
#[must_use]
pub(crate) fn matches_filter(entry: &AgentHubEntry, query: &str) -> bool {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return true;
    }
    let contains = |s: &str| s.to_ascii_lowercase().contains(&needle);
    contains(&entry.id)
        || contains(&entry.name)
        || contains(&entry.role)
        || contains(entry.status.as_str())
        || contains(&entry.task)
        || contains(&entry.activity)
        || entry.parent.as_deref().is_some_and(contains)
        || entry.model.as_deref().is_some_and(contains)
        || entry.current_tool.as_deref().is_some_and(contains)
}

/// Resolve the logical parent ID of `entry` inside the roster (`None` for main, `"main"` for
/// top-level subagents when a main entry is present in `present_ids`).
fn effective_parent<'a>(
    entry: &'a AgentHubEntry,
    present_ids: &HashSet<&str>,
    main_id: Option<&'a str>,
) -> Option<&'a str> {
    if entry.is_main {
        return None;
    }
    if let Some(parent) = entry.parent.as_deref() {
        if present_ids.contains(parent) {
            return Some(parent);
        }
        return None;
    }
    if let Some((prefix, _)) = entry.id.rsplit_once('.')
        && present_ids.contains(prefix)
    {
        return Some(prefix);
    }
    main_id.filter(|id| present_ids.contains(*id))
}

/// Order and optionally indent roster rows in tree lineage mode (`tree_mode == true`) or flat
/// mode (`tree_mode == false`), filtered by `query`.
#[must_use]
pub(crate) fn build_roster_rows<'a>(
    entries: &'a [AgentHubEntry],
    tree_mode: bool,
    query: &str,
) -> Vec<RosterRow<'a>> {
    let filtered: Vec<&'a AgentHubEntry> = entries
        .iter()
        .filter(|e| matches_filter(e, query))
        .collect();
    if !tree_mode {
        return filtered
            .into_iter()
            .map(|entry| RosterRow {
                entry,
                depth: 0,
                tree_prefix: String::new(),
            })
            .collect();
    }
    let present_ids: HashSet<&str> = filtered.iter().map(|e| e.id.as_str()).collect();
    let main_id = filtered.iter().find(|e| e.is_main).map(|e| e.id.as_str());
    let mut children: HashMap<Option<&'a str>, Vec<&'a AgentHubEntry>> = HashMap::new();
    for entry in &filtered {
        let parent = effective_parent(entry, &present_ids, main_id);
        children.entry(parent).or_default().push(*entry);
    }
    let mut rows = Vec::with_capacity(filtered.len());
    let mut visited: HashSet<&'a str> = HashSet::new();
    let mut active_guides: Vec<bool> = Vec::new();
    if let Some(roots) = children.get(&None) {
        emit_tree_level(
            roots,
            &children,
            0,
            &mut active_guides,
            &mut visited,
            &mut rows,
        );
    }
    // Emit any remaining entries (e.g. broken cycle) safely at root depth.
    for entry in filtered {
        if visited.insert(entry.id.as_str()) {
            rows.push(RosterRow {
                entry,
                depth: 0,
                tree_prefix: String::new(),
            });
        }
    }
    rows
}

fn emit_tree_level<'a>(
    siblings: &[&'a AgentHubEntry],
    children: &HashMap<Option<&'a str>, Vec<&'a AgentHubEntry>>,
    depth: usize,
    guides: &mut Vec<bool>,
    visited: &mut HashSet<&'a str>,
    out: &mut Vec<RosterRow<'a>>,
) {
    let total = siblings.len();
    for (idx, &entry) in siblings.iter().enumerate() {
        if !visited.insert(entry.id.as_str()) {
            continue;
        }
        let is_last = idx + 1 == total;
        let tree_prefix = if depth == 0 {
            String::new()
        } else {
            let mut prefix = String::new();
            for &cont in guides.iter().take(depth.saturating_sub(1)) {
                prefix.push_str(if cont { "│ " } else { "  " });
            }
            prefix.push_str(if is_last { "└─" } else { "├─" });
            prefix
        };
        out.push(RosterRow {
            entry,
            depth,
            tree_prefix,
        });
        if let Some(kids) = children.get(&Some(entry.id.as_str())) {
            if depth > 0 {
                guides.push(!is_last);
            }
            emit_tree_level(kids, children, depth + 1, guides, visited, out);
            if depth > 0 {
                guides.pop();
            }
        }
    }
}

/// True when `candidate_id` equals `root_id` or descends from `root_id` via `parent` links or
/// dotted `<parent>.<child>` naming.
#[must_use]
pub(crate) fn is_in_subtree(candidate_id: &str, root_id: &str, entries: &[AgentHubEntry]) -> bool {
    if candidate_id == root_id {
        return true;
    }
    if candidate_id
        .strip_prefix(root_id)
        .is_some_and(|rest| rest.starts_with('.'))
    {
        return true;
    }
    let parents: HashMap<&str, Option<&str>> = entries
        .iter()
        .map(|e| (e.id.as_str(), e.parent.as_deref()))
        .collect();
    let main_id = entries.iter().find(|e| e.is_main).map(|e| e.id.as_str());
    if main_id == Some(root_id) {
        return true;
    }
    let mut cur = candidate_id;
    let mut hops = 0usize;
    while hops < 32 {
        hops += 1;
        let Some(parent) = parents.get(cur).copied().flatten() else {
            return false;
        };
        if parent == root_id {
            return true;
        }
        cur = parent;
    }
    false
}

/// Filter activity log entries by kind (`f`), agent scope (`s`), and optional search query (`/`).
#[must_use]
pub(crate) fn filter_activity<'a>(
    items: &'a [AgentActivityItem],
    entries: &[AgentHubEntry],
    selected_id: Option<&str>,
    filter: ActivityFilter,
    scope: ActivityScope,
    search: &str,
) -> Vec<&'a AgentActivityItem> {
    let needle = search.trim().to_ascii_lowercase();
    items
        .iter()
        .filter(|item| filter.matches(item.kind))
        .filter(|item| match (scope, selected_id) {
            (ActivityScope::All, _) | (_, None) => true,
            (ActivityScope::Selected, Some(sel)) => item.agent_id == sel,
            (ActivityScope::Subtree, Some(sel)) => is_in_subtree(&item.agent_id, sel, entries),
        })
        .filter(|item| {
            needle.is_empty()
                || item.agent_id.to_ascii_lowercase().contains(&needle)
                || item.summary.to_ascii_lowercase().contains(&needle)
                || item
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.to_ascii_lowercase().contains(&needle))
        })
        .collect()
}

/// Format an agent's elapsed/active duration (`12s`, `3m05s`, `1h12m`).
#[must_use]
pub(crate) fn format_duration_ms(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Format USD cost from float dollars (`$0.004`, `$1.25`).
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "cost_usd is clamped non-negative and fits microdollars"
)]
pub(crate) fn format_cost_usd(usd: f64) -> String {
    let micros = (usd.max(0.0) * 1_000_000.0).round() as u64;
    render::money(micros)
}

/// Build the structured inspector detail lines for `entry` (pure and testable).
#[must_use]
pub(crate) fn format_inspector_lines(entry: &AgentHubEntry, now_ms: i64) -> Vec<String> {
    let mut lines = Vec::new();
    let end_ms = if entry.finished_ms > 0 {
        entry.finished_ms
    } else {
        now_ms
    };
    let age = format_duration_ms(end_ms.saturating_sub(entry.started_ms));
    lines.push(format!(
        "{}  [{}]  gen {}  ({age})",
        entry.id,
        entry.status.as_str(),
        entry.generation
    ));
    let model = entry.model.as_deref().unwrap_or("—");
    lines.push(format!("role: {}  ·  model: {model}", entry.role));
    let lineage = match &entry.parent {
        Some(p) => format!("{p} → {}", entry.id),
        None if entry.is_main => "root (main)".to_owned(),
        None => format!("main → {}", entry.id),
    };
    lines.push(format!("lineage: {lineage}"));
    if entry.unread > 0 {
        lines.push(format!("unread: {} message(s)", entry.unread));
    }
    if let Some(tool) = &entry.current_tool {
        let mut tool_line = format!("tool: {tool}");
        if let Some(started) = entry.current_tool_started_ms {
            let _ = write!(
                tool_line,
                " ({})",
                format_duration_ms(now_ms.saturating_sub(started))
            );
        }
        if let Some(args) = &entry.current_tool_args {
            let _ = write!(tool_line, "  {args}");
        }
        lines.push(tool_line);
    } else if !entry.activity.is_empty() {
        lines.push(format!("activity: {}", entry.activity));
    }
    if let Some(intent) = &entry.last_intent {
        lines.push(format!("intent: {intent}"));
    }
    if let Some(retry) = &entry.retry_state {
        lines.push(format!("retry: {retry}"));
    }
    match (entry.context_tokens, entry.context_window) {
        (Some(used), Some(win)) if win > 0 => {
            let pct = used.saturating_mul(100) / win.max(1);
            lines.push(format!(
                "context: {} / {} ({pct}%)",
                render::tokens(used),
                render::tokens(win)
            ));
        }
        (Some(used), _) if used > 0 => {
            lines.push(format!("context: {}", render::tokens(used)));
        }
        _ => lines.push("context: —".to_owned()),
    }
    let has_usage = entry.requests > 0
        || entry.tool_calls > 0
        || entry.input_tokens > 0
        || entry.output_tokens > 0
        || entry.cost_usd.is_some();
    if has_usage {
        let mut u = format!(
            "usage: {} req · {} tools · ↑{} ↓{}",
            entry.requests,
            entry.tool_calls,
            render::tokens(entry.input_tokens),
            render::tokens(entry.output_tokens),
        );
        if let Some(cost) = entry.cost_usd {
            let _ = write!(u, " · {}", format_cost_usd(cost));
        }
        lines.push(u);
    } else {
        lines.push("usage —".to_owned());
    }
    if let Some(path) = &entry.output_path {
        lines.push(format!("output: {path}"));
    }
    if let Some(path) = &entry.patch_path {
        lines.push(format!("patch: {path}"));
    }
    if let Some(branch) = &entry.isolated_branch {
        lines.push(format!("branch: {branch}"));
    }
    if !entry.task.trim().is_empty() {
        lines.push(format!("task: {}", entry.task));
    }
    lines
}
