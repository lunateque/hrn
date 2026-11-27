//! Stored subscription credentials: the on-disk shape, the token-reply mapping (provider expiry
//! skews, identity claims) and a non-secret status for `hrn auth status`.

use crate::jwt::chatgpt_identity;
use crate::manager::write_json_atomic;
use crate::subscription::Subscription;
use hrn_edge_http::guard;
use hrn_kernel::{HrnError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::path::Path;

/// Claude access tokens count as expired five minutes early (omp `skew-ms=300000`).
const CLAUDE_EXPIRY_SKEW_MS: u64 = 300_000;
/// Proactive refresh margin on top of the per-provider skew (omp `OAUTH_REFRESH_SKEW_MS`).
const REFRESH_MARGIN_MS: u64 = 60_000;
/// Expiry assumed when a token reply states none.
const DEFAULT_LIFETIME_MS: u64 = 3_600_000;

/// A stored subscription login. `Debug` never shows the tokens.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionCredential {
    /// [`Subscription::id`] of the login.
    pub provider: String,
    /// Bearer access token.
    pub access: String,
    /// Refresh token (rotated by the provider on refresh).
    #[serde(default)]
    pub refresh: String,
    /// Access-token expiry in Unix milliseconds, provider skew already applied.
    #[serde(default)]
    pub expires: u64,
    /// Account e-mail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Claude account UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Claude organization UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    /// Claude organization name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    /// ChatGPT account id (`chatgpt-account-id` header), from the access-token claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatgpt_account_id: Option<String>,
    /// ChatGPT plan (`plus`, `pro`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// ChatGPT data-residency routing value, when constrained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residency: Option<String>,
}

impl fmt::Debug for SubscriptionCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionCredential")
            .field("provider", &self.provider)
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires", &self.expires)
            .field("email", &self.email)
            .field("account_id", &self.account_id)
            .field("chatgpt_account_id", &self.chatgpt_account_id)
            .field("plan", &self.plan)
            .finish_non_exhaustive()
    }
}

impl SubscriptionCredential {
    /// Whether the access token stays valid past the proactive refresh margin.
    pub fn is_fresh(&self, now_ms: u64) -> bool {
        !self.access.trim().is_empty() && now_ms.saturating_add(REFRESH_MARGIN_MS) < self.expires
    }

    /// Who is logged in, for status lines: the e-mail, else an account id.
    pub fn account_label(&self) -> Option<&str> {
        self.email
            .as_deref()
            .or(self.account_id.as_deref())
            .or(self.chatgpt_account_id.as_deref())
    }

    /// Stable account identity key derived from JWT / provider claims:
    /// `account_id`, else `chatgpt_account_id`, else `email`, else `"default"`.
    #[must_use]
    pub fn identity_key(&self) -> String {
        [
            self.account_id.as_deref(),
            self.chatgpt_account_id.as_deref(),
            self.email.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("default")
        .to_owned()
    }
}

/// Plural on-disk credential store: `{ "accounts": [...] }`.
///
/// `Debug` inherits token redaction from [`SubscriptionCredential`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionStore {
    /// Stored accounts for one subscription provider.
    pub accounts: Vec<SubscriptionCredential>,
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account: Option<ClaudeAccount>,
    #[serde(default)]
    organization: Option<ClaudeOrganization>,
}

#[derive(Deserialize)]
struct ClaudeAccount {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    email_address: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeOrganization {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Map a token-endpoint reply onto a credential. On refresh, `previous` keeps what the reply
/// omits: an unrotated refresh token, and the Claude organization (never re-mapped on refresh).
pub(crate) fn credential_from_reply(
    kind: Subscription,
    body: &[u8],
    previous: Option<&SubscriptionCredential>,
    now: u64,
) -> Result<SubscriptionCredential> {
    let reply: TokenReply = serde_json::from_slice(body)
        .map_err(|e| HrnError::Auth(format!("parse {} token reply: {e}", kind.label())))?;
    if reply.access_token.trim().is_empty() {
        return Err(HrnError::Auth(format!(
            "{} token reply has no access_token",
            kind.label()
        )));
    }
    let mut cred = previous.cloned().unwrap_or_default();
    kind.id().clone_into(&mut cred.provider);
    if let Some(refresh) = reply.refresh_token.filter(|r| !r.trim().is_empty()) {
        cred.refresh = refresh;
    }
    let lifetime = reply.expires_in.map(|secs| secs.saturating_mul(1000));
    match kind {
        Subscription::Claude => {
            cred.expires = now
                .saturating_add(lifetime.unwrap_or(DEFAULT_LIFETIME_MS))
                .saturating_sub(CLAUDE_EXPIRY_SKEW_MS);
            if let Some(account) = reply.account {
                cred.account_id = account.uuid.or(cred.account_id);
                cred.email = account.email_address.or(cred.email);
            }
            if previous.is_none()
                && let Some(org) = reply.organization
            {
                cred.org_id = org.uuid;
                cred.org_name = org.name;
            }
        }
        Subscription::Chatgpt => {
            let identity = chatgpt_identity(&reply.access_token, reply.id_token.as_deref());
            cred.expires = lifetime
                .map(|ms| now.saturating_add(ms))
                .or(identity.expires_ms)
                .unwrap_or_else(|| now.saturating_add(DEFAULT_LIFETIME_MS));
            cred.chatgpt_account_id = identity.account_id.or(cred.chatgpt_account_id);
            cred.plan = identity.plan.or(cred.plan);
            cred.residency = identity.residency;
            cred.email = identity.email.or(cred.email);
        }
        Subscription::KimiCode => {
            cred.expires = now.saturating_add(lifetime.unwrap_or(DEFAULT_LIFETIME_MS));
            if let Some(account) = reply.account {
                cred.account_id = account.uuid.or(cred.account_id);
                cred.email = account.email_address.or(cred.email);
            }
        }
    }
    cred.access = reply.access_token;
    Ok(cred)
}

/// Parse either the plural `{ "accounts": [...] }` store or a legacy single-object credential.
fn parse_subscription_store(
    kind: Subscription,
    file: &Path,
    raw: &str,
) -> Result<SubscriptionStore> {
    let parse_err = |e: serde_json::Error| {
        HrnError::Auth(format!(
            "parse `{}`: {e}; run `hrn login {}` again",
            file.display(),
            kind.id()
        ))
    };
    let value: Value = serde_json::from_str(raw).map_err(parse_err)?;
    if value
        .as_object()
        .is_some_and(|m| m.contains_key("accounts"))
    {
        serde_json::from_value::<SubscriptionStore>(value).map_err(parse_err)
    } else {
        let single: SubscriptionCredential = serde_json::from_value(value).map_err(parse_err)?;
        Ok(SubscriptionStore {
            accounts: vec![single],
        })
    }
}

/// Read all stored accounts of `kind`, transparently supporting both the plural
/// `{ "accounts": [...] }` layout and legacy single-object files.
pub fn read_subscription_accounts(
    kind: Subscription,
    file: &Path,
) -> Result<Vec<SubscriptionCredential>> {
    guard::check_token_path(&file.to_string_lossy())?;
    let raw = match std::fs::read_to_string(file) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(HrnError::Auth(format!(
                "not logged in to {}: run `hrn login {}`",
                kind.label(),
                kind.id()
            )));
        }
        Err(e) => {
            return Err(HrnError::Auth(format!("read `{}`: {e}", file.display())));
        }
    };
    Ok(parse_subscription_store(kind, file, &raw)?.accounts)
}

/// Read the first stored credential of `kind`; a missing or empty file asks the user to log in.
pub fn read_subscription(kind: Subscription, file: &Path) -> Result<SubscriptionCredential> {
    read_subscription_accounts(kind, file)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            HrnError::Auth(format!(
                "not logged in to {}: run `hrn login {}`",
                kind.label(),
                kind.id()
            ))
        })
}

/// Write `accounts` in plural `{ "accounts": [...] }` format atomically (`0600` on Unix).
pub(crate) fn write_subscription_accounts(
    file: &Path,
    accounts: Vec<SubscriptionCredential>,
) -> Result {
    guard::check_token_path(&file.to_string_lossy())?;
    write_json_atomic(file, &SubscriptionStore { accounts })
}

/// Add or replace `cred` in `file` keyed by [`SubscriptionCredential::identity_key`], migrating
/// legacy single-object files to `{ "accounts": [...] }` without dropping sibling accounts.
///
/// Fails rather than overwriting if an existing file cannot be parsed.
pub fn upsert_subscription_account(
    kind: Subscription,
    file: &Path,
    cred: SubscriptionCredential,
) -> Result<Vec<SubscriptionCredential>> {
    guard::check_token_path(&file.to_string_lossy())?;
    let mut accounts = if file.exists() {
        read_subscription_accounts(kind, file)?
    } else {
        Vec::new()
    };
    let target_key = cred.identity_key();
    let existing = accounts.iter_mut().find(|entry| {
        if entry.identity_key() == target_key {
            return true;
        }
        // Upgrade an older email-only record when the new login carries the same email plus an id.
        entry.account_id.is_none()
            && entry.chatgpt_account_id.is_none()
            && entry.email.is_some()
            && entry.email == cred.email
    });
    if let Some(slot) = existing {
        *slot = cred;
    } else {
        accounts.push(cred);
    }
    write_subscription_accounts(file, accounts.clone())?;
    Ok(accounts)
}

/// Non-secret summary of a stored subscription login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionStatus {
    /// No credential file.
    Missing,
    /// A stored login.
    LoggedIn {
        /// E-mail or account id.
        account: Option<String>,
        /// ChatGPT plan, when known.
        plan: Option<String>,
        /// Access-token expiry in Unix milliseconds.
        expires_ms: u64,
        /// Whether a refresh token allows renewal without a new login.
        has_refresh: bool,
    },
    /// The file exists but cannot be used.
    Invalid(String),
}

fn status_of(cred: &SubscriptionCredential) -> SubscriptionStatus {
    SubscriptionStatus::LoggedIn {
        account: cred.account_label().map(str::to_owned),
        plan: cred.plan.clone(),
        expires_ms: cred.expires,
        has_refresh: !cred.refresh.trim().is_empty(),
    }
}

/// Inspect the primary stored credential of `kind` without exposing tokens.
pub fn subscription_status(kind: Subscription, file: &Path) -> SubscriptionStatus {
    if let Err(e) = guard::check_token_path(&file.to_string_lossy()) {
        return SubscriptionStatus::Invalid(e.to_string());
    }
    if !file.exists() {
        return SubscriptionStatus::Missing;
    }
    match read_subscription(kind, file) {
        Ok(cred) => status_of(&cred),
        Err(e) => SubscriptionStatus::Invalid(e.to_string()),
    }
}

/// Inspect every stored credential of `kind` without exposing tokens.
#[must_use]
pub fn subscription_accounts_status(kind: Subscription, file: &Path) -> Vec<SubscriptionStatus> {
    if let Err(e) = guard::check_token_path(&file.to_string_lossy()) {
        return vec![SubscriptionStatus::Invalid(e.to_string())];
    }
    if !file.exists() {
        return Vec::new();
    }
    match read_subscription_accounts(kind, file) {
        Ok(accounts) => accounts.iter().map(status_of).collect(),
        Err(e) => vec![SubscriptionStatus::Invalid(e.to_string())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::tests::fake_jwt;
    use serde_json::json;

    #[test]
    fn claude_reply_maps_identity_with_five_minute_skew() {
        let body = br#"{"access_token":"sk-ant-oat01-a","refresh_token":"sk-ant-ort01-r",
            "expires_in":28800,"account":{"uuid":"acc-1","email_address":"a@b.c"},
            "organization":{"uuid":"org-1","name":"Org"}}"#;
        let cred = credential_from_reply(Subscription::Claude, body, None, 1_000_000).expect("map");
        assert_eq!(cred.provider, "claude");
        assert_eq!(cred.access, "sk-ant-oat01-a");
        assert_eq!(cred.refresh, "sk-ant-ort01-r");
        assert_eq!(cred.expires, 1_000_000 + 28_800_000 - 300_000);
        assert_eq!(cred.account_id.as_deref(), Some("acc-1"));
        assert_eq!(cred.email.as_deref(), Some("a@b.c"));
        assert_eq!(cred.org_id.as_deref(), Some("org-1"));
        assert_eq!(cred.org_name.as_deref(), Some("Org"));

        // A refresh reply without a new refresh token or organization keeps both.
        let body = br#"{"access_token":"sk-ant-oat01-b","expires_in":100,
            "organization":{"uuid":"org-2"}}"#;
        let next = credential_from_reply(Subscription::Claude, body, Some(&cred), 2_000_000)
            .expect("refresh");
        assert_eq!(next.access, "sk-ant-oat01-b");
        assert_eq!(next.refresh, "sk-ant-ort01-r");
        assert_eq!(next.org_id.as_deref(), Some("org-1"));
        assert_eq!(next.expires, 2_000_000 + 100_000 - 300_000);
    }

    #[test]
    fn chatgpt_reply_reads_account_id_and_plan_from_the_jwt() {
        let access = fake_jwt(&json!({
            "exp": 1_900_000_000u64,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-42",
                "chatgpt_plan_type": "plus",
                "chatgpt_data_residency": "eu"
            }
        }));
        let id_token = fake_jwt(&json!({"email": "u@example.com"}));
        let body = json!({"access_token": access, "refresh_token": "rt", "id_token": id_token})
            .to_string();
        let cred =
            credential_from_reply(Subscription::Chatgpt, body.as_bytes(), None, 5).expect("map");
        assert_eq!(cred.chatgpt_account_id.as_deref(), Some("acct-42"));
        assert_eq!(cred.plan.as_deref(), Some("plus"));
        assert_eq!(cred.residency.as_deref(), Some("eu"));
        assert_eq!(cred.email.as_deref(), Some("u@example.com"));
        assert_eq!(cred.expires, 1_900_000_000_000, "no expires_in: JWT exp");
        let with_ttl = json!({"access_token": access, "expires_in": 10}).to_string();
        let cred = credential_from_reply(Subscription::Chatgpt, with_ttl.as_bytes(), None, 5)
            .expect("map");
        assert_eq!(cred.expires, 10_005, "ChatGPT skew is zero");
    }

    #[test]
    fn debug_output_never_shows_tokens() {
        let cred = SubscriptionCredential {
            access: "secret-access".into(),
            refresh: "secret-refresh".into(),
            ..SubscriptionCredential::default()
        };
        let shown = format!("{cred:?}");
        assert!(!shown.contains("secret"), "{shown}");
        let store = SubscriptionStore {
            accounts: vec![cred],
        };
        let store_shown = format!("{store:?}");
        assert!(!store_shown.contains("secret"), "{store_shown}");
    }

    #[test]
    fn credential_file_migrates_single_to_accounts() {
        let dir = std::env::temp_dir().join(format!("hrn-sub-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("claude.json");

        // Seed a legacy single-object credential file.
        let legacy = SubscriptionCredential {
            provider: "claude".into(),
            access: "tok-a-1".into(),
            refresh: "ref-a-1".into(),
            expires: 9_000_000_000_000,
            email: Some("a@example.com".into()),
            account_id: Some("acct-a".into()),
            ..SubscriptionCredential::default()
        };
        write_json_atomic(&file, &legacy).expect("write legacy");

        // Transparent read returns the single legacy account.
        let loaded = read_subscription_accounts(Subscription::Claude, &file).expect("read legacy");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].identity_key(), "acct-a");

        // Upserting a second account migrates the file to `{ "accounts": [...] }` keeping both.
        let second = SubscriptionCredential {
            provider: "claude".into(),
            access: "tok-b-1".into(),
            refresh: "ref-b-1".into(),
            expires: 9_000_000_000_000,
            email: Some("b@example.com".into()),
            account_id: Some("acct-b".into()),
            ..SubscriptionCredential::default()
        };
        let after_add =
            upsert_subscription_account(Subscription::Claude, &file, second).expect("add b");
        assert_eq!(after_add.len(), 2);
        let raw = std::fs::read_to_string(&file).expect("read migrated");
        let v: Value = serde_json::from_str(&raw).expect("json");
        assert!(v.get("accounts").and_then(Value::as_array).is_some());

        // Re-logging in account A replaces it in place without dropping account B.
        let updated_a = SubscriptionCredential {
            provider: "claude".into(),
            access: "tok-a-2".into(),
            refresh: "ref-a-2".into(),
            expires: 9_500_000_000_000,
            email: Some("a@example.com".into()),
            account_id: Some("acct-a".into()),
            ..SubscriptionCredential::default()
        };
        let after_update =
            upsert_subscription_account(Subscription::Claude, &file, updated_a).expect("update a");
        assert_eq!(after_update.len(), 2);
        assert_eq!(after_update[0].access, "tok-a-2");
        assert_eq!(after_update[1].identity_key(), "acct-b");
        assert_eq!(after_update[1].access, "tok-b-1");

        // Corrupt JSON must error rather than silently dropping existing accounts.
        std::fs::write(&file, "{corrupt").expect("corrupt");
        let err = upsert_subscription_account(
            Subscription::Claude,
            &file,
            SubscriptionCredential::default(),
        );
        assert!(err.is_err());

        // Consumer CLI store paths remain blocked before touching the filesystem.
        let blocked = Path::new("/nonexistent-hrn-test/.claude/credentials.json");
        assert!(read_subscription_accounts(Subscription::Claude, blocked).is_err());
        assert!(matches!(
            subscription_status(Subscription::Claude, blocked),
            SubscriptionStatus::Invalid(_)
        ));
        assert!(matches!(
            subscription_accounts_status(Subscription::Claude, blocked).as_slice(),
            [SubscriptionStatus::Invalid(_)]
        ));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
