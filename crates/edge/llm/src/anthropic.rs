//! Anthropic Messages API SSE stream assembler (request encoding lives in `anthropic_wire`).

use crate::anthropic_wire::encode_request_for_host;
use crate::common::{fatal, next_event, parse_event, truncated_stream};
use crate::config::{AuthMode, ProviderEntry};
use bytes::Bytes;
use hrn_edge_http::{HttpClient, SseStream};
use hrn_kernel::{
    CancelFlag, ChunkSink, FinishReason, HrnError, LlmChunk, LlmRequest, LlmResponse, Result,
    ThinkingBlock, ToolCall, Usage,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

/// `anthropic-version` header value.
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

const PROVIDER: &str = "anthropic";

/// Usage object as it appears in `message_start` / `message_delta` (every field optional: later
/// events carry cumulative values only for the fields that changed).
#[derive(Default, Deserialize)]
struct UsageFields {
    #[serde(rename = "input_tokens")]
    input: Option<u64>,
    #[serde(rename = "output_tokens")]
    output: Option<u64>,
    #[serde(rename = "cache_read_input_tokens")]
    cache_read: Option<u64>,
    #[serde(rename = "cache_creation_input_tokens")]
    cache_write: Option<u64>,
}

#[derive(Default)]
struct UsageTotals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    seen: bool,
}

impl UsageTotals {
    fn merge(&mut self, f: &UsageFields) {
        self.seen = true;
        if let Some(v) = f.input {
            self.input = v;
        }
        if let Some(v) = f.output {
            self.output = v;
        }
        if let Some(v) = f.cache_read {
            self.cache_read = v;
        }
        if let Some(v) = f.cache_write {
            self.cache_write = v;
        }
    }

    fn to_usage(&self) -> Usage {
        let prompt = self
            .input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write);
        Usage {
            prompt_tokens: prompt,
            completion_tokens: self.output,
            total_tokens: prompt.saturating_add(self.output),
            cache_read_tokens: (self.cache_read > 0).then_some(self.cache_read),
            cache_write_tokens: (self.cache_write > 0).then_some(self.cache_write),
            reasoning_tokens: None,
            cost_micros: None,
        }
    }
}

/// Stream one request against Anthropic `/v1/messages`.
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
    let url = format!("{base}/v1/messages");
    let direct_anthropic = crate::tier::is_direct_anthropic_url(base);
    let payload = encode_request_for_host(
        model_id,
        req,
        provider.prompt_cache,
        provider.strip_temperature,
        direct_anthropic,
    )?;
    let body = Bytes::from(serde_json::to_vec(&payload)?);

    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        ("accept".to_owned(), "text/event-stream".to_owned()),
        ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
    ];
    match provider.auth_mode {
        AuthMode::Bearer
        | AuthMode::OauthJson
        | AuthMode::Claude
        | AuthMode::Chatgpt
        | AuthMode::KimiCode => {
            headers.push(("authorization".to_owned(), format!("Bearer {token}")));
        }
        AuthMode::Default | AuthMode::ApiKey => {
            headers.push(("x-api-key".to_owned(), token.to_owned()));
        }
        AuthMode::Agy => {
            return Err(HrnError::Config(
                "auth_mode = agy is only valid for agy_acp providers".into(),
            ));
        }
    }
    headers.extend(provider.resolve_env_headers());

    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(180));
    let mut sse =
        crate::common::post_sse_cancellable(http, &url, &headers, body, timeout, cancel).await?;
    read_stream(&mut sse, model_id, false, cancel, sink).await
}

/// Assemble a Messages event stream into a response. `strip_tool_prefix` undoes the one-`_`
/// tool-name prefix of Claude subscription requests.
pub(crate) async fn read_stream(
    sse: &mut SseStream,
    model_id: &str,
    strip_tool_prefix: bool,
    cancel: &CancelFlag,
    sink: &dyn ChunkSink,
) -> Result<LlmResponse> {
    let mut asm = AnthropicAssembler::new(model_id.to_owned());
    asm.strip_tool_prefix = strip_tool_prefix;
    while let Some(ev) = next_event(sse, cancel).await? {
        let data = ev.data.trim();
        if data.is_empty() {
            continue;
        }
        asm.handle_data(data, sink).await?;
        if asm.stopped {
            break;
        }
    }
    asm.finish()
}

/// One content block, keyed by its stream `index`.
enum Block {
    Text,
    Thinking {
        text: String,
        signature: Option<String>,
    },
    Redacted(String),
    Tool {
        id: String,
        name: String,
        args: String,
    },
    Other,
}

struct AnthropicAssembler {
    model: String,
    content: String,
    reasoning: String,
    blocks: BTreeMap<u64, Block>,
    usage: UsageTotals,
    finish_reason: Option<FinishReason>,
    stopped: bool,
    strip_tool_prefix: bool,
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn api_error(val: &Value) -> HrnError {
    let err = val.get("error");
    let kind = err.map_or("", |e| str_field(e, "type"));
    let message = err.map_or("anthropic stream error", |e| {
        e.get("message")
            .and_then(Value::as_str)
            .unwrap_or("anthropic stream error")
    });
    match kind {
        "authentication_error" | "permission_error" => HrnError::Auth(message.to_owned()),
        "overloaded_error" | "rate_limit_error" | "api_error" => HrnError::Provider {
            message: format!("{kind}: {message}"),
            retryable: true,
            retry_after_secs: None,
        },
        _ => fatal(format!("{kind}: {message}")),
    }
}

impl AnthropicAssembler {
    fn new(model: String) -> Self {
        Self {
            model,
            content: String::new(),
            reasoning: String::new(),
            blocks: BTreeMap::new(),
            usage: UsageTotals::default(),
            finish_reason: None,
            stopped: false,
            strip_tool_prefix: false,
        }
    }

    async fn handle_data(&mut self, data: &str, sink: &dyn ChunkSink) -> Result {
        let val = parse_event(PROVIDER, data)?;
        let index = val.get("index").and_then(Value::as_u64).unwrap_or(0);
        match str_field(&val, "type") {
            "message_start" => {
                if let Some(u) = val.get("message").and_then(|m| m.get("usage"))
                    && let Ok(parsed) = serde_json::from_value::<UsageFields>(u.clone())
                {
                    self.usage.merge(&parsed);
                }
            }
            "content_block_start" => {
                let block = val.get("content_block").unwrap_or(&Value::Null);
                self.start_block(index, block, sink).await;
            }
            "content_block_delta" => {
                if let Some(delta) = val.get("delta") {
                    self.apply_delta(index, delta, sink).await;
                }
            }
            "message_delta" => {
                if let Some(sr) = val.get("delta").map(|d| str_field(d, "stop_reason"))
                    && !sr.is_empty()
                {
                    self.finish_reason = Some(FinishReason::from_wire(sr));
                }
                if let Some(u) = val.get("usage")
                    && let Ok(parsed) = serde_json::from_value::<UsageFields>(u.clone())
                {
                    self.usage.merge(&parsed);
                }
            }
            "message_stop" => self.stopped = true,
            "error" => return Err(api_error(&val)),
            // `ping`, `content_block_stop` and future event types carry nothing we need.
            _ => {}
        }
        Ok(())
    }

    async fn start_block(&mut self, index: u64, block: &Value, sink: &dyn ChunkSink) {
        let parsed = match str_field(block, "type") {
            "text" => {
                let initial = str_field(block, "text");
                if !initial.is_empty() {
                    self.content.push_str(initial);
                    sink.on_chunk(LlmChunk::ContentDelta(initial.to_owned()))
                        .await;
                }
                Block::Text
            }
            "thinking" => Block::Thinking {
                text: str_field(block, "thinking").to_owned(),
                signature: Some(str_field(block, "signature").to_owned()).filter(|s| !s.is_empty()),
            },
            "redacted_thinking" => Block::Redacted(str_field(block, "data").to_owned()),
            "tool_use" => {
                let name = str_field(block, "name");
                let name = if self.strip_tool_prefix {
                    name.strip_prefix('_').unwrap_or(name)
                } else {
                    name
                };
                Block::Tool {
                    id: str_field(block, "id").to_owned(),
                    name: name.to_owned(),
                    args: String::new(),
                }
            }
            _ => Block::Other,
        };
        self.blocks.insert(index, parsed);
    }

    async fn apply_delta(&mut self, index: u64, delta: &Value, sink: &dyn ChunkSink) {
        match str_field(delta, "type") {
            "text_delta" => {
                let text = str_field(delta, "text");
                if !text.is_empty() {
                    self.content.push_str(text);
                    sink.on_chunk(LlmChunk::ContentDelta(text.to_owned())).await;
                }
            }
            "thinking_delta" => {
                let think = str_field(delta, "thinking");
                if !think.is_empty() {
                    if let Some(Block::Thinking { text, .. }) = self.blocks.get_mut(&index) {
                        text.push_str(think);
                    }
                    self.reasoning.push_str(think);
                    sink.on_chunk(LlmChunk::ReasoningDelta(think.to_owned()))
                        .await;
                }
            }
            "signature_delta" => {
                if let Some(Block::Thinking { signature, .. }) = self.blocks.get_mut(&index) {
                    signature
                        .get_or_insert_with(String::new)
                        .push_str(str_field(delta, "signature"));
                }
            }
            "input_json_delta" => {
                if let Some(Block::Tool { args, .. }) = self.blocks.get_mut(&index) {
                    args.push_str(str_field(delta, "partial_json"));
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> Result<LlmResponse> {
        if !self.stopped {
            return Err(truncated_stream(PROVIDER, "message_stop"));
        }
        let mut tool_calls = Vec::new();
        let mut thinking_blocks = Vec::new();
        for block in self.blocks.into_values() {
            match block {
                Block::Tool { id, name, args } => {
                    if id.is_empty() || name.is_empty() {
                        return Err(fatal("anthropic: tool_use block without id or name"));
                    }
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: if args.trim().is_empty() {
                            "{}".to_owned()
                        } else {
                            args
                        },
                        signature: None,
                    });
                }
                // An unsigned thinking block cannot be replayed, so it is not kept.
                Block::Thinking {
                    text,
                    signature: Some(signature),
                } => {
                    thinking_blocks.push(ThinkingBlock {
                        model: self.model.clone(),
                        text,
                        signature: Some(signature),
                        redacted: None,
                    });
                }
                Block::Redacted(data) => thinking_blocks.push(ThinkingBlock {
                    model: self.model.clone(),
                    text: String::new(),
                    signature: None,
                    redacted: Some(data),
                }),
                Block::Thinking { .. } | Block::Text | Block::Other => {}
            }
        }
        let reason = self.finish_reason.unwrap_or(FinishReason::Stop);
        let finish_reason = if !tool_calls.is_empty() && reason == FinishReason::Stop {
            FinishReason::ToolCalls
        } else {
            reason
        };
        Ok(LlmResponse {
            model_used: self.model,
            fallback_from: None,
            content: self.content,
            reasoning: self.reasoning,
            thinking_blocks,
            tool_calls,
            usage: self.usage.seen.then(|| self.usage.to_usage()),
            finish_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::VecSink;

    async fn run(events: &[&str]) -> (Result<LlmResponse>, VecSink) {
        let sink = VecSink::default();
        let mut asm = AnthropicAssembler::new("claude-x".into());
        for e in events {
            if let Err(err) = asm.handle_data(e, &sink).await {
                return (Err(err), sink);
            }
        }
        (asm.finish(), sink)
    }

    const FULL_STREAM: [&str; 14] = [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":40,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me "}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"look"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG=="}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Reading."}}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_2","name":"grep","input":{}}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":55}}"#,
        r#"{"type":"message_stop"}"#,
    ];

    #[tokio::test]
    async fn full_stream_with_signed_thinking_and_interleaved_tool_blocks() {
        let (resp, sink) = run(&FULL_STREAM).await;
        let resp = resp.expect("response");
        assert_eq!(resp.content, "Reading.");
        assert_eq!(sink.content(), "Reading.");
        assert_eq!(sink.reasoning(), "let me look");
        assert_eq!(resp.thinking_blocks.len(), 1);
        let tb = &resp.thinking_blocks[0];
        assert_eq!(
            (tb.text.as_str(), tb.signature.as_deref()),
            ("let me look", Some("SIG=="))
        );
        assert_eq!(tb.model, "claude-x");
        assert_eq!(resp.tool_calls.len(), 2);
        // Deltas for block 2 arrived after block 3 started: they must still land on block 2.
        assert_eq!(resp.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(resp.tool_calls[1].arguments, "{}");
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        let usage = resp.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 140);
        assert_eq!(usage.completion_tokens, 55);
        assert_eq!(usage.cache_read_tokens, Some(40));
    }

    #[tokio::test]
    async fn stream_cut_before_message_stop_is_a_retryable_truncation() {
        let (resp, _) = run(&FULL_STREAM[..8]).await;
        let err = resp.expect_err("truncated");
        assert!(err.is_retryable(), "{err}");
    }

    #[tokio::test]
    async fn malformed_event_and_error_events_are_classified() {
        let (resp, _) = run(&[r#"{"type":"content_block_delta","index":0,"delta":"#]).await;
        assert!(!resp.expect_err("malformed").is_retryable());

        let overloaded =
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        assert!(run(&[overloaded]).await.0.expect_err("e").is_retryable());
        let invalid =
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#;
        assert!(!run(&[invalid]).await.0.expect_err("e").is_retryable());
        let auth = r#"{"type":"error","error":{"type":"authentication_error","message":"no"}}"#;
        assert!(matches!(run(&[auth]).await.0, Err(HrnError::Auth(_))));
    }

    #[tokio::test]
    async fn redacted_thinking_is_kept_and_unsigned_thinking_dropped() {
        let (resp, _) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"ENC"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"x"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .await;
        let resp = resp.expect("response");
        assert_eq!(resp.thinking_blocks.len(), 1);
        assert_eq!(resp.thinking_blocks[0].redacted.as_deref(), Some("ENC"));
        assert!(resp.usage.is_none());
        assert_eq!(resp.finish_reason, FinishReason::Stop);
    }

    #[tokio::test]
    async fn subscription_tool_prefix_round_trips() {
        let names = ["read_file", "_private", "web_search"];
        let sink = VecSink::default();
        let mut asm = AnthropicAssembler::new("claude-x".into());
        asm.strip_tool_prefix = true;
        for (index, name) in names.iter().enumerate() {
            let wire = crate::claude_code::wire_tool_name(name);
            let event = serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": format!("toolu_{index}"), "name": wire},
            });
            asm.handle_data(&event.to_string(), &sink)
                .await
                .expect("event");
        }
        asm.handle_data(r#"{"type":"message_stop"}"#, &sink)
            .await
            .expect("stop");
        let resp = asm.finish().expect("response");
        let decoded: Vec<&str> = resp.tool_calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(decoded, names);
    }
}
