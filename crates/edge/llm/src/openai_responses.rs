//! `OpenAI` `/responses` API wire encoder and SSE stream assembler
//! (supports `prompt_cache_key`, `reasoning.effort`, and Codex models).

use crate::common::{ToolCallAssembler, fatal, next_event, parse_event, truncated_stream};
use crate::config::{ModelPreset, ProviderEntry};
use crate::openai_chat::{build_headers, mapped_effort};
use bytes::Bytes;
use hrn_edge_http::{HttpClient, SseStream};
use hrn_kernel::{
    CancelFlag, ChunkSink, FinishReason, HrnError, ImageAttachment, LlmChunk, LlmRequest,
    LlmResponse, Result, Role, ThinkingLevel, Tier, Usage,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

const PROVIDER: &str = "openai-responses";
/// Prefix on synthetic summary `Msg::read_key` carrying opaque remote-compaction replay items.
const REMOTE_COMPACTION_PREFIX: &str = "hrn:remote_compaction:";

/// Stream one request against `<base_url>/responses`.
#[expect(
    clippy::too_many_arguments,
    reason = "wire dispatch passes provider, preset, model_id, token, req, cancel, sink"
)]
pub(crate) async fn stream(
    http: &HttpClient,
    provider: &ProviderEntry,
    preset: &ModelPreset,
    model_id: &str,
    token: &str,
    req: &LlmRequest,
    cancel: &CancelFlag,
    sink: &dyn ChunkSink,
) -> Result<LlmResponse> {
    let url = format!("{}/responses", provider.effective_base_url());
    let strip_temp = provider.strip_temperature || model_forbids_temperature(model_id);
    let payload = encode_request_with_map(model_id, req, strip_temp, &preset.reasoning_effort_map)?;
    let body = Bytes::from(serde_json::to_vec(&payload)?);
    let headers = build_headers(provider, preset, token, req.session_id.as_deref());

    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(180));
    let mut sse = http.post_sse(&url, &headers, body, timeout).await?;
    read_stream(&mut sse, model_id, cancel, sink).await
}

/// Assemble a `/responses` event stream into a response.
pub(crate) async fn read_stream(
    sse: &mut SseStream,
    model_id: &str,
    cancel: &CancelFlag,
    sink: &dyn ChunkSink,
) -> Result<LlmResponse> {
    let mut asm = ResponsesAssembler::new(model_id.to_owned());
    while let Some(ev) = next_event(sse, cancel).await? {
        let data = ev.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        asm.handle_data(data, sink).await?;
        if asm.terminal {
            break;
        }
    }
    asm.finish()
}

pub(crate) fn encode_request(model_id: &str, req: &LlmRequest, strip_temp: bool) -> Result<Value> {
    encode_request_with_map(model_id, req, strip_temp, &BTreeMap::new())
}

/// Convert canonical [`hrn_kernel::Msg`] items into `(instructions, input)` for the `OpenAI`
/// `/responses` and `/responses/compact` endpoints, replaying opaque remote-compaction preserve
/// items when present on a projected summary message.
pub(crate) fn encode_input_items(
    messages: &[hrn_kernel::Msg],
) -> Result<(Vec<String>, Vec<Value>)> {
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                if !msg.content.trim().is_empty() {
                    instructions.push(msg.content.clone());
                }
            }
            Role::User => {
                if let Some(replayed) = decode_remote_preserve(msg.read_key.as_deref()) {
                    input.extend(replayed);
                } else {
                    input.push(encode_user_item(&msg.content, &msg.images));
                }
            }
            Role::Assistant => {
                if !msg.content.is_empty() {
                    input.push(json!({"role": "assistant", "content": msg.content}));
                }
                for call in &msg.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    }));
                }
            }
            Role::Tool => {
                let id = msg.tool_call_id.as_deref().unwrap_or_default();
                if id.is_empty() {
                    return Err(fatal("tool result message without tool_call_id"));
                }
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": id,
                    "output": msg.content,
                }));
                if !msg.images.is_empty() {
                    let tool = msg.tool_name.as_deref().unwrap_or("tool");
                    let caption = format!("[attached image from {tool}]");
                    input.push(encode_user_item(&caption, &msg.images));
                }
            }
        }
    }
    Ok((instructions, input))
}

fn decode_remote_preserve(read_key: Option<&str>) -> Option<Vec<Value>> {
    let encoded = read_key?.strip_prefix(REMOTE_COMPACTION_PREFIX)?;
    let val: Value = serde_json::from_str(encoded).ok()?;
    let items: Vec<Value> = if let Some(arr) = val.get("output").and_then(Value::as_array) {
        arr.iter().filter(|v| v.is_object()).cloned().collect()
    } else if let Some(arr) = val.as_array() {
        arr.iter().filter(|v| v.is_object()).cloned().collect()
    } else if val.is_object() && val.get("type").is_some() {
        vec![val]
    } else {
        Vec::new()
    };
    (!items.is_empty()).then_some(items)
}

pub(crate) fn encode_request_with_map(
    model_id: &str,
    req: &LlmRequest,
    strip_temp: bool,
    effort_map: &BTreeMap<ThinkingLevel, String>,
) -> Result<Value> {
    let (instructions, input) = encode_input_items(&req.messages)?;

    let mut body = json!({
        "model": model_id,
        "input": input,
        "stream": true,
        "store": false,
    });
    let Some(obj) = body.as_object_mut() else {
        return Err(fatal("request body is not an object"));
    };
    if !instructions.is_empty() {
        obj.insert("instructions".into(), json!(instructions.join("\n\n")));
    }
    if let Some(sid) = &req.session_id {
        obj.insert("prompt_cache_key".into(), json!(sid));
    }
    if let Some(effort) = mapped_effort(req.thinking, effort_map) {
        // `summary: auto` is what makes the API stream reasoning summary text at all.
        obj.insert(
            "reasoning".into(),
            json!({"effort": effort, "summary": "auto"}),
        );
    }
    if !strip_temp && let Some(temp) = req.temperature {
        obj.insert("temperature".into(), json!(temp));
    }
    if let Some(max) = req.max_tokens {
        obj.insert("max_output_tokens".into(), json!(max));
    }
    if let Some(tier) = req.service_tier.filter(|t| *t != Tier::None) {
        obj.insert("service_tier".into(), json!(tier.as_str()));
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        obj.insert("tools".into(), Value::Array(tools));
    }
    Ok(body)
}

fn encode_user_item(text: &str, images: &[ImageAttachment]) -> Value {
    if images.is_empty() {
        return json!({"role": "user", "content": text});
    }
    let mut parts: Vec<Value> = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        parts.push(json!({"type": "input_text", "text": text}));
    }
    for img in images {
        let url = format!("data:{};base64,{}", img.media_type, img.data_base64);
        parts.push(json!({"type": "input_image", "image_url": url}));
    }
    json!({"role": "user", "content": parts})
}

fn model_forbids_temperature(model_id: &str) -> bool {
    let lc = model_id.to_ascii_lowercase();
    lc.contains("codex")
        || lc.starts_with("o1")
        || lc.starts_with("o3")
        || lc.starts_with("o4")
        || lc.contains("/o1")
        || lc.contains("/o3")
        || lc.contains("/o4")
}

struct ResponsesAssembler {
    model: String,
    content: String,
    reasoning: String,
    calls: ToolCallAssembler,
    usage: Option<Usage>,
    finish_reason: FinishReason,
    refused: bool,
    terminal: bool,
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// `error` event (`{type, code, message}`) or `response.failed` (`response.error.{code,message}`).
fn failure(val: &Value) -> HrnError {
    let err = val
        .get("response")
        .and_then(|r| r.get("error"))
        .unwrap_or(val);
    let message = Some(str_field(err, "message"))
        .filter(|m| !m.is_empty())
        .unwrap_or("responses stream error");
    let code = str_field(err, "code");
    let retryable = matches!(code, "server_error" | "rate_limit_exceeded" | "overloaded");
    HrnError::Provider {
        message: if code.is_empty() {
            message.to_owned()
        } else {
            format!("{code}: {message}")
        },
        retryable,
        retry_after_secs: None,
    }
}

impl ResponsesAssembler {
    fn new(model: String) -> Self {
        Self {
            model,
            content: String::new(),
            reasoning: String::new(),
            calls: ToolCallAssembler::default(),
            usage: None,
            finish_reason: FinishReason::Stop,
            refused: false,
            terminal: false,
        }
    }

    /// Key of the function-call item an event refers to: `item_id`, else the output position.
    fn call_key(val: &Value, item: Option<&Value>) -> String {
        let id = item.map_or_else(|| str_field(val, "item_id"), |i| str_field(i, "id"));
        if id.is_empty() {
            format!(
                "out{}",
                val.get("output_index").and_then(Value::as_u64).unwrap_or(0)
            )
        } else {
            id.to_owned()
        }
    }

    /// Apply a `function_call` item (from `output_item.added` / `.done`).
    fn apply_call_item(&mut self, val: &Value, item: &Value) {
        let slot = self.calls.by_key(&Self::call_key(val, Some(item)));
        let call_id = str_field(item, "call_id");
        let call_id = if call_id.is_empty() {
            str_field(item, "id")
        } else {
            call_id
        };
        if slot.id.is_empty() {
            call_id.clone_into(&mut slot.id);
        }
        if slot.name.is_empty() {
            str_field(item, "name").clone_into(&mut slot.name);
        }
        // `done` carries the authoritative full argument string.
        let args = str_field(item, "arguments");
        if !args.is_empty() {
            args.clone_into(&mut slot.args);
        }
    }

    async fn text_delta(&mut self, val: &Value, reasoning: bool, sink: &dyn ChunkSink) {
        let delta = str_field(val, "delta");
        if delta.is_empty() {
            return;
        }
        if reasoning {
            self.reasoning.push_str(delta);
            sink.on_chunk(LlmChunk::ReasoningDelta(delta.to_owned()))
                .await;
        } else {
            self.content.push_str(delta);
            sink.on_chunk(LlmChunk::ContentDelta(delta.to_owned()))
                .await;
        }
    }

    async fn handle_data(&mut self, data: &str, sink: &dyn ChunkSink) -> Result {
        let val = parse_event(PROVIDER, data)?;
        let kind = str_field(&val, "type");
        match kind {
            "response.output_text.delta" => self.text_delta(&val, false, sink).await,
            "response.refusal.delta" => {
                self.refused = true;
                self.text_delta(&val, false, sink).await;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.text_delta(&val, true, sink).await;
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = val.get("item")
                    && str_field(item, "type") == "function_call"
                {
                    self.apply_call_item(&val, item);
                }
            }
            "response.function_call_arguments.delta" => {
                let delta = str_field(&val, "delta");
                self.calls
                    .by_key(&Self::call_key(&val, None))
                    .args
                    .push_str(delta);
            }
            "response.function_call_arguments.done" => {
                let args = str_field(&val, "arguments");
                if !args.is_empty() {
                    args.clone_into(&mut self.calls.by_key(&Self::call_key(&val, None)).args);
                }
            }
            "response.completed" | "response.incomplete" => {
                self.terminal = true;
                let resp = val.get("response").unwrap_or(&Value::Null);
                if let Some(u) = resp.get("usage").filter(|u| !u.is_null()) {
                    self.usage = parse_responses_usage(u);
                }
                if kind == "response.incomplete" {
                    let reason = resp
                        .get("incomplete_details")
                        .map_or("", |d| str_field(d, "reason"));
                    self.finish_reason = FinishReason::from_wire(reason);
                    if self.finish_reason == FinishReason::Stop {
                        // Unknown reason: still an incomplete answer, never a clean stop.
                        self.finish_reason = FinishReason::Length;
                    }
                }
            }
            "error" | "response.failed" => return Err(failure(&val)),
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<LlmResponse> {
        if !self.terminal {
            return Err(truncated_stream(PROVIDER, "response.completed"));
        }
        let has_calls = !self.calls.is_empty();
        let tool_calls = self.calls.finish(PROVIDER)?;
        let finish_reason = match self.finish_reason {
            FinishReason::Stop if self.refused => FinishReason::ContentFilter,
            FinishReason::Stop if has_calls => FinishReason::ToolCalls,
            other => other,
        };
        Ok(LlmResponse {
            model_used: self.model,
            fallback_from: None,
            content: self.content,
            reasoning: self.reasoning,
            thinking_blocks: Vec::new(),
            tool_calls,
            usage: self.usage,
            finish_reason,
        })
    }
}

fn parse_responses_usage(u: &Value) -> Option<Usage> {
    let prompt = u.get("input_tokens").and_then(Value::as_u64)?;
    let completion = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    let total = u
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| prompt.saturating_add(completion));
    let cache_read = u
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .filter(|&v| v > 0);
    let reasoning = u
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .filter(|&v| v > 0);
    Some(Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        reasoning_tokens: reasoning,
        cost_micros: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::VecSink;
    use hrn_kernel::Msg;

    #[test]
    fn encodes_prompt_cache_key_reasoning_and_strips_codex_temp() {
        let req = LlmRequest {
            model: None,
            messages: vec![Msg::system("sys"), Msg::user("hi")],
            tools: Vec::new(),
            thinking: ThinkingLevel::High,
            thinking_budgets: None,
            temperature: Some(0.5),
            max_tokens: None,
            session_id: Some("ses_1".into()),
            service_tier: None,
        };
        let body = encode_request(
            "gpt-5.2-codex",
            &req,
            model_forbids_temperature("gpt-5.2-codex"),
        )
        .expect("encode");
        assert_eq!(body.get("prompt_cache_key"), Some(&json!("ses_1")));
        assert_eq!(
            body.get("reasoning").and_then(|r| r.get("effort")),
            Some(&json!("high"))
        );
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn encodes_user_and_tool_images() {
        let img = ImageAttachment {
            media_type: "image/png".into(),
            data_base64: "AAAA".into(),
            label: None,
            width: None,
            height: None,
        };
        let mut tool = Msg::tool_result("c1", "read_file", "img");
        tool.images.push(img.clone());
        let req = LlmRequest {
            model: None,
            messages: vec![Msg::user_with_images("look", vec![img]), tool],
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            thinking_budgets: None,
            temperature: None,
            max_tokens: None,
            session_id: None,
            service_tier: None,
        };
        let body = encode_request("gpt-5", &req, false).expect("encode");
        let input = body["input"].as_array().expect("input");
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(
            input[0]["content"][1]["image_url"],
            "data:image/png;base64,AAAA"
        );
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][1]["type"], "input_image");
    }

    async fn run(events: &[&str]) -> (Result<LlmResponse>, VecSink) {
        let sink = VecSink::default();
        let mut asm = ResponsesAssembler::new("gpt".into());
        for e in events {
            if let Err(err) = asm.handle_data(e, &sink).await {
                return (Err(err), sink);
            }
        }
        (asm.finish(), sink)
    }

    #[tokio::test]
    async fn two_function_calls_with_interleaved_deltas_and_done_events() {
        let (resp, sink) = run(&[
            r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            r#"{"type":"response.output_text.delta","delta":"Ok."}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":""}}"#,
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"grep","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"{\"pattern\":\"x\"}"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"a.rs\"}"}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"a.rs\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"grep","arguments":"{\"pattern\":\"x\"}"}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":20,"output_tokens":8,"input_tokens_details":{"cached_tokens":16},"output_tokens_details":{"reasoning_tokens":3}}}}"#,
        ])
        .await;
        let resp = resp.expect("response");
        assert_eq!(sink.reasoning(), "thinking");
        assert_eq!(resp.content, "Ok.");
        assert_eq!(resp.tool_calls.len(), 2);
        assert_eq!(resp.tool_calls[0].id, "call_1");
        assert_eq!(resp.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(resp.tool_calls[1].arguments, r#"{"pattern":"x"}"#);
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        let u = resp.usage.expect("usage");
        assert_eq!(
            (u.cache_read_tokens, u.reasoning_tokens),
            (Some(16), Some(3))
        );
    }

    #[tokio::test]
    async fn incomplete_maps_reason_and_missing_terminal_event_is_truncation() {
        let (resp, _) = run(&[
            r#"{"type":"response.output_text.delta","delta":"cut"}"#,
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
        ])
        .await;
        assert_eq!(resp.expect("response").finish_reason, FinishReason::Length);

        let (resp, _) = run(&[
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#,
        ])
        .await;
        assert_eq!(
            resp.expect("response").finish_reason,
            FinishReason::ContentFilter
        );

        let (resp, _) = run(&[r#"{"type":"response.output_text.delta","delta":"x"}"#]).await;
        assert!(resp.expect_err("truncated").is_retryable());
    }

    #[tokio::test]
    async fn failure_events_are_classified_by_code() {
        let (r, _) = run(&[
            r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"oops"}}}"#,
        ])
        .await;
        assert!(r.expect_err("e").is_retryable());
        let (r, _) = run(&[
            r#"{"type":"response.failed","response":{"error":{"code":"invalid_prompt","message":"bad"}}}"#,
        ])
        .await;
        assert!(!r.expect_err("e").is_retryable());
        let (r, _) =
            run(&[r#"{"type":"error","code":"rate_limit_exceeded","message":"slow"}"#]).await;
        assert!(r.expect_err("e").is_retryable());
    }
}
