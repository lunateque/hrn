//! Proactive and 401-triggered refresh of subscription credentials: an in-process single flight
//! plus the cross-process lock file. The file is re-read under both locks, so a token another
//! task or process has just written is reused instead of being refreshed again.

use crate::lock::FileLock;
use crate::manager::{OAuthManager, TokenBody, now_ms, post_token};
use crate::sub_store::{
    SubscriptionCredential, credential_from_reply, read_subscription, read_subscription_accounts,
    write_subscription_accounts,
};
use crate::subscription::Subscription;
use hrn_edge_http::{HttpClient, guard};
use hrn_kernel::{HrnError, Result};
use serde_json::json;
use std::path::Path;

/// `User-Agent` of Claude token refreshes (Anthropic SDK 0.128.0, as Claude Code sends it).
const CLAUDE_REFRESH_UA: &str = "anthropic-sdk-typescript/0.128.0 userOAuthProvider";
/// Beta header Claude token refreshes carry.
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";

/// Why a refresh happens (ChatGPT records it as `refresh_reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshReason {
    /// The access token is about to expire.
    ProactiveExpiry,
    /// The server rejected the access token with 401.
    UnauthorizedRecovery,
}

impl RefreshReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProactiveExpiry => "proactive_expiry",
            Self::UnauthorizedRecovery => "unauthorized_recovery",
        }
    }
}

impl OAuthManager {
    /// A usable credential for `kind`, refreshed first when it is about to expire.
    pub async fn subscription(
        &self,
        http: &HttpClient,
        kind: Subscription,
        file: &Path,
    ) -> Result<SubscriptionCredential> {
        let cred = read_subscription(kind, file)?;
        if cred.is_fresh(now_ms()) {
            return Ok(cred);
        }
        let key = cred.identity_key();
        self.refresh_subscription_account(
            http,
            kind,
            file,
            Some(&key),
            kind.spec().token_url,
            &cred.access,
            RefreshReason::ProactiveExpiry,
        )
        .await
    }

    /// A usable credential for a specific account identity inside `file`, refreshed if expiring.
    pub async fn subscription_account(
        &self,
        http: &HttpClient,
        kind: Subscription,
        file: &Path,
        account_key: &str,
    ) -> Result<SubscriptionCredential> {
        let accounts = read_subscription_accounts(kind, file)?;
        let cred = accounts
            .into_iter()
            .find(|c| c.identity_key() == account_key)
            .ok_or_else(|| {
                HrnError::Auth(format!(
                    "{} account `{account_key}` not found in `{}`",
                    kind.label(),
                    file.display()
                ))
            })?;
        if cred.is_fresh(now_ms()) {
            return Ok(cred);
        }
        self.refresh_subscription_account(
            http,
            kind,
            file,
            Some(account_key),
            kind.spec().token_url,
            &cred.access,
            RefreshReason::ProactiveExpiry,
        )
        .await
    }

    /// The credential to retry with once the server rejected `rejected_access` with 401.
    pub async fn subscription_after_401(
        &self,
        http: &HttpClient,
        kind: Subscription,
        file: &Path,
        rejected_access: &str,
    ) -> Result<SubscriptionCredential> {
        self.refresh_subscription_account(
            http,
            kind,
            file,
            None,
            kind.spec().token_url,
            rejected_access,
            RefreshReason::UnauthorizedRecovery,
        )
        .await
    }

    /// Refresh one specific account after a 401, preserving sibling accounts in `file`.
    pub async fn subscription_account_after_401(
        &self,
        http: &HttpClient,
        kind: Subscription,
        file: &Path,
        account_key: &str,
        rejected_access: &str,
    ) -> Result<SubscriptionCredential> {
        self.refresh_subscription_account(
            http,
            kind,
            file,
            Some(account_key),
            kind.spec().token_url,
            rejected_access,
            RefreshReason::UnauthorizedRecovery,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn refresh_subscription(
        &self,
        http: &HttpClient,
        kind: Subscription,
        file: &Path,
        token_url: &str,
        seen_access: &str,
        reason: RefreshReason,
    ) -> Result<SubscriptionCredential> {
        self.refresh_subscription_account(http, kind, file, None, token_url, seen_access, reason)
            .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal single-flight refresh threads http, kind, file, optional account key, url, seen token and reason"
    )]
    pub(crate) async fn refresh_subscription_account(
        &self,
        http: &HttpClient,
        kind: Subscription,
        file: &Path,
        account_key: Option<&str>,
        token_url: &str,
        seen_access: &str,
        reason: RefreshReason,
    ) -> Result<SubscriptionCredential> {
        guard::check_token_path(&file.to_string_lossy())?;
        let flight = self.file_lock(&file.to_string_lossy());
        let _flight = flight.lock().await;
        let _cross_process = FileLock::acquire(file).await?;
        let mut accounts = read_subscription_accounts(kind, file)?;
        let idx = if let Some(key) = account_key {
            accounts
                .iter()
                .position(|c| c.identity_key() == key)
                .ok_or_else(|| {
                    HrnError::Auth(format!(
                        "{} account `{key}` not found in `{}`",
                        kind.label(),
                        file.display()
                    ))
                })?
        } else {
            if accounts.is_empty() {
                return Err(HrnError::Auth(format!(
                    "not logged in to {}: run `hrn login {}`",
                    kind.label(),
                    kind.id()
                )));
            }
            0
        };
        let Some(current) = accounts.get(idx).cloned() else {
            return Err(HrnError::Auth(format!(
                "not logged in to {}: run `hrn login {}`",
                kind.label(),
                kind.id()
            )));
        };
        if current.access != seen_access && current.is_fresh(now_ms()) {
            return Ok(current);
        }
        let refreshed = refresh_remote(http, kind, file, token_url, &current, reason).await?;
        if let Some(slot) = accounts.get_mut(idx) {
            *slot = refreshed.clone();
        }
        write_subscription_accounts(file, accounts)?;
        Ok(refreshed)
    }
}

async fn refresh_remote(
    http: &HttpClient,
    kind: Subscription,
    file: &Path,
    token_url: &str,
    cred: &SubscriptionCredential,
    reason: RefreshReason,
) -> Result<SubscriptionCredential> {
    let relogin = |msg: String| format!("{msg}; run `hrn login {}` again", kind.id());
    if cred.refresh.trim().is_empty() {
        return Err(HrnError::Auth(relogin(format!(
            "{} login has no refresh token",
            kind.label()
        ))));
    }
    let spec = kind.spec();
    let what = format!("{} token refresh", kind.label());
    let sent = match kind {
        // `organization_id` is not sent: omp adds it only for a stored `activeOrganizationId`,
        // which an hrn login never records.
        Subscription::Claude => {
            let body = TokenBody::Json(json!({
                "grant_type": "refresh_token",
                "client_id": spec.client_id,
                "refresh_token": cred.refresh,
            }));
            let headers = [
                ("anthropic-beta", CLAUDE_OAUTH_BETA),
                ("user-agent", CLAUDE_REFRESH_UA),
            ];
            post_token(http, token_url, body, &headers, spec.token_timeout, &what).await
        }
        Subscription::Chatgpt => {
            let body = TokenBody::Form(vec![
                ("grant_type", "refresh_token".to_owned()),
                ("client_id", spec.client_id.to_owned()),
                ("refresh_token", cred.refresh.clone()),
                ("refresh_reason", reason.as_str().to_owned()),
            ]);
            post_token(http, token_url, body, &[], spec.token_timeout, &what).await
        }
        Subscription::KimiCode => {
            let auth_dir = file.parent().unwrap_or_else(|| Path::new("."));
            let device_id = crate::kimi::load_or_create_kimi_device_id(auth_dir).await?;
            let kimi_headers = crate::kimi::kimi_fingerprint_headers(&device_id);
            let refs: Vec<(&str, &str)> = kimi_headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let body = TokenBody::Form(vec![
                ("grant_type", "refresh_token".to_owned()),
                ("client_id", spec.client_id.to_owned()),
                ("refresh_token", cred.refresh.clone()),
            ]);
            post_token(http, token_url, body, &refs, spec.token_timeout, &what).await
        }
    };
    let reply = sent.map_err(|err| match err {
        HrnError::Auth(msg) => HrnError::Auth(relogin(msg)),
        other => other,
    })?;
    credential_from_reply(kind, &reply, Some(cred), now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::write_json_atomic;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// Serve one request on loopback with `reply_body`; the handle yields the raw request.
    async fn serve_once(reply_body: &'static str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.expect("read");
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().expect("len"))
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
                assert!(n > 0, "connection closed early");
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply_body}",
                reply_body.len()
            );
            sock.write_all(reply.as_bytes()).await.expect("write");
            sock.shutdown().await.ok();
            String::from_utf8_lossy(&buf).into_owned()
        });
        (format!("http://127.0.0.1:{port}/v1/oauth/token"), handle)
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hrn-sub-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir.join("login.json")
    }

    fn stored(provider: &str, access: &str, expires: u64) -> SubscriptionCredential {
        SubscriptionCredential {
            provider: provider.into(),
            access: access.into(),
            refresh: "rt-old".into(),
            expires,
            org_id: Some("org-1".into()),
            ..SubscriptionCredential::default()
        }
    }

    #[tokio::test]
    async fn claude_refresh_posts_json_with_the_oauth_beta_and_sdk_agent() {
        let file = temp_file("claude");
        write_json_atomic(&file, &stored("claude", "expired", 1)).expect("seed");
        let (url, server) =
            serve_once(r#"{"access_token":"fresh","refresh_token":"rt-new","expires_in":3600}"#)
                .await;
        let http = HttpClient::new().expect("http");
        let cred = OAuthManager::default()
            .refresh_subscription(
                &http,
                Subscription::Claude,
                &file,
                &url,
                "expired",
                RefreshReason::ProactiveExpiry,
            )
            .await
            .expect("refresh");
        assert_eq!(
            (cred.access.as_str(), cred.refresh.as_str()),
            ("fresh", "rt-new")
        );
        assert_eq!(cred.org_id.as_deref(), Some("org-1"));
        let request = server.await.expect("join").to_ascii_lowercase();
        assert!(request.starts_with("post /v1/oauth/token "), "{request}");
        assert!(
            request.contains("content-type: application/json"),
            "{request}"
        );
        assert!(
            request.contains("anthropic-beta: oauth-2025-04-20"),
            "{request}"
        );
        assert!(
            request.contains("user-agent: anthropic-sdk-typescript/0.128.0 useroauthprovider"),
            "{request}"
        );
        assert!(
            request.contains(r#""grant_type":"refresh_token""#),
            "{request}"
        );
        assert!(request.contains(r#""refresh_token":"rt-old""#), "{request}");
        assert!(
            request.contains(r#""client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e""#),
            "{request}"
        );
        let saved = read_subscription(Subscription::Claude, &file).expect("saved");
        assert_eq!(saved.access, "fresh");
        std::fs::remove_dir_all(file.parent().expect("dir")).expect("cleanup");
    }

    #[tokio::test]
    async fn chatgpt_refresh_posts_a_form_with_the_refresh_reason() {
        let file = temp_file("chatgpt");
        write_json_atomic(&file, &stored("chatgpt", "rejected", u64::MAX)).expect("seed");
        let (url, server) = serve_once(r#"{"access_token":"fresh","expires_in":60}"#).await;
        let http = HttpClient::new().expect("http");
        let cred = OAuthManager::default()
            .refresh_subscription(
                &http,
                Subscription::Chatgpt,
                &file,
                &url,
                "rejected",
                RefreshReason::UnauthorizedRecovery,
            )
            .await
            .expect("refresh");
        assert_eq!(
            (cred.access.as_str(), cred.refresh.as_str()),
            ("fresh", "rt-old")
        );
        let request = server.await.expect("join");
        assert!(
            request.contains("content-type: application/x-www-form-urlencoded"),
            "{request}"
        );
        assert!(
            request.ends_with(
                "grant_type=refresh_token&client_id=app_EMoamEEZ73f0CkXaXp7hrann\
                 &refresh_token=rt-old&refresh_reason=unauthorized_recovery"
            ),
            "{request}"
        );
        std::fs::remove_dir_all(file.parent().expect("dir")).expect("cleanup");
    }

    #[tokio::test]
    async fn a_token_another_process_already_refreshed_is_reused() {
        let file = temp_file("reuse");
        write_json_atomic(&file, &stored("claude", "written-by-peer", u64::MAX)).expect("seed");
        let http = HttpClient::new().expect("http");
        // An unreachable token URL proves no request is made.
        let cred = OAuthManager::default()
            .refresh_subscription(
                &http,
                Subscription::Claude,
                &file,
                "http://127.0.0.1:9/x",
                "rejected",
                RefreshReason::UnauthorizedRecovery,
            )
            .await
            .expect("reuse");
        assert_eq!(cred.access, "written-by-peer");
        std::fs::remove_dir_all(file.parent().expect("dir")).expect("cleanup");
    }

    #[tokio::test]
    async fn refreshing_one_account_preserves_sibling_accounts() {
        let file = temp_file("multi-sibling");
        let mut a = stored("claude", "expired-a", 1);
        a.account_id = Some("acct-a".into());
        let mut b = stored("claude", "fresh-b", u64::MAX);
        b.account_id = Some("acct-b".into());
        crate::sub_store::upsert_subscription_account(Subscription::Claude, &file, a)
            .expect("seed a");
        crate::sub_store::upsert_subscription_account(Subscription::Claude, &file, b)
            .expect("seed b");

        let (url, _server) =
            serve_once(r#"{"access_token":"fresh-a","refresh_token":"rt-a2","expires_in":3600}"#)
                .await;
        let http = HttpClient::new().expect("http");
        let updated = OAuthManager::default()
            .refresh_subscription_account(
                &http,
                Subscription::Claude,
                &file,
                Some("acct-a"),
                &url,
                "expired-a",
                RefreshReason::ProactiveExpiry,
            )
            .await
            .expect("refresh a");
        assert_eq!(updated.access, "fresh-a");
        let all = read_subscription_accounts(Subscription::Claude, &file).expect("read all");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].access, "fresh-a");
        assert_eq!(all[1].identity_key(), "acct-b");
        assert_eq!(all[1].access, "fresh-b");
        std::fs::remove_dir_all(file.parent().expect("dir")).expect("cleanup");
    }

    #[tokio::test]
    async fn blocked_token_path_fails_before_creating_lock_file() {
        let root = std::env::temp_dir().join(format!("hrn-sub-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let blocked_dir = root.join(".claude");
        let file = blocked_dir.join("credentials.json");
        let http = HttpClient::new().expect("http");
        let err = OAuthManager::default()
            .refresh_subscription_account(
                &http,
                Subscription::Claude,
                &file,
                None,
                "http://127.0.0.1:9/v1/oauth/token",
                "tok",
                RefreshReason::ProactiveExpiry,
            )
            .await
            .expect_err("blocked consumer CLI path");
        assert!(matches!(err, HrnError::Config(_)), "{err}");
        assert!(
            !blocked_dir.exists(),
            "guard must reject before creating parent dirs or .lock files"
        );
    }
}
