//! One NDJSON line in, responses and events out: the RPC command table.

use super::Core;
use crate::backend::CompactionReport;
use hrn_kernel::{HrnError, Result};
use serde_json::{Value, json};
use std::sync::Arc;

/// A command's answer: a `response` body, or `Deferred` when the handler answered itself.
pub(crate) enum Reply {
    Body(Value),
    Deferred,
}

impl Reply {
    pub(crate) fn data(data: Value) -> Self {
        Self::Body(Core::ok(Some(data)))
    }

    pub(crate) fn done() -> Self {
        Self::Body(Core::ok(None))
    }

    pub(crate) fn err(message: impl Into<String>) -> Self {
        Self::Body(Core::fail(message.into(), None))
    }
}

/// The trimmed string field `key` of `frame`; `None` when absent or blank.
pub(crate) fn field(frame: &Value, key: &str) -> Option<String> {
    frame
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The boolean field `key`; absent means `fallback`, any other type is an error.
pub(crate) fn flag(
    frame: &Value,
    key: &str,
    fallback: bool,
) -> std::result::Result<bool, HrnError> {
    match frame.get(key) {
        None | Some(Value::Null) => Ok(fallback),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(HrnError::Config(format!("`{key}` must be a boolean"))),
    }
}

/// `mode`-style field restricted to a few values.
pub(crate) fn one_of(frame: &Value, key: &str, allowed: &[&str]) -> Result<String> {
    field(frame, key)
        .filter(|v| allowed.contains(&v.as_str()))
        .ok_or_else(|| HrnError::Config(format!("`{key}` must be one of {}", allowed.join(", "))))
}

/// The `compact` response and `manual_compaction_end` result payload.
pub(crate) fn compaction_json(report: &CompactionReport) -> Value {
    json!({
        "summary": report.summary,
        "firstKeptEntryId": report.first_kept,
        "tokensBefore": report.tokens_before,
        "details": { "savedChars": report.saved_chars },
    })
}

/// `set_event_filter`: which event frames the host wants, and in which projection.
pub(crate) fn set_event_filter(core: &Arc<Core>, frame: &Value) -> Reply {
    let events = match frame.get("events") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str().filter(|s| !s.is_empty()) {
                    Some(name) => names.push(name.to_owned()),
                    None => {
                        return Reply::err(
                            "`events` must be null or an array of non-empty strings",
                        );
                    }
                }
            }
            Some(names)
        }
        Some(_) => return Reply::err("`events` must be null or an array of non-empty strings"),
    };
    let updates = match frame.get("messageUpdates") {
        None | Some(Value::Null) => "full",
        Some(Value::String(mode)) if mode == "full" || mode == "delta" => mode.as_str(),
        Some(_) => return Reply::err("`messageUpdates` must be \"full\" or \"delta\""),
    };
    {
        let mut state = core.lock();
        state.events.clone_from(&events);
        state.translator.set_delta(updates == "delta");
    }
    Reply::data(json!({ "events": events, "messageUpdates": updates }))
}

/// `set_fast_mode`: toggle session priority service tier.
///
/// `active` matches `enabled` whenever the active model supports service tiers (until
/// provider-rejection fallback feedback exists).
pub(crate) fn set_fast_mode(core: &Arc<Core>, frame: &Value) -> Result<Reply> {
    let enabled = flag(frame, "enabled", true)?;
    let model = core.lock().model.clone();
    let supported = core.backend.supports_service_tier(&model);
    if enabled && !supported {
        return Ok(Reply::err(
            "Fast mode is unavailable for the current model.",
        ));
    }
    core.backend.set_fast_mode(enabled);
    core.lock().fast_mode_enabled = enabled;
    let active = enabled && supported;
    Ok(Reply::data(json!({ "enabled": enabled, "active": active })))
}

/// `set_slow_mode`: toggle Anthropic subscription slow-mode lane for this session.
pub(crate) fn set_slow_mode(core: &Arc<Core>, frame: &Value) -> Result<Reply> {
    let enabled = flag(frame, "enabled", true)?;
    let model = core.lock().model.clone();
    let supported = core.backend.supports_slow_mode(&model);
    core.backend.set_slow_mode(enabled);
    core.lock().slow_mode_enabled = enabled;
    let active = enabled && supported;
    Ok(Reply::data(json!({ "enabled": enabled, "active": active })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queue_edit_only_takes_its_own_queue() {
        let frame = json!({ "message": "x", "queue": "steering" });
        assert!(one_of(&frame, "queue", &["steering", "followUp"]).is_ok_and(|q| q == "steering"));
        assert!(
            one_of(
                &json!({ "queue": "aside" }),
                "queue",
                &["steering", "followUp"]
            )
            .is_err()
        );
        assert!(field(&json!({ "message": "  " }), "message").is_none());
    }

    #[test]
    fn flags_reject_the_wrong_type() {
        assert!(flag(&json!({ "enabled": false }), "enabled", true).is_ok_and(|v| !v));
        assert!(flag(&json!({}), "enabled", true).is_ok_and(|v| v));
        assert!(flag(&json!({ "enabled": "yes" }), "enabled", true).is_err());
    }
}
