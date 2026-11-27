//! Tier 3 helpers: choosing the summary cut, extracting facts from the branch (todos, modified
//! files) and building the summarizer prompt.

use crate::compaction::{active_start, estimate_msg_tokens};
use hrn_kernel::{DagNode, DagPayload, Msg, NodeId, Role, TodoItem, TodoPhase};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Upper bound for the transcript handed to the summarizer model.
const MAX_TRANSCRIPT_BYTES: usize = 240 * 1024;

/// Embedded prompt template for `/handoff [focus]`.
pub const HANDOFF_TEMPLATE: &str = include_str!("../prompts/handoff.md");

/// Registered compaction methods in preference/discovery order.
pub const COMPACTION_METHODS: &[&str] = &["handoff", "shake", "summary", "remote", "snapcompact"];

/// Where a Tier 3 summary may cut the branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryPlan {
    /// Index (into the branch) of the last node folded into the summary.
    pub covers_idx: usize,
    /// Id of that node (stored as `covers_node_id` on the `Summary` payload).
    pub covers_node_id: NodeId,
}

/// Choose the Tier 3 cut: everything before the last `keep_recent_turns` user turns is summarized.
///
/// A cut is only ever placed right before a `User` or `Assistant` message, so tool-use/tool-result
/// pairs are never split. When the session is one long agentic turn (too few user messages), the
/// tail is the last `2 * keep_recent_turns` assistant steps instead. `None` means there is not
/// enough history to fold.
pub fn plan_summary(nodes: &[DagNode], keep_recent_turns: usize) -> Option<SummaryPlan> {
    let keep = keep_recent_turns.max(1);
    let start = active_start(nodes);
    let region = nodes.get(start..)?;
    let bounds = |role: Role| -> Vec<usize> {
        region
            .iter()
            .enumerate()
            .filter(
                |(_, n)| matches!(&n.payload, DagPayload::Message { msg, .. } if msg.role == role),
            )
            .map(|(i, _)| start + i)
            .collect()
    };
    let users = bounds(Role::User);
    let tail_start = if users.len() > keep {
        users.get(users.len() - keep).copied()?
    } else {
        let assistants = bounds(Role::Assistant);
        let keep_steps = keep * 2;
        if assistants.len() > keep_steps {
            assistants.get(assistants.len() - keep_steps).copied()?
        } else {
            return None;
        }
    };
    let covers_idx = tail_start.checked_sub(1)?;
    // Something must actually be folded: at least one message before the cut.
    let folds_a_message = nodes
        .get(start..=covers_idx)?
        .iter()
        .any(|n| matches!(n.payload, DagPayload::Message { .. }));
    if !folds_a_message {
        return None;
    }
    Some(SummaryPlan {
        covers_idx,
        covers_node_id: nodes.get(covers_idx)?.id.clone(),
    })
}

/// Choose a summary cut respecting the token budget `keep_recent_tokens`.
///
/// Walks backward across valid turn boundaries (`Role::User`, or `Role::Assistant` for single-turn
/// agentic sessions) so tool call/result pairs are never split. When the entire active branch fits
/// within `keep_recent_tokens` and `allow_turn_fallback` is true, falls back to keeping at most
/// `fallback_keep_turns` turns (or the latest turn when at least two boundaries exist).
#[must_use]
pub fn plan_summary_for_tokens(
    nodes: &[DagNode],
    keep_recent_tokens: u64,
    fallback_keep_turns: usize,
    allow_turn_fallback: bool,
) -> Option<SummaryPlan> {
    let start = active_start(nodes);
    let region = nodes.get(start..)?;
    let bounds = |role: Role| -> Vec<usize> {
        region
            .iter()
            .enumerate()
            .filter(
                |(_, n)| matches!(&n.payload, DagPayload::Message { msg, .. } if msg.role == role),
            )
            .map(|(i, _)| start + i)
            .collect()
    };
    let users = bounds(Role::User);
    let boundaries = if users.len() >= 2 {
        users
    } else {
        bounds(Role::Assistant)
    };
    if boundaries.len() < 2 {
        return None;
    }

    let segment_tokens = |from: usize, to: usize| -> u64 {
        nodes
            .get(from..to)
            .unwrap_or_default()
            .iter()
            .filter_map(|n| match &n.payload {
                DagPayload::Message { msg, .. } => Some(estimate_msg_tokens(msg)),
                _ => None,
            })
            .sum()
    };

    let last_idx = boundaries.len() - 1;
    let mut tail_start = *boundaries.get(last_idx)?;
    let mut kept_tokens = segment_tokens(tail_start, nodes.len());
    let mut cut_at_start = false;
    for pos in (0..last_idx).rev() {
        let seg_start = *boundaries.get(pos)?;
        let seg_tok = segment_tokens(seg_start, tail_start);
        if kept_tokens.saturating_add(seg_tok) <= keep_recent_tokens {
            kept_tokens = kept_tokens.saturating_add(seg_tok);
            tail_start = seg_start;
            if pos == 0 {
                cut_at_start = true;
            }
        } else {
            break;
        }
    }

    if cut_at_start || tail_start <= start {
        if !allow_turn_fallback {
            return None;
        }
        if let Some(plan) = plan_summary(nodes, fallback_keep_turns) {
            return Some(plan);
        }
        tail_start = *boundaries.get(last_idx)?;
    }

    let covers_idx = tail_start.checked_sub(1)?;
    let folds_a_message = nodes
        .get(start..=covers_idx)?
        .iter()
        .any(|n| matches!(n.payload, DagPayload::Message { .. }));
    if !folds_a_message {
        return None;
    }
    Some(SummaryPlan {
        covers_idx,
        covers_node_id: nodes.get(covers_idx)?.id.clone(),
    })
}

fn tool_calls_named<'a>(
    nodes: &'a [DagNode],
    names: &'a [&str],
) -> impl DoubleEndedIterator<Item = (&'a str, Value)> + 'a {
    nodes.iter().flat_map(move |n| {
        let calls: &[hrn_kernel::ToolCall] = match &n.payload {
            DagPayload::Message { msg, .. } if msg.role == Role::Assistant => &msg.tool_calls,
            _ => &[],
        };
        calls
            .iter()
            .filter(move |c| names.contains(&c.name.as_str()))
            .filter_map(|c| {
                serde_json::from_str::<Value>(&c.arguments)
                    .ok()
                    .map(|v| (c.name.as_str(), v))
            })
    })
}

/// The checklist as of the newest `todo` board on this branch; sessions from before the board
/// node fall back to the last well-formed `todo` call's payload.
pub fn extract_todos(nodes: &[DagNode]) -> Vec<TodoItem> {
    if let Some(phases) = latest_todo_board(nodes) {
        return hrn_kernel::todo::flatten(&phases);
    }
    tool_calls_named(nodes, &["todo"])
        .rev()
        .find_map(|(_, args)| {
            args.get("items")
                .and_then(hrn_kernel::todo::phase_from_items)
                .map(|phase| phase.tasks)
        })
        .unwrap_or_default()
}

/// The phase list of the newest `TodoBoard` node, if any.
pub fn latest_todo_board(nodes: &[DagNode]) -> Option<Vec<TodoPhase>> {
    nodes.iter().rev().find_map(|n| match &n.payload {
        DagPayload::TodoBoard { phases } => Some(phases.clone()),
        _ => None,
    })
}

/// Paths written by `edit` / `write` / `apply_patch` calls on this branch, sorted.
pub fn extract_modified_files(nodes: &[DagNode]) -> Vec<String> {
    let mut files = BTreeSet::new();
    for (name, args) in tool_calls_named(nodes, &["edit", "write", "apply_patch"]) {
        // `edit` is hashline (`input` holds `[PATH#TAG]` sections) or replace mode (`path`).
        if let Some(input) = args
            .get("input")
            .and_then(Value::as_str)
            .filter(|_| name == "edit")
        {
            files.extend(crate::scope::hashline_paths(input));
        } else if name == "apply_patch" {
            let patch = args
                .get("patch")
                .and_then(Value::as_str)
                .unwrap_or_default();
            for line in patch.lines() {
                for marker in [
                    "*** Add File:",
                    "*** Update File:",
                    "*** Delete File:",
                    "*** Move to:",
                ] {
                    if let Some(path) = line.trim().strip_prefix(marker) {
                        files.insert(path.trim().to_owned());
                    }
                }
            }
        } else if let Some(path) = args.get("path").and_then(Value::as_str) {
            files.insert(path.to_owned());
        }
    }
    files.into_iter().collect()
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let cut = text.floor_char_boundary(max);
    format!("{}...", text.get(..cut).unwrap_or(""))
}

/// Build the summarization prompt for Tier 3 compaction.
pub fn build_summary_prompt(messages: &[Msg], todos: &[TodoItem], files: &[String]) -> Vec<Msg> {
    build_summary_prompt_with_instructions(messages, todos, files, None)
}

/// Build the summarization prompt for Tier 3 compaction, appending optional custom instructions
/// from `/compact [instructions]`.
#[must_use]
pub fn build_summary_prompt_with_instructions(
    messages: &[Msg],
    todos: &[TodoItem],
    files: &[String],
    instructions: Option<&str>,
) -> Vec<Msg> {
    let transcript = format_transcript(
        messages,
        todos,
        files,
        instructions,
        "[CUSTOM INSTRUCTIONS]",
    );
    vec![
        Msg::system(
            "Summarize this coding session concisely for seamless continuation. Preserve: (1) user goal & constraints, (2) files modified/read and key findings, (3) decisions made, (4) active todos & exact next step. Keep under 600 words.",
        ),
        Msg::user(transcript),
    ]
}

/// Build the `/handoff [focus]` prompt using [`HANDOFF_TEMPLATE`].
pub fn build_handoff_prompt(
    messages: &[Msg],
    todos: &[TodoItem],
    files: &[String],
    focus: Option<&str>,
) -> Vec<Msg> {
    let transcript = format_transcript(messages, todos, files, focus, "[FOCUS]");
    vec![Msg::system(HANDOFF_TEMPLATE.trim()), Msg::user(transcript)]
}

/// True when `text` contains a markdown `## Goal` heading (required for manual `/handoff`).
#[must_use]
pub fn has_goal_section(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("##") else {
            return false;
        };
        let heading = rest.trim_start();
        if heading.len() < 4 || !heading[..4].eq_ignore_ascii_case("goal") {
            return false;
        }
        heading
            .as_bytes()
            .get(4)
            .is_none_or(|b| !b.is_ascii_alphanumeric())
    })
}

fn format_transcript(
    messages: &[Msg],
    todos: &[TodoItem],
    files: &[String],
    extra: Option<&str>,
    extra_heading: &str,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    for msg in messages {
        let (role, limit) = match msg.role {
            Role::System => ("SYSTEM", 1200),
            Role::User => ("USER", 2400),
            Role::Assistant => ("ASSISTANT", 1800),
            Role::Tool => ("TOOL", 700),
        };
        let mut line = String::new();
        if !msg.content.is_empty() || msg.tool_calls.is_empty() {
            let _ = writeln!(line, "[{role}]: {}", clip(&msg.content, limit));
        }
        for call in &msg.tool_calls {
            let _ = writeln!(
                line,
                "[ASSISTANT CALLED]: {}({})",
                call.name,
                clip(&call.arguments, 300)
            );
        }
        lines.push(line);
    }
    // Keep the newest part when the transcript is too big; a leading earlier summary is pinned.
    let pinned = usize::from(messages.first().is_some_and(|m| {
        m.content.starts_with("[Conversation summary")
            || m.content.starts_with("[Handoff from earlier turns]")
    }));
    let mut total: usize = lines.iter().map(String::len).sum();
    let mut drop_from = pinned;
    let mut omitted = 0usize;
    while total > MAX_TRANSCRIPT_BYTES && drop_from + 1 < lines.len() {
        total -= lines.get(drop_from).map_or(0, String::len);
        if let Some(l) = lines.get_mut(drop_from) {
            l.clear();
        }
        drop_from += 1;
        omitted += 1;
    }
    let mut transcript = String::with_capacity(total + 256);
    for (i, line) in lines.iter().enumerate() {
        if i == pinned && omitted > 0 {
            let _ = writeln!(transcript, "[... {omitted} earlier messages omitted ...]");
        }
        transcript.push_str(line);
    }
    if !files.is_empty() {
        transcript.push_str("\n[FILES MODIFIED]:\n");
        for f in files {
            let _ = writeln!(transcript, "- {f}");
        }
    }
    if !todos.is_empty() {
        transcript.push_str("\n[ACTIVE TODOS]:\n");
        for t in todos {
            let _ = writeln!(transcript, "- ({}) {}", t.status.as_str(), t.content);
        }
    }
    if let Some(extra) = extra.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = write!(transcript, "\n{extra_heading}:\n{extra}\n");
    }
    transcript
}
