//! Argument extraction helpers shared by the built-in tools.

use hrn_kernel::{HrnError, Result};
use serde_json::Value;
use tokio::task::JoinError;

/// A required string argument.
pub fn required_str<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Tool(format!("{tool}: argument `{key}` must be a string")))
}

/// An optional string argument.
pub fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// An optional boolean argument (absent means `false`).
pub fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// An optional integer argument clamped into `min..=max` (`default` when absent).
pub fn bounded_usize(args: &Value, key: &str, default: usize, min: usize, max: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(default)
        .clamp(min, max)
}

/// One-line approval summary: the string value of `key`, or a placeholder.
pub fn summary_of(args: &Value, key: &str) -> String {
    opt_str(args, key).unwrap_or("<unknown>").to_owned()
}

/// Map a failed blocking task to a tool error.
fn join_failed(tool: &str, err: &JoinError) -> HrnError {
    HrnError::Internal(format!("{tool} worker failed: {err}"))
}

/// Run blocking `work` on the blocking pool; a panicked or cancelled worker becomes a tool error.
pub async fn blocking<T: Send + 'static>(
    tool: &str,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| join_failed(tool, &e))?
}
