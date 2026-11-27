//! Rendering the checklist: the model-facing summary, the `/todo` markdown, and the flat list
//! compaction prompts quote.

use crate::dag::{DEFAULT_PHASE, TodoItem, TodoPhase, TodoStatus};
use crate::serde_json::Value;
use crate::todo::normalize;
use std::fmt::Write;

/// The model-facing result text: counts, the active task, then the per-phase tree.
#[must_use]
pub fn summary(phases: &[TodoPhase]) -> String {
    if phases.iter().all(|p| p.tasks.is_empty()) {
        return "Todo list cleared.".to_owned();
    }
    let total: usize = phases.iter().map(|p| p.tasks.len()).sum();
    let closed: usize = phases
        .iter()
        .flat_map(|p| &p.tasks)
        .filter(|t| t.status.closed())
        .count();
    let active = phases
        .iter()
        .flat_map(|p| &p.tasks)
        .find(|t| t.status == TodoStatus::InProgress)
        .map(|t| t.content.clone());
    let mut out = String::new();
    let _ = write!(out, "Todo progress: {closed}/{total} closed");
    if let Some(task) = &active {
        let _ = write!(out, " · active: {task}");
    }
    let _ = writeln!(out);
    let multi = phases.len() > 1;
    for phase in phases {
        if phase.tasks.is_empty() {
            continue;
        }
        if multi {
            let done = phase.tasks.iter().filter(|t| t.status.closed()).count();
            let _ = writeln!(out, "[{}] {done}/{}", phase.name, phase.tasks.len());
        }
        for task in &phase.tasks {
            let _ = writeln!(out, "  {} {}", task.status.marker(), task.content);
        }
    }
    out.trim_end().to_owned()
}

/// Serialize the checklist as markdown headings plus a checklist (`[ ]`, `[/]`, `[x]`, `[-]`).
#[must_use]
pub fn to_markdown(phases: &[TodoPhase]) -> String {
    let mut out = String::new();
    for phase in phases {
        let _ = writeln!(out, "# {}", phase.name);
        for task in &phase.tasks {
            let mark = match task.status {
                TodoStatus::Pending => " ",
                TodoStatus::InProgress => "/",
                TodoStatus::Completed => "x",
                TodoStatus::Abandoned => "-",
            };
            let _ = writeln!(out, "- [{mark}] {}", task.content);
        }
    }
    out
}

/// Parse the markdown [`to_markdown`] writes; orphan tasks fall into `DEFAULT_PHASE`.
#[must_use]
pub fn from_markdown(text: &str) -> Vec<TodoPhase> {
    let mut phases: Vec<TodoPhase> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(head) = line.strip_prefix("# ").map(str::trim) {
            if !head.is_empty() && !phases.iter().any(|p| p.name == head) {
                phases.push(TodoPhase {
                    name: head.to_owned(),
                    tasks: Vec::new(),
                });
            }
            continue;
        }
        let Some((mark, content)) = line
            .strip_prefix("- [")
            .or_else(|| line.strip_prefix("* ["))
            .and_then(|r| r.split_once(']'))
        else {
            continue;
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        let status = match mark.trim() {
            "x" | "X" => TodoStatus::Completed,
            "/" => TodoStatus::InProgress,
            "-" | "~" => TodoStatus::Abandoned,
            _ => TodoStatus::Pending,
        };
        if phases.is_empty() {
            phases.push(TodoPhase {
                name: DEFAULT_PHASE.to_owned(),
                tasks: Vec::new(),
            });
        }
        if let Some(phase) = phases.last_mut() {
            phase.tasks.push(TodoItem {
                content: content.to_owned(),
                status,
            });
        }
    }
    normalize(&mut phases);
    phases
}

/// The flat task list, in phase order (the shape compaction prompts quote).
#[must_use]
pub fn flatten(phases: &[TodoPhase]) -> Vec<TodoItem> {
    phases.iter().flat_map(|p| p.tasks.clone()).collect()
}

fn item_from_value(entry: &Value) -> Option<TodoItem> {
    let content = entry.get("content")?.as_str()?.trim();
    if content.is_empty() {
        return None;
    }
    // A present status must name one: a malformed list falls back to the previous call, the
    // way the strict legacy parser did.
    let status = match entry.get("status") {
        Some(value) => TodoStatus::parse(value.as_str()?)?,
        None => TodoStatus::Pending,
    };
    Some(TodoItem {
        content: content.to_owned(),
        status,
    })
}

/// The phase a flat task array lands in (legacy `items` payloads). Strict: one malformed
/// entry fails the whole list, so the caller falls back to the previous well-formed call.
#[must_use]
pub fn phase_from_items(items: &Value) -> Option<TodoPhase> {
    let tasks = items
        .as_array()?
        .iter()
        .map(item_from_value)
        .collect::<Option<Vec<_>>>()?;
    Some(TodoPhase {
        name: DEFAULT_PHASE.to_owned(),
        tasks,
    })
}
