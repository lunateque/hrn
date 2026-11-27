//! Claude Pro/Max limits from Anthropic's OAuth usage endpoint (as omp reads them).

use crate::claude_code::{VERSION_ENV, resolve_version};
use crate::usage::{AccountUsage, UsageWindow, now_secs};
use hrn_edge_http::HttpClient;
use hrn_edge_oauth::SubscriptionCredential;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// The betas Claude Code sends; the OAuth endpoints refuse a token without the oauth one.
const BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
advanced-tool-use-2025-11-20,effort-2025-11-24,extended-cache-ttl-2025-04-11";
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BYTES: usize = 256 * 1024;

/// Read the account's identity and limits; failures are reported on the returned account.
pub(crate) async fn fetch(
    http: &HttpClient,
    cred: &SubscriptionCredential,
    auth_dir: &Path,
) -> AccountUsage {
    let version = resolve_version(
        std::env::var(VERSION_ENV).ok(),
        &auth_dir.join("claude-code-version.txt"),
    );
    let headers = vec![
        ("accept".to_owned(), "application/json".to_owned()),
        ("anthropic-beta".to_owned(), BETAS.to_owned()),
        (
            "authorization".to_owned(),
            format!("Bearer {}", cred.access),
        ),
        (
            "user-agent".to_owned(),
            format!("claude-cli/{version} (external, cli)"),
        ),
    ];
    let (usage, profile) = tokio::join!(
        http.get(USAGE_URL, &headers, TIMEOUT, MAX_BYTES),
        http.get(PROFILE_URL, &headers, TIMEOUT, MAX_BYTES),
    );
    let profile = profile
        .ok()
        .filter(|r| r.status.is_success())
        .and_then(|r| serde_json::from_slice::<Value>(&r.body).ok());
    let identity = identity(cred, profile.as_ref());
    let (windows, problem) = match usage {
        Err(e) => (Vec::new(), Some(format!("usage request failed: {e}"))),
        Ok(reply) if matches!(reply.status.as_u16(), 401 | 403) => (
            Vec::new(),
            Some(format!(
                "Anthropic rejected the login (HTTP {}); run `hrn login claude`",
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

/// `email · organization · plan`.
fn identity(cred: &SubscriptionCredential, profile: Option<&Value>) -> String {
    let email = profile
        .and_then(|p| p.pointer("/account/email").or_else(|| p.get("email")))
        .and_then(Value::as_str)
        .or(cred.email.as_deref());
    let org = cred.org_name.as_deref();
    let plan = profile.and_then(plan);
    [email, org, plan.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// "Max 5x", "Pro", "Team", ... from the profile's organization.
fn plan(profile: &Value) -> Option<String> {
    let org = profile.get("organization")?;
    let tier = org.get("rate_limit_tier").and_then(Value::as_str);
    if let Some(multiplier) = tier
        .and_then(|t| {
            t.to_ascii_lowercase()
                .split_once("max_")
                .map(|(_, rest)| rest.to_owned())
        })
        .map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect::<String>()
        })
        .filter(|m| m.ends_with('x') && m.len() > 1)
    {
        return Some(format!("Max {multiplier}"));
    }
    let kind = org.get("organization_type").and_then(Value::as_str)?;
    match kind.to_ascii_lowercase().as_str() {
        "claude_max" => Some("Max".to_owned()),
        "claude_pro" => Some("Pro".to_owned()),
        "claude_team" => Some("Team".to_owned()),
        "claude_enterprise" => Some("Enterprise".to_owned()),
        _ => None,
    }
}

/// Used percent and reset epoch of one bucket.
type Found = (Option<f64>, Option<u64>);

/// One bucket: `{ utilization | percent, resets_at }`.
fn bucket(node: &Value, now: u64) -> Option<Found> {
    let number = |v: &Value| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok());
    let used = node
        .get("utilization")
        .or_else(|| node.get("percent"))
        .and_then(number);
    let resets = node
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(iso_to_epoch)
        .map(|at| at.saturating_sub(now));
    (used.is_some() || resets.is_some()).then_some((used, resets))
}

/// The windows of a usage payload, in omp's order: 5 hour, 7 day, then model-scoped weeks.
pub(crate) fn windows(payload: &Value, now: u64) -> Vec<UsageWindow> {
    let limits: Vec<&Value> = payload
        .get("limits")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let of_kind = |kind: &str| {
        limits
            .iter()
            .find(|l| l.get("kind").and_then(Value::as_str) == Some(kind))
            .and_then(|l| bucket(l, now))
    };
    let legacy = |field: &str| payload.get(field).and_then(|b| bucket(b, now));
    let mut rows: Vec<(String, Found)> = Vec::new();
    let mut push = |label: String, found: Option<Found>| {
        if let Some(found) = found
            && !rows.iter().any(|(l, _)| *l == label)
        {
            rows.push((label, found));
        }
    };
    push(
        "Claude 5 Hour".into(),
        legacy("five_hour").or_else(|| of_kind("session")),
    );
    push(
        "Claude 7 Day".into(),
        legacy("seven_day").or_else(|| of_kind("weekly_all")),
    );
    push("Claude 7 Day (Opus)".into(), legacy("seven_day_opus"));
    push("Claude 7 Day (Sonnet)".into(), legacy("seven_day_sonnet"));
    for limit in &limits {
        if limit.get("kind").and_then(Value::as_str) != Some("weekly_scoped") {
            continue;
        }
        let name = limit
            .pointer("/scope/model/display_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty());
        if let Some(name) = name {
            push(format!("Claude 7 Day ({name})"), bucket(limit, now));
        }
    }
    rows.into_iter()
        .map(|(label, (used_percent, resets_in_secs))| UsageWindow {
            label,
            used_percent,
            resets_in_secs,
        })
        .collect()
}

/// Unix seconds of an RFC 3339 timestamp (`2026-05-01T12:00:00.123Z`, `+02:00` offsets).
pub(crate) fn iso_to_epoch(text: &str) -> Option<u64> {
    let (date, time) = text.split_once(['T', ' '])?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let clock_end = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(clock_end);
    let mut t = clock.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t
        .next()
        .and_then(|s| s.split('.').next()?.parse().ok())
        .unwrap_or(0);
    let offset = match zone.split_at_checked(1) {
        Some((sign @ ("+" | "-"), rest)) => {
            let (h, m) = rest.split_once(':').unwrap_or((rest, "0"));
            let secs = h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60;
            if sign == "+" { secs } else { -secs }
        }
        _ => 0,
    };
    // Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second - offset).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso_timestamps_convert_to_epoch_seconds() {
        assert_eq!(iso_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_epoch("2000-03-01T00:00:00.500Z"), Some(951_868_800));
        assert_eq!(iso_to_epoch("2000-03-01T02:00:00+02:00"), Some(951_868_800));
        assert_eq!(iso_to_epoch("garbage"), None);
    }

    #[test]
    fn legacy_buckets_and_scoped_limits_become_windows() {
        let payload = json!({
            "five_hour": { "utilization": 12.5, "resets_at": "1970-01-01T01:00:00Z" },
            "seven_day": null,
            "seven_day_opus": null,
            "limits": [
                { "kind": "weekly_all", "percent": 77, "resets_at": "1970-01-02T00:00:00Z" },
                { "kind": "weekly_scoped", "percent": "5", "resets_at": null,
                  "scope": { "model": { "display_name": "Fable" } } },
                { "kind": "weekly_scoped", "percent": 9, "scope": { "model": {} } }
            ]
        });
        let rows = windows(&payload, 0);
        let got: Vec<_> = rows
            .iter()
            .map(|w| (w.label.as_str(), w.used_percent, w.resets_in_secs))
            .collect();
        assert_eq!(
            got,
            [
                ("Claude 5 Hour", Some(12.5), Some(3600)),
                ("Claude 7 Day", Some(77.0), Some(86_400)),
                ("Claude 7 Day (Fable)", Some(5.0), None),
            ]
        );
    }

    #[test]
    fn the_plan_comes_from_the_tier_then_the_organization_type() {
        let max = json!({ "organization": { "rate_limit_tier": "default_claude_max_20x" } });
        assert_eq!(plan(&max).as_deref(), Some("Max 20x"));
        let pro = json!({ "organization": { "organization_type": "claude_pro" } });
        assert_eq!(plan(&pro).as_deref(), Some("Pro"));
        assert_eq!(plan(&json!({})), None);
    }
}
