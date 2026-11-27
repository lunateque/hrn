//! Runtime events as omp's RPC event stream: one [`Translator`] per connection turns the
//! runtime's flat `AgentEvent`s into `agent_*`, `turn_*`, `message_*` and `tool_execution_*`
//! frames with the nesting and ids a host expects.
//!
//! Known gap: the runtime reports a step's tool calls only after the model finished the step, so
//! an assistant message carries text and thinking blocks but no `toolCall` blocks; the calls
//! arrive as `tool_execution_*` frames and `toolResult` messages.

use crate::wire::now_ms;
use crate::wire::put;
use hrn_kernel::{AgentEvent, FinishReason, Usage};
use serde_json::{Value, json};
use std::collections::HashMap;

/// One content block of the assistant message being streamed.
enum Block {
    Thinking(String),
    Text(String),
}

impl Block {
    fn json(&self) -> Value {
        match self {
            Self::Thinking(t) => json!({ "type": "thinking", "thinking": t }),
            Self::Text(t) => json!({ "type": "text", "text": t }),
        }
    }
}

/// The assistant message being streamed.
struct Step {
    id: String,
    blocks: Vec<Block>,
    /// The block `blocks` last holds, when one is still receiving deltas.
    streaming: Option<usize>,
}

/// The `stopReason` omp uses for a finish reason.
const fn stop_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::ToolCalls => "toolUse",
        FinishReason::Length => "length",
        FinishReason::ContentFilter | FinishReason::Error => "error",
        FinishReason::Aborted => "aborted",
        FinishReason::Stop | FinishReason::Other => "stop",
    }
}

/// `Usage` as omp spells it; `None` when the runtime recorded none.
pub(crate) fn usage_json(usage: Option<&Usage>) -> Value {
    let Some(usage) = usage else {
        return Value::Null;
    };
    let cache_read = usage.cache_read_tokens.unwrap_or(0);
    let cache_write = usage.cache_write_tokens.unwrap_or(0);
    let input = usage.prompt_tokens.saturating_sub(cache_read + cache_write);
    #[expect(
        clippy::cast_precision_loss,
        reason = "a price is displayed, not summed"
    )]
    let total = usage.cost_micros.unwrap_or(0) as f64 / 1_000_000.0;
    json!({
        "input": input,
        "output": usage.completion_tokens,
        "cacheRead": cache_read,
        "cacheWrite": cache_write,
        "totalTokens": usage.total_tokens,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": total },
    })
}

/// The content index the step is streaming into.
fn index_of(step: &Step) -> usize {
    step.streaming
        .unwrap_or(step.blocks.len().saturating_sub(1))
}

/// Turns runtime events into RPC frames.
#[expect(
    clippy::struct_excessive_bools,
    reason = "stream state flags: each tracks one independent protocol position"
)]
#[derive(Default)]
pub(crate) struct Translator {
    ids: u64,
    /// Send `message_update` without the growing snapshots.
    delta: bool,
    /// A `Compacted` event belongs to the `compact` command, which reports its own frame pair.
    manual: bool,
    messages: Vec<Value>,
    results: Vec<Value>,
    step: Option<Step>,
    turn_open: bool,
    llm_done: bool,
    last_assistant: Option<Value>,
    calls: HashMap<String, (String, Value)>,
    partial: HashMap<String, String>,
    /// Usage of the newest step, for `contextUsage`.
    last_usage: Option<Usage>,
    /// Output tokens per second of the newest step.
    last_speed: Option<f64>,
}

impl Translator {
    pub(crate) fn set_delta(&mut self, delta: bool) {
        self.delta = delta;
    }

    /// Route `Compacted` events to [`Translator::take_compaction`] instead of the stream.
    pub(crate) fn set_manual_compaction(&mut self, manual: bool) {
        self.manual = manual;
    }

    /// The newest step's usage, for `get_state.contextUsage`.
    pub(crate) const fn last_usage(&self) -> Option<Usage> {
        self.last_usage
    }

    /// The newest step's output rate.
    pub(crate) const fn last_speed(&self) -> Option<f64> {
        self.last_speed
    }

    fn next_id(&mut self, kind: &str) -> String {
        self.ids += 1;
        format!("{kind}-{}", self.ids)
    }

    /// Close the previous turn when its step has ended and a new one begins.
    fn begin_turn(&mut self, frames: &mut Vec<Value>) {
        if self.turn_open && self.llm_done {
            self.end_turn(frames);
        }
        if !self.turn_open {
            self.turn_open = true;
            frames.push(json!({ "type": "turn_start" }));
        }
    }

    fn end_turn(&mut self, frames: &mut Vec<Value>) {
        frames.push(json!({
            "type": "turn_end",
            "message": self.last_assistant.clone().unwrap_or(Value::Null),
            "toolResults": std::mem::take(&mut self.results),
        }));
        self.turn_open = false;
        self.llm_done = false;
    }

    fn open_step(&mut self, frames: &mut Vec<Value>) {
        self.begin_turn(frames);
        if self.step.is_some() {
            return;
        }
        let id = self.next_id("msg");
        frames.push(json!({
            "type": "message_start",
            "messageId": id,
            "message": { "role": "assistant", "content": [], "timestamp": now_ms() },
        }));
        self.step = Some(Step {
            id,
            blocks: Vec::new(),
            streaming: None,
        });
    }

    fn snapshot(step: &Step) -> Value {
        json!({
            "role": "assistant",
            "content": step.blocks.iter().map(Block::json).collect::<Vec<_>>(),
            "timestamp": now_ms(),
        })
    }

    /// The `message_update` frame carrying `event`.
    fn update(&self, step: &Step, mut event: Value) -> Value {
        if self.delta {
            put(&mut event, "messageId", json!(step.id));
            put(&mut event, "message", json!({ "role": "assistant" }));
            return json!({
                "type": "message_update",
                "messageId": step.id,
                "message": { "role": "assistant" },
                "assistantMessageEvent": event,
            });
        }
        let snapshot = Self::snapshot(step);
        put(&mut event, "partial", snapshot.clone());
        json!({
            "type": "message_update",
            "messageId": step.id,
            "message": snapshot,
            "assistantMessageEvent": event,
        })
    }

    /// End the block being streamed, if any.
    fn close_block(&mut self, frames: &mut Vec<Value>) {
        let Some(index) = self.step.as_ref().and_then(|s| s.streaming) else {
            return;
        };
        if let Some(step) = self.step.as_mut() {
            step.streaming = None;
        }
        let event = match self.step.as_ref().and_then(|s| s.blocks.get(index)) {
            Some(Block::Thinking(t)) => {
                json!({ "type": "thinking_end", "contentIndex": index, "content": t })
            }
            Some(Block::Text(t)) => {
                json!({ "type": "text_end", "contentIndex": index, "content": t })
            }
            None => return,
        };
        if let Some(step) = self.step.as_ref() {
            let frame = self.update(step, event);
            frames.push(frame);
        }
    }

    fn delta_frames(&mut self, text: &str, thinking: bool, frames: &mut Vec<Value>) {
        self.open_step(frames);
        let continues = self.step.as_ref().is_some_and(|s| {
            s.streaming.is_some_and(|index| {
                matches!(
                    (s.blocks.get(index), thinking),
                    (Some(Block::Thinking(_)), true) | (Some(Block::Text(_)), false)
                )
            })
        });
        if !continues {
            self.close_block(frames);
            if let Some(step) = self.step.as_mut() {
                step.blocks.push(if thinking {
                    Block::Thinking(String::new())
                } else {
                    Block::Text(String::new())
                });
                step.streaming = Some(step.blocks.len() - 1);
            }
            if let Some(step) = self.step.as_ref() {
                let kind = if thinking {
                    "thinking_start"
                } else {
                    "text_start"
                };
                let event = json!({ "type": kind, "contentIndex": index_of(step) });
                let frame = self.update(step, event);
                frames.push(frame);
            }
        }
        if let Some(step) = self.step.as_mut()
            && let Some(Block::Thinking(t) | Block::Text(t)) = step.blocks.last_mut()
        {
            t.push_str(text);
        }
        if let Some(step) = self.step.as_ref() {
            let kind = if thinking {
                "thinking_delta"
            } else {
                "text_delta"
            };
            let event = json!({
                "type": kind,
                "contentIndex": index_of(step),
                "delta": text,
            });
            let frame = self.update(step, event);
            frames.push(frame);
        }
    }

    /// Close the assistant message of the current step, adding `extra` to it.
    fn finish_message(&mut self, frames: &mut Vec<Value>, extra: &Value) {
        self.open_step(frames);
        self.close_block(frames);
        let Some(step) = self.step.take() else {
            return;
        };
        let mut message = Self::snapshot(&step);
        if let (Some(target), Some(extra)) = (message.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        frames.push(json!({
            "type": "message_end",
            "messageId": step.id,
            "message": message,
        }));
        self.messages.push(message.clone());
        self.last_assistant = Some(message);
    }

    fn user_message(&mut self, text: &str, frames: &mut Vec<Value>) {
        let id = self.next_id("msg");
        let message = json!({
            "role": "user",
            "content": [{ "type": "text", "text": text }],
            "timestamp": now_ms(),
        });
        frames.push(json!({ "type": "message_start", "messageId": id, "message": message }));
        frames.push(json!({ "type": "message_end", "messageId": id, "message": message }));
        self.messages.push(message);
    }

    fn end_run(&mut self, frames: &mut Vec<Value>) {
        if self.turn_open {
            self.end_turn(frames);
        }
        frames.push(json!({
            "type": "agent_end",
            "messages": std::mem::take(&mut self.messages),
            "isTerminal": true,
            "yielded": true,
        }));
        self.calls.clear();
        self.partial.clear();
    }

    /// The frames `event` produces, in order.
    #[expect(
        clippy::too_many_lines,
        reason = "one flat arm per runtime event; splitting the table would hide the mapping"
    )]
    pub(crate) fn handle(&mut self, event: &AgentEvent) -> Vec<Value> {
        let mut frames = Vec::new();
        match event {
            AgentEvent::TurnStart { user_text, .. } => {
                self.messages.clear();
                self.results.clear();
                self.step = None;
                self.turn_open = false;
                self.llm_done = false;
                frames.push(json!({ "type": "agent_start" }));
                self.begin_turn(&mut frames);
                self.user_message(user_text, &mut frames);
            }
            AgentEvent::SteerInjected { text, .. } => self.user_message(text, &mut frames),
            AgentEvent::ThinkingDelta { delta } => self.delta_frames(delta, true, &mut frames),
            AgentEvent::TextDelta { delta } => self.delta_frames(delta, false, &mut frames),
            AgentEvent::LlmCall {
                model,
                finish_reason,
                usage,
                duration_ms,
                ..
            } => {
                self.begin_turn(&mut frames);
                self.last_usage = *usage;
                if let Some(usage) = usage.filter(|_| *duration_ms > 0) {
                    #[expect(clippy::cast_precision_loss, reason = "a rate is displayed")]
                    let per_second = usage.completion_tokens as f64 * 1000.0 / *duration_ms as f64;
                    self.last_speed = Some(per_second);
                }
                let extra = json!({
                    "model": model,
                    "usage": usage_json(usage.as_ref()),
                    "stopReason": stop_reason(*finish_reason),
                });
                self.finish_message(&mut frames, &extra);
                self.llm_done = true;
            }
            AgentEvent::ToolStart {
                call_id,
                name,
                arguments,
            } => {
                let args = serde_json::from_str(arguments)
                    .unwrap_or_else(|_| Value::String(arguments.clone()));
                self.calls
                    .insert(call_id.clone(), (name.clone(), args.clone()));
                frames.push(json!({
                    "type": "tool_execution_start",
                    "toolCallId": call_id,
                    "toolName": name,
                    "args": args,
                }));
            }
            AgentEvent::ToolDelta { call_id, delta } => {
                let text = self.partial.entry(call_id.clone()).or_default();
                text.push_str(delta);
                let (name, args) = self
                    .calls
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), Value::Null));
                frames.push(json!({
                    "type": "tool_execution_update",
                    "toolCallId": call_id,
                    "toolName": name,
                    "args": args,
                    "partialResult": { "content": [{ "type": "text", "text": text }] },
                }));
            }
            AgentEvent::ToolDone {
                call_id,
                name,
                output,
                is_error,
                diff,
                images,
            } => {
                self.calls.remove(call_id);
                self.partial.remove(call_id);
                let mut content = vec![json!({ "type": "text", "text": output })];
                content.extend(images.iter().map(|image| {
                    json!({
                        "type": "image",
                        "data": image.data_base64,
                        "mimeType": image.media_type,
                    })
                }));
                let details = diff.as_ref().map(|d| json!({ "diff": d }));
                frames.push(json!({
                    "type": "tool_execution_end",
                    "toolCallId": call_id,
                    "toolName": name,
                    "result": { "content": content, "details": details },
                    "isError": is_error,
                }));
                let id = self.next_id("msg");
                let message = json!({
                    "role": "toolResult",
                    "toolCallId": call_id,
                    "toolName": name,
                    "content": content,
                    "details": details,
                    "isError": is_error,
                    "timestamp": now_ms(),
                });
                frames
                    .push(json!({ "type": "message_start", "messageId": id, "message": message }));
                frames.push(json!({ "type": "message_end", "messageId": id, "message": message }));
                self.messages.push(message.clone());
                self.results.push(message);
            }
            AgentEvent::Compacted {
                covers_node_id,
                method,
                tokens_before,
                tokens_after,
                saved_chars,
            } => {
                // A manual compaction reports through the `compact` command's own frames.
                if !self.manual {
                    frames.push(json!({ "type": "auto_compaction_start", "reason": "threshold" }));
                    let before = if *tokens_before > 0 {
                        *tokens_before
                    } else {
                        self.last_usage
                            .map_or(0, |u| u.prompt_tokens + u.completion_tokens)
                    };
                    frames.push(json!({
                        "type": "auto_compaction_end",
                        "action": "context-full",
                        "aborted": false,
                        "willRetry": false,
                        "result": {
                            "summary": "",
                            "firstKeptEntryId": covers_node_id,
                            "tokensBefore": before,
                            "details": {
                                "method": method,
                                "tokensAfter": tokens_after,
                                "savedChars": saved_chars,
                            },
                        },
                    }));
                }
            }
            AgentEvent::AssistantNote { text } => frames.push(json!({
                "type": "notice",
                "level": "info",
                "source": "assistant",
                "message": text,
            })),
            AgentEvent::AdvisorNote { model, note } => frames.push(json!({
                "type": "notice",
                "level": "info",
                "source": "advisor",
                "message": format!("{model}: {note}"),
            })),
            AgentEvent::TurnDone { .. } => self.end_run(&mut frames),
            AgentEvent::TurnError { code, message, .. } => {
                let stop = if code == "cancelled" {
                    "aborted"
                } else {
                    "error"
                };
                let extra = json!({ "stopReason": stop, "errorMessage": message });
                self.finish_message(&mut frames, &extra);
                self.llm_done = true;
                self.end_run(&mut frames);
            }
            AgentEvent::StatusChanged { .. }
            | AgentEvent::ApprovalRequested { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::AskRequested { .. }
            | AgentEvent::TodosUpdated { .. }
            | AgentEvent::GoalUpdated { .. }
            | AgentEvent::CompactionSpeculating { .. }
            | AgentEvent::SessionTitle { .. } => {}
        }
        frames
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
