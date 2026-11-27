//! The todo ops: applying one mutation and parsing the model's payload.

use crate::dag::{DEFAULT_PHASE, TodoItem, TodoPhase, TodoStatus};
use crate::serde_json::Value;

/// One `todo` tool operation, applied to the session's phases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TodoOp {
    /// Replace the entire list (phased `list` or a flat `items` under one phase).
    Init {
        /// Phases with their tasks.
        list: Vec<TodoPhase>,
    },
    /// Mark one task `in_progress`, demoting any other active task.
    Start {
        /// Exact task content.
        task: String,
    },
    /// Mark the targeted task, phase, or every task `completed`.
    Done {
        /// Exact task content, when targeting one task.
        task: Option<String>,
        /// Exact phase name, when targeting a phase.
        phase: Option<String>,
    },
    /// Mark the targeted task, phase, or every task `abandoned`.
    Drop {
        /// Exact task content, when targeting one task.
        task: Option<String>,
        /// Exact phase name, when targeting a phase.
        phase: Option<String>,
    },
    /// Remove the targeted task, clear a phase's tasks, or clear all task lists.
    Rm {
        /// Exact task content, when targeting one task.
        task: Option<String>,
        /// Exact phase name, when targeting a phase.
        phase: Option<String>,
    },
    /// Append new pending tasks to a phase, creating it when missing.
    Append {
        /// Phase name; `None` selects the active or next task's phase.
        phase: Option<String>,
        /// Task contents.
        items: Vec<String>,
    },
    /// Echo the current list without changing it.
    View,
}

impl TodoOp {
    /// The operation's name, as results and logs label it.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Init { .. } => "init",
            Self::Start { .. } => "start",
            Self::Done { .. } => "done",
            Self::Drop { .. } => "drop",
            Self::Rm { .. } => "rm",
            Self::Append { .. } => "append",
            Self::View => "view",
        }
    }
}

/// A parsed `todo` tool payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoCall {
    /// The operation to apply.
    pub op: TodoOp,
    /// `true` when the model omitted `op` and it was inferred from the payload.
    pub inferred: bool,
}

/// Names of the phases, for "not found" hints.
fn phase_names(phases: &[TodoPhase]) -> String {
    phases
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Locate a task by exact content: `(phase index, task index)`.
fn find_task(phases: &[TodoPhase], task: &str) -> Option<(usize, usize)> {
    phases.iter().enumerate().find_map(|(pi, phase)| {
        phase
            .tasks
            .iter()
            .position(|t| t.content == task)
            .map(|ti| (pi, ti))
    })
}

fn not_found(phases: &[TodoPhase], task: &str) -> String {
    if phases.iter().all(|p| p.tasks.is_empty()) {
        return format!("Task \"{task}\" not found (the todo list is empty)");
    }
    let hint = if looks_like_id(task) {
        " Tasks are referenced by their exact content, not by an id."
    } else {
        ""
    };
    format!("Task \"{task}\" not found.{hint}")
}

fn looks_like_id(task: &str) -> bool {
    task.strip_prefix("task-")
        .or_else(|| task.strip_prefix("Task-"))
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// Apply `op` to a clone of `phases`, returning the working copy and op-specific errors. Any
/// error means the caller discards the mutation and keeps the previous list.
#[must_use]
pub fn apply(phases: &[TodoPhase], op: &TodoOp) -> (Vec<TodoPhase>, Vec<String>) {
    let mut work = phases.to_vec();
    let mut errors: Vec<String> = Vec::new();
    match op {
        TodoOp::Init { list } => work.clone_from(list),
        TodoOp::Start { task } => start(&mut work, phases, task, &mut errors),
        TodoOp::Done { task, phase } | TodoOp::Drop { task, phase } => {
            let to = match op {
                TodoOp::Drop { .. } => TodoStatus::Abandoned,
                _ => TodoStatus::Completed,
            };
            let hits = targets(&work, task.as_deref(), phase.as_deref(), &mut errors);
            for (pi, ti) in hits {
                if let Some(item) = work.get_mut(pi).and_then(|p| p.tasks.get_mut(ti)) {
                    item.status = to;
                }
            }
        }
        TodoOp::Rm { task, phase } => remove(
            &mut work,
            phases,
            task.as_deref(),
            phase.as_deref(),
            &mut errors,
        ),
        TodoOp::Append { phase, items } => append(&mut work, phase.as_deref(), items, &mut errors),
        TodoOp::View => {}
    }
    if errors.is_empty() {
        normalize(&mut work);
    }
    (work, errors)
}

/// `start`: one task becomes the single active one; an unknown content is an error.
fn start(work: &mut [TodoPhase], phases: &[TodoPhase], task: &str, errors: &mut Vec<String>) {
    match find_task(work, task) {
        Some((pi, ti)) => {
            demote_active(work);
            if let Some(item) = work.get_mut(pi).and_then(|p| p.tasks.get_mut(ti)) {
                item.status = TodoStatus::InProgress;
            }
        }
        None => errors.push(not_found(phases, task)),
    }
}

fn demote_active(work: &mut [TodoPhase]) {
    for phase in work {
        for t in &mut phase.tasks {
            if t.status == TodoStatus::InProgress {
                t.status = TodoStatus::Pending;
            }
        }
    }
}

/// `rm`: one task by content, a phase's tasks by name, or every task list.
fn remove(
    work: &mut Vec<TodoPhase>,
    phases: &[TodoPhase],
    task: Option<&str>,
    phase: Option<&str>,
    errors: &mut Vec<String>,
) {
    if let Some(content) = task {
        match find_task(work, content) {
            Some((pi, ti)) => {
                if let Some(p) = work.get_mut(pi) {
                    p.tasks.remove(ti);
                }
            }
            None => errors.push(not_found(phases, content)),
        }
        return;
    }
    let mut cleared = false;
    for p in work {
        if phase.is_none_or(|name| p.name == name) {
            p.tasks.clear();
            cleared = true;
        }
    }
    if !cleared {
        errors.push(format!(
            "Phase \"{}\" not found (phases: {})",
            phase.unwrap_or_default(),
            phase_names(phases)
        ));
    }
}

/// `append`: new pending tasks join the named phase, the active/next task's phase, or `Tasks`.
fn append(
    work: &mut Vec<TodoPhase>,
    phase: Option<&str>,
    items: &[String],
    errors: &mut Vec<String>,
) {
    if phase.is_some_and(str::is_empty) {
        errors.push("Missing phase name for append operation".to_owned());
    }
    let name = phase.map_or_else(
        || {
            work.iter()
                .find_map(|p| {
                    p.tasks
                        .iter()
                        .any(|t| matches!(t.status, TodoStatus::InProgress | TodoStatus::Pending))
                        .then(|| p.name.clone())
                })
                .or_else(|| work.last().map(|p| p.name.clone()))
                .unwrap_or_else(|| DEFAULT_PHASE.to_owned())
        },
        str::to_owned,
    );
    let mut added = Vec::new();
    for item in items {
        if work
            .iter()
            .any(|p| p.tasks.iter().any(|t| &t.content == item))
        {
            errors.push(format!("Task \"{item}\" already exists"));
        } else if !added.iter().any(|a: &String| a == item) {
            added.push(item.clone());
        }
    }
    if !errors.is_empty() {
        return;
    }
    let fresh: Vec<TodoItem> = added
        .into_iter()
        .map(|content| TodoItem {
            content,
            status: TodoStatus::Pending,
        })
        .collect();
    match work.iter_mut().find(|p| p.name == name) {
        Some(p) => p.tasks.extend(fresh),
        None => work.push(TodoPhase { name, tasks: fresh }),
    }
}

/// Resolve a `done`/`drop`/`rm` target to `(phase, task)` index pairs.
fn targets(
    work: &[TodoPhase],
    task: Option<&str>,
    phase: Option<&str>,
    errors: &mut Vec<String>,
) -> Vec<(usize, usize)> {
    if let Some(content) = task {
        if let Some(at) = find_task(work, content) {
            return vec![at];
        }
        errors.push(not_found(work, content));
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (pi, p) in work.iter().enumerate() {
        if phase.is_none_or(|name| p.name == name) {
            hits.extend((0..p.tasks.len()).map(|ti| (pi, ti)));
        }
    }
    if hits.is_empty()
        && let Some(name) = phase
    {
        errors.push(format!(
            "Phase \"{name}\" not found (phases: {})",
            phase_names(work)
        ));
    }
    hits
}

/// Enforce the single-active-task rule: the first `in_progress` stays, the rest go back to
/// `pending`, and with none active anywhere the first pending task is promoted.
pub fn normalize(phases: &mut [TodoPhase]) {
    let mut active = false;
    for phase in phases.iter_mut() {
        for task in &mut phase.tasks {
            if task.status == TodoStatus::InProgress {
                if active {
                    task.status = TodoStatus::Pending;
                } else {
                    active = true;
                }
            }
        }
    }
    if active {
        return;
    }
    for phase in phases.iter_mut() {
        if let Some(task) = phase
            .tasks
            .iter_mut()
            .find(|t| t.status == TodoStatus::Pending)
        {
            task.status = TodoStatus::InProgress;
            return;
        }
    }
}

/// Parse the single-op `todo` tool payload. An omitted `op` is inferred only for the
/// unambiguous `list`/`items` shapes omp accepts.
///
/// # Errors
/// Human-readable validation text when the payload cannot name an operation.
pub fn parse_call(args: &Value) -> Result<TodoCall, String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);
    let strings = |key: &str| -> Option<Vec<String>> {
        args.get(key).and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
    };
    let list = args.get("list");
    let items = strings("items");
    let phase = text("phase");
    let task = text("task");
    let mut inferred = false;
    let op = match text("op").as_deref() {
        Some("init") => init_op(list, items, phase)?,
        Some("start") => TodoOp::Start {
            task: task.ok_or_else(|| "Missing task content".to_owned())?,
        },
        Some("done") => TodoOp::Done { task, phase },
        Some("drop") => TodoOp::Drop { task, phase },
        Some("rm") => TodoOp::Rm { task, phase },
        Some("append") => TodoOp::Append {
            phase,
            items: non_empty(items, "Missing items for append operation")?,
        },
        Some("view") => TodoOp::View,
        Some(other) => {
            return Err(format!(
                "unknown todo op `{other}` (init, start, done, rm, drop, append, view)"
            ));
        }
        None => {
            inferred = true;
            let has_list = list
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
            let has_items = items.as_ref().is_some_and(|i| !i.is_empty());
            match (has_list, has_items) {
                (true, _) => init_op(list, None, phase)?,
                (false, true) if phase.is_some() => TodoOp::Append {
                    phase,
                    items: non_empty(items, "Missing items for append operation")?,
                },
                (false, true) => init_op(None, items, phase)?,
                _ => {
                    return Err(
                        "`op` is required unless `list` or `items` names an operation".to_owned(),
                    );
                }
            }
        }
    };
    Ok(TodoCall { op, inferred })
}

fn non_empty(items: Option<Vec<String>>, missing: &str) -> Result<Vec<String>, String> {
    match items {
        Some(items) if !items.is_empty() => Ok(items),
        _ => Err(missing.to_owned()),
    }
}

fn init_op(
    list: Option<&Value>,
    items: Option<Vec<String>>,
    phase: Option<String>,
) -> Result<TodoOp, String> {
    if let Some(raw) = list.and_then(Value::as_array) {
        return Ok(TodoOp::Init {
            list: parse_phases(raw)?,
        });
    }
    let items = items.ok_or_else(|| "Missing list for init operation".to_owned())?;
    if items.is_empty() {
        return Ok(TodoOp::Init { list: Vec::new() });
    }
    Ok(TodoOp::Init {
        list: vec![named_phase(phase, items)],
    })
}

fn named_phase(phase: Option<String>, items: Vec<String>) -> TodoPhase {
    TodoPhase {
        name: phase
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| DEFAULT_PHASE.to_owned()),
        tasks: items
            .into_iter()
            .map(|content| TodoItem {
                content,
                status: TodoStatus::Pending,
            })
            .collect(),
    }
}

fn parse_phases(list: &[Value]) -> Result<Vec<TodoPhase>, String> {
    let mut phases: Vec<TodoPhase> = Vec::with_capacity(list.len());
    for entry in list {
        let phase = named_phase(
            entry
                .get("phase")
                .and_then(Value::as_str)
                .map(str::to_owned),
            entry
                .get("items")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
                .ok_or_else(|| "each init phase needs a non-empty `items` array".to_owned())?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| "init items must be non-empty strings".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        if phases.iter().any(|p: &TodoPhase| p.name == phase.name) {
            return Err(format!("Duplicate phase \"{}\" in init list", phase.name));
        }
        let mut seen: Vec<&str> = Vec::new();
        for task in &phase.tasks {
            if seen.contains(&task.content.as_str()) {
                return Err(format!("Duplicate task \"{}\" in init list", task.content));
            }
            seen.push(&task.content);
        }
        phases.push(phase);
    }
    Ok(phases)
}
