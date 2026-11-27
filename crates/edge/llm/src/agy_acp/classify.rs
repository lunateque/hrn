//! ACP JSON-RPC error classification, quota-exhaustion detection, and effort mapping.

use super::wire::{ACP_AUTH_REQUIRED_CODE, AcpPromptResult, InlineQuota};
use crate::usage_claude::iso_to_epoch;
use hrn_kernel::{HrnError, ThinkingLevel};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Classify a JSON-RPC error returned by `agy acp` into an [`HrnError`].
pub fn classify_jsonrpc_error(code: i64, message: &str, data: Option<&Value>) -> HrnError {
    let lower = message.to_ascii_lowercase();
    if code == ACP_AUTH_REQUIRED_CODE
        || lower.contains("authentication required")
        || lower.contains("not authenticated")
        || lower.contains("unauthenticated")
    {
        return HrnError::Auth(
            "Antigravity CLI is not authenticated; run `agy` or `hrn login agy`".to_owned(),
        );
    }
    let data_str = data.map(Value::to_string).unwrap_or_default();
    let combined = format!("{lower} {}", data_str.to_ascii_lowercase());
    let is_quota = combined.contains("resource_exhausted")
        || combined.contains("spend_cap_reached")
        || combined.contains("429")
        || combined.contains("rate limit")
        || combined.contains("rate_limit")
        || combined.contains("usage limit")
        || combined.contains("too many requests")
        || combined.contains("quota");
    if is_quota {
        let retry_after_secs = data.and_then(|d| retry_after_from_value(d, now_epoch_secs()));
        return HrnError::Provider {
            message: format!("Usage limit reached (agy_acp): {message}"),
            retryable: true,
            retry_after_secs,
        };
    }
    HrnError::Provider {
        message: format!("agy_acp error ({code}): {message}"),
        retryable: false,
        retry_after_secs: None,
    }
}

/// Detect quota exhaustion from a completed [`AcpPromptResult`] and its accumulated visible text.
///
/// Covers both structured refusal/quota responses and the known `agy` behavior where quota
/// exhaustion completes with `stopReason == "end_turn"`, `total_tokens == 0`, and plain-text
/// `"Usage Limit Reached ... You have reached your current quota"`.
pub fn detect_quota_exhaustion(
    result: &AcpPromptResult,
    accumulated_text: &str,
    now_secs: u64,
) -> Option<HrnError> {
    let lower = accumulated_text.to_ascii_lowercase();
    let has_quota_phrase = lower.contains("usage limit reached")
        || lower.contains("reached your current quota")
        || lower.contains("resource_exhausted")
        || lower.contains("spend_cap_reached");
    let zero_tokens = result.usage.is_none_or(|u| u.total_tokens == 0);
    let stop_lower = result.stop_reason.to_ascii_lowercase();

    let triggered = (stop_lower == "end_turn" && zero_tokens && has_quota_phrase)
        || (stop_lower == "refusal" && has_quota_phrase)
        || (result
            .quota
            .as_ref()
            .and_then(|q| q.remaining_fraction)
            .is_some_and(|f| f <= 0.0)
            && zero_tokens);

    if !triggered {
        return None;
    }

    let retry_after_secs = result
        .quota
        .as_ref()
        .and_then(|q| retry_after_from_quota(q, now_secs));
    let excerpt = accumulated_text.trim();
    let msg = if excerpt.is_empty() {
        "Usage limit reached on Antigravity subscription".to_owned()
    } else {
        format!("Usage limit reached on Antigravity subscription: {excerpt}")
    };
    Some(HrnError::Provider {
        message: msg,
        retryable: true,
        retry_after_secs,
    })
}

/// Compute `retry_after_secs` from [`InlineQuota`] relative to `now_secs`.
pub fn retry_after_from_quota(quota: &InlineQuota, now_secs: u64) -> Option<u64> {
    if let Some(secs) = quota.retry_after_secs {
        return Some(secs);
    }
    let reset = quota.reset_time.as_deref()?;
    let epoch = iso_to_epoch(reset)?;
    if epoch == 0 || epoch <= now_secs {
        return None;
    }
    Some(epoch - now_secs)
}

fn retry_after_from_value(val: &Value, now_secs: u64) -> Option<u64> {
    if let Some(secs) = val
        .get("retry_after_secs")
        .or_else(|| val.get("retryAfterSecs"))
        .and_then(Value::as_u64)
    {
        return Some(secs);
    }
    let reset = val
        .get("reset_time")
        .or_else(|| val.get("resetTime"))
        .and_then(Value::as_str)?;
    let epoch = iso_to_epoch(reset)?;
    (epoch > now_secs).then_some(epoch - now_secs)
}

/// Map `hrn`'s [`ThinkingLevel`] ladder onto ACP `thought_level` config option values.
pub const fn thinking_level_to_acp(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Auto | ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh | ThinkingLevel::Max => "xhigh",
    }
}

/// Current Unix epoch seconds.
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
