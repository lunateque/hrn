//! Zero-token Antigravity quota reader (`agy --print /usage --output-format json`).

use super::wire::now_epoch_secs;
use crate::usage::{AccountUsage, UsageWindow};
use crate::usage_claude::iso_to_epoch;
use hrn_edge_proc::{RunSpec, run_streaming};
use hrn_kernel::{CancelFlag, HrnError, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default timeout for `agy --print /usage --output-format json`.
pub const DEFAULT_USAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `agy --print /usage --output-format json` (0 tokens) and convert into [`AccountUsage`].
pub async fn fetch_agy_usage(binary: &Path, timeout: Duration) -> AccountUsage {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let spec = RunSpec {
        program: binary.to_string_lossy().into_owned(),
        args: vec![
            "--print".to_owned(),
            "/usage".to_owned(),
            "--output-format".to_owned(),
            "json".to_owned(),
        ],
        cwd,
        env: BTreeMap::new(),
        timeout,
        max_output_bytes: 256 * 1024,
    };
    let out = match run_streaming(&spec, &CancelFlag::new(), |_| {}).await {
        Ok(out) => out,
        Err(err) => {
            return AccountUsage {
                identity: "Antigravity".to_owned(),
                problem: Some(format!("failed to invoke `agy /usage`: {err}")),
                windows: Vec::new(),
            };
        }
    };
    if out.timed_out {
        return AccountUsage {
            identity: "Antigravity".to_owned(),
            problem: Some("`agy /usage` timed out".to_owned()),
            windows: Vec::new(),
        };
    }
    if out.exit_code != Some(0) {
        let detail = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        return AccountUsage {
            identity: "Antigravity".to_owned(),
            problem: Some(format!(
                "`agy /usage` failed (exit {:?}): {detail}",
                out.exit_code
            )),
            windows: Vec::new(),
        };
    }
    match parse_agy_usage_json(&out.stdout, now_epoch_secs()) {
        Ok(acct) => acct,
        Err(err) => AccountUsage {
            identity: "Antigravity".to_owned(),
            problem: Some(err.to_string()),
            windows: Vec::new(),
        },
    }
}

/// Parse the JSON payload produced by `agy --print /usage --output-format json`.
///
/// Unused quota buckets report `"reset_time": "1970-01-01T00:00:00Z"` (Unix epoch `0`); those are
/// mapped to `resets_in_secs: None` rather than `Some(0)`.
pub fn parse_agy_usage_json(raw: &str, now_secs: u64) -> Result<AccountUsage> {
    let trimmed = raw.trim();
    let payload: Value = serde_json::from_str(trimmed).map_err(|e| HrnError::Provider {
        message: format!("invalid `agy /usage` JSON: {e}"),
        retryable: false,
        retry_after_secs: None,
    })?;

    if let Some(status) = payload.get("status").and_then(Value::as_str)
        && !status.eq_ignore_ascii_case("SUCCESS")
    {
        let resp = payload
            .get("response")
            .and_then(Value::as_str)
            .unwrap_or(status);
        return Ok(AccountUsage {
            identity: "Antigravity".to_owned(),
            problem: Some(resp.to_owned()),
            windows: Vec::new(),
        });
    }

    let groups = payload
        .get("command")
        .and_then(|c| c.get("data"))
        .and_then(|d| d.get("groups"))
        .and_then(Value::as_array)
        .ok_or_else(|| HrnError::Provider {
            message: "`agy /usage` JSON missing `command.data.groups`".to_owned(),
            retryable: false,
            retry_after_secs: None,
        })?;

    let mut windows = Vec::new();
    for group in groups {
        let group_name = group
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Antigravity")
            .trim();
        let Some(buckets) = group.get("buckets").and_then(Value::as_array) else {
            continue;
        };
        for bucket in buckets {
            let window_tag = bucket
                .get("window")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let bucket_name = bucket
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let label = match (window_tag, bucket_name) {
                (Some(w), _) => format!("{group_name} ({w})"),
                (None, Some(b)) => format!("{group_name} — {b}"),
                (None, None) => group_name.to_owned(),
            };
            let used_percent = bucket
                .get("remaining_fraction")
                .and_then(Value::as_f64)
                .filter(|f| f.is_finite())
                .map(|rem| (1.0 - rem).clamp(0.0, 1.0) * 100.0);
            let resets_in_secs = bucket
                .get("reset_time")
                .and_then(Value::as_str)
                .and_then(iso_to_epoch)
                .filter(|&epoch| epoch > 0 && epoch > now_secs)
                .map(|epoch| epoch - now_secs);

            windows.push(UsageWindow {
                label,
                used_percent,
                resets_in_secs,
            });
        }
    }

    Ok(AccountUsage {
        identity: "Antigravity CLI".to_owned(),
        problem: None,
        windows,
    })
}
