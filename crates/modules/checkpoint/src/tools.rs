//! The `checkpoint` and `rewind` tools.

use crate::Shared;
use hrn_edge_tools::args::{blocking, opt_str, required_str};
use hrn_edge_tools::{Change, Kind};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, Result, RewindTo, SessionEffect, Tool, ToolApproval, ToolContext, ToolOutput,
    ToolSpec,
};
use std::fmt::Write as _;
use std::sync::Arc;

/// Changed files listed in a tool result before the rest are only counted.
const SHOWN_CHANGES: usize = 10;

/// The tool descriptions, as data.
const DESCRIPTION_CHECKPOINT: &str = include_str!("../prompts/checkpoint.md");
const DESCRIPTION_REWIND: &str = include_str!("../prompts/rewind.md");

/// Snapshot the working tree and mark the conversation.
pub(crate) struct CheckpointTool(pub(crate) Arc<Shared>);

impl Tool for CheckpointTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "checkpoint".to_owned(),
            description: DESCRIPTION_CHECKPOINT.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "What you are about to investigate or try." }
                },
                "required": ["goal"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::write()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let shared = Arc::clone(&self.0);
        let ctx = ctx.clone();
        Box::pin(async move {
            let goal = required_str(&args, "goal", "checkpoint")?.to_owned();
            blocking("checkpoint", move || {
                let _busy = shared.lock();
                let repo = shared.writer(&ctx.cwd)?;
                if repo.active(&ctx.session_id)?.is_some() {
                    return Ok(ToolOutput::err("Checkpoint already active."));
                }
                let id = repo.snapshot(
                    Kind::Explicit,
                    &goal,
                    Some(&ctx.session_id),
                    Some(&ctx.turn_id),
                )?;
                repo.set_active(&ctx.session_id, &id)?;
                let mut out = ToolOutput::ok(format!(
                    "Checkpoint {id}: {goal}\nFinish exploration and formulate findings, then \
                     call `rewind`."
                ));
                out.session_effect = Some(SessionEffect::Mark {
                    label: mark_label(&id),
                });
                Ok(out)
            })
            .await
        })
    }
}

/// The label of the conversation marker of checkpoint `id`.
fn mark_label(id: &str) -> String {
    format!("checkpoint:{id}")
}

/// Restore the snapshot and return the conversation to its mark.
pub(crate) struct RewindTool(pub(crate) Arc<Shared>);

impl Tool for RewindTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "rewind".to_owned(),
            description: DESCRIPTION_REWIND.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "report": { "type": "string", "description": "Your findings; the only thing kept of the rewound work." },
                    "checkpoint": { "type": "string", "description": "Snapshot id to return to (default: the active checkpoint)." }
                },
                "required": ["report"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::write()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let shared = Arc::clone(&self.0);
        let ctx = ctx.clone();
        Box::pin(async move {
            let report = required_str(&args, "report", "rewind")?.trim().to_owned();
            if report.is_empty() {
                return Ok(ToolOutput::err("Report cannot be empty."));
            }
            let wanted = opt_str(&args, "checkpoint").map(str::to_owned);
            blocking("rewind", move || {
                let _busy = shared.lock();
                let repo = shared.writer(&ctx.cwd)?;
                let Some(id) =
                    wanted.map_or_else(|| repo.active(&ctx.session_id), |w| Ok(Some(w)))?
                else {
                    return Ok(ToolOutput::err(
                        "No active checkpoint. Create a checkpoint before calling rewind.",
                    ));
                };
                let Some(snapshot) = repo.lookup(&id)? else {
                    return Ok(ToolOutput::err(format!(
                        "No checkpoint `{id}` for this directory (see `hrn checkpoints list`).",
                    )));
                };
                let to = match snapshot.kind {
                    Kind::Explicit => RewindTo::Mark(mark_label(&snapshot.id)),
                    Kind::Auto => match &snapshot.turn {
                        Some(turn) => RewindTo::TurnStart(turn.clone()),
                        None => {
                            return Ok(ToolOutput::err(format!(
                                "snapshot {} does not record its turn",
                                snapshot.id
                            )));
                        }
                    },
                    Kind::Restore => {
                        return Ok(ToolOutput::err(format!(
                            "snapshot {} only records the state a restore replaced; restore it \
                             with `hrn checkpoints restore {}` instead",
                            snapshot.id, snapshot.id
                        )));
                    }
                };
                let restored =
                    repo.restore(&snapshot.id, Some(&ctx.session_id), Some(&ctx.turn_id))?;
                repo.clear_active(&ctx.session_id)?;
                let summary = format!(
                    "Files restored to checkpoint {}: {}. Undo with `hrn checkpoints restore {}`.",
                    snapshot.id,
                    describe(&restored.changes),
                    restored.undo
                );
                let mut out = ToolOutput::ok(format!("Rewound. {summary}"));
                out.session_effect = Some(SessionEffect::Rewind {
                    to,
                    report: format!("{summary}\nFindings:\n{report}"),
                });
                Ok(out)
            })
            .await
        })
    }
}

/// `3 files changed (M a.txt, D b.txt, ...)`, or that nothing differed.
pub(crate) fn describe(changes: &[Change]) -> String {
    if changes.is_empty() {
        return "no file differed".to_owned();
    }
    let mut text = format!(
        "{} file{} changed (",
        changes.len(),
        if changes.len() == 1 { "" } else { "s" }
    );
    for (i, change) in changes.iter().take(SHOWN_CHANGES).enumerate() {
        if i > 0 {
            text.push_str(", ");
        }
        let _ = write!(text, "{} {}", change.status, change.path);
    }
    if changes.len() > SHOWN_CHANGES {
        let _ = write!(text, ", and {} more", changes.len() - SHOWN_CHANGES);
    }
    text.push(')');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_and_rewind_require_write_tier() {
        let shared = Arc::new(Shared::for_test());
        let cp = CheckpointTool(Arc::clone(&shared));
        let rw = RewindTool(shared);
        assert_eq!(
            cp.approval(&json!({"goal": "test"})).tier,
            hrn_kernel::ToolTier::Write
        );
        assert_eq!(
            rw.approval(&json!({"report": "done"})).tier,
            hrn_kernel::ToolTier::Write
        );
    }
}
