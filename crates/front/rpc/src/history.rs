//! Stored conversation as omp's `AgentMessage`s, for `get_messages` and friends.

use hrn_kernel::{DagNode, DagPayload, ImageAttachment, Msg, Role};
use serde_json::{Value, json};

fn image_blocks(images: &[ImageAttachment]) -> impl Iterator<Item = Value> + '_ {
    images
        .iter()
        .map(|i| json!({ "type": "image", "data": i.data_base64, "mimeType": i.media_type }))
}

/// One stored message; `None` for system frames, which hosts never see.
pub(crate) fn message_json(msg: &Msg, at_ms: i64) -> Option<Value> {
    match msg.role {
        Role::System => None,
        Role::User => {
            let mut content = vec![json!({ "type": "text", "text": msg.content })];
            content.extend(image_blocks(&msg.images));
            Some(json!({ "role": "user", "content": content, "timestamp": at_ms }))
        }
        Role::Assistant => {
            let mut content = Vec::new();
            if let Some(thinking) = msg.reasoning.as_deref().filter(|t| !t.is_empty()) {
                content.push(json!({ "type": "thinking", "thinking": thinking }));
            }
            if !msg.content.is_empty() {
                content.push(json!({ "type": "text", "text": msg.content }));
            }
            for call in &msg.tool_calls {
                let arguments = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments.clone()));
                content.push(json!({
                    "type": "toolCall",
                    "id": call.id,
                    "name": call.name,
                    "arguments": arguments,
                }));
            }
            let stop = if msg.tool_calls.is_empty() {
                "stop"
            } else {
                "toolUse"
            };
            Some(json!({
                "role": "assistant",
                "content": content,
                "stopReason": stop,
                "timestamp": at_ms,
            }))
        }
        Role::Tool => {
            let mut content = vec![json!({ "type": "text", "text": msg.content })];
            content.extend(image_blocks(&msg.images));
            Some(json!({
                "role": "toolResult",
                "toolCallId": msg.tool_call_id,
                "toolName": msg.tool_name,
                "content": content,
                "isError": msg.is_error,
                "timestamp": at_ms,
            }))
        }
    }
}

/// The messages of a branch, oldest first.
pub(crate) fn messages(nodes: &[DagNode]) -> Vec<Value> {
    nodes
        .iter()
        .filter_map(|node| match &node.payload {
            DagPayload::Message { msg, .. } => message_json(msg, node.created_at_ms),
            _ => None,
        })
        .collect()
}

/// Message counts of a branch: user, assistant, tool calls, tool results.
pub(crate) fn counts(nodes: &[DagNode]) -> (usize, usize, usize, usize) {
    let mut out = (0, 0, 0, 0);
    for node in nodes {
        if let DagPayload::Message { msg, .. } = &node.payload {
            match msg.role {
                Role::User => out.0 += 1,
                Role::Assistant => {
                    out.1 += 1;
                    out.2 += msg.tool_calls.len();
                }
                Role::Tool => out.3 += 1,
                Role::System => {}
            }
        }
    }
    out
}

/// The text of the newest assistant message that has any.
pub(crate) fn last_assistant_text(nodes: &[DagNode]) -> Option<String> {
    nodes.iter().rev().find_map(|node| match &node.payload {
        DagPayload::Message { msg, .. }
            if msg.role == Role::Assistant && !msg.content.is_empty() =>
        {
            Some(msg.content.clone())
        }
        _ => None,
    })
}
