//! Kimi Code subscription usage limits (`GET /coding/v1/usages`) with `KimiCLI` fingerprint headers.

use crate::usage::{AccountUsage, UsageWindow, now_secs};
use crate::usage_claude::iso_to_epoch;
use hrn_edge_http::HttpClient;
use hrn_edge_oauth::{
    SubscriptionCredential, kimi_fingerprint_headers, load_or_create_kimi_device_id,
};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BYTES: usize = 256 * 1024;

/// Read the Kimi Code account's usage limits from the official `/coding/v1/usages` endpoint.
pub(crate) async fn fetch(
    http: &HttpClient,
    cred: &SubscriptionCredential,
    auth_dir: &Path,
) -> AccountUsage {
    fetch_from_url(http, cred, auth_dir, USAGE_URL).await
}

/// Read Kimi Code usage limits from an explicit URL (used by loopback stub tests).
pub(crate) async fn fetch_from_url(
    http: &HttpClient,
    cred: &SubscriptionCredential,
    auth_dir: &Path,
    url: &str,
) -> AccountUsage {
    let identity = cred
        .account_label()
        .unwrap_or("Kimi Code account")
        .to_owned();
    let device_id = match load_or_create_kimi_device_id(auth_dir).await {
        Ok(id) => id,
        Err(e) => {
            return AccountUsage {
                identity,
                problem: Some(format!("device id unavailable: {e}")),
                windows: Vec::new(),
            };
        }
    };
    let mut headers = kimi_fingerprint_headers(&device_id);
    headers.push(("accept".to_owned(), "application/json".to_owned()));
    headers.push((
        "authorization".to_owned(),
        format!("Bearer {}", cred.access),
    ));

    let (windows, problem) = match http.get(url, &headers, TIMEOUT, MAX_BYTES).await {
        Err(e) => (Vec::new(), Some(format!("usage request failed: {e}"))),
        Ok(reply) if matches!(reply.status.as_u16(), 401 | 403) => (
            Vec::new(),
            Some(format!(
                "Kimi Code rejected the login (HTTP {}); run `hrn login kimi-code`",
                reply.status.as_u16()
            )),
        ),
        Ok(reply) if !reply.status.is_success() => (
            Vec::new(),
            Some(format!(
                "usage request failed: HTTP {}",
                reply.status.as_u16()
            )),
        ),
        Ok(reply) => match serde_json::from_slice::<Value>(&reply.body) {
            Ok(payload) => (windows(&payload, now_secs()), None),
            Err(e) => (Vec::new(), Some(format!("unreadable usage reply: {e}"))),
        },
    };
    AccountUsage {
        identity,
        problem,
        windows,
    }
}

fn num(val: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let v = val.get(*key)?;
        v.as_f64()
            .or_else(|| v.as_str()?.trim().parse::<f64>().ok())
            .filter(|n| n.is_finite())
    })
}

fn resets_in(val: &Value, now: u64) -> Option<u64> {
    if let Some(secs) = val
        .get("reset_after_seconds")
        .or_else(|| val.get("resetIn"))
        .and_then(Value::as_u64)
    {
        return Some(secs);
    }
    let raw = val.get("resetTime").or_else(|| val.get("resets_at"))?;
    if let Some(epoch) = raw.as_u64() {
        return Some(epoch.saturating_sub(now));
    }
    raw.as_str()
        .and_then(iso_to_epoch)
        .map(|epoch| epoch.saturating_sub(now))
}

fn parse_bucket(label: String, node: &Value, now: u64) -> Option<UsageWindow> {
    let used_percent = num(node, &["used_percent", "percent", "utilization"]).or_else(|| {
        let limit = num(node, &["limit"])?;
        if limit <= 0.0 {
            return None;
        }
        if let Some(used) = num(node, &["used"]) {
            return Some(((used / limit) * 100.0).clamp(0.0, 100.0));
        }
        let remaining = num(node, &["remaining"])?;
        Some((((limit - remaining) / limit) * 100.0).clamp(0.0, 100.0))
    });
    let resets_in_secs = resets_in(node, now);
    (used_percent.is_some() || resets_in_secs.is_some()).then_some(UsageWindow {
        label,
        used_percent,
        resets_in_secs,
    })
}

fn format_window_label(window: &Value, idx: usize) -> String {
    let duration = window.get("duration").and_then(Value::as_u64);
    let unit = window
        .get("timeUnit")
        .or_else(|| window.get("time_unit"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match (duration, unit) {
        (Some(mins), u) if u.contains("MINUTE") => {
            if mins >= 60 && mins.is_multiple_of(60) {
                format!("Kimi {}h", mins / 60)
            } else {
                format!("Kimi {mins}m")
            }
        }
        (Some(hours), u) if u.contains("HOUR") => format!("Kimi {hours}h"),
        (Some(days), u) if u.contains("DAY") => format!("Kimi {days}d"),
        _ => format!("Kimi Window {}", idx + 1),
    }
}

/// Extract rate-limit windows from a Kimi Code `/coding/v1/usages` JSON payload.
pub(crate) fn windows(payload: &Value, now: u64) -> Vec<UsageWindow> {
    let mut out = Vec::new();
    if let Some(usage) = payload.get("usage").filter(|v| v.is_object())
        && let Some(win) = parse_bucket("Kimi Weekly".to_owned(), usage, now)
    {
        out.push(win);
    }
    if let Some(limits) = payload.get("limits").and_then(Value::as_array) {
        for (idx, item) in limits.iter().enumerate() {
            let detail = item.get("detail").unwrap_or(item);
            let label = item.get("window").map_or_else(
                || format!("Kimi Limit {}", idx + 1),
                |w| format_window_label(w, idx),
            );
            if let Some(win) = parse_bucket(label, detail, now) {
                out.push(win);
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "usage_kimi_tests.rs"]
mod tests;
