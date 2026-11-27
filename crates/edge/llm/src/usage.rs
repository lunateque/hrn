//! Provider usage limits for the subscription logins (`/usage`, `hrn usage`), shaped like
//! `omp usage`: per provider, per account, one row per rate-limit window.

use crate::{usage_claude, usage_codex, usage_kimi};
use hrn_edge_http::HttpClient;
use hrn_edge_oauth::{
    AccountUsageSnapshot, CredentialPool, OAuthManager, Subscription, read_subscription_accounts,
};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// One rate-limit window of an account.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    /// What the window limits ("Claude 5 Hour", "7 days (Spark)").
    pub label: String,
    /// Share of the limit used, in percent; `None` when the provider did not report it.
    pub used_percent: Option<f64>,
    /// Seconds until the window resets, when reported.
    pub resets_in_secs: Option<u64>,
}

/// One logged-in account.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountUsage {
    /// E-mail, organization and plan, joined for display.
    pub identity: String,
    /// Why the limits could not be read (rejected login, network error).
    pub problem: Option<String>,
    /// The windows the provider reported.
    pub windows: Vec<UsageWindow>,
}

/// The accounts of one provider.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderUsage {
    /// Display name ("Anthropic", "Openai Codex").
    pub provider: String,
    /// Logged-in accounts for this provider.
    pub accounts: Vec<AccountUsage>,
}

/// The limits of every logged-in subscription.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageReport {
    /// One entry per provider with a login.
    pub providers: Vec<ProviderUsage>,
}

static DEFAULT_POOL: LazyLock<CredentialPool> = LazyLock::new(CredentialPool::default);
static REPORT_CACHE: LazyLock<Mutex<HashMap<(String, String), AccountUsage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Seconds since the Unix epoch.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Read the limits of every stored account of every subscription whose credentials are in
/// `auth_dir`, using the process-default usage cache.
pub async fn fetch_usage(http: &HttpClient, auth_dir: &Path) -> UsageReport {
    fetch_usage_with_pool(http, auth_dir, &DEFAULT_POOL).await
}

/// Read and cache the limits of every stored account of every subscription in `auth_dir`,
/// recording snapshots into `pool` (5 min ±25% TTL, 24h last-good, 10s failure backoff).
pub async fn fetch_usage_with_pool(
    http: &HttpClient,
    auth_dir: &Path,
    pool: &CredentialPool,
) -> UsageReport {
    let oauth = OAuthManager::default();
    let (claude, chatgpt, kimi, agy) = tokio::join!(
        Box::pin(provider_accounts(
            http,
            &oauth,
            auth_dir,
            Subscription::Claude,
            pool,
        )),
        Box::pin(provider_accounts(
            http,
            &oauth,
            auth_dir,
            Subscription::Chatgpt,
            pool,
        )),
        Box::pin(provider_accounts(
            http,
            &oauth,
            auth_dir,
            Subscription::KimiCode,
            pool,
        )),
        Box::pin(agy_usage_accounts(pool)),
    );
    let mut providers = Vec::new();
    if !claude.is_empty() {
        providers.push(ProviderUsage {
            provider: "Anthropic".to_owned(),
            accounts: claude,
        });
    }
    if !chatgpt.is_empty() {
        providers.push(ProviderUsage {
            provider: "Openai Codex".to_owned(),
            accounts: chatgpt,
        });
    }
    if !kimi.is_empty() {
        providers.push(ProviderUsage {
            provider: "Kimi Code".to_owned(),
            accounts: kimi,
        });
    }
    if !agy.is_empty() {
        providers.push(ProviderUsage {
            provider: "Antigravity".to_owned(),
            accounts: agy,
        });
    }
    UsageReport { providers }
}

/// Fetch Antigravity quota via `agy --print /usage --output-format json` (0 tokens) when `agy`
/// is installed, caching snapshots in `pool` and `REPORT_CACHE`.
async fn agy_usage_accounts(pool: &CredentialPool) -> Vec<AccountUsage> {
    let Some(binary) = crate::agy_acp::discover_agy_binary(None) else {
        return Vec::new();
    };
    let provider_key = "agy";
    let acct_key = "default";
    let cache_key = (provider_key.to_owned(), acct_key.to_owned());
    let now = now_ms();
    if !pool.usage_needs_refresh(provider_key, acct_key, now)
        && let Some(cached) = REPORT_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&cache_key)
            .cloned()
    {
        return vec![cached];
    }
    let row = crate::agy_acp::fetch_agy_usage(&binary, crate::agy_acp::DEFAULT_USAGE_TIMEOUT).await;
    if row.problem.is_none() {
        if let Some(snap) = snapshot_from_windows(&row.windows, now) {
            pool.record_usage(provider_key, acct_key, snap);
        }
        REPORT_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(cache_key, row.clone());
    } else {
        pool.record_usage_failure(provider_key, acct_key, now);
    }
    vec![row]
}

/// Fetch usage for every stored account in `kind`'s credential file.
async fn provider_accounts(
    http: &HttpClient,
    oauth: &OAuthManager,
    auth_dir: &Path,
    kind: Subscription,
    pool: &CredentialPool,
) -> Vec<AccountUsage> {
    let file = auth_dir.join(kind.file_name());
    if !file.is_file() {
        return Vec::new();
    }
    let stored = match read_subscription_accounts(kind, &file) {
        Ok(accounts) if !accounts.is_empty() => accounts,
        Ok(_) => return Vec::new(),
        Err(e) => {
            return vec![AccountUsage {
                identity: kind.label().to_owned(),
                problem: Some(format!(
                    "login unusable ({e}); run `hrn login {}`",
                    kind.id()
                )),
                windows: Vec::new(),
            }];
        }
    };
    let mut out = Vec::with_capacity(stored.len());
    for entry in &stored {
        let acct_key = entry.identity_key();
        let cache_key = (kind.id().to_owned(), acct_key.clone());
        let now = now_ms();
        if !pool.usage_needs_refresh(kind.id(), &acct_key, now)
            && let Some(cached) = REPORT_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&cache_key)
                .cloned()
        {
            out.push(cached);
            continue;
        }
        let cred = match oauth
            .subscription_account(http, kind, &file, &acct_key)
            .await
        {
            Ok(cred) => cred,
            Err(e) => {
                pool.record_usage_failure(kind.id(), &acct_key, now);
                out.push(AccountUsage {
                    identity: entry.account_label().unwrap_or(kind.label()).to_owned(),
                    problem: Some(format!(
                        "login unusable ({e}); run `hrn login {}`",
                        kind.id()
                    )),
                    windows: Vec::new(),
                });
                continue;
            }
        };
        let row = match kind {
            Subscription::Claude => usage_claude::fetch(http, &cred, auth_dir).await,
            Subscription::Chatgpt => usage_codex::fetch(http, &cred).await,
            Subscription::KimiCode => usage_kimi::fetch(http, &cred, auth_dir).await,
        };
        if row.problem.is_none() {
            if let Some(snap) = snapshot_from_windows(&row.windows, now) {
                pool.record_usage(kind.id(), &acct_key, snap);
            }
            REPORT_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(cache_key, row.clone());
        } else {
            pool.record_usage_failure(kind.id(), &acct_key, now);
        }
        out.push(row);
    }
    out
}

/// Convert rendered usage windows into a pool-routing snapshot using the most constrained window.
fn snapshot_from_windows(
    windows: &[UsageWindow],
    fetched_at_ms: u64,
) -> Option<AccountUsageSnapshot> {
    windows
        .iter()
        .filter_map(|w| {
            let used = w.used_percent?;
            let remaining_pct = (100.0 - used).clamp(0.0, 100.0);
            let resets_at_ms = w
                .resets_in_secs
                .map(|s| fetched_at_ms.saturating_add(s.saturating_mul(1000)));
            Some(AccountUsageSnapshot {
                remaining_pct,
                resets_at_ms,
                fetched_at_ms,
            })
        })
        .min_by(|a, b| a.remaining_pct.total_cmp(&b.remaining_pct))
}

const BAR_CELLS: u8 = 28;

/// `3h12m`, `2d14h`, `45m`, `<1m`.
fn duration(secs: u64) -> String {
    let (days, hours, mins) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60);
    match (days, hours, mins) {
        (0, 0, 0) => "<1m".to_owned(),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h{m:02}m"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

fn bar(percent: f64) -> String {
    let clamped = percent.clamp(0.0, 100.0);
    let cells = f64::from(BAR_CELLS);
    // A cell is filled when the percentage reaches its midpoint.
    let filled = (0..BAR_CELLS)
        .filter(|&i| (f64::from(i) + 0.5) * 100.0 / cells <= clamped)
        .count();
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(usize::from(BAR_CELLS) - filled)
    )
}

/// The report as text for the terminal and the `/usage` notice.
pub fn render_usage(report: &UsageReport) -> String {
    if report.providers.is_empty() {
        return "No subscription is logged in. Run `/login` (or `hrn login claude|chatgpt|kimi-code|agy`) \
                to see provider usage limits.\n"
            .to_owned();
    }
    let width = report
        .providers
        .iter()
        .flat_map(|p| &p.accounts)
        .flat_map(|a| &a.windows)
        .map(|w| w.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for provider in &report.providers {
        let n = provider.accounts.len();
        let _ = writeln!(
            out,
            "{} — {n} account{}",
            provider.provider,
            if n == 1 { "" } else { "s" }
        );
        for account in &provider.accounts {
            let _ = match &account.problem {
                Some(problem) => writeln!(out, "  ✗ {} — {problem}", account.identity),
                None => writeln!(out, "  ● {}", account.identity),
            };
            for window in &account.windows {
                out.push_str(&window_line(window, width));
            }
        }
        out.push('\n');
    }
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
}

fn window_line(window: &UsageWindow, width: usize) -> String {
    let label = &window.label;
    let Some(percent) = window.used_percent else {
        return format!("      ○ {label:<width$}  not reported\n");
    };
    let reset = window
        .resets_in_secs
        .map_or_else(String::new, |s| format!(" · resets in {}", duration(s)));
    format!(
        "      ● {label:<width$}  {}  {percent:.1}% used{reset}\n",
        bar(percent)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_use_the_two_largest_units() {
        assert_eq!(duration(30), "<1m");
        assert_eq!(duration(45 * 60), "45m");
        assert_eq!(duration(3 * 3600 + 12 * 60), "3h12m");
        assert_eq!(duration(2 * 86_400 + 14 * 3600 + 59), "2d14h");
    }

    #[test]
    fn a_report_lists_accounts_windows_and_problems() {
        let report = UsageReport {
            providers: vec![
                ProviderUsage {
                    provider: "Anthropic".into(),
                    accounts: vec![AccountUsage {
                        identity: "a@b.c · Max 5x".into(),
                        problem: None,
                        windows: vec![
                            UsageWindow {
                                label: "Claude 5 Hour".into(),
                                used_percent: Some(50.0),
                                resets_in_secs: Some(3600),
                            },
                            UsageWindow {
                                label: "Claude 7 Day".into(),
                                used_percent: None,
                                resets_in_secs: None,
                            },
                        ],
                    }],
                },
                ProviderUsage {
                    provider: "Openai Codex".into(),
                    accounts: vec![AccountUsage {
                        identity: "ChatGPT".into(),
                        problem: Some("account disabled".into()),
                        windows: Vec::new(),
                    }],
                },
            ],
        };
        let text = render_usage(&report);
        assert!(
            text.starts_with("Anthropic — 1 account\n  ● a@b.c · Max 5x\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "{}{}  50.0% used · resets in 1h00m",
                "█".repeat(14),
                "░".repeat(14)
            )),
            "{text}"
        );
        assert!(text.contains("○ Claude 7 Day  "), "{text}");
        assert!(text.contains("✗ ChatGPT — account disabled"), "{text}");
    }

    #[test]
    fn no_login_says_so() {
        assert!(render_usage(&UsageReport::default()).contains("No subscription is logged in"));
    }
}
