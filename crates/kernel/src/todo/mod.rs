//! The session todo checklist: the single-mutation ops the `todo` tool applies and their
//! rendering. The same code runs in the tool (model-driven edits) and the front end (`/todo`
//! edits), so both agree on phases, the active-task rule and the summary text.

pub mod boards;
pub mod ops;
pub mod render;

pub use boards::TodoBoards;
pub use ops::{TodoCall, TodoOp, apply, normalize, parse_call};
pub use render::{flatten, from_markdown, phase_from_items, summary, to_markdown};
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{TodoItem, TodoPhase, TodoStatus};

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_owned(),
            status,
        }
    }

    fn phases() -> Vec<TodoPhase> {
        vec![
            TodoPhase {
                name: "Plan".to_owned(),
                tasks: vec![item("design", TodoStatus::Completed)],
            },
            TodoPhase {
                name: "Build".to_owned(),
                tasks: vec![
                    item("write code", TodoStatus::InProgress),
                    item("test it", TodoStatus::Pending),
                ],
            },
        ]
    }

    #[test]
    fn init_accepts_phases_and_inference_covers_the_flat_forms() {
        let args = serde_json::json!({"op":"init","list":[{"phase":"Ship","items":["a","b"]}]});
        let call = parse_call(&args).expect("parsed");
        assert_eq!(call.op.name(), "init");
        // Omitted op with items + a known phase infers append; bare items init under "Tasks".
        let appended =
            parse_call(&serde_json::json!({"items":["x"],"phase":"Build"})).expect("inferred");
        assert_eq!(appended.op.name(), "append");
        assert!(appended.inferred);
        let inited = parse_call(&serde_json::json!({"items":["x"]})).expect("inferred");
        assert_eq!(inited.op.name(), "init");
        assert!(parse_call(&serde_json::json!({})).is_err());
    }

    #[test]
    fn start_moves_the_pointer_and_done_reports_progress() {
        let (next, errors) = apply(
            &phases(),
            &TodoOp::Start {
                task: "test it".into(),
            },
        );
        assert!(errors.is_empty());
        let active: Vec<&str> = next
            .iter()
            .flat_map(|p| &p.tasks)
            .filter(|t| t.status == TodoStatus::InProgress)
            .map(|t| t.content.as_str())
            .collect();
        assert_eq!(active, ["test it"]);
        let (done, _) = apply(
            &next,
            &TodoOp::Done {
                task: None,
                phase: Some("Build".into()),
            },
        );
        let text = summary(&done);
        assert!(
            text.contains("3/3 closed") && text.contains("[Build] 2/2"),
            "{text}"
        );
    }

    #[test]
    fn unknown_task_errors_keep_the_list() {
        let (next, errors) = apply(
            &phases(),
            &TodoOp::Start {
                task: "task-1".to_owned(),
            },
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("not by an id"), "{errors:?}");
        assert_eq!(next, phases());
    }

    #[test]
    fn append_defaults_to_the_active_phase_and_rejects_duplicates() {
        let (next, errors) = apply(
            &phases(),
            &TodoOp::Append {
                phase: None,
                items: vec!["ship it".into()],
            },
        );
        assert!(errors.is_empty());
        assert_eq!(next[1].tasks.last().unwrap().content, "ship it");
        assert_eq!(next[1].tasks.last().unwrap().status, TodoStatus::Pending);
        let (dup, errors) = apply(
            &phases(),
            &TodoOp::Append {
                phase: Some("Build".into()),
                items: vec!["test it".into()],
            },
        );
        assert_eq!(dup, phases());
        assert!(errors[0].contains("already exists"));
    }

    #[test]
    fn markdown_round_trips_phases_and_statuses() {
        let md = to_markdown(&phases());
        let back = from_markdown(&md);
        assert_eq!(back, phases());
    }
}
