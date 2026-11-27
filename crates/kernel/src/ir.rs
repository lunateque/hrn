//! Canonical provider-agnostic message IR, tool specifications, and LLM telemetry types.

use crate::SessionEffect;
use crate::thinking::{ThinkingBudgets, ThinkingLevel};
use crate::tier::Tier;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Conversation message role in canonical IR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// System prompt or injected context frame.
    System,
    /// End-user prompt or steer message.
    User,
    /// Assistant reply (may carry text, reasoning, and/or tool calls).
    Assistant,
    /// Result of a single tool invocation.
    Tool,
}

/// One tool invocation emitted by an assistant response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-supplied call identifier.
    pub id: String,
    /// Registered tool name.
    pub name: String,
    /// Raw JSON string of arguments.
    pub arguments: String,
    /// Opaque provider continuation token that must be echoed back with this call
    /// (Gemini `thoughtSignature`); `None` for providers that do not use one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// One reasoning block produced by a provider, kept so signed blocks can be replayed verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// Upstream model id that produced the block; signatures are only valid for that model.
    pub model: String,
    /// Visible reasoning text (empty for redacted blocks).
    pub text: String,
    /// Provider signature authenticating `text` (Anthropic `signature_delta`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Opaque payload of a `redacted_thinking` block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<String>,
}

/// One inline image attached to a user prompt or multimodal tool result (`png`, `jpeg`, `webp`, `gif`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    /// MIME media type (e.g. `"image/png"`, `"image/jpeg"`, `"image/webp"`).
    pub media_type: String,
    /// Standard base64-encoded image bytes.
    pub data_base64: String,
    /// Optional human-friendly label or source path (e.g. `"clipboard.png"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional image width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Optional image height in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// Canonical message in the provider-agnostic IR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Msg {
    /// Message role.
    pub role: Role,
    /// Visible text content (or tool output).
    pub content: String,
    /// Optional inline image attachments (user uploads/pastes or multimodal tool results).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
    /// Optional reasoning/thinking trace for display (never replayed on its own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Signed reasoning blocks to replay to the producing model (dropped on provider switch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_blocks: Vec<ThinkingBlock>,
    /// Tool calls attached to an `Assistant` message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Tool call identifier for `Role::Tool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool name for `Role::Tool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Whether a `Role::Tool` message carries a failed tool execution.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// Optional metadata tag for compaction (e.g., canonical path + line range for `read_file`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_key: Option<String>,
}

impl Msg {
    fn new(role: Role, content: String) -> Self {
        Self {
            role,
            content,
            images: Vec::new(),
            reasoning: None,
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            read_key: None,
        }
    }

    /// Construct a `System` message.
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content.into())
    }

    /// Construct a `User` message.
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content.into())
    }

    /// Construct a `User` message with inline image attachments.
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageAttachment>) -> Self {
        Self {
            images,
            ..Self::new(Role::User, content.into())
        }
    }

    /// Construct an `Assistant` text message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content.into())
    }

    /// Construct an `Assistant` message with tool calls.
    pub fn assistant_tool_calls(
        content: impl Into<String>,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            reasoning,
            tool_calls,
            ..Self::new(Role::Assistant, content.into())
        }
    }

    /// Construct a `Tool` result message.
    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: Some(call_id.into()),
            tool_name: Some(name.into()),
            ..Self::new(Role::Tool, content.into())
        }
    }
}

/// JSON Schema specification advertised to the LLM for a tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Unique tool name.
    pub name: String,
    /// Concise model-facing description.
    pub description: String,
    /// JSON Schema object describing accepted arguments.
    pub parameters: Value,
}

/// Side-effect classification governing parallel batching vs sequential barriers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    /// Pure read-only inspection safe to run concurrently in a batch.
    ReadOnly,
    /// Mutates files, processes, or external state; acts as a sequential barrier.
    #[default]
    Mutating,
}

/// Output returned by a tool execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Text or JSON content sent back to the model.
    pub content: String,
    /// Whether this output represents a tool error.
    pub is_error: bool,
    /// Optional unified diff for TUI preview (`edit` / `write`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Optional supersede key (`<canonical_path>:<start>-<end>`) for Tier 1 compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_key: Option<String>,
    /// Optional inline image attachments (e.g. `read_file` on images, screenshots).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
    /// Deferred tools this call activated (`tool_search`); the runtime records them on the
    /// session so their schemas join the function list from the next step on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activated_tools: Vec<String>,
    /// A change to the session's DAG this call asks for, applied once its results are stored
    /// (`checkpoint` / `rewind`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_effect: Option<SessionEffect>,
    /// Optional structured tool metadata (e.g. `local://` spill locator from `eval`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl ToolOutput {
    /// Build a successful text output.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            diff: None,
            read_key: None,
            images: Vec::new(),
            activated_tools: Vec::new(),
            session_effect: None,
            meta: None,
        }
    }

    /// Build a successful JSON output.
    pub fn ok_json<T: Serialize>(value: &T) -> Self {
        match serde_json::to_string_pretty(value) {
            Ok(text) => Self::ok(text),
            Err(err) => Self::err(format!("failed to serialize tool output: {err}")),
        }
    }

    /// Build an error output.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            content: message.into(),
            is_error: true,
            diff: None,
            read_key: None,
            images: Vec::new(),
            activated_tools: Vec::new(),
            session_effect: None,
            meta: None,
        }
    }
}

/// Normalized completion reason across providers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Normal end of turn.
    #[default]
    Stop,
    /// Model requested one or more tool calls.
    ToolCalls,
    /// Output truncated by max-token limit.
    Length,
    /// Blocked by content/safety filter.
    ContentFilter,
    /// Provider returned a terminal error status.
    Error,
    /// Stream was aborted or cancelled before completion.
    Aborted,
    /// Unknown or provider-specific finish reason.
    Other,
}

impl FinishReason {
    /// Normalize provider wire finish reasons (`stop`, `end_turn`, `tool_use`, `max_tokens`, etc.).
    pub fn from_wire(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "stop" | "end_turn" | "completed" | "stop_sequence" => Self::Stop,
            "tool_calls" | "tool_use" | "function_call" => Self::ToolCalls,
            "length"
            | "max_tokens"
            | "incomplete"
            | "max_output_tokens"
            | "model_context_window_exceeded" => Self::Length,
            "content_filter" | "safety" | "recitation" | "refusal" | "blocklist"
            | "prohibited_content" | "spii" => Self::ContentFilter,
            "error" | "failed" => Self::Error,
            "aborted" | "cancelled" | "canceled" => Self::Aborted,
            _ => Self::Other,
        }
    }
}

/// Normalized token usage telemetry for one LLM call or accumulated turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Total prompt/input tokens (always inclusive of cached read/write tokens).
    pub prompt_tokens: u64,
    /// Generated completion/output tokens.
    pub completion_tokens: u64,
    /// Total tokens (`prompt_tokens + completion_tokens`).
    pub total_tokens: u64,
    /// Prompt tokens served from provider cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written into provider cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Reasoning/thinking output tokens when reported separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Cost in millionths of a US dollar; `None` when the model has no configured price or is
    /// billed by a subscription.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
}

impl Usage {
    /// Accumulate another call's usage into `self`.
    pub fn accumulate(&mut self, other: Self) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        if let Some(v) = other.cache_read_tokens {
            let slot = self.cache_read_tokens.get_or_insert(0);
            *slot = slot.saturating_add(v);
        }
        if let Some(v) = other.cache_write_tokens {
            let slot = self.cache_write_tokens.get_or_insert(0);
            *slot = slot.saturating_add(v);
        }
        if let Some(v) = other.reasoning_tokens {
            let slot = self.reasoning_tokens.get_or_insert(0);
            *slot = slot.saturating_add(v);
        }
        if let Some(v) = other.cost_micros {
            let slot = self.cost_micros.get_or_insert(0);
            *slot = slot.saturating_add(v);
        }
    }
}

/// What a model costs, in US dollars per million tokens (so one token costs that many
/// millionths of a dollar).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    /// Uncached prompt tokens.
    pub input: f64,
    /// Completion tokens (reasoning tokens are billed as output and already counted in them).
    pub output: f64,
    /// Prompt tokens served from cache (default: the `input` price).
    #[serde(default)]
    pub cache_read: Option<f64>,
    /// Prompt tokens written into cache (default: the `input` price).
    #[serde(default)]
    pub cache_write: Option<f64>,
}

#[expect(
    clippy::cast_precision_loss,
    reason = "token counts stay far below 2^52, where f64 is exact"
)]
const fn tokens_f64(tokens: u64) -> f64 {
    tokens as f64
}

impl ModelPrice {
    /// Every price is finite and not negative.
    pub fn is_valid(&self) -> bool {
        [
            Some(self.input),
            Some(self.output),
            self.cache_read,
            self.cache_write,
        ]
        .into_iter()
        .flatten()
        .all(|p| p.is_finite() && p >= 0.0)
    }

    /// Cost of `usage` in millionths of a dollar, rounded to the nearest.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "prices are validated finite and non-negative; the rounded sum is far below u64::MAX"
    )]
    pub fn cost_micros(&self, usage: &Usage) -> u64 {
        let read = usage.cache_read_tokens.unwrap_or(0);
        let write = usage.cache_write_tokens.unwrap_or(0);
        let fresh = usage
            .prompt_tokens
            .saturating_sub(read)
            .saturating_sub(write);
        let micros = tokens_f64(fresh) * self.input
            + tokens_f64(read) * self.cache_read.unwrap_or(self.input)
            + tokens_f64(write) * self.cache_write.unwrap_or(self.input)
            + tokens_f64(usage.completion_tokens) * self.output;
        micros.round() as u64
    }
}

/// What one model used on one UTC day: a line of `hrn stats`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageRow {
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    /// Upstream model id that served the calls.
    pub model: String,
    /// Top-level sessions that made calls (a subagent's calls count toward its tokens and cost
    /// but not as a session).
    pub sessions: u64,
    /// LLM calls.
    pub calls: u64,
    /// Summed usage; `cost_micros` is `None` when none of the calls was priced.
    pub usage: Usage,
}

/// Capabilities declared by an LLM provider/model adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// Supports native tool/function calling.
    pub tools: bool,
    /// Supports extended thinking / reasoning effort.
    pub thinking: bool,
    /// Supports explicit prompt caching (`cache_control` or `prompt_cache_key`).
    pub prompt_cache: bool,
    /// Context window size in tokens (used for Tier 3 compaction threshold).
    pub context_window: u64,
    /// Optional extended context window size when `/extended-context` is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_window: Option<u64>,
}

/// One provider-agnostic LLM chat request.
#[derive(Clone, Debug)]
pub struct LlmRequest {
    /// Model alias, preset, or role name (`None` uses default role).
    pub model: Option<String>,
    /// Ordered conversation messages in canonical IR.
    pub messages: Vec<Msg>,
    /// Tool specs exposed for this step.
    pub tools: Vec<ToolSpec>,
    /// Requested thinking / reasoning effort level.
    pub thinking: ThinkingLevel,
    /// The serving model's thinking metadata (`None`: no clamp table, every level supported).
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Optional sampling temperature.
    pub temperature: Option<f32>,
    /// Optional output token cap.
    pub max_tokens: Option<u32>,
    /// Stable session identifier (mapped to `prompt_cache_key` where supported).
    pub session_id: Option<String>,
    /// Optional upstream service tier (`[llm.tier]` / `/fast`).
    pub service_tier: Option<Tier>,
}

/// Streaming chunk emitted during an LLM call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmChunk {
    /// Visible assistant text delta.
    ContentDelta(String),
    /// Reasoning / thinking delta.
    ReasoningDelta(String),
}

/// Completed LLM response after streaming finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmResponse {
    /// Resolved model identifier that served the call.
    pub model_used: String,
    /// Original model if fallback occurred.
    pub fallback_from: Option<String>,
    /// Full visible text content.
    pub content: String,
    /// Full reasoning trace, if any.
    pub reasoning: String,
    /// Signed reasoning blocks to replay to the producing model.
    pub thinking_blocks: Vec<ThinkingBlock>,
    /// Requested tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage telemetry.
    pub usage: Option<Usage>,
    /// Normalized stop reason.
    pub finish_reason: FinishReason,
}

/// Provider-native remote compaction result stored inside [`crate::DagPayload::Summary`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCompaction {
    /// Human-readable compact summary text for display and fallback projection.
    pub summary_text: String,
    /// Opaque provider replay state echoed back on subsequent turns (e.g. OpenAI Responses item).
    pub preserve: Value,
}

/// Summary of a configured model exposed in CLI/TUI selectors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Short alias or preset name.
    pub alias: String,
    /// Upstream provider model id.
    pub id: String,
    /// Configured provider name.
    pub provider: String,
    /// Whether this entry is the default.
    pub default: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_split_cached_from_fresh_input_and_default_to_the_input_rate() {
        let price = ModelPrice {
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: None,
        };
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 100_000,
            cache_read_tokens: Some(400_000),
            cache_write_tokens: Some(100_000),
            ..Usage::default()
        };
        // 500k fresh at $3 + 400k read at $0.30 + 100k written at the input rate + 100k out at $15
        assert_eq!(
            price.cost_micros(&usage),
            1_500_000 + 120_000 + 300_000 + 1_500_000
        );
        assert_eq!(price.cost_micros(&Usage::default()), 0);
    }

    #[test]
    fn accumulating_adds_cost_only_when_a_call_was_priced() {
        let mut total = Usage::default();
        total.accumulate(Usage::default());
        assert_eq!(total.cost_micros, None);
        total.accumulate(Usage {
            cost_micros: Some(5),
            ..Usage::default()
        });
        total.accumulate(Usage::default());
        total.accumulate(Usage {
            cost_micros: Some(7),
            ..Usage::default()
        });
        assert_eq!(total.cost_micros, Some(12));
    }
}
