//! OpenAI-compatible `/chat/completions` streaming adapter (`LiteLLM`, `OpenRouter`, Ollama, vLLM, and custom gateways).

use crate::common::{ToolCallAssembler, fatal, next_event, parse_event, truncated_stream};
use crate::config::{ModelPreset, ProviderEntry};
use bytes::Bytes;
use hrn_edge_http::{HttpClient, StreamReply};
use hrn_kernel::{
    CancelFlag, ChunkSink, FinishReason, HrnError, ImageAttachment, LlmChunk, LlmRequest,
    LlmResponse, Msg, Result, Role, ThinkingLevel, Tier, Usage,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

#[path = "openai_effort.rs"]
mod effort;
use effort::{adjust_rejected_reasoning_effort, clamp_effort_to_supported};

const PROVIDER: &str = "openai-chat";

/// True when `base_url` points at `openrouter.ai` (host-branch for `OpenRouter` wire identity).
pub(crate) fn is_openrouter_url(base_url: &str) -> bool {
    let rest = base_url
        .trim()
        .split_once("://")
        .map_or(base_url.trim(), |(_, r)| r);
    let host_port = rest.split('/').next().unwrap_or("");
    let host = host_port
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "openrouter.ai" || host.ends_with(".openrouter.ai")
}

/// Build HTTP headers for an OpenAI-compatible request, adding `OpenRouter` identity headers and
/// optional session prompt-cache header (`x-grok-conv-id`) when configured.
pub(crate) fn build_headers(
    provider: &ProviderEntry,
    preset: &ModelPreset,
    token: &str,
    session_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        ("accept".to_owned(), "text/event-stream".to_owned()),
    ];
    if !token.is_empty() {
        headers.push(("authorization".to_owned(), format!("Bearer {token}")));
    }
    let env_headers = provider.resolve_env_headers();
    let has_hdr = |name: &str| {
        env_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    };
    if is_openrouter_url(provider.effective_base_url()) {
        if !has_hdr("X-OpenRouter-Title") {
            headers.push(("X-OpenRouter-Title".to_owned(), "hrn".to_owned()));
        }
        if !has_hdr("HTTP-Referer") {
            headers.push((
                "HTTP-Referer".to_owned(),
                "https://github.com/lunateque/hrn".to_owned(),
            ));
        }
    }
    if let (Some(hdr), Some(sid)) = (preset.prompt_cache_session_header.as_deref(), session_id)
        && !hdr.trim().is_empty()
        && !sid.is_empty()
        && !has_hdr(hdr)
    {
        headers.push((hdr.to_owned(), sid.to_owned()));
    }
    headers.extend(env_headers);
    headers
}

/// Map a [`ThinkingLevel`] onto a wire effort string, applying any per-model `reasoning_effort_map`
/// clamp (e.g. xAI `minimal -> low`, `xhigh/max -> high`).
pub(crate) fn mapped_effort(
    level: ThinkingLevel,
    effort_map: &BTreeMap<ThinkingLevel, String>,
) -> Option<&str> {
    if let Some(mapped) = effort_map.get(&level) {
        return Some(mapped.as_str());
    }
    match level {
        ThinkingLevel::Off | ThinkingLevel::Auto => None,
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("xhigh"),
    }
}

/// Stream one request against `<base_url>/chat/completions`.
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
    let base = provider.effective_base_url();
    let url = format!("{base}/chat/completions");
    let openrouter = is_openrouter_url(base);
    let mut payload = encode_request(
        model_id,
        req,
        provider.strip_temperature,
        preset,
        openrouter,
    )?;
    let headers = build_headers(provider, preset, token, req.session_id.as_deref());
    let timeout = effective_stream_timeout(provider, preset);
    let body = Bytes::from(serde_json::to_vec(&payload)?);
    let mut sse = match http.post_stream(&url, &headers, body, timeout).await? {
        StreamReply::Stream(stream) => stream,
        StreamReply::Failed(reply)
            if reply.status == 400
                && adjust_rejected_reasoning_effort(&mut payload, req.thinking, &reply.text()) =>
        {
            let retry_body = Bytes::from(serde_json::to_vec(&payload)?);
            http.post_sse(&url, &headers, retry_body, timeout).await?
        }
        StreamReply::Failed(reply) => return Err(reply.status_error()),
    };
    read_stream(&mut sse, model_id, cancel, sink).await
}

fn effective_stream_timeout(provider: &ProviderEntry, preset: &ModelPreset) -> Duration {
    match (
        provider.timeout_secs,
        preset.stream_idle_timeout_ms.filter(|&ms| ms > 0),
    ) {
        (Some(secs), Some(ms)) => Duration::from_millis(secs.saturating_mul(1000).max(ms)),
        (None, Some(ms)) => Duration::from_millis(ms),
        (secs, None) => Duration::from_secs(secs.unwrap_or(180)),
    }
}

/// Assemble an OpenAI `/chat/completions` SSE stream into a [`LlmResponse`].
pub(crate) async fn read_stream(
    sse: &mut hrn_edge_http::SseStream,
    model_id: &str,
    cancel: &CancelFlag,
    sink: &dyn ChunkSink,
) -> Result<LlmResponse> {
    let mut asm = ChatAssembler::new(model_id.to_owned());
    while let Some(ev) = next_event(sse, cancel).await? {
        let data = ev.data.trim();
        if data.is_empty() {
            continue;
        }
        asm.handle_data(data, sink).await?;
        if asm.done {
            break;
        }
    }
    asm.finish()
}

pub(crate) fn encode_request(
    model_id: &str,
    req: &LlmRequest,
    strip_temp: bool,
    preset: &ModelPreset,
    openrouter: bool,
) -> Result<Value> {
    let anthropic_cache = openrouter && model_id.starts_with("anthropic/");
    let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len());
    for msg in &req.messages {
        encode_msg(
            msg,
            &mut messages,
            preset.replay_reasoning_content,
            anthropic_cache,
        )?;
    }
    let mut body = json!({
        "model": model_id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    let Some(obj) = body.as_object_mut() else {
        return Err(fatal("request body is not an object"));
    };
    if !strip_temp && let Some(temp) = req.temperature {
        obj.insert("temperature".into(), json!(temp));
    }
    if let Some(max) = req.max_tokens {
        obj.insert("max_tokens".into(), json!(max));
    }
    if let Some(tier) = req.service_tier.filter(|t| *t != Tier::None) {
        obj.insert("service_tier".into(), json!(tier.as_str()));
    }
    let supported_efforts = preset
        .thinking_budgets
        .as_ref()
        .or(req.thinking_budgets.as_ref())
        .and_then(|b| b.efforts.as_deref())
        .filter(|e| !e.is_empty());
    let effort = if !matches!(req.thinking, ThinkingLevel::Off | ThinkingLevel::Auto)
        && let Some(efforts) = supported_efforts
    {
        clamp_effort_to_supported(req.thinking, efforts).map(|clamped| {
            ThinkingLevel::parse(clamped)
                .and_then(|lvl| preset.reasoning_effort_map.get(&lvl))
                .map_or(clamped, String::as_str)
        })
    } else {
        mapped_effort(req.thinking, &preset.reasoning_effort_map)
    };
    match preset.thinking_format.as_deref() {
        Some("zai") => {
            if req.thinking == ThinkingLevel::Off {
                obj.insert("thinking".into(), json!({"type": "disabled"}));
            } else if effort.is_some() {
                obj.insert("thinking".into(), json!({"type": "enabled"}));
            }
        }
        Some("openrouter") => {
            if let Some(effort) = effort {
                obj.insert("reasoning".into(), json!({"effort": effort}));
            }
        }
        _ => {
            if let Some(effort) = effort
                && (preset.reasoning == Some(true)
                    || !preset.reasoning_effort_map.is_empty()
                    || (preset.reasoning != Some(false) && supported_efforts.is_some()))
            {
                obj.insert("reasoning_effort".into(), json!(effort));
            }
        }
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        obj.insert("tools".into(), Value::Array(tools));
    }
    Ok(body)
}

fn encode_user_msg(text: &str, images: &[ImageAttachment]) -> Value {
    if images.is_empty() {
        return json!({"role": "user", "content": text});
    }
    let mut parts: Vec<Value> = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    for img in images {
        let url = format!("data:{};base64,{}", img.media_type, img.data_base64);
        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
    }
    json!({"role": "user", "content": parts})
}

fn encode_msg(
    msg: &Msg,
    out: &mut Vec<Value>,
    replay_reasoning: bool,
    anthropic_cache: bool,
) -> Result {
    match msg.role {
        Role::System => {
            if anthropic_cache && !msg.content.is_empty() {
                out.push(json!({
                    "role": "system",
                    "content": [{
                        "type": "text",
                        "text": msg.content,
                        "cache_control": {"type": "ephemeral"},
                    }],
                }));
            } else {
                out.push(json!({"role": "system", "content": msg.content}));
            }
        }
        Role::User => out.push(encode_user_msg(&msg.content, &msg.images)),
        Role::Assistant => {
            if msg.tool_calls.is_empty() {
                if msg.content.is_empty() {
                    // Strict gateways reject an assistant turn with neither text nor calls.
                    return Ok(());
                }
                out.push(json!({"role": "assistant", "content": msg.content}));
            } else {
                let tcs: Vec<Value> = msg
                    .tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {"name": c.name, "arguments": c.arguments},
                        })
                    })
                    .collect();
                let content = if msg.content.is_empty() {
                    Value::Null
                } else {
                    Value::String(msg.content.clone())
                };
                let mut item = json!({
                    "role": "assistant",
                    "content": content,
                    "tool_calls": tcs,
                });
                if replay_reasoning
                    && let Some(r) = msg.reasoning.as_deref().filter(|s| !s.is_empty())
                    && let Some(map) = item.as_object_mut()
                {
                    map.insert("reasoning_content".into(), Value::String(r.to_owned()));
                }
                out.push(item);
            }
        }
        Role::Tool => {
            let id = msg.tool_call_id.as_deref().unwrap_or_default();
            if id.is_empty() {
                return Err(fatal("tool result message without tool_call_id"));
            }
            out.push(json!({"role": "tool", "tool_call_id": id, "content": msg.content}));
            if !msg.images.is_empty() {
                let tool = msg.tool_name.as_deref().unwrap_or("tool");
                let caption = format!("[attached image from {tool}]");
                out.push(encode_user_msg(&caption, &msg.images));
            }
        }
    }
    Ok(())
}

/// Mid-stream `error` payload: either `{"message": ..., "type"/"code": ...}` or a bare string.
fn stream_error(err: &Value) -> HrnError {
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| err.as_str())
        .unwrap_or("upstream error")
        .to_owned();
    let kind = err.get("type").and_then(Value::as_str).unwrap_or_default();
    let code = err.get("code");
    let numeric = code.and_then(Value::as_u64).or_else(|| {
        code.and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok())
    });
    let retryable = matches!(
        kind,
        "server_error" | "rate_limit_exceeded" | "overloaded_error"
    ) || code.and_then(Value::as_str) == Some("rate_limit_exceeded")
        || numeric.is_some_and(|c| c == 408 || c == 429 || c >= 500);
    HrnError::Provider {
        message,
        retryable,
        retry_after_secs: None,
    }
}

struct ChatAssembler {
    model: String,
    content: String,
    reasoning: String,
    calls: ToolCallAssembler,
    usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
    done: bool,
}

impl ChatAssembler {
    fn new(model: String) -> Self {
        Self {
            model,
            content: String::new(),
            reasoning: String::new(),
            calls: ToolCallAssembler::default(),
            usage: None,
            finish_reason: None,
            done: false,
        }
    }

    async fn handle_data(&mut self, data: &str, sink: &dyn ChunkSink) -> Result {
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let val = parse_event(PROVIDER, data)?;
        if let Some(err) = val.get("error").filter(|e| !e.is_null()) {
            return Err(stream_error(err));
        }
        if let Some(u) = val.get("usage").filter(|u| !u.is_null()) {
            self.usage = parse_usage(u).or(self.usage.take());
        }
        // Usage-only trailer chunks have `choices: []`.
        let Some(choice) = val
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return Ok(());
        };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(FinishReason::from_wire(fr));
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some(r) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            && !r.is_empty()
        {
            self.reasoning.push_str(r);
            sink.on_chunk(LlmChunk::ReasoningDelta(r.to_owned())).await;
        }
        if let Some(c) = delta.get("content").and_then(Value::as_str)
            && !c.is_empty()
        {
            self.content.push_str(c);
            sink.on_chunk(LlmChunk::ContentDelta(c.to_owned())).await;
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                self.apply_tool_delta(tc);
            }
        }
        Ok(())
    }

    fn apply_tool_delta(&mut self, tc: &Value) {
        let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
        let id = tc.get("id").and_then(Value::as_str);
        let slot = self.calls.by_index(index, id);
        if let Some(f) = tc.get("function") {
            // The name is sent once; some gateways repeat it on every chunk, so never append.
            if let Some(n) = f.get("name").and_then(Value::as_str)
                && slot.name.is_empty()
            {
                n.clone_into(&mut slot.name);
            }
            if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                slot.args.push_str(a);
            }
        }
    }

    fn finish(self) -> Result<LlmResponse> {
        if !self.done && self.finish_reason.is_none() {
            return Err(truncated_stream(PROVIDER, "finish_reason / [DONE]"));
        }
        let has_calls = !self.calls.is_empty();
        let tool_calls = self.calls.finish(PROVIDER)?;
        let reason = self.finish_reason.unwrap_or(FinishReason::Stop);
        let finish_reason = if has_calls && reason == FinishReason::Stop {
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
            tool_calls,
            usage: self.usage,
            finish_reason,
        })
    }
}

fn parse_usage(u: &Value) -> Option<Usage> {
    let prompt = u.get("prompt_tokens").and_then(Value::as_u64)?;
    let completion = u
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = u
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| prompt.saturating_add(completion));
    let cache_read = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .filter(|&v| v > 0);
    let reasoning = u
        .get("completion_tokens_details")
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
#[path = "openai_chat_tests.rs"]
mod tests;
