//! Anthropic Messages request encoding: system/tool cache markers, thinking budget and
//! signed-thinking replay.

use crate::common::fatal;
use hrn_kernel::{ImageAttachment, LlmRequest, Msg, Result, Role, ThinkingLevel, Tier};
use serde_json::{Value, json};

/// The built-in Anthropic budget ladder; a model's `thinking.effortBudgets` entry wins.
fn thinking_budget_tokens(req: &LlmRequest, level: ThinkingLevel) -> Option<u32> {
    let fallback = || match level {
        ThinkingLevel::Off | ThinkingLevel::Auto => None,
        ThinkingLevel::Minimal => Some(1024),
        ThinkingLevel::Low => Some(2048),
        ThinkingLevel::Medium => Some(6144),
        ThinkingLevel::High => Some(16_384),
        ThinkingLevel::Xhigh | ThinkingLevel::Max => Some(32_000),
    };
    match &req.thinking_budgets {
        Some(budgets) => budgets.budget_for(level, fallback),
        None => fallback(),
    }
}

/// Thinking blocks of `msg` that the API will accept back: produced by this very model.
fn replayable<'a>(
    msg: &'a Msg,
    model_id: &'a str,
) -> impl Iterator<Item = &'a hrn_kernel::ThinkingBlock> {
    msg.thinking_blocks
        .iter()
        .filter(move |b| b.model == model_id)
}

/// With thinking enabled the API rejects (400) an assistant tool-use turn of the *current* tool
/// loop that does not start with its signed thinking block. That happens when thinking was just
/// switched on, or the history came from another model/provider. In that case the request is sent
/// without extended thinking instead of failing.
fn thinking_usable(req: &LlmRequest, model_id: &str) -> bool {
    let last_user = req.messages.iter().rposition(|m| m.role == Role::User);
    let tail_start = last_user.map_or(0, |i| i + 1);
    req.messages
        .iter()
        .skip(tail_start)
        .filter(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        .all(|m| replayable(m, model_id).next().is_some())
}

/// Build the JSON body for `POST /v1/messages` (direct Anthropic host).
pub(crate) fn encode_request(
    model_id: &str,
    req: &LlmRequest,
    prompt_cache: bool,
    strip_temp: bool,
) -> Result<Value> {
    encode_request_for_host(model_id, req, prompt_cache, strip_temp, true)
}

/// Build the JSON body for `POST /v1/messages`, emitting `service_tier` on direct Anthropic only.
pub(crate) fn encode_request_for_host(
    model_id: &str,
    req: &LlmRequest,
    prompt_cache: bool,
    strip_temp: bool,
    direct_anthropic: bool,
) -> Result<Value> {
    let budget =
        thinking_budget_tokens(req, req.thinking).filter(|_| thinking_usable(req, model_id));
    let mut sys_parts: Vec<&str> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for msg in &req.messages {
        match msg.role {
            Role::System => {
                if !msg.content.trim().is_empty() {
                    sys_parts.push(&msg.content);
                }
            }
            Role::User => {
                if !msg.content.is_empty() {
                    push_merged_user(&mut messages, json!({"type": "text", "text": msg.content}));
                }
                for img in &msg.images {
                    push_merged_user(&mut messages, encode_image_block(img));
                }
            }
            Role::Assistant => {
                if let Some(encoded) = encode_assistant(msg, model_id, budget.is_some()) {
                    messages.push(encoded);
                }
            }
            Role::Tool => {
                push_merged_user(&mut messages, encode_tool_result(msg)?);
            }
        }
    }

    let max_tokens = req
        .max_tokens
        .unwrap_or(16_384)
        .max(budget.map_or(0, |b| b.saturating_add(4096)));
    let mut body = json!({
        "model": model_id,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": messages,
    });
    let Some(obj) = body.as_object_mut() else {
        return Err(fatal("request body is not an object"));
    };

    if !sys_parts.is_empty() {
        obj.insert("system".into(), encode_system(&sys_parts, prompt_cache));
    }

    if let Some(budget) = budget {
        obj.insert(
            "thinking".into(),
            json!({"type": "enabled", "budget_tokens": budget}),
        );
    } else if !strip_temp && let Some(temp) = req.temperature {
        obj.insert("temperature".into(), json!(temp));
    }
    if direct_anthropic && matches!(req.service_tier, Some(Tier::Priority | Tier::Ultrafast)) {
        obj.insert("service_tier".into(), json!("priority"));
    }

    if !req.tools.is_empty() {
        let last = req.tools.len() - 1;
        let tools: Vec<Value> = req
            .tools
            .iter()
            .enumerate()
            .map(|(idx, t)| {
                let mut v = json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                });
                if prompt_cache
                    && idx == last
                    && let Some(tool) = v.as_object_mut()
                {
                    tool.insert("cache_control".into(), json!({"type": "ephemeral"}));
                }
                v
            })
            .collect();
        obj.insert("tools".into(), Value::Array(tools));
    }
    Ok(body)
}

/// Format the Anthropic `system` field, adding ephemeral cache breakpoints when `prompt_cache` is on.
fn encode_system(sys_parts: &[&str], prompt_cache: bool) -> Value {
    let joined = sys_parts.join("\n\n");
    if !prompt_cache {
        return json!(joined);
    }
    let mut blocks: Vec<Value> = Vec::new();
    let mut rest = joined.as_str();
    if let Some(at) = rest.find("<project-context>") {
        let (head, tail) = rest.split_at(at);
        if !head.trim().is_empty() {
            blocks.push(json!({
                "type": "text",
                "text": head.trim_end(),
                "cache_control": {"type": "ephemeral"}
            }));
        }
        rest = tail;
    }
    if !rest.trim().is_empty() {
        blocks.push(json!({"type": "text", "text": rest}));
    }
    if let Some(last) = blocks.last_mut().and_then(Value::as_object_mut) {
        last.insert("cache_control".into(), json!({"type": "ephemeral"}));
    }
    Value::Array(blocks)
}

/// Tool results and user text share one `user` message (the API requires strict alternation of
/// `tool_use` → `tool_result`); `tool_result` blocks are kept ahead of text.
fn push_merged_user(messages: &mut Vec<Value>, block: Value) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        arr.push(block);
        return;
    }
    messages.push(json!({"role": "user", "content": [block]}));
}

fn encode_image_block(img: &ImageAttachment) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": img.media_type,
            "data": img.data_base64,
        }
    })
}

fn encode_tool_result(msg: &Msg) -> Result<Value> {
    let id = msg.tool_call_id.as_deref().unwrap_or_default();
    if id.is_empty() {
        return Err(fatal("tool result message without tool_call_id"));
    }
    let text = if msg.content.is_empty() {
        "(no output)"
    } else {
        msg.content.as_str()
    };
    let content = if msg.images.is_empty() {
        Value::String(text.to_owned())
    } else {
        let mut parts = Vec::with_capacity(msg.images.len() + 1);
        parts.push(json!({"type": "text", "text": text}));
        for img in &msg.images {
            parts.push(encode_image_block(img));
        }
        Value::Array(parts)
    };
    let mut block = json!({"type": "tool_result", "tool_use_id": id, "content": content});
    if msg.is_error
        && let Some(obj) = block.as_object_mut()
    {
        obj.insert("is_error".into(), Value::Bool(true));
    }
    Ok(block)
}

/// `None` when the assistant message carries nothing the API would accept (empty text blocks
/// are rejected with a 400).
fn encode_assistant(msg: &Msg, model_id: &str, with_thinking: bool) -> Option<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    if with_thinking {
        for b in replayable(msg, model_id) {
            blocks.push(match &b.redacted {
                Some(data) => json!({"type": "redacted_thinking", "data": data}),
                None => json!({"type": "thinking", "thinking": b.text, "signature": b.signature}),
            });
        }
    }
    if !msg.content.is_empty() {
        blocks.push(json!({"type": "text", "text": msg.content}));
    }
    for call in &msg.tool_calls {
        let input: Value = serde_json::from_str(&call.arguments)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": input,
        }));
    }
    (!blocks.is_empty()).then(|| json!({"role": "assistant", "content": blocks}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::{ThinkingBlock, ToolCall, ToolSpec};

    fn req(messages: Vec<Msg>, thinking: ThinkingLevel) -> LlmRequest {
        LlmRequest {
            model: None,
            messages,
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "read".into(),
                parameters: json!({"type": "object"}),
            }],
            thinking,
            thinking_budgets: None,
            temperature: Some(0.2),
            max_tokens: None,
            session_id: None,
            service_tier: None,
        }
    }

    fn tool_turn(model: &str, signed: bool) -> Msg {
        let mut m = Msg::assistant("");
        m.tool_calls.push(ToolCall {
            id: "toolu_1".into(),
            name: "read".into(),
            arguments: "{}".into(),
            signature: None,
        });
        if signed {
            m.thinking_blocks.push(ThinkingBlock {
                model: model.into(),
                text: "hmm".into(),
                signature: Some("sig".into()),
                redacted: None,
            });
        }
        m
    }

    #[test]
    fn emits_prompt_cache_markers_and_thinking_budget() {
        let r = req(
            vec![Msg::system("sys"), Msg::user("hi")],
            ThinkingLevel::High,
        );
        let body = encode_request("claude-opus-5-5", &r, true, false).expect("encode");
        assert_eq!(
            body.get("system")
                .and_then(|s| s.get(0))
                .and_then(|b| b.get("cache_control"))
                .and_then(|c| c.get("type")),
            Some(&json!("ephemeral"))
        );
        assert_eq!(
            body.get("tools")
                .and_then(|t| t.get(0))
                .and_then(|b| b.get("cache_control"))
                .and_then(|c| c.get("type")),
            Some(&json!("ephemeral"))
        );
        assert_eq!(
            body.get("thinking").and_then(|t| t.get("budget_tokens")),
            Some(&json!(16_384))
        );
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn signed_thinking_is_replayed_first_for_the_same_model() {
        let mut result = Msg::tool_result("toolu_1", "read", "ok");
        result.is_error = true;
        let r = req(
            vec![Msg::user("go"), tool_turn("m1", true), result],
            ThinkingLevel::Medium,
        );
        let body = encode_request("m1", &r, false, false).expect("encode");
        let msgs = body
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages");
        let first = msgs[1]["content"][0].clone();
        assert_eq!(first["type"], "thinking");
        assert_eq!(first["signature"], "sig");
        assert_eq!(msgs[1]["content"][1]["type"], "tool_use");
        assert_eq!(msgs[2]["content"][0]["is_error"], true);
        assert!(body.get("thinking").is_some());
    }

    #[test]
    fn unsigned_tool_loop_turn_downgrades_to_no_thinking_instead_of_a_400() {
        // History produced by another model, or before thinking was switched on.
        let r = req(
            vec![
                Msg::user("go"),
                tool_turn("other", false),
                Msg::tool_result("toolu_1", "read", "ok"),
            ],
            ThinkingLevel::High,
        );
        let body = encode_request("m1", &r, false, false).expect("encode");
        assert!(body.get("thinking").is_none());
        assert_eq!(body["temperature"], json!(0.2_f32));
        let msgs = body["messages"].as_array().expect("messages");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn empty_blocks_are_dropped_and_missing_tool_id_is_rejected() {
        let r = req(
            vec![Msg::user(""), Msg::assistant(""), Msg::user("real")],
            ThinkingLevel::Off,
        );
        let body = encode_request("m1", &r, false, false).expect("encode");
        let msgs = body["messages"].as_array().expect("messages");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"][0]["text"], "real");

        let mut bad = Msg::tool_result("x", "read", "out");
        bad.tool_call_id = None;
        assert!(encode_request("m1", &req(vec![bad], ThinkingLevel::Off), false, false).is_err());
    }

    #[test]
    fn encodes_user_and_tool_images() {
        let img = ImageAttachment {
            media_type: "image/png".into(),
            data_base64: "AAAA".into(),
            label: Some("shot.png".into()),
            width: Some(10),
            height: Some(10),
        };
        let mut tool_res = Msg::tool_result("toolu_1", "read_file", "Image: shot.png");
        tool_res.images.push(img.clone());
        let r = req(
            vec![
                Msg::user_with_images("see this", vec![img]),
                tool_turn("m1", false),
                tool_res,
            ],
            ThinkingLevel::Off,
        );
        let body = encode_request("m1", &r, false, false).expect("encode");
        let msgs = body["messages"].as_array().expect("messages");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][1]["type"], "image");
        assert_eq!(msgs[0]["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(msgs[2]["content"][0]["content"][1]["type"], "image");
    }

    #[test]
    fn activation_keeps_the_cached_prefix_byte_identical() {
        // Two requests differing only by an activated deferred tool (appended, sorted): every
        // earlier tool and the whole system string stay byte-identical, and the markers sit at
        // the array end plus the static system head.
        let base = vec![
            ToolSpec {
                name: "read".into(),
                description: "r".into(),
                parameters: json!({"type": "object"}),
            },
            ToolSpec {
                name: "bash".into(),
                description: "b".into(),
                parameters: json!({"type": "object"}),
            },
        ];
        let mut grown = base.clone();
        grown.push(ToolSpec {
            name: "mcp__srv__zoom".into(),
            description: "z".into(),
            parameters: json!({"type": "object"}),
        });
        let req = |tools: Vec<ToolSpec>| LlmRequest {
            model: None,
            messages: vec![
                Msg::system("static head\n<project-context>\nWorkspace: /ws\n</project-context>"),
                Msg::user("hi"),
            ],
            tools,
            thinking: ThinkingLevel::Off,
            thinking_budgets: None,
            temperature: Some(0.2),
            max_tokens: None,
            session_id: None,
            service_tier: None,
        };
        let a = encode_request("claude-opus-5", &req(base.clone()), true, false).expect("a");
        let b = encode_request("claude-opus-5", &req(grown), true, false).expect("b");
        let fa = a.get("tools").expect("tools").as_array().expect("arr");
        let fb = b.get("tools").expect("tools").as_array().expect("arr");
        // The array-end marker moves with growth; the tool bytes themselves never change.
        let strip = |arr: &[Value]| -> Vec<Value> {
            arr.iter()
                .map(|t| {
                    let mut copy = t.clone();
                    if let Some(obj) = copy.as_object_mut() {
                        obj.remove("cache_control");
                    }
                    copy
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            strip(fa),
            strip(&fb[..fa.len()]),
            "the prefix is append-only"
        );
        assert_eq!(fb.len(), 3, "the activated schema is appended");
        let sa = a.get("system").expect("system").as_array().expect("arr");
        let sb = b.get("system").expect("system").as_array().expect("arr");
        assert_eq!(&sa[..], &sb[..], "the system blocks do not move");
        assert!(sa[0]["cache_control"].is_object(), "static head marker");
        assert_eq!(
            fa.last().expect("last")["cache_control"]["type"].as_str(),
            Some("ephemeral"),
            "the marker rides the array end"
        );
    }
}
