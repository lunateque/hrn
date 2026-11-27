//! Per-file single-flight RFC 6749 `refresh_token` manager with atomic `0600` persistence, plus
//! the token-endpoint plumbing shared with the PKCE and subscription logins.

use bytes::Bytes;
use hrn_edge_http::{HttpClient, guard};
use hrn_kernel::{HrnError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Largest token-endpoint error body echoed into an error message.
const ERROR_BODY_CHARS: usize = 500;

/// Configuration for one `OAuth2` token file under `[providers.<name>.oauth]`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    /// Token refresh endpoint URL (must be `https://`, or loopback `http://` for local tests).
    pub token_url: String,
    /// `OAuth2` client identifier.
    pub client_id: String,
    /// Path to assistant-managed token JSON (never a consumer CLI store).
    pub token_file: String,
    /// Optional scope string.
    #[serde(default)]
    pub scope: Option<String>,
    /// Refresh this many seconds before `expires_at` (default 300).
    #[serde(default = "default_skew")]
    pub refresh_skew_secs: u64,
    /// Send refresh request as JSON instead of `application/x-www-form-urlencoded`.
    #[serde(default)]
    pub json_body: bool,
}

const fn default_skew() -> u64 {
    300
}

/// On-disk `OAuth2` token JSON structure. `Debug` never prints tokens.
#[derive(Clone, Deserialize, Serialize)]
pub struct StoredToken {
    /// Active bearer access token.
    #[serde(alias = "access")]
    pub access_token: String,
    /// Optional refresh token.
    #[serde(default, alias = "refresh", skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Expiry timestamp in Unix seconds (milliseconds normalized on load).
    #[serde(default, alias = "expires")]
    pub expires_at: u64,
    /// Optional token type (`Bearer`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    /// Any other provider fields (`id_token`, `account_id`, …) preserved across refreshes.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl std::fmt::Debug for StoredToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let extra_keys: Vec<&str> = self.extra.keys().map(String::as_str).collect();
        f.debug_struct("StoredToken")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at", &self.expires_at)
            .field("token_type", &self.token_type)
            .field("extra_keys", &extra_keys)
            .finish()
    }
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Non-secret health snapshot of an on-disk `OAuth2` token file (for `hrn auth status` / `check`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenFileStatus {
    /// File does not exist yet (user needs to run `hrn login <provider>`).
    Missing,
    /// File exists and access token is usable with `remaining_secs` left (or `None` if no expiry).
    Valid {
        /// Seconds remaining until expiry (`None` when `expires_at == 0`).
        remaining_secs: Option<u64>,
        /// Whether a `refresh_token` is present for automatic renewal.
        has_refresh: bool,
    },
    /// Access token is expired or within `refresh_skew_secs`.
    Expired {
        /// Whether automatic refresh can renew it without re-logging in.
        has_refresh: bool,
    },
    /// File exists but could not be read or parsed.
    Invalid(String),
}

/// Inspect an `OAuth2` token file without ever returning token values.
pub fn inspect_token_file(path: &Path, refresh_skew_secs: u64) -> TokenFileStatus {
    if let Err(e) = guard::check_token_path(&path.to_string_lossy()) {
        return TokenFileStatus::Invalid(e.to_string());
    }
    if !path.exists() {
        return TokenFileStatus::Missing;
    }
    let tok = match read_token(path) {
        Ok(t) => t,
        Err(e) => return TokenFileStatus::Invalid(e.to_string()),
    };
    let has_refresh = tok
        .refresh_token
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    if tok.access_token.trim().is_empty() {
        return TokenFileStatus::Invalid("empty access_token".into());
    }
    let now = now_secs();
    if is_expiring(&tok, now, refresh_skew_secs) {
        return TokenFileStatus::Expired { has_refresh };
    }
    let remaining_secs = (tok.expires_at > 0).then(|| tok.expires_at.saturating_sub(now));
    TokenFileStatus::Valid {
        remaining_secs,
        has_refresh,
    }
}

/// Shared single-flight `OAuth2` token manager.
#[derive(Clone, Default)]
pub struct OAuthManager {
    locks: Arc<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl OAuthManager {
    /// Return a fresh `access_token`, refreshing on disk if within `refresh_skew_secs`.
    ///
    /// Concurrent callers in this process share one refresh (single flight). When another
    /// process refreshed the same file first and rotated the refresh token, the failed refresh
    /// falls back to the token that process wrote.
    pub async fn access_token(&self, http: &HttpClient, cfg: &OAuthConfig) -> Result<String> {
        guard::check_oauth_client_id(&cfg.client_id)?;
        guard::check_token_path(&cfg.token_file)?;
        guard::check_outbound_url(&cfg.token_url)?;
        require_secure_url(&cfg.token_url, "token_url")?;

        let path = Path::new(&cfg.token_file);
        if let Ok(tok) = read_token(path)
            && is_usable(&tok, cfg.refresh_skew_secs)
        {
            return Ok(tok.access_token);
        }

        let lock = self.file_lock(&cfg.token_file);
        let _guard = lock.lock().await;

        let tok = read_token(path)?;
        if is_usable(&tok, cfg.refresh_skew_secs) {
            return Ok(tok.access_token);
        }
        let refresh = tok
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                HrnError::Auth(format!(
                    "OAuth token `{}` is expired and has no refresh_token; run `hrn login`",
                    cfg.token_file
                ))
            })?;

        match refresh_remote(http, cfg, &tok, refresh).await {
            Ok(updated) => {
                write_json_atomic(path, &updated)?;
                Ok(updated.access_token)
            }
            Err(err @ HrnError::Auth(_)) => match read_token(path) {
                Ok(other)
                    if other.access_token != tok.access_token
                        && is_usable(&other, cfg.refresh_skew_secs) =>
                {
                    Ok(other.access_token)
                }
                _ => Err(err),
            },
            Err(err) => Err(err),
        }
    }

    pub(crate) fn file_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut guard = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            guard
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }
}

/// Require an `https://` URL; plain `http://` is accepted only for loopback hosts so tokens and
/// authorization codes never cross the network unencrypted.
pub(crate) fn require_secure_url(url: &str, what: &str) -> Result {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    if lower.starts_with("http://")
        && matches!(
            guard::extract_host(url),
            Some("localhost" | "127.0.0.1" | "[::1]")
        )
    {
        return Ok(());
    }
    Err(HrnError::Config(format!(
        "OAuth {what} must use https:// (got `{url}`)"
    )))
}

/// Map a non-2xx token-endpoint reply to an error: transient statuses are retryable provider
/// errors, everything else (`invalid_grant`, bad client, …) needs the user to log in again.
pub(crate) fn token_endpoint_error(
    what: &str,
    status: u16,
    body: &str,
    retry_after_secs: Option<u64>,
) -> HrnError {
    let excerpt: String = body.chars().take(ERROR_BODY_CHARS).collect();
    let message = format!("{what} failed HTTP {status}: {excerpt}");
    if matches!(status, 408 | 429 | 500..=599) {
        HrnError::Provider {
            message,
            retryable: true,
            retry_after_secs,
        }
    } else {
        HrnError::Auth(message)
    }
}

/// Body of one token-endpoint request.
pub(crate) enum TokenBody {
    /// `application/json`.
    Json(Value),
    /// `application/x-www-form-urlencoded`, pairs kept in order.
    Form(Vec<(&'static str, String)>),
    /// `application/x-www-form-urlencoded` with owned keys and values.
    FormOwned(Vec<(String, String)>),
}

/// POST one token-endpoint request and return the reply body; a non-2xx reply becomes
/// [`token_endpoint_error`]. `extra_headers` only add identification headers.
pub(crate) async fn post_token(
    http: &HttpClient,
    url: &str,
    body: TokenBody,
    extra_headers: &[(&str, &str)],
    timeout: Duration,
    what: &str,
) -> Result<Bytes> {
    let (content_type, bytes) = match body {
        TokenBody::Json(value) => ("application/json", Bytes::from(serde_json::to_vec(&value)?)),
        TokenBody::Form(pairs) => {
            let mut s = form_urlencoded::Serializer::new(String::new());
            for (key, value) in &pairs {
                s.append_pair(key, value);
            }
            (
                "application/x-www-form-urlencoded",
                Bytes::from(s.finish().into_bytes()),
            )
        }
        TokenBody::FormOwned(pairs) => {
            let mut s = form_urlencoded::Serializer::new(String::new());
            for (key, value) in &pairs {
                s.append_pair(key, value);
            }
            (
                "application/x-www-form-urlencoded",
                Bytes::from(s.finish().into_bytes()),
            )
        }
    };
    let mut headers = vec![
        ("content-type".to_owned(), content_type.to_owned()),
        ("accept".to_owned(), "application/json".to_owned()),
    ];
    headers.extend(
        extra_headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    let resp = http.post_raw(url, &headers, bytes, timeout).await?;
    if !resp.status.is_success() {
        return Err(token_endpoint_error(
            what,
            resp.status.as_u16(),
            &resp.text(),
            resp.retry_after_secs(),
        ));
    }
    Ok(resp.body)
}

async fn refresh_remote(
    http: &HttpClient,
    cfg: &OAuthConfig,
    previous: &StoredToken,
    refresh_token: &str,
) -> Result<StoredToken> {
    let body = if cfg.json_body {
        let mut map = Map::new();
        map.insert("grant_type".into(), json!("refresh_token"));
        map.insert("refresh_token".into(), json!(refresh_token));
        map.insert("client_id".into(), json!(cfg.client_id));
        if let Some(scope) = &cfg.scope {
            map.insert("scope".into(), json!(scope));
        }
        TokenBody::Json(Value::Object(map))
    } else {
        let mut pairs = vec![
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", refresh_token.to_owned()),
            ("client_id", cfg.client_id.clone()),
        ];
        if let Some(scope) = &cfg.scope {
            pairs.push(("scope", scope.clone()));
        }
        TokenBody::Form(pairs)
    };
    let reply = post_token(
        http,
        &cfg.token_url,
        body,
        &[],
        Duration::from_secs(30),
        "OAuth refresh",
    )
    .await?;
    parse_token_response(&reply, Some(previous))
}

/// Parse a token endpoint reply. `previous` supplies the refresh token (providers that do not
/// rotate omit it) and the extra fields to keep.
pub(crate) fn parse_token_response(
    body: &[u8],
    previous: Option<&StoredToken>,
) -> Result<StoredToken> {
    let parsed: RefreshResponse = serde_json::from_slice(body)
        .map_err(|e| HrnError::Auth(format!("parse OAuth token response: {e}")))?;
    if parsed.access_token.trim().is_empty() {
        return Err(HrnError::Auth("OAuth response missing access_token".into()));
    }
    let now = now_secs();
    let expires_at = parsed
        .expires_at
        .map(normalize_epoch_secs)
        .or_else(|| parsed.expires_in.map(|d| now.saturating_add(d)))
        .unwrap_or_else(|| now.saturating_add(3600));
    let mut extra = previous.map(|p| p.extra.clone()).unwrap_or_default();
    extra.extend(parsed.extra);
    Ok(StoredToken {
        access_token: parsed.access_token,
        refresh_token: parsed
            .refresh_token
            .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
        expires_at,
        token_type: parsed.token_type.or_else(|| Some("Bearer".to_owned())),
        extra,
    })
}

fn read_token(path: &Path) -> Result<StoredToken> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| HrnError::Auth(format!("read OAuth token `{}`: {e}", path.display())))?;
    let mut tok: StoredToken = serde_json::from_str(&raw)
        .map_err(|e| HrnError::Auth(format!("parse OAuth token `{}`: {e}", path.display())))?;
    tok.expires_at = normalize_epoch_secs(tok.expires_at);
    Ok(tok)
}

/// Write `value` as pretty JSON next to `path` (mode `0600` on Unix) and atomically replace `path`.
pub(crate) fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_bytes_atomic(path, &bytes)
}

/// Write `bytes` to a `0600` temp file next to `path` (created with `O_CREAT | O_EXCL`), sync to
/// disk, and atomically rename into place. Never removes the previous file before replacement.
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result {
    guard::check_token_path(&path.to_string_lossy())?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| HrnError::Io(format!("create `{}`: {e}", parent.display())))?;
    }
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".tmp.{}.{seq}", std::process::id()));
    let tmp = std::path::PathBuf::from(tmp_name);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_result = options.open(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(err) = write_result {
        let cleanup = std::fs::remove_file(&tmp);
        return Err(HrnError::Io(format!(
            "write secret temp file `{}`: {err}{}",
            tmp.display(),
            cleanup
                .err()
                .map_or_else(String::new, |c| format!(" (cleanup failed: {c})"))
        )));
    }

    // `rename` replaces an existing destination on every platform; on Windows a scanner or
    // reader holding the old file open makes it fail transiently, so retry briefly.
    let mut last_err = None;
    for attempt in 0..5u64 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = Some(err),
        }
        std::thread::sleep(Duration::from_millis(20 * (attempt + 1)));
    }
    let err = last_err.map(|e| e.to_string()).unwrap_or_default();
    let cleanup = std::fs::remove_file(&tmp);
    Err(HrnError::Io(format!(
        "persist secret {}: {err}{}",
        path.display(),
        cleanup
            .err()
            .map_or_else(String::new, |c| format!(" (cleanup failed: {c})"))
    )))
}

fn is_usable(tok: &StoredToken, skew: u64) -> bool {
    !tok.access_token.trim().is_empty() && !is_expiring(tok, now_secs(), skew)
}

fn is_expiring(tok: &StoredToken, now: u64, skew: u64) -> bool {
    tok.expires_at > 0 && now.saturating_add(skew) >= tok.expires_at
}

fn normalize_epoch_secs(ts: u64) -> u64 {
    if ts > 10_000_000_000 { ts / 1000 } else { ts }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Current Unix time in milliseconds.
pub(crate) fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
