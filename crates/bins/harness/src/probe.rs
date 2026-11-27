//! Pre-save API-key validation probe shared by `/login`, `hrn login`, and `/setup`.
//!
//! Validates credentials with a cheap read-only `GET` before anything is written to disk.
//! Never echoes API keys in `Debug`, error messages, or logs, and refuses plaintext HTTP to
//! non-loopback hosts when an API key is present unless `[llm] allow_plaintext_http = true`.

use hrn_edge_http::{HttpClient, guard};
use hrn_edge_llm::ProviderKind;
use hrn_kernel::{HrnError, Result};
use std::fmt;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_BYTES: usize = 256 * 1024;
const ERROR_EXCERPT_CHARS: usize = 160;
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Parameters for one pre-save API-key validation probe. `Debug` redacts `api_key`.
#[derive(Clone)]
pub(crate) struct ProbeTarget {
    pub(crate) provider_id: String,
    pub(crate) kind: ProviderKind,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) allow_plaintext_http: bool,
}

impl fmt::Debug for ProbeTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProbeTarget")
            .field("provider_id", &self.provider_id)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("allow_plaintext_http", &self.allow_plaintext_http)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "(empty)"
                } else {
                    "[redacted]"
                },
            )
            .finish()
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "[::1]"
    )
}

fn is_https(url: &str) -> bool {
    url.trim()
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

fn check_transport_security(
    host: &str,
    url: &str,
    key: &str,
    loopback: bool,
    allow_plaintext_http: bool,
) -> Result {
    if !key.is_empty() && !allow_plaintext_http && !is_https(url) && !loopback {
        return Err(HrnError::Config(format!(
            "refusing to send API key over plaintext HTTP to non-loopback host `{host}` \
             (use https://, or set `[llm] allow_plaintext_http = true` in global config for \
             trusted LAN gateways)"
        )));
    }
    Ok(())
}

fn is_openrouter(provider_id: &str, base_url: &str) -> bool {
    if provider_id.eq_ignore_ascii_case("openrouter") {
        return true;
    }
    guard::extract_host(base_url).is_some_and(|h| {
        let low = h.to_ascii_lowercase();
        low == "openrouter.ai" || low.ends_with(".openrouter.ai")
    })
}

fn is_builtin_openai_host(host: &str) -> bool {
    let low = host.to_ascii_lowercase();
    matches!(
        low.as_str(),
        "api.openai.com"
            | "openrouter.ai"
            | "api.groq.com"
            | "api.x.ai"
            | "api.moonshot.ai"
            | "api.moonshot.cn"
            | "api.deepseek.com"
            | "api.kimi.com"
    ) || low.ends_with(".openrouter.ai")
}

/// Custom OpenAI-compatible gateways (`provider_id == "custom"` or non-builtin `OpenaiChat` /
/// `OpenaiResponses` base URL) may not implement `GET /models`.
fn is_custom_openai_gateway(provider_id: &str, kind: ProviderKind, base_url: &str) -> bool {
    if !matches!(
        kind,
        ProviderKind::OpenaiChat | ProviderKind::OpenaiResponses
    ) {
        return false;
    }
    if provider_id.eq_ignore_ascii_case("custom") {
        return true;
    }
    guard::extract_host(base_url).is_some_and(|h| !is_builtin_openai_host(h))
}

/// Construct the cheap read-only validation URL for `(provider_id, kind, base_url)`.
#[must_use]
pub(crate) fn probe_url(provider_id: &str, kind: ProviderKind, base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if is_openrouter(provider_id, base) {
        if base.ends_with("/api/v1") || base.ends_with("/v1") {
            return format!("{base}/auth/key");
        }
        return format!("{base}/api/v1/auth/key");
    }
    match kind {
        ProviderKind::Anthropic => {
            if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            }
        }
        ProviderKind::Gemini => {
            if base.ends_with("/v1beta") || base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1beta/models")
            }
        }
        ProviderKind::OpenaiChat | ProviderKind::OpenaiResponses => format!("{base}/models"),
        ProviderKind::AgyAcp => "acp://stdio".to_owned(),
    }
}

/// Scrub any accidental reflection of `secret` and clamp error text length.
fn sanitize_error_text(raw: &str, secret: &str) -> String {
    let scrubbed = if secret.is_empty() {
        raw.to_owned()
    } else {
        raw.replace(secret, "[redacted]")
    };
    let clean: String = scrubbed
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = clean.trim();
    match trimmed.char_indices().nth(ERROR_EXCERPT_CHARS) {
        Some((cut, _)) => format!("{}…", trimmed.get(..cut).unwrap_or_default()),
        None => trimmed.to_owned(),
    }
}

/// Validate `target` against the upstream endpoint before saving credentials to disk.
pub(crate) async fn probe_api_key(http: &HttpClient, target: &ProbeTarget) -> Result {
    if target.kind == ProviderKind::AgyAcp {
        return Err(HrnError::Config(
            "`agy_acp` authenticates through the `agy` CLI over stdio; run `hrn login agy` or `hrn doctor`"
                .to_owned(),
        ));
    }
    let url = probe_url(&target.provider_id, target.kind, &target.base_url);
    guard::check_outbound_url(&url)?;
    let host = guard::extract_host(&url)
        .ok_or_else(|| HrnError::Config(format!("invalid probe URL `{url}`")))?;
    let loopback = is_loopback_host(host);
    if target.kind == ProviderKind::Gemini && !loopback {
        guard::check_gemini_base_url(&target.base_url)?;
    }
    let key = target.api_key.trim();
    check_transport_security(host, &url, key, loopback, target.allow_plaintext_http)?;

    let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
    match target.kind {
        ProviderKind::Anthropic => {
            headers.push(("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()));
            if !key.is_empty() {
                headers.push(("x-api-key".to_owned(), key.to_owned()));
            }
        }
        ProviderKind::Gemini => {
            if !key.is_empty() {
                headers.push(("x-goog-api-key".to_owned(), key.to_owned()));
            }
        }
        ProviderKind::OpenaiChat | ProviderKind::OpenaiResponses => {
            if is_openrouter(&target.provider_id, &target.base_url) {
                headers.push(("X-OpenRouter-Title".to_owned(), "hrn".to_owned()));
            }
            if !key.is_empty() {
                headers.push(("authorization".to_owned(), format!("Bearer {key}")));
            }
        }
        ProviderKind::AgyAcp => {}
    }

    let resp = http
        .get(&url, &headers, PROBE_TIMEOUT, MAX_PROBE_BYTES)
        .await
        .map_err(|e| {
            HrnError::Auth(format!(
                "API key validation failed: {}",
                sanitize_error_text(&e.to_string(), key)
            ))
        })?;
    if resp.status.is_success() {
        return Ok(());
    }
    let code = resp.status.as_u16();
    if matches!(code, 401 | 403) {
        return Err(HrnError::Auth(format!(
            "API key rejected (HTTP {code}); check your key and try again"
        )));
    }
    if matches!(code, 404 | 405)
        && is_custom_openai_gateway(&target.provider_id, target.kind, &target.base_url)
    {
        return Ok(());
    }
    let detail = sanitize_error_text(&resp.text(), key);
    if detail.is_empty() {
        Err(HrnError::Auth(format!(
            "API key validation failed (HTTP {code})"
        )))
    } else {
        Err(HrnError::Auth(format!(
            "API key validation failed (HTTP {code}): {detail}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_urls_match_provider_conventions() {
        assert_eq!(
            probe_url(
                "openrouter",
                ProviderKind::OpenaiChat,
                "https://openrouter.ai/api/v1"
            ),
            "https://openrouter.ai/api/v1/auth/key"
        );
        assert_eq!(
            probe_url(
                "openrouter",
                ProviderKind::OpenaiChat,
                "https://openrouter.ai"
            ),
            "https://openrouter.ai/api/v1/auth/key"
        );
        assert_eq!(
            probe_url(
                "anthropic",
                ProviderKind::Anthropic,
                "https://api.anthropic.com"
            ),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            probe_url(
                "anthropic",
                ProviderKind::Anthropic,
                "https://api.anthropic.com/v1"
            ),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            probe_url(
                "gemini",
                ProviderKind::Gemini,
                "https://generativelanguage.googleapis.com"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
        assert_eq!(
            probe_url(
                "openai",
                ProviderKind::OpenaiResponses,
                "https://api.openai.com/v1"
            ),
            "https://api.openai.com/v1/models"
        );
    }

    #[tokio::test]
    async fn plaintext_http_guard_respects_config_and_redacts_debug() {
        let target = ProbeTarget {
            provider_id: "openai".into(),
            kind: ProviderKind::OpenaiResponses,
            base_url: "http://api.example.com/v1".into(),
            api_key: "sk-secret-key-999".into(),
            allow_plaintext_http: false,
        };
        let shown = format!("{target:?}");
        assert!(!shown.contains("sk-secret-key-999"), "{shown}");
        assert!(shown.contains("[redacted]"), "{shown}");

        // Default (`allow_plaintext_http = false`) rejects non-loopback plaintext HTTP with key.
        let http = HttpClient::new().expect("http");
        let err = probe_api_key(&http, &target)
            .await
            .expect_err("must reject plaintext http with key by default");
        let msg = err.to_string();
        assert!(msg.contains("plaintext HTTP"), "{msg}");
        assert!(msg.contains("allow_plaintext_http"), "{msg}");
        assert!(!msg.contains("sk-secret-key-999"), "{msg}");

        // Opting in via `[llm] allow_plaintext_http = true` allows trusted LAN HTTP gateways.
        check_transport_security(
            "api.example.com",
            "http://api.example.com/v1/models",
            "sk-secret-key-999",
            false,
            true,
        )
        .expect("allow_plaintext_http = true must permit non-loopback http");
    }

    async fn spawn_status_server(status_line: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn custom_openai_gateway_accepts_404_and_405_but_rejects_auth_and_5xx() {
        let http = HttpClient::new().expect("http");

        for (provider_id, kind, status) in [
            ("custom", ProviderKind::OpenaiChat, "404 Not Found"),
            (
                "my-proxy",
                ProviderKind::OpenaiResponses,
                "405 Method Not Allowed",
            ),
        ] {
            let base = spawn_status_server(status, r#"{"error":"no models endpoint"}"#).await;
            let target = ProbeTarget {
                provider_id: provider_id.into(),
                kind,
                base_url: base,
                api_key: "sk-test-token".into(),
                allow_plaintext_http: false,
            };
            probe_api_key(&http, &target)
                .await
                .unwrap_or_else(|e| panic!("{provider_id} {status} should succeed: {e}"));
        }

        for (status, expected) in [
            ("401 Unauthorized", "rejected (HTTP 401)"),
            ("403 Forbidden", "rejected (HTTP 403)"),
            ("500 Internal Server Error", "HTTP 500"),
        ] {
            let base = spawn_status_server(status, "denied").await;
            let target = ProbeTarget {
                provider_id: "custom".into(),
                kind: ProviderKind::OpenaiChat,
                base_url: base,
                api_key: "sk-test-token".into(),
                allow_plaintext_http: false,
            };
            let err = probe_api_key(&http, &target)
                .await
                .expect_err("must reject auth/server error");
            assert!(err.to_string().contains(expected), "{status}: {err}");
        }

        let base = spawn_status_server("404 Not Found", "missing").await;
        let non_openai = ProbeTarget {
            provider_id: "anthropic".into(),
            kind: ProviderKind::Anthropic,
            base_url: base,
            api_key: "sk-ant-test".into(),
            allow_plaintext_http: false,
        };
        let err = probe_api_key(&http, &non_openai)
            .await
            .expect_err("non-openai 404 must still fail");
        assert!(err.to_string().contains("HTTP 404"), "{err}");
    }
}
