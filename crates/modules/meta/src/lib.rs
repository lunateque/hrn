//! `meta`: coordination tools, `ask` (an HTML form for the user) and `todo` (task checklist).

use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{
    AgentEvent, AskRequest, BoxFuture, Form, HrnError, Registry, Result, SessionEffect, Tool,
    ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::sync::Arc;

/// The system-prompt line that tells the model when to reach for `ask`.
const ASK_PROMPT: &str =
    "If blocked or uncertain about a design decision, ask concisely using the `ask` tool.";

/// The `ask` and `todo` descriptions, as data.
const ASK_DESCRIPTION: &str = include_str!("../prompts/ask.md");
const TODO_DESCRIPTION: &str = include_str!("../prompts/todo.md");

/// Show the user one HTML form through the front-end's `AskHandler`.
struct AskTool;

impl Tool for AskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask".to_owned(),
            description: ASK_DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "html": {
                        "type": "string",
                        "description": "One HTML <form>; content outside the first <form> is ignored. Controls: radio/checkbox (share a `name`), select, text-like inputs, textarea, hidden; `checked`/`selected`/`value` preselect, `required`/`min`/`max`/`pattern` are enforced. Radio and checkbox groups of 2+ options get a free-text \"Other\" answer automatically - do not add your own. Answers: radio/select a string, a checkbox group or multi-select an array, a lone checkbox its value when checked, a number input a number."
                    }
                },
                "required": ["html"],
                "additionalProperties": false
            }),
        }
    }

    // A barrier: two prompts must never be on screen at once.
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let ctx = ctx.clone();
        Box::pin(async move {
            let html = args
                .get("html")
                .and_then(Value::as_str)
                .ok_or_else(|| HrnError::Tool("ask: `html` must be a string".into()))?;
            let form = match Form::parse(html) {
                Ok(form) => form,
                Err(why) => return Ok(ToolOutput::err(format!("ask: {why}"))),
            };
            if !form.is_interactive() {
                let values = form.values(&form.defaults());
                return Ok(ToolOutput::ok(answers_text(&values, false)?));
            }
            let Some(handler) = ctx.ask.clone() else {
                return Ok(ToolOutput::err(
                    "`ask` is unavailable in this session (no interactive user); decide with \
                     your best documented assumption",
                ));
            };
            let request = AskRequest {
                id: ctx.call_id.clone(),
                session_id: ctx.session_id.clone(),
                form,
            };
            let submission = handler.ask(request, ctx.cancel.clone()).await?;
            Ok(ToolOutput::ok(answers_text(
                &submission.values,
                submission.timed_out,
            )?))
        })
    }
}

fn answers_text(values: &Value, timed_out: bool) -> Result<String> {
    let head = if timed_out {
        "No answer before the timeout; submitted the form's current values:"
    } else {
        "User answers:"
    };
    Ok(format!("{head}\n{}", serde_json::to_string_pretty(values)?))
}

/// The session's task checklist: one mutation per call, applied to the live board.
struct TodoTool;

impl Tool for TodoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo".to_owned(),
            description: TODO_DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["init", "start", "done", "rm", "drop", "append", "view"]
                    },
                    "list": {
                        "type": "array",
                        "description": "init: the full replacement, one entry per phase.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "phase": { "type": "string" },
                                "items": { "type": "array", "items": { "type": "string" }, "minItems": 1 }
                            },
                            "required": ["items"]
                        }
                    },
                    "items": {
                        "type": "array",
                        "description": "append: task contents; or a flat init payload.",
                        "items": { "type": "string" }
                    },
                    "phase": {
                        "type": "string",
                        "description": "Exact phase name (init flat form names it; append \
                                        creates it; done/drop/rm target it)."
                    },
                    "task": {
                        "type": "string",
                        "description": "Exact task content (start; done/drop/rm one task)."
                    }
                },
                "required": ["op"],
                "additionalProperties": false
            }),
        }
    }

    // A barrier, so concurrent `todo` calls cannot race to publish different lists.
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let events = ctx.events.clone();
        let boards = Arc::clone(&ctx.todos);
        let root = ctx.root_session_id.clone();
        Box::pin(async move {
            let call = match hrn_kernel::todo::parse_call(&args) {
                Ok(call) => call,
                Err(why) => {
                    return Ok(ToolOutput::err(format!("Invalid todo arguments: {why}")));
                }
            };
            let before = boards.phases(&root);
            let (updated, errors) = hrn_kernel::todo::apply(&before, &call.op);
            let mut text =
                hrn_kernel::todo::summary(if errors.is_empty() { &updated } else { &before });
            if !errors.is_empty() {
                text = format!("Errors: {}\n{text}", errors.join("; "));
            }
            let mut out = ToolOutput::ok(text);
            out.is_error = !errors.is_empty();
            if call.op.name() != "view" && errors.is_empty() {
                boards.set(&root, updated.clone());
                out.session_effect = Some(SessionEffect::TodoEdit {
                    phases: updated.clone(),
                });
                events
                    .emit(AgentEvent::TodosUpdated { phases: updated })
                    .await;
            }
            Ok(out)
        })
    }
}

/// Register `ask` and `todo`.
///
/// # Errors
///
/// When a tool name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    registry.tool(Arc::new(AskTool))?;
    registry.tool(Arc::new(TodoTool))?;
    registry.prompt_section(ASK_PROMPT);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::testing::tool_context as test_ctx;
    use hrn_kernel::{AskHandler, CancelFlag, Submission};

    /// Submits the form's defaults, or reports a cancel.
    struct Defaults(bool);

    impl AskHandler for Defaults {
        fn ask(&self, request: AskRequest, _cancel: CancelFlag) -> BoxFuture<Result<Submission>> {
            let cancel = self.0;
            Box::pin(async move {
                if cancel {
                    return Err(HrnError::Tool("the user cancelled the form".into()));
                }
                Ok(Submission {
                    values: request.form.values(&request.form.defaults()),
                    timed_out: false,
                })
            })
        }
    }

    const FORM: &str = "<form><input type=radio name=a value=x checked> X \
                        <input type=radio name=a value=y> Y</form>";

    fn run(handler: Option<bool>, html: &str) -> Result<ToolOutput> {
        let mut ctx = test_ctx(std::env::temp_dir());
        ctx.ask = handler.map(|cancel| Arc::new(Defaults(cancel)) as Arc<dyn AskHandler>);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(AskTool.execute(&ctx, json!({ "html": html })))
    }

    #[test]
    fn returns_the_submitted_values_as_json() {
        let out = run(Some(false), FORM).expect("ok");
        assert!(!out.is_error);
        assert_eq!(out.content, "User answers:\n{\n  \"a\": \"x\"\n}");
    }

    #[test]
    fn a_cancelled_form_is_an_error_and_no_handler_is_reported() {
        assert!(run(Some(true), FORM).is_err());
        let out = run(None, FORM).expect("tool output");
        assert!(out.is_error && out.content.contains("unavailable"));
        let bad = run(Some(false), "<form><p>nothing</p></form>").expect("tool output");
        assert!(bad.is_error && bad.content.contains("no named controls"));
    }

    #[test]
    fn hidden_only_forms_answer_without_a_user() {
        let out = run(None, "<form><input type=hidden name=k value=v></form>").expect("ok");
        assert_eq!(out.content, "User answers:\n{\n  \"k\": \"v\"\n}");
    }
}
