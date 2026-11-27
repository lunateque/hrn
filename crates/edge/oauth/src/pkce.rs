//! RFC 7636 PKCE (`S256`) + localhost callback exchange for `hrn login`.

use crate::callback::{CodeSources, await_code};
use crate::manager::{StoredToken, TokenBody, post_token, require_secure_url, write_json_atomic};
use crate::sha256::{base64url_encode, sha256, to_hex};
use hrn_edge_http::{HttpClient, guard};
use hrn_kernel::{CancelFlag, HrnError, Result};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;

/// How long the whole browser round-trip may take.
pub(crate) const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);

/// Generated PKCE verifier + `S256` challenge pair.
#[derive(Clone, Debug)]
pub struct PkceChallenge {
    /// High-entropy verifier kept secret until token exchange.
    pub verifier: String,
    /// `BASE64URL(SHA256(verifier))`.
    pub challenge: String,
    /// CSRF state nonce.
    pub state: String,
}

/// Parameters for an interactive `OAuth2` PKCE browser login flow.
#[derive(Clone, Debug)]
pub struct PkceLoginConfig {
    /// Authorization endpoint URL.
    pub authorize_url: String,
    /// Token exchange endpoint URL.
    pub token_url: String,
    /// `OAuth2` client identifier.
    pub client_id: String,
    /// Space-separated scope string.
    pub scope: String,
    /// Optional fixed callback port (0 picks an ephemeral `127.0.0.1` port).
    pub callback_port: u16,
    /// Callback path (e.g. `/callback`).
    pub callback_path: String,
    /// Optional explicit redirect URI override (useful for `--manual` / headless SSH logins).
    pub redirect_uri_override: Option<String>,
    /// Extra query parameters appended to `authorize_url` (e.g. `audience`, `prompt`).
    pub extra_params: Vec<(String, String)>,
    /// Send token exchange as JSON instead of `application/x-www-form-urlencoded`.
    pub json_body: bool,
    /// Destination path for saved `0600` token JSON.
    pub token_file: String,
}

/// Generate a fresh PKCE (`S256`) verifier (96 random bytes, 128 characters: the RFC 7636
/// maximum), its challenge, and a hex state nonce, all from the OS CSPRNG.
pub fn generate_pkce() -> Result<PkceChallenge> {
    let verifier = base64url_encode(&random_bytes(96)?);
    let challenge = base64url_encode(&sha256(verifier.as_bytes()));
    let state = to_hex(&random_bytes(16)?);
    Ok(PkceChallenge {
        verifier,
        challenge,
        state,
    })
}

fn validate_pkce_config(cfg: &PkceLoginConfig) -> Result {
    guard::check_oauth_client_id(&cfg.client_id)?;
    guard::check_token_path(&cfg.token_file)?;
    guard::check_outbound_url(&cfg.authorize_url)?;
    guard::check_outbound_url(&cfg.token_url)?;
    require_secure_url(&cfg.authorize_url, "authorize_url")?;
    require_secure_url(&cfg.token_url, "token_url")?;
    Ok(())
}

/// Construct the full authorization URL with PKCE `S256` challenge and optional extra params
/// (`client_id`, `response_type`, `redirect_uri`, `scope`, challenge, method, `state`, extras).
pub fn build_authorize_url(
    cfg: &PkceLoginConfig,
    redirect_uri: &str,
    pkce: &PkceChallenge,
) -> Result<String> {
    validate_pkce_config(cfg)?;
    let mut qs = form_urlencoded::Serializer::new(String::new());
    qs.append_pair("client_id", &cfg.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri);
    if !cfg.scope.trim().is_empty() {
        qs.append_pair("scope", &cfg.scope);
    }
    qs.append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &pkce.state);
    for (k, v) in &cfg.extra_params {
        qs.append_pair(k, v);
    }
    let separator = if cfg.authorize_url.contains('?') {
        '&'
    } else {
        '?'
    };
    Ok(format!("{}{separator}{}", cfg.authorize_url, qs.finish()))
}

/// Parse user-pasted manual callback input: accepts a full redirect URL (`...?code=X&state=Y`),
/// `code#state`, or a bare authorization code. Requires a matching `state` on every URL-shaped
/// input and every `code#state` input. Strips surrounding quotes/angle brackets and any terminal
/// line-wrap whitespace inside pasted URLs or `code#state` values.
pub fn parse_manual_callback(input: &str, expected_state: &str) -> Result<String> {
    let trimmed = strip_wrapping_delimiters(input.trim());
    if trimmed.is_empty() {
        return Err(HrnError::Auth(
            "empty authorization code / redirect URL".into(),
        ));
    }
    if trimmed.contains("://") || trimmed.contains('?') {
        let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        let Some((_, query_and_frag)) = compact.split_once('?') else {
            return Err(HrnError::Auth(
                "callback URL is missing required `code` and `state` query parameters".into(),
            ));
        };
        let query = query_and_frag.split('#').next().unwrap_or(query_and_frag);
        let params: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        if let Some(err) = params.get("error") {
            return Err(HrnError::Auth(format!(
                "OAuth provider returned error `{err}`"
            )));
        }
        match params.get("state").map(String::as_str) {
            Some(state) if !state.is_empty() && state == expected_state => {}
            Some(_) => {
                return Err(HrnError::Auth(
                    "OAuth state mismatch in pasted callback URL; retry login".into(),
                ));
            }
            None => {
                return Err(HrnError::Auth(
                    "callback URL is missing required `state` parameter; retry login".into(),
                ));
            }
        }
        return params
            .get("code")
            .filter(|c| !c.is_empty())
            .cloned()
            .ok_or_else(|| HrnError::Auth("callback URL is missing `code` parameter".into()));
    }
    if trimmed.contains('#') {
        let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        if let Some((code, state)) = compact.split_once('#') {
            if state.is_empty() {
                return Err(HrnError::Auth(
                    "pasted `code#state` is missing required state; retry login".into(),
                ));
            }
            if state != expected_state {
                return Err(HrnError::Auth(
                    "OAuth state mismatch in pasted code#state; retry login".into(),
                ));
            }
            if code.is_empty() {
                return Err(HrnError::Auth("empty authorization code".into()));
            }
            return Ok(code.to_owned());
        }
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(HrnError::Auth(
            "invalid authorization code (contains whitespace)".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn strip_wrapping_delimiters(mut s: &str) -> &str {
    loop {
        let next = s
            .strip_prefix('<')
            .and_then(|r| r.strip_suffix('>'))
            .or_else(|| s.strip_prefix('"').and_then(|r| r.strip_suffix('"')))
            .or_else(|| s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
            .map(str::trim);
        match next {
            Some(inner) => s = inner,
            None => return s,
        }
    }
}

/// Run an interactive localhost PKCE login flow and persist the resulting [`StoredToken`].
pub async fn run_pkce_login<F>(
    http: &HttpClient,
    cfg: &PkceLoginConfig,
    on_authorize_url: F,
) -> Result<StoredToken>
where
    F: FnOnce(&str),
{
    validate_pkce_config(cfg)?;

    let listener = TcpListener::bind(("127.0.0.1", cfg.callback_port))
        .await
        .map_err(|e| HrnError::Io(format!("bind OAuth callback listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| HrnError::Io(format!("OAuth callback listener address: {e}")))?
        .port();
    let path = normalize_callback_path(&cfg.callback_path);
    let redirect_uri = cfg
        .redirect_uri_override
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}{path}"));
    let pkce = generate_pkce()?;
    let auth_url = build_authorize_url(cfg, &redirect_uri, &pkce)?;
    on_authorize_url(&auth_url);

    let code = tokio::time::timeout(
        LOGIN_TIMEOUT,
        await_code(CodeSources {
            listener: Some(&listener),
            path: &path,
            state: &pkce.state,
            pasted: None,
            notice: &|_| {},
            cancel: None,
        }),
    )
    .await
    .map_err(|_| HrnError::Auth("OAuth login timed out waiting for browser callback".into()))??;
    let token = exchange_code(http, cfg, &redirect_uri, &code, &pkce.verifier).await?;
    write_json_atomic(Path::new(&cfg.token_file), &token)?;
    Ok(token)
}

/// Run an interactive PKCE login racing localhost callback with pasted redirect/code input and
/// supporting cancellation (used by in-TUI `/login`).
pub async fn run_pkce_login_interactive(
    http: &HttpClient,
    cfg: &PkceLoginConfig,
    manual: bool,
    pasted: Option<&mut UnboundedReceiver<String>>,
    on_authorize_url: &(dyn Fn(&str) + Sync),
    on_notice: &(dyn Fn(&str) + Sync),
    cancel: Option<&CancelFlag>,
) -> Result<StoredToken> {
    validate_pkce_config(cfg)?;
    let path = normalize_callback_path(&cfg.callback_path);
    let (listener, port) = if manual {
        (
            None,
            if cfg.callback_port == 0 {
                1455
            } else {
                cfg.callback_port
            },
        )
    } else {
        let listener = TcpListener::bind(("127.0.0.1", cfg.callback_port))
            .await
            .map_err(|e| HrnError::Io(format!("bind OAuth callback listener: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| HrnError::Io(format!("OAuth callback listener address: {e}")))?
            .port();
        (Some(listener), port)
    };
    let redirect_uri = cfg
        .redirect_uri_override
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}{path}"));
    let pkce = generate_pkce()?;
    let auth_url = build_authorize_url(cfg, &redirect_uri, &pkce)?;
    on_authorize_url(&auth_url);

    let code = tokio::time::timeout(
        LOGIN_TIMEOUT,
        await_code(CodeSources {
            listener: listener.as_ref(),
            path: &path,
            state: &pkce.state,
            pasted,
            notice: on_notice,
            cancel,
        }),
    )
    .await
    .map_err(|_| HrnError::Auth("OAuth login timed out waiting for browser callback".into()))??;
    drop(listener);
    let token = exchange_code(http, cfg, &redirect_uri, &code, &pkce.verifier).await?;
    write_json_atomic(Path::new(&cfg.token_file), &token)?;
    Ok(token)
}

/// Run a headless/SSH manual PKCE login flow where `read_code` supplies the pasted redirect URL
/// or bare authorization code after displaying the authorization URL.
pub async fn run_pkce_manual_login<F, R>(
    http: &HttpClient,
    cfg: &PkceLoginConfig,
    on_authorize_url: F,
    read_code: R,
) -> Result<StoredToken>
where
    F: FnOnce(&str),
    R: FnOnce() -> Result<String>,
{
    validate_pkce_config(cfg)?;
    let path = normalize_callback_path(&cfg.callback_path);
    let port = if cfg.callback_port == 0 {
        1455
    } else {
        cfg.callback_port
    };
    let redirect_uri = cfg
        .redirect_uri_override
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}{path}"));
    let pkce = generate_pkce()?;
    let auth_url = build_authorize_url(cfg, &redirect_uri, &pkce)?;
    on_authorize_url(&auth_url);

    let raw_input = read_code()?;
    let code = parse_manual_callback(&raw_input, &pkce.state)?;
    let token = exchange_code(http, cfg, &redirect_uri, &code, &pkce.verifier).await?;
    write_json_atomic(Path::new(&cfg.token_file), &token)?;
    Ok(token)
}

fn normalize_callback_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "/callback".to_owned()
    } else if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    }
}

async fn exchange_code(
    http: &HttpClient,
    cfg: &PkceLoginConfig,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<StoredToken> {
    let body = if cfg.json_body {
        TokenBody::Json(json!({
            "grant_type": "authorization_code",
            "client_id": cfg.client_id,
            "code": code,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        }))
    } else {
        TokenBody::Form(vec![
            ("grant_type", "authorization_code".to_owned()),
            ("client_id", cfg.client_id.clone()),
            ("code", code.to_owned()),
            ("redirect_uri", redirect_uri.to_owned()),
            ("code_verifier", verifier.to_owned()),
        ])
    };
    let reply = post_token(
        http,
        &cfg.token_url,
        body,
        &[],
        Duration::from_secs(30),
        "OAuth code exchange",
    )
    .await?;
    crate::manager::parse_token_response(&reply, None)
}

/// `len` bytes from the OS CSPRNG.
pub(crate) fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| HrnError::Internal(format!("OS random source unavailable: {e}")))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_values_are_rfc7636_shaped_and_unique() {
        let a = generate_pkce().expect("csprng");
        let b = generate_pkce().expect("csprng");
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
        // RFC 7636 §4.1 allows 43..=128 characters; 96 random bytes encode to exactly 128.
        assert_eq!(a.verifier.len(), 128);
        assert_eq!(a.state.len(), 32);
        assert!(a.state.bytes().all(|c| c.is_ascii_hexdigit()));
        assert!(
            a.verifier
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_eq!(
            a.challenge,
            base64url_encode(&sha256(a.verifier.as_bytes()))
        );
    }

    #[test]
    fn manual_callback_parses_url_hash_or_bare_code_and_checks_state() {
        assert_eq!(
            parse_manual_callback("http://127.0.0.1:1455/callback?code=abc123&state=s1", "s1")
                .expect("url"),
            "abc123"
        );
        assert_eq!(
            parse_manual_callback("abc123#s1", "s1").expect("hash"),
            "abc123"
        );
        assert_eq!(
            parse_manual_callback("bare-code", "s1").expect("bare"),
            "bare-code"
        );
        assert!(
            parse_manual_callback("http://127.0.0.1:1455/callback?code=abc&state=wrong", "s1")
                .is_err()
        );
        assert!(
            parse_manual_callback("http://127.0.0.1:1455/callback?code=ATTACKER_CODE", "s1")
                .is_err(),
            "URL without state must be rejected"
        );
        assert!(
            parse_manual_callback("?code=abc", "s1").is_err(),
            "query string without state must be rejected"
        );
        assert!(
            parse_manual_callback("http://127.0.0.1:1455/callback?code=abc&state=", "s1").is_err(),
            "URL with empty state must be rejected"
        );
        assert!(
            parse_manual_callback("abc#", "s1").is_err(),
            "code# with empty state must be rejected"
        );
        assert!(
            parse_manual_callback("abc#wrong", "s1").is_err(),
            "code# with mismatched state must be rejected"
        );
        assert!(
            parse_manual_callback("http://127.0.0.1:1455/callback?error=denied", "s1").is_err()
        );
    }

    #[test]
    fn manual_callback_handles_wrapped_long_urls_and_delimiters() {
        let long_code = "c".repeat(620);
        let state = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
        let wrapped = format!(
            "<http://localhost:1455/auth/callback?code={}\r\n  {}&state={}\n>",
            &long_code[..300],
            &long_code[300..],
            state
        );
        assert_eq!(
            parse_manual_callback(&wrapped, state).expect("wrapped long url"),
            long_code
        );
        let wrapped_hash = format!("\"{}\n#{}\"", &long_code[..80], state);
        assert_eq!(
            parse_manual_callback(&wrapped_hash, state).expect("wrapped code#state"),
            &long_code[..80]
        );
    }
}
