//! `hrn dry-balance [provider] --sessions N`: pure offline simulation of multi-account pool
//! balancing (zero network calls).

use crate::cli::DryBalanceArgs;
use crate::config::Config;
use crate::term::out;
use hrn_edge_oauth::{
    AccountPolicy, CredentialPool, DEFAULT_RESERVE_PCT, Subscription, SubscriptionCredential,
    canonical_provider, read_subscription_accounts,
};
use hrn_kernel::{HrnError, Result};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

/// Simulated session distribution across one provider's stored accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BalanceReport {
    pub(crate) provider: String,
    pub(crate) sessions: usize,
    pub(crate) counts: Vec<(String, usize)>,
}

/// Pure offline simulation: distributes `sessions` synthetic session seeds across `accounts`
/// through [`CredentialPool::select_for_session`]. Zero network I/O.
pub(crate) fn simulate(
    provider: &str,
    accounts: &[SubscriptionCredential],
    policies: &[AccountPolicy],
    reserve_pct: u8,
    sessions: usize,
) -> BalanceReport {
    let pool = CredentialPool::new(reserve_pct);
    let usage = HashMap::new();
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    for acct in accounts {
        totals.entry(acct.identity_key()).or_insert(0);
    }
    for i in 0..sessions {
        let seed = format!("dry-session-{i}");
        if let Some(picked) =
            pool.select_for_session(provider, accounts, policies, &usage, 0, Some(&seed))
        {
            *totals.entry(picked.identity_key()).or_insert(0) += 1;
        }
    }
    BalanceReport {
        provider: canonical_provider(provider).to_owned(),
        sessions,
        counts: totals.into_iter().collect(),
    }
}

/// Format one simulated report as a crisp terminal table.
pub(crate) fn render(report: &BalanceReport) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "{} ({} sessions)", report.provider, report.sessions);
    if report.counts.is_empty() {
        let _ = writeln!(text, "  (no accounts stored)");
        return text;
    }
    let denom = report.sessions.max(1);
    for (account, count) in &report.counts {
        #[expect(
            clippy::cast_precision_loss,
            reason = "display percentage over dry-balance session counts"
        )]
        let pct = (*count as f64) * 100.0 / (denom as f64);
        let _ = writeln!(text, "  {}/{account}  {count} ({pct:.1}%)", report.provider);
    }
    text
}

/// Read local credentials from `auth_dir` and run the offline dry-balance simulation.
pub(crate) fn simulate_from_auth_dir(
    auth_dir: &Path,
    cfg: Option<&Config>,
    args: &DryBalanceArgs,
) -> Result<Vec<BalanceReport>> {
    let targets: Vec<Subscription> = match args.provider.as_deref() {
        Some(raw) => {
            let canon = canonical_provider(raw);
            let kind = Subscription::from_id(canon).ok_or_else(|| {
                HrnError::Config(format!(
                    "unknown subscription provider `{raw}` (expected `claude`, `chatgpt`, or `kimi-code`)"
                ))
            })?;
            vec![kind]
        }
        None => Subscription::ALL
            .into_iter()
            .filter(|k| auth_dir.join(k.file_name()).is_file())
            .collect(),
    };
    let policies: &[AccountPolicy] = cfg.map_or(&[], |c| c.llm.accounts.as_slice());
    let reserve = cfg.map_or(DEFAULT_RESERVE_PCT, |c| c.llm.retry.usage_reserve_pct);
    let mut reports = Vec::with_capacity(targets.len());
    for kind in targets {
        let file = auth_dir.join(kind.file_name());
        let accounts = if file.is_file() {
            read_subscription_accounts(kind, &file)?
        } else {
            Vec::new()
        };
        reports.push(simulate(
            kind.id(),
            &accounts,
            policies,
            reserve,
            args.sessions,
        ));
    }
    Ok(reports)
}

/// Execute `hrn dry-balance [provider] --sessions N` and write results to stdout (zero network).
pub(crate) fn run(auth_dir: &Path, cfg: Option<&Config>, args: &DryBalanceArgs) -> Result {
    let reports = simulate_from_auth_dir(auth_dir, cfg, args)?;
    if reports.is_empty() {
        out(
            "No subscription accounts stored. Run `hrn login claude`, `hrn login chatgpt`, or `hrn login kimi-code`.\n",
        );
        return Ok(());
    }
    for (idx, report) in reports.iter().enumerate() {
        if idx > 0 {
            out("\n");
        }
        out(&render(report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_edge_oauth::upsert_subscription_account;

    fn cred(provider: &str, id: &str) -> SubscriptionCredential {
        SubscriptionCredential {
            provider: provider.into(),
            access: format!("tok-{id}"),
            refresh: format!("ref-{id}"),
            expires: u64::MAX / 2,
            account_id: Some(id.into()),
            ..SubscriptionCredential::default()
        }
    }

    #[test]
    fn dry_balance_distributes_offline() {
        let dir = std::env::temp_dir().join(format!("hrn-dry-balance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("claude.json");
        upsert_subscription_account(Subscription::Claude, &file, cred("claude", "acct-a"))
            .expect("seed a");
        upsert_subscription_account(Subscription::Claude, &file, cred("claude", "acct-b"))
            .expect("seed b");

        let args = DryBalanceArgs {
            provider: Some("claude".into()),
            sessions: 1000,
        };
        let reports = simulate_from_auth_dir(&dir, None, &args).expect("offline simulate");
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.provider, "claude");
        assert_eq!(report.sessions, 1000);
        let total: usize = report.counts.iter().map(|(_, n)| *n).sum();
        assert_eq!(total, 1000, "rows must sum to 1000");
        assert_eq!(report.counts.len(), 2);
        for (acct, count) in &report.counts {
            assert!(
                *count > 0,
                "equal-priority account {acct} should receive traffic: {:?}",
                report.counts
            );
        }
        let rendered = render(report);
        assert!(rendered.contains("claude/acct-a"), "{rendered}");
        assert!(rendered.contains("claude/acct-b"), "{rendered}");

        // Higher priority policy routes all sessions to the preferred account.
        let accounts = vec![cred("claude", "acct-a"), cred("claude", "acct-b")];
        let policies = vec![AccountPolicy {
            provider: "claude".into(),
            account: "acct-b".into(),
            priority: 10,
            reserve_pct: None,
        }];
        let prioritized = simulate("claude", &accounts, &policies, 10, 100);
        let b_count = prioritized
            .counts
            .iter()
            .find(|(id, _)| id == "acct-b")
            .map(|(_, n)| *n);
        assert_eq!(b_count, Some(100));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
