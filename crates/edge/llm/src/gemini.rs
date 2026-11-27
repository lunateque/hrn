//! Official Gemini Developer API (`generativelanguage.googleapis.com`) SSE adapter.

use crate::common::{fatal, next_event, parse_event, synthetic_call_id, truncated_stream};
use crate::config::ProviderEntry;
use bytes::Bytes;
use hrn_edge_http::{HttpClient, guard};
use hrn_kernel::{
    CancelFlag, ChunkSink, FinishReason, HrnError, ImageAttachment, LlmChunk, LlmRequest,
    LlmResponse, Result, Role, ThinkingLevel, Tier, ToolCall, Usage,
};
use serde_json::{Value, json};
use std::time::Duration;

const PROVIDER: &str = "gemini";

/// Stream one request against Gemini `streamGenerateContent?alt=sse`.
pub(crate) async fn stream(
    http: &HttpClient,
    provider: &ProviderEntry,
    model_id: &str,
    token: &str,
    req: &LlmRequest,
    cancel: &CancelFlag,
    sink: &dyn ChunkSink,
) -> Result<LlmResponse> {
    let base = provider.effective_base_url();
    guard::check_gemini_base_url(base)?;
    guard::check_model_id(model_id)?;
    // The id is interpolated into the URL path: only plain model-name characters are allowed.
    if model_id.is_empty()
        || !model_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(HrnError::Config(format!(
            "invalid Gemini model id `{model_id}`"
        )));
    }

    let url = format!("{base}/v1beta/models/{model_id}:streamGenerateContent?alt=sse");
    let payload = encode_request(model_id, req, provider.strip_temperature)?;
    let body = Bytes::from(serde_json::to_vec(&payload)?);

    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        ("accept".to_owned(), "text/event-stream".to_owned()),
        ("x-goog-api-key".to_owned(), token.to_owned()),
    ];
    if let Some(tier) = wire_service_tier(req.service_tier) {
        headers.push(("x-goog-service-tier".to_owned(), tier.to_owned()));
    }
    headers.extend(provider.resolve_env_headers());

    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(180));
    let mut sse = http.post_sse(&url, &headers, body, timeout).await?;
    let mut asm = GeminiAssembler::new(model_id.to_owned());
    while let Some(ev) = next_event(&mut sse, cancel).await? {
        let data = ev.data.trim();
        if !data.is_empty() {
            asm.handle_data(data, sink).await?;
        }
    }
    asm.finish()
}

pub(crate) fn wire_service_tier(tier: Option<Tier>) -> Option<&'static str> {
    match tier {
        Some(Tier::Flex) => Some("flex"),
        Some(Tier::Priority | Tier::Ultrafast) => Some("priority"),
        _ => None,
    }
}

pub(crate) fn encode_request(model_id: &str, req: &LlmRequest, strip_temp: bool) -> Result<Value> {
    let mut sys_parts: Vec<&str> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();

    for msg in &req.messages {
        match msg.role {
            Role::System => {
                if !msg.content.trim().is_empty() {
                    sys_parts.push(&msg.content);
                }
            }
            Role::User => {
                let mut parts: Vec<Value> = Vec::with_capacity(msg.images.len() + 1);
                if !msg.content.is_empty() {
                    parts.push(json!({"text": msg.content}));
                }
                for img in &msg.images {
                    parts.push(encode_inline_image(img));
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
            Role::Assistant => {
                let mut parts: Vec<Value> = Vec::new();
                if !msg.content.is_empty() {
                    parts.push(json!({"text": msg.content}));
                }
                for tc in &msg.tool_calls {
                    let args: Value = serde_json::from_str(&tc.arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    let mut part = json!({"functionCall": {"name": tc.name, "args": args}});
                    if let (Some(sig), Some(obj)) = (&tc.signature, part.as_object_mut()) {
                        obj.insert("thoughtSignature".into(), json!(sig));
                    }
                    parts.push(part);
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            Role::Tool => push_tool_result(&mut contents, msg)?,
        }
    }

    let mut gen_cfg = serde_json::Map::new();
    if !strip_temp && let Some(t) = req.temperature {
        gen_cfg.insert("temperature".into(), json!(t));
    }
    if let Some(max) = req.max_tokens {
        gen_cfg.insert("maxOutputTokens".into(), json!(max));
    }
    if let Some(cfg) = thinking_config(req, model_id) {
        gen_cfg.insert("thinkingConfig".into(), cfg);
    }

    let mut body = json!({"contents": contents});
    let Some(obj) = body.as_object_mut() else {
        return Err(fatal("request body is not an object"));
    };
    if !sys_parts.is_empty() {
        obj.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": sys_parts.join("\n\n")}]}),
        );
    }
    if !gen_cfg.is_empty() {
        obj.insert("generationConfig".into(), Value::Object(gen_cfg));
    }
    if let Some(tier) = wire_service_tier(req.service_tier) {
        obj.insert("serviceTier".into(), json!(tier));
    }
    if !req.tools.is_empty() {
        // `parametersJsonSchema` takes full JSON Schema; the OpenAPI-subset `parameters` field
        // rejects keywords such as `additionalProperties` with a 400.
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parametersJsonSchema": t.parameters,
                })
            })
            .collect();
        obj.insert("tools".into(), json!([{"functionDeclarations": decls}]));
    }
    Ok(body)
}

fn encode_inline_image(img: &ImageAttachment) -> Value {
    json!({"inlineData": {"mimeType": img.media_type, "data": img.data_base64}})
}

fn push_tool_result(contents: &mut Vec<Value>, msg: &hrn_kernel::Msg) -> Result {
    let name = msg.tool_name.as_deref().unwrap_or_default();
    if name.is_empty() {
        return Err(fatal(
            "tool result message without tool_name (Gemini matches by name)",
        ));
    }
    let key = if msg.is_error { "error" } else { "output" };
    let mut new_parts = Vec::with_capacity(1 + msg.images.len());
    new_parts.push(json!({"functionResponse": {"name": name, "response": {key: msg.content}}}));
    for img in &msg.images {
        new_parts.push(encode_inline_image(img));
    }
    // Parallel calls need ALL their responses in ONE user turn.
    if let Some(last) = contents.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(parts) = last.get_mut("parts").and_then(Value::as_array_mut)
        && parts
            .iter()
            .all(|p| p.get("functionResponse").is_some() || p.get("inlineData").is_some())
    {
        parts.extend(new_parts);
    } else {
        contents.push(json!({"role": "user", "parts": new_parts}));
    }
    Ok(())
}

/// `Off` disables thinking where the model allows it (2.5 Pro cannot, so the field is omitted).
fn thinking_config(req: &LlmRequest, model_id: &str) -> Option<Value> {
    let level = req.thinking;
    let configured = req.thinking_budgets.as_ref();
    let budget: u32 = match level {
        ThinkingLevel::Off | ThinkingLevel::Auto => {
            return (!model_id.to_ascii_lowercase().contains("pro"))
                .then(|| json!({"thinkingBudget": 0}));
        }
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 6144,
        ThinkingLevel::High => 16_384,
        ThinkingLevel::Xhigh => 24_576,
        ThinkingLevel::Max => 32_768,
    };
    let budget = configured
        .and_then(|b| b.effort_budgets.get(&level).copied())
        .unwrap_or(budget);
    Some(json!({"thinkingBudget": budget, "includeThoughts": true}))
}

fn parse_gemini_usage(u: &Value) -> Option<Usage> {
    let prompt = u.get("promptTokenCount").and_then(Value::as_u64)?;
    let candidates = u
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let thoughts = u
        .get("thoughtsTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // Thinking tokens are billed as output but reported separately.
    let completion = candidates.saturating_add(thoughts);
    let cache_read = u
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .filter(|&v| v > 0);
    let total = u
        .get("totalTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| prompt.saturating_add(completion));
    Some(Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        reasoning_tokens: (thoughts > 0).then_some(thoughts),
        cost_micros: None,
    })
}

/// Mid-stream `{"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", ...}}`.
fn stream_error(err: &Value) -> HrnError {
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("gemini error");
    let code = err.get("code").and_then(Value::as_u64).unwrap_or(0);
    let status = err
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(code, 401 | 403) {
        return HrnError::Auth(message.to_owned());
    }
    let retryable = matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
        || matches!(
            status,
            "RESOURCE_EXHAUSTED" | "UNAVAILABLE" | "INTERNAL" | "DEADLINE_EXCEEDED"
        );
    HrnError::Provider {
        message: format!("{status} {message}").trim().to_owned(),
        retryable,
        retry_after_secs: None,
    }
}

struct GeminiAssembler {
    model: String,
    content: String,
    reasoning: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
    blocked: bool,
}

impl GeminiAssembler {
    fn new(model: String) -> Self {
        Self {
            model,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: None,
            blocked: false,
        }
    }

    async fn handle_data(&mut self, data: &str, sink: &dyn ChunkSink) -> Result {
        let val = parse_event(PROVIDER, data)?;
        if let Some(err) = val.get("error").filter(|e| !e.is_null()) {
            return Err(stream_error(err));
        }
        if let Some(u) = val.get("usageMetadata") {
            self.usage = parse_gemini_usage(u).or(self.usage.take());
        }
        if val
            .get("promptFeedback")
            .and_then(|f| f.get("blockReason"))
            .is_some()
        {
            self.blocked = true;
        }
        let Some(cand) = val
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return Ok(());
        };
        if let Some(fr) = cand.get("finishReason").and_then(Value::as_str) {
            if fr == "MALFORMED_FUNCTION_CALL" {
                return Err(HrnError::Provider {
                    message: "gemini produced a malformed function call".to_owned(),
                    retryable: true,
                    retry_after_secs: None,
                });
            }
            self.finish_reason = Some(FinishReason::from_wire(fr));
        }
        let parts = cand
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array);
        for part in parts.into_iter().flatten() {
            self.apply_part(part, sink).await;
        }
        Ok(())
    }

    async fn apply_part(&mut self, part: &Value, sink: &dyn ChunkSink) {
        let is_thought = part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            if is_thought {
                self.reasoning.push_str(text);
                sink.on_chunk(LlmChunk::ReasoningDelta(text.to_owned()))
                    .await;
            } else {
                self.content.push_str(text);
                sink.on_chunk(LlmChunk::ContentDelta(text.to_owned())).await;
            }
        }
        if let Some(fc) = part.get("functionCall") {
            let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
            self.tool_calls.push(ToolCall {
                id: fc
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map_or_else(synthetic_call_id, str::to_owned),
                name: fc
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: args.to_string(),
                signature: part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    }

    fn finish(self) -> Result<LlmResponse> {
        if self.tool_calls.iter().any(|c| c.name.is_empty()) {
            return Err(fatal("gemini: function call without a name"));
        }
        let reason = match self.finish_reason {
            Some(r) => r,
            None if self.blocked => FinishReason::ContentFilter,
            None => return Err(truncated_stream(PROVIDER, "finishReason")),
        };
        let finish_reason = if !self.tool_calls.is_empty() && reason == FinishReason::Stop {
            FinishReason::ToolCalls
        } else {
            reason
        };
        Ok(LlmResponse {
            model_used: self.model,
            fallback_from: None,
            content: self.content,
            reasoning: self.reasoning,
            thinking_blocks: Vec::new(),
            tool_calls: self.tool_calls,
            usage: self.usage,
            finish_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::VecSink;
    use hrn_kernel::Msg;

    fn request(messages: Vec<Msg>) -> LlmRequest {
        LlmRequest {
            model: None,
            messages,
            tools: Vec::new(),
            thinking: ThinkingLevel::Medium,
            thinking_budgets: None,
            temperature: None,
            max_tokens: None,
            session_id: None,
            service_tier: None,
        }
    }

    #[test]
    fn parallel_tool_results_share_one_user_turn_and_signature_is_echoed() {
        let mut assistant = Msg::assistant("");
        for (id, name, sig) in [("c1", "read", Some("SIG")), ("c2", "grep", None)] {
            assistant.tool_calls.push(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: "{}".into(),
                signature: sig.map(str::to_owned),
            });
        }
        let mut failed = Msg::tool_result("c2", "grep", "boom");
        failed.is_error = true;
        let body = encode_request(
            "gemini-2.5-flash",
            &request(vec![
                Msg::user("go"),
                assistant,
                Msg::tool_result("c1", "read", "text"),
                failed,
            ]),
            false,
        )
        .expect("encode");
        let contents = body["contents"].as_array().expect("contents");
        assert_eq!(contents.len(), 3, "one user turn for both responses");
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "SIG");
        let responses = contents[2]["parts"].as_array().expect("parts");
        assert_eq!(responses.len(), 2);
        assert_eq!(
            responses[0]["functionResponse"]["response"]["output"],
            "text"
        );
        assert_eq!(
            responses[1]["functionResponse"]["response"]["error"],
            "boom"
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn thinking_off_is_omitted_for_pro_models_that_cannot_disable_it() {
        let off = LlmRequest {
            thinking: ThinkingLevel::Off,
            thinking_budgets: None,
            model: None,
            messages: vec![],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            session_id: None,
            service_tier: None,
        };
        assert!(thinking_config(&off, "gemini-2.5-pro").is_none());
        assert_eq!(
            thinking_config(&off, "gemini-2.5-flash"),
            Some(json!({"thinkingBudget": 0}))
        );
    }

    async fn run(events: &[&str]) -> (Result<LlmResponse>, VecSink) {
        let sink = VecSink::default();
        let mut asm = GeminiAssembler::new("gemini".into());
        for e in events {
            if let Err(err) = asm.handle_data(e, &sink).await {
                return (Err(err), sink);
            }
        }
        (asm.finish(), sink)
    }

    #[tokio::test]
    async fn thoughts_text_calls_signature_and_usage_assemble() {
        let (resp, sink) = run(&[
            r#"{"candidates":[{"content":{"parts":[{"text":"plan","thought":true}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"Hello "}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"p":1}},"thoughtSignature":"S1"},{"functionCall":{"name":"grep","args":{}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":4,"thoughtsTokenCount":6,"totalTokenCount":20}}"#,
        ])
        .await;
        let resp = resp.expect("response");
        assert_eq!(sink.reasoning(), "plan");
        assert_eq!(resp.content, "Hello ");
        assert_eq!(resp.tool_calls.len(), 2);
        assert_ne!(resp.tool_calls[0].id, resp.tool_calls[1].id);
        assert_eq!(resp.tool_calls[0].signature.as_deref(), Some("S1"));
        assert_eq!(resp.tool_calls[0].arguments, r#"{"p":1}"#);
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        let u = resp.usage.expect("usage");
        assert_eq!(
            (u.completion_tokens, u.total_tokens, u.reasoning_tokens),
            (10, 20, Some(6))
        );
    }

    #[tokio::test]
    async fn call_ids_stay_unique_across_responses() {
        let ev = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{}}}]},"finishReason":"STOP"}]}"#;
        let a = run(&[ev]).await.0.expect("a");
        let b = run(&[ev]).await.0.expect("b");
        assert_ne!(a.tool_calls[0].id, b.tool_calls[0].id);
    }

    #[tokio::test]
    async fn blocked_prompt_truncation_and_errors() {
        let (resp, _) = run(&[r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#]).await;
        assert_eq!(
            resp.expect("response").finish_reason,
            FinishReason::ContentFilter
        );
        let (resp, _) = run(&[r#"{"candidates":[{"content":{"parts":[{"text":"x"}]}}]}"#]).await;
        assert!(resp.expect_err("truncated").is_retryable());
        let (resp, _) =
            run(&[r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"q"}}"#]).await;
        assert!(resp.expect_err("e").is_retryable());
        let (resp, _) = run(&[r#"{"error":{"code":403,"message":"key"}}"#]).await;
        assert!(matches!(resp, Err(HrnError::Auth(_))));
        let (resp, _) = run(&[r#"{"candidates":["#]).await;
        assert!(!resp.expect_err("malformed").is_retryable());
    }

    #[test]
    fn encodes_user_and_tool_images() {
        let img = ImageAttachment {
            media_type: "image/png".into(),
            data_base64: "CCCC".into(),
            label: None,
            width: None,
            height: None,
        };
        let mut tool = Msg::tool_result("c1", "read_file", "Image");
        tool.images.push(img.clone());
        let body = encode_request(
            "gemini-2.5-flash",
            &request(vec![Msg::user_with_images("see", vec![img]), tool]),
            false,
        )
        .expect("encode");
        let contents = body["contents"].as_array().expect("contents");
        assert_eq!(
            contents[0]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(contents[1]["parts"][1]["inlineData"]["data"], "CCCC");
    }
}
