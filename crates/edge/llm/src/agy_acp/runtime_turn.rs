//! Per-turn streaming driver, parallel tool-call coalescing, and ACP session control helpers
//! used by [`super::runtime::AgyAcpRuntime`].

use super::bridge::{BridgeToolResult, McpBridge};
use super::client::AcpClient;
use super::config::{AgyProviderConfig, AgyToolMode};
use super::session_map::AgySessionEntry;
use super::wire::{AcpPromptResult, AcpUpdate, thinking_level_to_acp};
use hrn_kernel::{
    CancelFlag, ChunkSink, HrnError, LlmChunk, LlmRequest, Msg, Result, Role, ThinkingLevel,
    ToolCall,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Default ACP RPC timeout for fast control operations (`initialize`, `session/set_config_option`, `session/fork`).
pub(crate) const CONTROL_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Extended ACP RPC timeout for `session/new` and `session/resume`, covering `agy`'s internal
/// 30s upstream timeout + automatic retry during cold backend bootstrap.
pub(crate) const SESSION_INIT_RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// Await `fut` while honouring `cancel` so Esc / Ctrl-C aborts blocked control RPCs immediately.
pub(crate) async fn race_cancel<T, F>(cancel: &CancelFlag, fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(HrnError::Cancelled),
        res = fut => res,
    }
}

pub(crate) struct ActiveTurn {
    pub(crate) agy_session_id: String,
    pub(crate) model_id: String,
    pub(crate) effort: ThinkingLevel,
    pub(crate) expected_call_ids: Vec<String>,
    pub(crate) prompt_rx: oneshot::Receiver<Result<AcpPromptResult>>,
    pub(crate) update_rx: mpsc::UnboundedReceiver<AcpUpdate>,
    pub(crate) tool_rx: mpsc::UnboundedReceiver<ToolCall>,
    pub(crate) turn_deadline: tokio::time::Instant,
}

pub(crate) enum TurnStep {
    NeedTools {
        content: String,
        reasoning: String,
        tool_calls: Vec<ToolCall>,
    },
    Completed {
        content: String,
        reasoning: String,
        result: AcpPromptResult,
    },
}

pub(crate) async fn drive_turn(
    config: &AgyProviderConfig,
    tool_batch_window: Duration,
    client: &AcpClient,
    bridge: Option<&McpBridge>,
    turn: &mut ActiveTurn,
    cancel: &CancelFlag,
    sink: &dyn ChunkSink,
) -> Result<TurnStep> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let stall_budget = match config.tool_mode {
        AgyToolMode::Hrn => config.stall_timeout(),
        AgyToolMode::Native => config.tool_stall_timeout(),
    };
    let mut stall_deadline = tokio::time::Instant::now() + stall_budget;

    loop {
        let next_deadline = turn.turn_deadline.min(stall_deadline);
        tokio::select! {
            () = cancel.cancelled() => {
                abort_turn(client, bridge, &turn.agy_session_id).await;
                return Err(HrnError::Cancelled);
            }
            () = tokio::time::sleep_until(next_deadline) => {
                let msg = if tokio::time::Instant::now() >= turn.turn_deadline {
                    format!(
                        "agy_acp turn exceeded turn_timeout_ms ({}ms)",
                        config.turn_timeout_ms
                    )
                } else {
                    format!(
                        "agy_acp stream stalled for {}ms with no output",
                        stall_budget.as_millis()
                    )
                };
                return Err(HrnError::Provider {
                    message: msg,
                    retryable: true,
                    retry_after_secs: None,
                });
            }
            maybe_update = turn.update_rx.recv() => {
                if let Some(update) = maybe_update {
                    stall_deadline = tokio::time::Instant::now() + stall_budget;
                    apply_update(update, &mut content, &mut reasoning, sink).await;
                }
            }
            maybe_call = turn.tool_rx.recv() => {
                if let Some(first_call) = maybe_call {
                    let mut calls = vec![first_call];
                    let batch_until = tokio::time::Instant::now() + tool_batch_window;
                    loop {
                        tokio::select! {
                            () = tokio::time::sleep_until(batch_until) => break,
                            () = cancel.cancelled() => {
                                abort_turn(client, bridge, &turn.agy_session_id).await;
                                return Err(HrnError::Cancelled);
                            }
                            more = turn.tool_rx.recv() => match more {
                                Some(tc) => calls.push(tc),
                                None => break,
                            },
                            maybe_up = turn.update_rx.recv() => {
                                if let Some(up) = maybe_up {
                                    apply_update(up, &mut content, &mut reasoning, sink).await;
                                }
                            }
                        }
                    }
                    return Ok(TurnStep::NeedTools {
                        content,
                        reasoning,
                        tool_calls: calls,
                    });
                }
            }
            prompt_done = &mut turn.prompt_rx => {
                while let Ok(up) = turn.update_rx.try_recv() {
                    apply_update(up, &mut content, &mut reasoning, sink).await;
                }
                let res = match prompt_done {
                    Ok(inner) => inner?,
                    Err(_) => {
                        return Err(HrnError::Provider {
                            message: "ACP prompt channel closed unexpectedly".to_owned(),
                            retryable: true,
                            retry_after_secs: None,
                        });
                    }
                };
                return Ok(TurnStep::Completed {
                    content,
                    reasoning,
                    result: res,
                });
            }
        }
    }
}

async fn apply_update(
    update: AcpUpdate,
    content: &mut String,
    reasoning: &mut String,
    sink: &dyn ChunkSink,
) {
    match update {
        AcpUpdate::ContentDelta(delta) => {
            content.push_str(&delta);
            sink.on_chunk(LlmChunk::ContentDelta(delta)).await;
        }
        AcpUpdate::ReasoningDelta(delta) => {
            reasoning.push_str(&delta);
            sink.on_chunk(LlmChunk::ReasoningDelta(delta)).await;
        }
        AcpUpdate::UsageUpdate { .. } | AcpUpdate::Other => {}
    }
}

pub(crate) async fn sync_model_and_effort(
    client: &AcpClient,
    agy_session_id: &str,
    previous: Option<&AgySessionEntry>,
    model_id: &str,
    effort: ThinkingLevel,
) -> Result {
    let model_changed = previous.is_none_or(|p| p.model_id != model_id);
    if model_changed && !model_id.trim().is_empty() {
        let _ = client
            .session_set_config_option(agy_session_id, "model", model_id, CONTROL_RPC_TIMEOUT)
            .await?;
    }
    let effort_changed = previous.is_none_or(|p| p.effort != effort);
    if effort_changed {
        let acp_effort = thinking_level_to_acp(effort);
        let _ = client
            .session_set_config_option(
                agy_session_id,
                "thought_level",
                acp_effort,
                CONTROL_RPC_TIMEOUT,
            )
            .await;
    }
    Ok(())
}

pub(crate) fn match_trailing_tool_results(
    messages: &[Msg],
    expected_call_ids: &[String],
) -> Option<Vec<(String, BridgeToolResult)>> {
    if expected_call_ids.is_empty() {
        return None;
    }
    let mut tail_results: HashMap<&str, BridgeToolResult> = HashMap::new();
    for msg in messages.iter().rev() {
        if msg.role != Role::Tool {
            break;
        }
        if let Some(id) = msg.tool_call_id.as_deref() {
            tail_results.insert(
                id,
                BridgeToolResult {
                    content: msg.content.clone(),
                    is_error: msg.is_error,
                },
            );
        }
    }
    let mut resolved = Vec::with_capacity(expected_call_ids.len());
    for id in expected_call_ids {
        let res = tail_results.remove(id.as_str())?;
        resolved.push((id.clone(), res));
    }
    Some(resolved)
}

pub(crate) fn build_prompt_blocks(
    text: &str,
    req: &LlmRequest,
    supports_image: bool,
) -> Vec<Value> {
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    if supports_image
        && let Some(last_user) = req.messages.iter().rev().find(|m| m.role == Role::User)
    {
        for img in &last_user.images {
            blocks.push(json!({
                "type": "image",
                "mimeType": img.media_type,
                "data": img.data_base64,
            }));
        }
    }
    blocks
}

pub(crate) async fn abort_turn(
    client: &AcpClient,
    bridge: Option<&McpBridge>,
    agy_session_id: &str,
) {
    if let Some(b) = bridge {
        b.fail_all_pending("Cancelled by user");
        b.detach_call_sink();
    }
    client.unsubscribe_updates(agy_session_id);
    let _ = client.session_cancel(agy_session_id).await;
}

pub(crate) fn should_reset_client(client: &AcpClient, err: &HrnError) -> bool {
    if matches!(err, HrnError::Cancelled) {
        return false;
    }
    !client.is_alive()
        || matches!(
            err,
            HrnError::Provider { message, .. }
                if message.contains("timed out") || message.contains("connection closed")
        )
}
