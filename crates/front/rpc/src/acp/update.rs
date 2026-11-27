//! Runtime events as ACP `session/update` notifications.
//!
//! The ACP front keeps one [`Update`] state machine per session: chunks carry the `messageId` of
//! the message they belong to, tool calls are created `pending` so an approval card can precede
//! `in_progress`, and plans replace the whole checklist on every todo update.

use crate::history::message_json;
use hrn_kernel::{AgentEvent, DagNode, DagPayload, Role, TodoPhase, TodoStatus};
use serde_json::{Value, json};

/// The next id of a kind.
fn next_id(prefix: &str, counter: &mut u64) -> String {
    *counter += 1;
    format!("{prefix}-{counter}")
}

/// The ACP plan status of a todo; abandoned work has none, so it is left out of the plan.
const fn plan_status(status: TodoStatus) -> Option<&'static str> {
    match status {
        TodoStatus::Pending => Some("pending"),
        TodoStatus::InProgress => Some("in_progress"),
        TodoStatus::Completed => Some("completed"),
        TodoStatus::Abandoned => None,
    }
}

/// The ACP `toolCall.kind` for a tool name.
pub(crate) fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "grep" | "glob" | "lsp" | "find" => "read",
        "write" | "edit" | "patch" => "edit",
        "bash" => "execute",
        "web_search" | "web_fetch" => "fetch",
        "task" | "eval" => "think",
        _ => "other",
    }
}

/// A one-line title for a tool call: the tool name plus the argument that names its target.
pub(crate) fn tool_title(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let target = ["path", "command", "pattern", "query", "url", "prompt"]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .unwrap_or("");
    let target: String = target.chars().take(120).collect();
    if target.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {target}")
    }
}

/// Per-session translation state.
#[derive(Default)]
pub(crate) struct Update {
    message: u64,
    thought: u64,
    user: u64,
    /// The assistant message currently streaming, if any.
    current: Option<String>,
    /// The reasoning message currently streaming, if any.
    current_thought: Option<String>,
}

/// A `session/update` frame for `session_id` carrying `update`.
pub(crate) fn frame(session_id: &str, update: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": session_id, "update": update },
    })
}

/// A text content block.
fn text(content: &str) -> Value {
    json!({ "type": "text", "text": content })
}

impl Update {
    /// Close the open message and thought streams.
    fn close(&mut self) {
        self.current = None;
        self.current_thought = None;
    }

    /// The id of the message `thought` streams into, starting one when needed.
    fn stream(&mut self, thought: bool) -> String {
        if thought {
            if let Some(id) = &self.current_thought {
                return id.clone();
            }
            let id = next_id("thought", &mut self.thought);
            self.current_thought = Some(id.clone());
            id
        } else {
            if let Some(id) = &self.current {
                return id.clone();
            }
            let id = next_id("msg", &mut self.message);
            self.current = Some(id.clone());
            id
        }
    }

    /// The update frames one runtime event produces.
    pub(crate) fn handle(&mut self, event: &AgentEvent) -> Vec<Value> {
        let mut out = Vec::new();
        match event {
            AgentEvent::TurnStart { user_text, .. } => {
                self.close();
                out.push(json!({
                    "sessionUpdate": "user_message_chunk",
                    "messageId": next_id("user", &mut self.user),
                    "content": text(user_text),
                }));
            }
            AgentEvent::SteerInjected { text: steer, .. } => {
                out.push(json!({
                    "sessionUpdate": "user_message_chunk",
                    "messageId": next_id("user", &mut self.user),
                    "content": text(steer),
                }));
            }
            AgentEvent::TextDelta { delta } => out.push(json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": self.stream(false),
                "content": text(delta),
            })),
            AgentEvent::ThinkingDelta { delta } => out.push(json!({
                "sessionUpdate": "agent_thought_chunk",
                "messageId": self.stream(true),
                "content": text(delta),
            })),
            AgentEvent::LlmCall { .. }
            | AgentEvent::TurnDone { .. }
            | AgentEvent::TurnError { .. } => self.close(),
            AgentEvent::ToolStart {
                call_id,
                name,
                arguments,
            } => {
                let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
                out.push(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": call_id,
                    "status": "in_progress",
                    "title": tool_title(name, arguments),
                    "kind": tool_kind(name),
                    "rawInput": args,
                }));
            }
            AgentEvent::ToolDelta { call_id, delta } => out.push(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id,
                "status": "in_progress",
                "content": [{ "type": "content", "content": text(delta) }],
            })),
            AgentEvent::ToolDone {
                call_id,
                name,
                output,
                is_error,
                diff,
                images,
            } => out.push(tool_done_update(
                call_id,
                name,
                output,
                *is_error,
                diff.as_deref(),
                images,
            )),
            AgentEvent::TodosUpdated { phases } => out.push(plan(phases)),
            AgentEvent::AssistantNote { text: body } => out.push(json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": next_id("note", &mut self.thought),
                "content": text(body),
            })),
            AgentEvent::AdvisorNote { model, note } => out.push(json!({
                "sessionUpdate": "agent_thought_chunk",
                "messageId": next_id("advisor", &mut self.thought),
                "content": text(&format!("{model}: {note}")),
            })),
            AgentEvent::StatusChanged { .. }
            | AgentEvent::ApprovalRequested { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::AskRequested { .. }
            | AgentEvent::Compacted { .. }
            | AgentEvent::CompactionSpeculating { .. }
            | AgentEvent::GoalUpdated { .. }
            | AgentEvent::SessionTitle { .. } => {}
        }
        out
    }
}

fn tool_done_update(
    call_id: &str,
    name: &str,
    output: &str,
    is_error: bool,
    diff: Option<&str>,
    images: &[hrn_kernel::ImageAttachment],
) -> Value {
    let mut content = vec![json!({ "type": "content", "content": text(output) })];
    if let Some(diff) = diff {
        content.push(json!({ "type": "content", "content": text(diff) }));
    }
    content.extend(images.iter().map(|image| {
        json!({
            "type": "content",
            "content": {
                "type": "image",
                "data": image.data_base64,
                "mimeType": image.media_type,
            },
        })
    }));
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": call_id,
        "status": if is_error { "failed" } else { "completed" },
        "title": name,
        "content": content,
    })
}

fn plan(phases: &[TodoPhase]) -> Value {
    let entries: Vec<Value> = phases
        .iter()
        .flat_map(|phase| &phase.tasks)
        .filter_map(|item| {
            plan_status(item.status).map(
                |status| json!({ "content": item.content, "priority": "medium", "status": status }),
            )
        })
        .collect();
    json!({ "sessionUpdate": "plan", "entries": entries })
}

/// The `session/update` frames that replay a stored branch to a client that called `session/load`.
pub(crate) fn replay(session_id: &str, nodes: &[DagNode]) -> Vec<Value> {
    let mut out = Vec::new();
    for node in nodes {
        let DagPayload::Message { msg, .. } = &node.payload else {
            continue;
        };
        let Some(message) = message_json(msg, node.created_at_ms) else {
            continue;
        };
        match msg.role {
            Role::User => out.push(frame(
                session_id,
                &json!({
                    "sessionUpdate": "user_message_chunk",
                    "messageId": format!("load-{}", node.id),
                    "content": text(&msg.content),
                }),
            )),
            Role::Assistant => {
                if !msg.content.is_empty() {
                    out.push(frame(
                        session_id,
                        &json!({
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": format!("load-{}", node.id),
                            "content": text(&msg.content),
                        }),
                    ));
                }
                let _ = message;
            }
            Role::Tool => out.push(frame(
                session_id,
                &json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": msg.tool_call_id.clone().unwrap_or_default(),
                    "status": if msg.is_error { "failed" } else { "completed" },
                    "content": [{ "type": "content", "content": text(&msg.content) }],
                }),
            )),
            Role::System => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_of_one_answer_share_a_message_id() {
        let mut u = Update::default();
        let mut ids = Vec::new();
        for delta in ["a", "b"] {
            for frame in u.handle(&AgentEvent::TextDelta {
                delta: delta.to_owned(),
            }) {
                ids.push(frame["messageId"].as_str().unwrap_or_default().to_owned());
            }
        }
        u.handle(&AgentEvent::LlmCall {
            model: "m".to_owned(),
            first_token_ms: Some(0),
            duration_ms: 0,
            finish_reason: hrn_kernel::FinishReason::Stop,
            usage: Some(hrn_kernel::Usage::default()),
        });
        let after = u.handle(&AgentEvent::TextDelta {
            delta: "c".to_owned(),
        });
        let next = after[0]["messageId"].as_str().unwrap_or_default();
        assert_eq!(ids[0], ids[1]);
        assert_ne!(ids[0], next);
    }

    #[test]
    fn a_failed_tool_reports_failed() {
        let mut u = Update::default();
        let frames = u.handle(&AgentEvent::ToolDone {
            call_id: "c1".to_owned(),
            name: "bash".to_owned(),
            output: "boom".to_owned(),
            is_error: true,
            diff: None,
            images: Vec::new(),
        });
        assert_eq!(frames[0]["status"], json!("failed"));
        assert_eq!(frames[0]["sessionUpdate"], json!("tool_call_update"));
    }
}
