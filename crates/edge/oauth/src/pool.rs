//! Multi-account subscription credential pool: ranking, cooldowns/invalidation, round-robin
//! balancing, and per-account usage snapshot cache.

use crate::sub_store::SubscriptionCredential;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Default remaining-quota reserve percentage below which healthier sibling accounts win.
pub const DEFAULT_RESERVE_PCT: u8 = 10;
/// Base usage-cache TTL (5 minutes) before ±25% deterministic per-account jitter.
pub const USAGE_CACHE_TTL_MS: u64 = 5 * 60 * 1_000;
/// Retention window for the last successful usage snapshot (24 hours).
pub const USAGE_LAST_GOOD_MS: u64 = 24 * 60 * 60 * 1_000;
/// Backoff after a failed usage refresh before retrying (10 seconds).
pub const USAGE_FAILURE_BACKOFF_MS: u64 = 10_000;

/// Per-account routing priority and optional reserve threshold (`[[llm.accounts]]`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountPolicy {
    /// Subscription provider id (e.g. `"claude"`, `"chatgpt"`, `"anthropic"`, `"codex"`).
    pub provider: String,
    /// Account identity (`account_id`, `chatgpt_account_id`, or `email`).
    pub account: String,
    /// Higher priority accounts are selected ahead of lower priority accounts (default `0`).
    #[serde(default)]
    pub priority: i32,
    /// Optional per-account reserve threshold (`0..=100`) overriding `[llm.retry] usage_reserve_pct`.
    #[serde(default)]
    pub reserve_pct: Option<u8>,
}

/// Cached remaining-allowance snapshot for one subscription account.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccountUsageSnapshot {
    /// Remaining share of the most constraining usage window, in percent (`0.0..=100.0`).
    pub remaining_pct: f64,
    /// Unix epoch milliseconds when the window resets, when known.
    pub resets_at_ms: Option<u64>,
    /// Unix epoch milliseconds when this snapshot was fetched.
    pub fetched_at_ms: u64,
}

#[derive(Clone, Default)]
struct CachedUsage {
    last_good: Option<AccountUsageSnapshot>,
    last_failure_ms: Option<u64>,
}

#[derive(Default)]
struct PoolState {
    default_reserve_pct: u8,
    /// `(canonical_provider, account_id) -> blocked_until_ms`.
    blocked: HashMap<(String, String), u64>,
    /// Accounts invalidated after 401/auth rejection until explicitly cleared.
    invalidated: HashSet<(String, String)>,
    /// `canonical_provider -> last_picked_account_id` for deterministic round-robin.
    last_picked: HashMap<String, String>,
    /// Per-account usage cache with TTL, 24h last-good retention, and 10s failure backoff.
    usage_cache: HashMap<(String, String), CachedUsage>,
}

/// Thread-safe subscription credential selector and usage cache.
pub struct CredentialPool {
    state: Mutex<PoolState>,
}

impl Default for CredentialPool {
    fn default() -> Self {
        Self::new(DEFAULT_RESERVE_PCT)
    }
}

impl CredentialPool {
    /// Build a pool with the given default reserve threshold (`0..=100`).
    #[must_use]
    pub fn new(default_reserve_pct: u8) -> Self {
        Self {
            state: Mutex::new(PoolState {
                default_reserve_pct: default_reserve_pct.min(100),
                ..PoolState::default()
            }),
        }
    }

    /// Update the default reserve threshold (`0..=100`) on config reload.
    pub fn set_default_reserve_pct(&self, reserve_pct: u8) {
        let mut guard = self.lock();
        guard.default_reserve_pct = reserve_pct.min(100);
    }

    /// Current default reserve percentage.
    #[must_use]
    pub fn default_reserve_pct(&self) -> u8 {
        self.lock().default_reserve_pct
    }

    /// Mark `(provider, account_id)` as rate-limited / blocked until `until_ms`.
    pub fn mark_blocked(&self, provider: &str, account_id: &str, until_ms: u64) {
        let mut guard = self.lock();
        let key = pool_key(provider, account_id);
        let entry = guard.blocked.entry(key).or_insert(0);
        if until_ms > *entry {
            *entry = until_ms;
        }
    }

    /// Mark `(provider, account_id)` as invalidated (e.g. unrecoverable 401) so selection skips it.
    pub fn invalidate(&self, provider: &str, account_id: &str) {
        let mut guard = self.lock();
        guard.invalidated.insert(pool_key(provider, account_id));
    }

    /// Clear an invalidated marker (e.g. after a fresh login).
    pub fn clear_invalidated(&self, provider: &str, account_id: &str) {
        let mut guard = self.lock();
        guard.invalidated.remove(&pool_key(provider, account_id));
    }

    /// Clear every invalidated marker (e.g. on live config reload or after external re-login).
    pub fn clear_all_invalidated(&self) {
        self.lock().invalidated.clear();
    }

    /// Clear all cached pool state (blocked, invalidated, round-robin, usage) for `provider`
    /// (used after `/logout`).
    pub fn clear_provider(&self, provider: &str) {
        let canon = canonical_provider(provider);
        let mut guard = self.lock();
        guard.blocked.retain(|(p, _), _| p != canon);
        guard.invalidated.retain(|(p, _)| p != canon);
        guard.last_picked.remove(canon);
        guard.usage_cache.retain(|(p, _), _| p != canon);
    }

    /// Whether `(provider, account_id)` is currently blocked at `now_ms`.
    #[must_use]
    pub fn is_blocked(&self, provider: &str, account_id: &str, now_ms: u64) -> bool {
        let guard = self.lock();
        let key = pool_key(provider, account_id);
        guard.blocked.get(&key).is_some_and(|until| *until > now_ms)
    }

    /// Deterministic jittered cache TTL (5 min ± 25%, i.e. 225s..=375s) for `(provider, account_id)`.
    #[must_use]
    pub fn ttl_ms_for(provider: &str, account_id: &str) -> u64 {
        let h = fnv1a64(&[
            canonical_provider(provider).as_bytes(),
            b":",
            account_id.as_bytes(),
        ]);
        // Factor in [750, 1250] permille => exact ±25% around USAGE_CACHE_TTL_MS.
        let permille = 750 + (h % 501);
        USAGE_CACHE_TTL_MS.saturating_mul(permille) / 1000
    }

    /// Store a freshly fetched usage snapshot for `(provider, account_id)`.
    pub fn record_usage(&self, provider: &str, account_id: &str, snapshot: AccountUsageSnapshot) {
        let mut guard = self.lock();
        let entry = guard
            .usage_cache
            .entry(pool_key(provider, account_id))
            .or_default();
        entry.last_good = Some(snapshot);
        entry.last_failure_ms = None;
    }

    /// Record a failed usage fetch at `now_ms` (arms the 10-second failure backoff while keeping
    /// any last-good snapshot up to 24 hours old).
    pub fn record_usage_failure(&self, provider: &str, account_id: &str, now_ms: u64) {
        let mut guard = self.lock();
        let entry = guard
            .usage_cache
            .entry(pool_key(provider, account_id))
            .or_default();
        entry.last_failure_ms = Some(now_ms);
    }

    /// Whether `(provider, account_id)` should be refreshed at `now_ms`, honoring both the
    /// jittered 5-minute TTL and the 10-second failure backoff.
    #[must_use]
    pub fn usage_needs_refresh(&self, provider: &str, account_id: &str, now_ms: u64) -> bool {
        let guard = self.lock();
        let key = pool_key(provider, account_id);
        let Some(entry) = guard.usage_cache.get(&key) else {
            return true;
        };
        if let Some(failed_at) = entry.last_failure_ms
            && now_ms.saturating_sub(failed_at) < USAGE_FAILURE_BACKOFF_MS
        {
            return false;
        }
        let Some(snap) = entry.last_good else {
            return true;
        };
        now_ms.saturating_sub(snap.fetched_at_ms) >= Self::ttl_ms_for(provider, account_id)
    }

    /// Return the cached usage snapshot if it is still within the 24-hour last-good window.
    #[must_use]
    pub fn cached_usage(
        &self,
        provider: &str,
        account_id: &str,
        now_ms: u64,
    ) -> Option<AccountUsageSnapshot> {
        let guard = self.lock();
        let key = pool_key(provider, account_id);
        let snap = guard.usage_cache.get(&key)?.last_good?;
        (now_ms.saturating_sub(snap.fetched_at_ms) <= USAGE_LAST_GOOD_MS).then_some(snap)
    }

    /// Snapshot map of all retained (≤24h) cached usage entries for `provider`.
    #[must_use]
    pub fn cached_usage_map(
        &self,
        provider: &str,
        now_ms: u64,
    ) -> HashMap<String, AccountUsageSnapshot> {
        let guard = self.lock();
        let canon = canonical_provider(provider);
        let mut out = HashMap::new();
        for ((prov, acct), entry) in &guard.usage_cache {
            if prov == canon
                && let Some(snap) = entry.last_good
                && now_ms.saturating_sub(snap.fetched_at_ms) <= USAGE_LAST_GOOD_MS
            {
                out.insert(acct.clone(), snap);
            }
        }
        out
    }

    /// Select the best available account for `provider` at `now_ms`, advancing deterministic
    /// round-robin within equal rank & priority.
    ///
    /// Ranking (best first):
    /// 1. Actively blocked (`until > now_ms`) and invalidated accounts are excluded.
    /// 2. Healthy accounts (`remaining_pct > reserve_pct` or no recent usage or reset elapsed).
    /// 3. Accounts inside reserve (`0 < remaining_pct <= reserve_pct`).
    /// 4. Accounts past renewable allowance (`remaining_pct <= 0`, before reset).
    /// 5. Higher [`AccountPolicy::priority`].
    /// 6. Deterministic round-robin within the tied top group.
    #[must_use]
    pub fn select(
        &self,
        provider: &str,
        accounts: &[SubscriptionCredential],
        policies: &[AccountPolicy],
        usage: &HashMap<String, AccountUsageSnapshot>,
        now_ms: u64,
    ) -> Option<SubscriptionCredential> {
        self.select_for_session(provider, accounts, policies, usage, now_ms, None)
    }

    /// Same ranking as [`Self::select`], with an optional `session_seed` for deterministic
    /// stateless per-session hashing (used by `hrn dry-balance`).
    #[must_use]
    pub fn select_for_session(
        &self,
        provider: &str,
        accounts: &[SubscriptionCredential],
        policies: &[AccountPolicy],
        usage: &HashMap<String, AccountUsageSnapshot>,
        now_ms: u64,
        session_seed: Option<&str>,
    ) -> Option<SubscriptionCredential> {
        let mut guard = self.lock();
        let canon = canonical_provider(provider).to_owned();
        // Prune expired blocks lazily so accounts return to rotation once their window resets.
        guard.blocked.retain(|_, until| *until > now_ms);

        let default_reserve = guard.default_reserve_pct;
        let mut ranked: Vec<(u8, i32, usize, String)> = Vec::new();
        for (idx, cred) in accounts.iter().enumerate() {
            let id = cred.identity_key();
            let key = (canon.clone(), id.clone());
            if guard.blocked.contains_key(&key) || guard.invalidated.contains(&key) {
                continue;
            }
            let policy = find_policy(policies, &canon, cred, &id);
            let priority = policy.map_or(0, |p| p.priority);
            let reserve_pct = policy
                .and_then(|p| p.reserve_pct)
                .unwrap_or(default_reserve)
                .min(100);
            let snap = usage.get(&id).copied().or_else(|| {
                guard
                    .usage_cache
                    .get(&key)
                    .and_then(|c| c.last_good)
                    .filter(|s| now_ms.saturating_sub(s.fetched_at_ms) <= USAGE_LAST_GOOD_MS)
            });
            let band = health_band(snap, reserve_pct, now_ms);
            ranked.push((band, priority, idx, id));
        }
        if ranked.is_empty() {
            return None;
        }
        let best_band = ranked.iter().map(|r| r.0).min()?;
        let best_priority = ranked
            .iter()
            .filter(|r| r.0 == best_band)
            .map(|r| r.1)
            .max()?;
        let tie_group: Vec<(usize, String)> = ranked
            .into_iter()
            .filter(|r| r.0 == best_band && r.1 == best_priority)
            .map(|r| (r.2, r.3))
            .collect();

        let chosen_pos = if let Some(seed) = session_seed {
            let h = fnv1a64(&[canon.as_bytes(), b":", seed.as_bytes()]);
            usize::try_from(h % u64::try_from(tie_group.len()).unwrap_or(1)).unwrap_or(0)
        } else {
            match guard.last_picked.get(&canon) {
                Some(prev) => tie_group
                    .iter()
                    .position(|(_, id)| id == prev)
                    .map_or(0, |pos| (pos + 1) % tie_group.len()),
                None => 0,
            }
        };
        let (orig_idx, picked_id) = tie_group.get(chosen_pos)?;
        if session_seed.is_none() {
            guard.last_picked.insert(canon, picked_id.clone());
        }
        accounts.get(*orig_idx).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Normalize subscription provider aliases so policies written as `"anthropic"`/`"claude"` or
/// `"codex"`/`"openai-codex"`/`"chatgpt"` match the same pool bucket.
#[must_use]
pub fn canonical_provider(provider: &str) -> &str {
    let lower = provider.trim();
    if lower.eq_ignore_ascii_case("anthropic") || lower.eq_ignore_ascii_case("claude") {
        "claude"
    } else if lower.eq_ignore_ascii_case("chatgpt")
        || lower.eq_ignore_ascii_case("codex")
        || lower.eq_ignore_ascii_case("openai-codex")
    {
        "chatgpt"
    } else if lower.eq_ignore_ascii_case("kimi-code") || lower.eq_ignore_ascii_case("kimi_code") {
        "kimi-code"
    } else {
        lower
    }
}

fn pool_key(provider: &str, account_id: &str) -> (String, String) {
    (
        canonical_provider(provider).to_owned(),
        account_id.trim().to_owned(),
    )
}

fn find_policy<'a>(
    policies: &'a [AccountPolicy],
    canon_provider: &str,
    cred: &SubscriptionCredential,
    id: &str,
) -> Option<&'a AccountPolicy> {
    policies.iter().find(|p| {
        canonical_provider(&p.provider) == canon_provider
            && (p.account == id || cred.email.as_deref() == Some(p.account.as_str()))
    })
}

/// Lower band number wins:
/// `0` = healthy (no snapshot, reset elapsed, or `remaining_pct > reserve_pct`),
/// `1` = inside reserve (`0.0 < remaining_pct <= reserve_pct`),
/// `2` = past renewable allowance (`remaining_pct <= 0.0` before reset).
fn health_band(snap: Option<AccountUsageSnapshot>, reserve_pct: u8, now_ms: u64) -> u8 {
    let Some(snap) = snap else {
        return 0;
    };
    if snap.resets_at_ms.is_some_and(|reset| reset <= now_ms) {
        return 0;
    }
    if snap.remaining_pct <= 0.0 {
        2
    } else {
        u8::from(snap.remaining_pct <= f64::from(reserve_pct))
    }
}

/// 64-bit FNV-1a hash used for deterministic TTL jitter and session balancing.
fn fnv1a64(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &byte in *part {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(id: &str) -> SubscriptionCredential {
        SubscriptionCredential {
            provider: "claude".into(),
            access: format!("tok-{id}"),
            refresh: format!("ref-{id}"),
            expires: u64::MAX,
            account_id: Some(id.into()),
            ..SubscriptionCredential::default()
        }
    }

    #[test]
    fn pool_blocked_expires_round_robin() {
        let pool = CredentialPool::new(10);
        let accounts = vec![acct("a"), acct("b")];
        let usage = HashMap::new();

        // Block A until t=10_000: selection must return B while blocked.
        pool.mark_blocked("claude", "a", 10_000);
        let p1 = pool
            .select("claude", &accounts, &[], &usage, 5_000)
            .expect("b while a blocked");
        assert_eq!(p1.identity_key(), "b");
        let p2 = pool
            .select("claude", &accounts, &[], &usage, 9_999)
            .expect("still b");
        assert_eq!(p2.identity_key(), "b");

        // At t=10_000 A's block expires and round-robin returns to A, then B, then A.
        let p3 = pool
            .select("claude", &accounts, &[], &usage, 10_000)
            .expect("a after expiry");
        assert_eq!(p3.identity_key(), "a");
        let p4 = pool
            .select("claude", &accounts, &[], &usage, 10_001)
            .expect("b next");
        assert_eq!(p4.identity_key(), "b");
        let p5 = pool
            .select("claude", &accounts, &[], &usage, 10_002)
            .expect("a again");
        assert_eq!(p5.identity_key(), "a");

        // Blocking both returns None so the router moves to fallback presets.
        pool.mark_blocked("claude", "a", 20_000);
        pool.mark_blocked("claude", "b", 20_000);
        assert!(
            pool.select("claude", &accounts, &[], &usage, 15_000)
                .is_none()
        );
    }

    #[test]
    fn pool_reserve_skips_account() {
        let pool = CredentialPool::new(10);
        let accounts = vec![acct("a"), acct("b")];
        let policies = vec![AccountPolicy {
            provider: "claude".into(),
            account: "a".into(),
            priority: 10, // even with higher priority, reserve loses to healthy B
            reserve_pct: Some(50),
        }];
        let mut usage = HashMap::new();
        // A has 40% remaining (60% used) <= reserve_pct 50% -> inside reserve.
        usage.insert(
            "a".to_owned(),
            AccountUsageSnapshot {
                remaining_pct: 40.0,
                resets_at_ms: Some(90_000),
                fetched_at_ms: 1_000,
            },
        );
        usage.insert(
            "b".to_owned(),
            AccountUsageSnapshot {
                remaining_pct: 80.0,
                resets_at_ms: Some(90_000),
                fetched_at_ms: 1_000,
            },
        );

        let picked = pool
            .select("claude", &accounts, &policies, &usage, 5_000)
            .expect("pick b");
        assert_eq!(picked.identity_key(), "b");

        // Once the window resets (now >= 90_000), A is healthy again and its priority wins.
        let after_reset = pool
            .select("claude", &accounts, &policies, &usage, 90_000)
            .expect("pick a after reset");
        assert_eq!(after_reset.identity_key(), "a");
    }

    #[test]
    fn invalidate_and_priority_ordering() {
        let pool = CredentialPool::new(10);
        let accounts = vec![acct("low"), acct("high")];
        let policies = vec![
            AccountPolicy {
                provider: "anthropic".into(),
                account: "high".into(),
                priority: 5,
                reserve_pct: None,
            },
            AccountPolicy {
                provider: "claude".into(),
                account: "low".into(),
                priority: 1,
                reserve_pct: None,
            },
        ];
        let empty = HashMap::new();
        assert_eq!(
            pool.select("claude", &accounts, &policies, &empty, 1_000)
                .expect("high")
                .identity_key(),
            "high"
        );
        pool.invalidate("claude", "high");
        assert_eq!(
            pool.select("claude", &accounts, &policies, &empty, 1_000)
                .expect("low after invalidate")
                .identity_key(),
            "low"
        );
        pool.clear_invalidated("claude", "high");
        assert_eq!(
            pool.select("claude", &accounts, &policies, &empty, 1_000)
                .expect("high restored")
                .identity_key(),
            "high"
        );
        pool.invalidate("claude", "high");
        pool.invalidate("claude", "low");
        assert!(
            pool.select("claude", &accounts, &policies, &empty, 1_000)
                .is_none()
        );
        pool.clear_all_invalidated();
        assert_eq!(
            pool.select("claude", &accounts, &policies, &empty, 1_000)
                .expect("restored after clear_all")
                .identity_key(),
            "high"
        );
    }

    #[test]
    fn usage_cache_enforces_jittered_ttl_last_good_24h_and_10s_backoff() {
        let pool = CredentialPool::new(10);
        let ttl = CredentialPool::ttl_ms_for("claude", "acct-1");
        assert!((225_000..=375_000).contains(&ttl), "ttl={ttl}");
        assert_eq!(ttl, CredentialPool::ttl_ms_for("anthropic", "acct-1"));

        let t0 = 1_000_000;
        assert!(pool.usage_needs_refresh("claude", "acct-1", t0));

        let snap = AccountUsageSnapshot {
            remaining_pct: 65.0,
            resets_at_ms: Some(t0 + 3_600_000),
            fetched_at_ms: t0,
        };
        pool.record_usage("claude", "acct-1", snap);
        assert!(!pool.usage_needs_refresh("claude", "acct-1", t0 + ttl - 1));
        assert!(pool.usage_needs_refresh("claude", "acct-1", t0 + ttl));

        // Failure triggers 10s backoff while retaining last-good snapshot up to 24h.
        let fail_t = t0 + ttl;
        pool.record_usage_failure("claude", "acct-1", fail_t);
        assert!(!pool.usage_needs_refresh("claude", "acct-1", fail_t + 9_999));
        assert!(pool.usage_needs_refresh("claude", "acct-1", fail_t + 10_000));
        assert_eq!(
            pool.cached_usage("claude", "acct-1", t0 + USAGE_LAST_GOOD_MS),
            Some(snap)
        );
        assert_eq!(
            pool.cached_usage("claude", "acct-1", t0 + USAGE_LAST_GOOD_MS + 1),
            None
        );
    }
}
