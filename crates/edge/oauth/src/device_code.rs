//! RFC 8628 OAuth 2.0 Device Authorization Grant client (`start_device_flow` +
//! `poll_device_token`), reusable by Kimi Code and future device-code providers.

use crate::manager::{
    StoredToken, TokenBody, parse_token_response, post_token, require_secure_url,
    token_endpoint_error,
};
use bytes::Bytes;
use hrn_edge_http::{HttpClient, guard};
use hrn_kernel::{CancelFlag, HrnError, Result};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::fmt;
use std::time::{Duration, Instant};

/// RFC 8628 §3.4 device code grant type.
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Default polling interval in seconds when the server omits `interval` (RFC 8628 §3.2).
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// Increment added to the polling interval upon `slow_down` (RFC 8628 §3.5).
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
/// Default request timeout for device-flow HTTP exchanges.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Cancellation-check granularity while sleeping between token polls.
const CANCEL_TICK: Duration = Duration::from_millis(25);

/// Configuration for an RFC 8628 device authorization flow.
#[derive(Clone)]
pub struct DeviceCodeConfig {
    /// Device authorization endpoint URL (`https://`, or loopback `http://` in tests).
    pub device_auth_url: String,
    /// Token polling endpoint URL (`https://`, or loopback `http://` in tests).
    pub token_url: String,
    /// `OAuth2` client identifier.
    pub client_id: String,
    /// Optional space-separated scope string.
    pub scope: Option<String>,
    /// Extra form/JSON parameters appended to the device authorization request.
    pub extra_params: Vec<(String, String)>,
    /// Extra HTTP headers attached to both device-auth and token polling requests.
    pub extra_headers: Vec<(String, String)>,
    /// Send JSON bodies instead of `application/x-www-form-urlencoded` (default `false`).
    pub json_body: bool,
    /// Duration of one logical RFC 8628 second during polling (`None` = 1 real second).
    /// Setting a smaller unit (e.g. 10 ms) lets unit tests exercise `slow_down` and expiry fast.
    pub poll_time_unit: Option<Duration>,
    /// Optional hard wall-clock cap on polling (overrides `expires_in` when shorter).
    pub max_wait: Option<Duration>,
}

impl fmt::Debug for DeviceCodeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.extra_headers.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("DeviceCodeConfig")
            .field("device_auth_url", &self.device_auth_url)
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .field("extra_headers", &header_names)
            .field("json_body", &self.json_body)
            .finish_non_exhaustive()
    }
}

/// Response from the RFC 8628 device authorization endpoint. `Debug` redacts `device_code`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceAuthResponse {
    /// Secret device verification code exchanged at the token endpoint.
    pub device_code: String,
    /// Short end-user code typed at `verification_uri`.
    pub user_code: String,
    /// Verification URL the user opens in a browser.
    pub verification_uri: String,
    /// Optional verification URL with `user_code` already embedded.
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Lifetime of `device_code` and `user_code` in seconds.
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
    /// Minimum polling interval in seconds (default 5 per RFC 8628 §3.2).
    #[serde(default = "default_interval")]
    pub interval: u64,
}

const fn default_expires_in() -> u64 {
    600
}

const fn default_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_SECS
}

impl fmt::Debug for DeviceAuthResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceAuthResponse")
            .field("device_code", &"[redacted]")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

impl DeviceAuthResponse {
    /// Preferred browser URL (`verification_uri_complete` when provided, else `verification_uri`).
    #[must_use]
    pub fn display_url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or(&self.verification_uri)
    }
}

#[derive(Deserialize)]
struct PollErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn validate_device_config(cfg: &DeviceCodeConfig) -> Result {
    guard::check_oauth_client_id(&cfg.client_id)?;
    guard::check_outbound_url(&cfg.device_auth_url)?;
    guard::check_outbound_url(&cfg.token_url)?;
    require_secure_url(&cfg.device_auth_url, "device_auth_url")?;
    require_secure_url(&cfg.token_url, "token_url")?;
    Ok(())
}

fn build_device_auth_body(cfg: &DeviceCodeConfig) -> TokenBody {
    if cfg.json_body {
        let mut map = Map::new();
        map.insert("client_id".into(), json!(cfg.client_id));
        if let Some(scope) = cfg.scope.as_deref().filter(|s| !s.trim().is_empty()) {
            map.insert("scope".into(), json!(scope));
        }
        for (k, v) in &cfg.extra_params {
            map.insert(k.clone(), json!(v));
        }
        TokenBody::Json(Value::Object(map))
    } else {
        let mut pairs = vec![("client_id".to_owned(), cfg.client_id.clone())];
        if let Some(scope) = cfg.scope.as_deref().filter(|s| !s.trim().is_empty()) {
            pairs.push(("scope".to_owned(), scope.to_owned()));
        }
        for (k, v) in &cfg.extra_params {
            pairs.push((k.clone(), v.clone()));
        }
        TokenBody::FormOwned(pairs)
    }
}

fn build_poll_body(cfg: &DeviceCodeConfig, device_code: &str) -> TokenBody {
    if cfg.json_body {
        TokenBody::Json(json!({
            "grant_type": DEVICE_CODE_GRANT_TYPE,
            "device_code": device_code,
            "client_id": cfg.client_id,
        }))
    } else {
        TokenBody::Form(vec![
            ("grant_type", DEVICE_CODE_GRANT_TYPE.to_owned()),
            ("device_code", device_code.to_owned()),
            ("client_id", cfg.client_id.clone()),
        ])
    }
}

/// Start an RFC 8628 device authorization flow and return the user code and verification URI.
pub async fn start_device_flow(
    http: &HttpClient,
    cfg: &DeviceCodeConfig,
) -> Result<DeviceAuthResponse> {
    validate_device_config(cfg)?;
    let extra: Vec<(&str, &str)> = cfg
        .extra_headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let body = build_device_auth_body(cfg);
    let reply = post_token(
        http,
        &cfg.device_auth_url,
        body,
        &extra,
        REQUEST_TIMEOUT,
        "device authorization",
    )
    .await?;
    let parsed: DeviceAuthResponse = serde_json::from_slice(&reply)
        .map_err(|e| HrnError::Auth(format!("parse device authorization response: {e}")))?;
    if parsed.device_code.trim().is_empty()
        || parsed.user_code.trim().is_empty()
        || parsed.verification_uri.trim().is_empty()
    {
        return Err(HrnError::Auth(
            "device authorization response missing required fields".into(),
        ));
    }
    guard::check_outbound_url(&parsed.verification_uri)?;
    require_secure_url(&parsed.verification_uri, "verification_uri")?;
    if let Some(complete) = &parsed.verification_uri_complete
        && !complete.trim().is_empty()
    {
        guard::check_outbound_url(complete)?;
        require_secure_url(complete, "verification_uri_complete")?;
    }
    Ok(parsed)
}

/// Poll the token endpoint until authorization completes, expires, is denied, or is cancelled.
pub async fn poll_device_token(
    http: &HttpClient,
    cfg: &DeviceCodeConfig,
    auth: &DeviceAuthResponse,
    cancel: &CancelFlag,
) -> Result<StoredToken> {
    let raw = poll_device_token_raw(http, cfg, auth, cancel).await?;
    parse_token_response(&raw, None)
}

/// Poll the token endpoint and return the raw JSON reply bytes (shared with subscription mapping).
pub(crate) async fn poll_device_token_raw(
    http: &HttpClient,
    cfg: &DeviceCodeConfig,
    auth: &DeviceAuthResponse,
    cancel: &CancelFlag,
) -> Result<Bytes> {
    validate_device_config(cfg)?;
    let unit = cfg.poll_time_unit.unwrap_or(Duration::from_secs(1));
    let mut interval_secs = auth.interval.max(1);
    let expires_dur = scaled_duration(unit, auth.expires_in.max(1));
    let total_wait = cfg.max_wait.map_or(expires_dur, |cap| cap.min(expires_dur));
    let deadline = Instant::now() + total_wait;

    loop {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(HrnError::Auth(
                "device authorization expired before approval; retry login".into(),
            ));
        }
        let step = scaled_duration(unit, interval_secs);
        sleep_cancellable(step, deadline, cancel).await?;
        if Instant::now() >= deadline {
            return Err(HrnError::Auth(
                "device authorization expired before approval; retry login".into(),
            ));
        }

        match poll_once(http, cfg, &auth.device_code).await? {
            PollStep::Done(bytes) => return Ok(bytes),
            PollStep::Pending => {}
            PollStep::SlowDown => {
                interval_secs = interval_secs.saturating_add(SLOW_DOWN_INCREMENT_SECS);
            }
        }
    }
}

enum PollStep {
    Done(Bytes),
    Pending,
    SlowDown,
}

async fn poll_once(
    http: &HttpClient,
    cfg: &DeviceCodeConfig,
    device_code: &str,
) -> Result<PollStep> {
    let (content_type, bytes) = encode_body(build_poll_body(cfg, device_code))?;
    let mut headers = vec![
        ("content-type".to_owned(), content_type.to_owned()),
        ("accept".to_owned(), "application/json".to_owned()),
    ];
    headers.extend(cfg.extra_headers.iter().cloned());
    let resp = http
        .post_raw(&cfg.token_url, &headers, bytes, REQUEST_TIMEOUT)
        .await?;
    if resp.status.is_success() {
        return Ok(PollStep::Done(resp.body));
    }
    let code = resp.status.as_u16();
    let parsed_err = serde_json::from_slice::<PollErrorBody>(&resp.body).ok();
    match parsed_err.as_ref().and_then(|e| e.error.as_deref()) {
        Some("authorization_pending") => Ok(PollStep::Pending),
        Some("slow_down") => Ok(PollStep::SlowDown),
        Some("expired_token") => Err(HrnError::Auth(
            "device authorization code expired; retry login".into(),
        )),
        Some("access_denied") => Err(HrnError::Auth("device authorization denied by user".into())),
        Some(other) => {
            let desc = parsed_err
                .as_ref()
                .and_then(|e| e.error_description.as_deref())
                .filter(|d| !d.trim().is_empty())
                .map(|d| format!(": {}", d.replace(device_code, "[redacted]")))
                .unwrap_or_default();
            Err(HrnError::Auth(format!(
                "device token error `{other}`{desc}"
            )))
        }
        None => {
            let scrubbed = resp.text().replace(device_code, "[redacted]");
            Err(token_endpoint_error(
                "device token poll",
                code,
                &scrubbed,
                resp.retry_after_secs(),
            ))
        }
    }
}

fn encode_body(body: TokenBody) -> Result<(&'static str, Bytes)> {
    match body {
        TokenBody::Json(value) => {
            Ok(("application/json", Bytes::from(serde_json::to_vec(&value)?)))
        }
        TokenBody::Form(pairs) => {
            let mut s = form_urlencoded::Serializer::new(String::new());
            for (k, v) in &pairs {
                s.append_pair(k, v);
            }
            Ok((
                "application/x-www-form-urlencoded",
                Bytes::from(s.finish().into_bytes()),
            ))
        }
        TokenBody::FormOwned(pairs) => {
            let mut s = form_urlencoded::Serializer::new(String::new());
            for (k, v) in &pairs {
                s.append_pair(k, v);
            }
            Ok((
                "application/x-www-form-urlencoded",
                Bytes::from(s.finish().into_bytes()),
            ))
        }
    }
}

fn scaled_duration(unit: Duration, count: u64) -> Duration {
    let mult = u32::try_from(count).unwrap_or(u32::MAX);
    unit.saturating_mul(mult)
}

async fn sleep_cancellable(step: Duration, deadline: Instant, cancel: &CancelFlag) -> Result {
    let target = (Instant::now() + step).min(deadline);
    loop {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        let now = Instant::now();
        if now >= target {
            return Ok(());
        }
        let chunk = target.saturating_duration_since(now).min(CANCEL_TICK);
        tokio::time::sleep(chunk).await;
    }
}

#[cfg(test)]
#[path = "device_code_tests.rs"]
mod tests;
