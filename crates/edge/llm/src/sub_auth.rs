//! A subscription login (Claude Pro/Max, `ChatGPT` Plus/Pro) in use for one request: the
//! credential, plus what a single 401 recovery needs.

use hrn_edge_http::HttpClient;
use hrn_edge_oauth::{OAuthManager, Subscription, SubscriptionCredential, sha256, to_hex};
use hrn_kernel::Result;
use std::path::Path;
use std::sync::LazyLock;

/// Credential plus refresh context for one request against a subscription backend.
pub(crate) struct SubscriptionAuth<'a> {
    /// Shared HTTP client.
    pub(crate) http: &'a HttpClient,
    oauth: &'a OAuthManager,
    kind: Subscription,
    /// The `hrn login` credential file.
    pub(crate) file: &'a Path,
    /// The credential the next attempt sends.
    pub(crate) cred: SubscriptionCredential,
    account_key: String,
    recovered: bool,
}

impl<'a> SubscriptionAuth<'a> {
    /// Wrap a credential the router has just loaded (and proactively refreshed).
    pub(crate) fn new(
        http: &'a HttpClient,
        oauth: &'a OAuthManager,
        kind: Subscription,
        file: &'a Path,
        cred: SubscriptionCredential,
    ) -> Self {
        let account_key = cred.identity_key();
        Self {
            http,
            oauth,
            kind,
            file,
            cred,
            account_key,
            recovered: false,
        }
    }

    /// After a 401: adopt a token another process refreshed meanwhile, or refresh it. At most
    /// once per request; `false` means the single recovery was already spent.
    pub(crate) async fn recover_unauthorized(&mut self) -> Result<bool> {
        if self.recovered {
            return Ok(false);
        }
        self.recovered = true;
        self.cred = self
            .oauth
            .subscription_account_after_401(
                self.http,
                self.kind,
                self.file,
                &self.account_key,
                &self.cred.access,
            )
            .await?;
        Ok(true)
    }
}

/// Seed of [`session_uuid`] for requests without a session id: one per process.
static PROCESS_SEED: LazyLock<String> = LazyLock::new(|| {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("hrn-process:{}:{nanos}", std::process::id())
});

/// A UUID-shaped id (version 4 layout) derived from `seed`: stable for one conversation, so the
/// backend sees one session per conversation, not one per request.
pub(crate) fn session_uuid(seed: Option<&str>) -> String {
    let seed = seed
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(PROCESS_SEED.as_str());
    let mut bytes = sha256(seed.as_bytes());
    if let Some(b) = bytes.get_mut(6) {
        *b = (*b & 0x0f) | 0x40;
    }
    if let Some(b) = bytes.get_mut(8) {
        *b = (*b & 0x3f) | 0x80;
    }
    let hex = to_hex(bytes.get(..16).unwrap_or_default());
    let part = |from: usize, to: usize| hex.get(from..to).unwrap_or_default();
    format!(
        "{}-{}-{}-{}-{}",
        part(0, 8),
        part(8, 12),
        part(12, 16),
        part(16, 20),
        part(20, 32)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_uuid_is_stable_and_version_4_shaped() {
        let a = session_uuid(Some("ses_1"));
        assert_eq!(a, session_uuid(Some("ses_1")));
        assert_ne!(a, session_uuid(Some("ses_2")));
        assert_eq!(session_uuid(None), session_uuid(Some(" ")));
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "{a}");
        assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"), "{a}");
    }
}
