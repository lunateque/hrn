//! `ChatGPT`/Codex limits from the `wham/usage` endpoint (as omp reads them).

use crate::codex::account_headers;
use crate::usage::{AccountUsage, UsageWindow, now_secs};
use hrn_edge_http::HttpClient;
use hrn_edge_oauth::SubscriptionCredential;
use serde_json::Value;
use std::time::Duration;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BYTES: usize = 256 * 1024;

/// Read the account's plan and limits; failures are reported on the returned account.
pub(crate) async fn fetch(http: &HttpClient, cred: &SubscriptionCredential) -> AccountUsage {
    let mut account = AccountUsage {
        identity: [cred.email.as_deref(), cred.plan.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · "),
        ..AccountUsage::default()
    };
    if account.identity.is_empty() {
        cred.account_label()
            .unwrap_or("ChatGPT account")
            .clone_into(&mut account.identity);
    }
    let headers = match account_headers(cred) {
        Ok(headers) => headers,
        Err(e) => {
            account.problem = Some(e.to_string());
            return account;
        }
    };
    match http.get(USAGE_URL, &headers, TIMEOUT, MAX_BYTES).await {
        Err(e) => account.problem = Some(format!("usage request failed: {e}")),
        Ok(reply) if matches!(reply.status.as_u16(), 401 | 403) => {
            account.problem = Some(format!(
                "ChatGPT rejected the login (HTTP {}); run `hrn login chatgpt`",
                reply.status.as_u16()
            ));
        }
        Ok(reply) if !reply.status.is_success() => {
            account.problem = Some(format!(
                "usage request failed: HTTP {}",
                reply.status.as_u16()
            ));
        }
        Ok(reply) => match serde_json::from_slice::<Value>(&reply.body) {
            Ok(payload) => {
                if let Some(plan) = payload.get("plan_type").and_then(Value::as_str)
                    && cred.plan.as_deref() != Some(plan)
                    && !account.identity.contains(plan)
                {
                    account.identity = format!("{} · {plan}", account.identity);
                }
                account.windows = windows(&payload, now_secs());
            }
            Err(e) => account.problem = Some(format!("unreadable usage reply: {e}")),
        },
    }
    account
}

/// "5 hours", "7 days", "1 hour" from a window length in seconds.
fn window_name(secs: u64) -> String {
    let (n, unit) = if secs >= 86_400 && secs.is_multiple_of(86_400) {
        (secs / 86_400, "day")
    } else {
        (secs.div_ceil(3_600).max(1), "hour")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// The windows of one `rate_limit` object, labelled `<length>[ (<scope>)]`.
fn limit_windows(limit: &Value, scope: Option<&str>, now: u64, out: &mut Vec<UsageWindow>) {
    for key in ["primary_window", "secondary_window"] {
        let Some(window) = limit.get(key).filter(|w| w.is_object()) else {
            continue;
        };
        let secs = window.get("limit_window_seconds").and_then(Value::as_u64);
        let mut label = secs.map_or_else(|| key.replace('_', " "), window_name);
        if let Some(scope) = scope {
            label = format!("{label} ({scope})");
        }
        let resets_in_secs = window
            .get("reset_after_seconds")
            .and_then(Value::as_u64)
            .or_else(|| {
                let at = window.get("reset_at").and_then(Value::as_u64)?;
                Some(at.saturating_sub(now))
            });
        out.push(UsageWindow {
            label,
            used_percent: window.get("used_percent").and_then(Value::as_f64),
            resets_in_secs,
        });
    }
}

/// The windows of a `wham/usage` payload: the account's own, then each additional limit.
pub(crate) fn windows(payload: &Value, now: u64) -> Vec<UsageWindow> {
    let mut out = Vec::new();
    if let Some(limit) = payload.get("rate_limit") {
        limit_windows(limit, None, now, &mut out);
    }
    for extra in payload
        .get("additional_rate_limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let scope = extra
            .get("limit_name")
            .or_else(|| extra.get("metered_feature"))
            .and_then(Value::as_str);
        if let Some(limit) = extra.get("rate_limit") {
            limit_windows(limit, scope, now, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn windows_are_named_by_length_and_scoped_by_their_limit() {
        let payload = json!({
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": { "used_percent": 41.5, "limit_window_seconds": 18_000,
                                    "reset_after_seconds": 600 },
                "secondary_window": { "used_percent": 3, "limit_window_seconds": 604_800,
                                      "reset_at": 100_000 }
            },
            "additional_rate_limits": [{
                "limit_name": "Spark",
                "rate_limit": { "primary_window": { "used_percent": 0, "limit_window_seconds": 3_600 } }
            }]
        });
        let got: Vec<_> = windows(&payload, 90_000)
            .into_iter()
            .map(|w| (w.label, w.used_percent, w.resets_in_secs))
            .collect();
        assert_eq!(
            got,
            [
                ("5 hours".to_owned(), Some(41.5), Some(600)),
                ("7 days".to_owned(), Some(3.0), Some(10_000)),
                ("1 hour (Spark)".to_owned(), Some(0.0), None),
            ]
        );
    }
}
