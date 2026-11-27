//! Unbypassable outbound policy guard:
//! 1. Denylists Google Antigravity / Code Assist hosts, consumer Gemini CLI models, the Google
//!    consumer CLI OAuth client IDs, and consumer CLI credential directories (zero `ToS` ban risk).
//!    Standard RFC 8252 PKCE `OAuth` flows configured for a user's own OIDC client are allowed
//!    and keep their own tokens; `hrn` never reads another CLI's credential store.
//! 2. Blocks SSRF targets (loopback, link-local, RFC1918, metadata IPs) for `web_fetch`.

use hrn_kernel::{HrnError, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const BLOCKED_HOSTS: &[&str] = &[
    "cloudcode-pa.googleapis.com",
    "daily-cloudcode-pa.sandbox.googleapis.com",
    "autopush-cloudcode-pa.sandbox.googleapis.com",
    "antigravity.google",
];

const BLOCKED_MODEL_TOKENS: &[&str] = &["antigravity", "gemini-cli", "code-assist", "cloudcode"];

const BLOCKED_CRED_SEGMENTS: &[&str] = &["/.gemini/", "/.claude/", "/.codex/", "/.omp/"];

/// Google consumer CLI OAuth client IDs whose reuse from third-party harnesses triggers bans.
const BLOCKED_OAUTH_CLIENT_PREFIXES: &[&str] = &[
    "681255809395-",  // Gemini CLI consumer OAuth client
    "1071006060591-", // Antigravity / Code Assist OAuth client
];

/// Plain API keys the owner issued himself; omp keeps them, but they are not a CLI login: the
/// gateway token files and the dotenv file of provider keys.
const ALLOWED_KEY_SEGMENTS: &[&str] = &["/.omp/agent/auth-gateway-tokens/", "/.omp/agent/.env/"];

const GEMINI_OFFICIAL_HOST: &str = "generativelanguage.googleapis.com";

/// Validate any outbound URL at the shared HTTP client boundary.
pub fn check_outbound_url(url: &str) -> Result {
    let host = extract_host(url)
        .ok_or_else(|| HrnError::Config(format!("invalid outbound URL: {url}")))?;
    check_outbound_host(host)
}

/// Validate a bare host name (no port, no path) against the `ToS` denylist.
///
/// The HTTP client calls this again with the host its own URI parser produced, so a URL that
/// [`extract_host`] and the client parse differently cannot slip past the denylist.
pub fn check_outbound_host(host: &str) -> Result {
    let lower = host.trim_end_matches('.').to_ascii_lowercase();
    for blocked in BLOCKED_HOSTS {
        let is_blocked = lower == *blocked
            || lower
                .strip_suffix(blocked)
                .is_some_and(|prefix| prefix.ends_with('.'));
        if is_blocked {
            return Err(HrnError::Config(format!(
                "outbound host `{host}` is blocked by ToS guard (Google Antigravity / Code Assist)"
            )));
        }
    }
    Ok(())
}

/// Validate that a configured model id does not target Antigravity / consumer Gemini CLI.
pub fn check_model_id(model_id: &str) -> Result {
    let lower = model_id.to_ascii_lowercase();
    for marker in BLOCKED_MODEL_TOKENS {
        if lower.contains(marker) {
            return Err(HrnError::Config(format!(
                "model `{model_id}` is blocked by ToS guard (`{marker}`)"
            )));
        }
    }
    Ok(())
}

/// Validate that Gemini API providers only target the official Gemini Developer API host.
pub fn check_gemini_base_url(base_url: &str) -> Result {
    check_outbound_url(base_url)?;
    let host = extract_host(base_url)
        .ok_or_else(|| HrnError::Config(format!("invalid Gemini base_url: {base_url}")))?;
    if !host.eq_ignore_ascii_case(GEMINI_OFFICIAL_HOST) {
        return Err(HrnError::Config(format!(
            "Gemini provider host `{host}` must be `{GEMINI_OFFICIAL_HOST}`"
        )));
    }
    Ok(())
}

/// Reject any credential/token file path pointing into consumer CLI stores.
pub fn check_token_path(path: &str) -> Result {
    let normalized = format!("/{}/", path.replace('\\', "/").to_ascii_lowercase());
    if ALLOWED_KEY_SEGMENTS
        .iter()
        .any(|seg| normalized.contains(seg))
        && !normalized.contains("/../")
    {
        return Ok(());
    }
    for seg in BLOCKED_CRED_SEGMENTS {
        if normalized.contains(seg) {
            return Err(HrnError::Config(format!(
                "token path `{path}` is blocked by guard (must not read consumer CLI store `{seg}`)"
            )));
        }
    }
    Ok(())
}

/// Reject the Google consumer CLI `OAuth` client IDs to prevent `ToS` account bans.
pub fn check_oauth_client_id(client_id: &str) -> Result {
    let lower = client_id.trim().to_ascii_lowercase();
    for blocked in BLOCKED_OAUTH_CLIENT_PREFIXES {
        if lower.starts_with(blocked) {
            return Err(HrnError::Config(
                "OAuth client_id is blocked by ToS guard (Google Gemini CLI / Antigravity \
                 client): use an official Gemini API key to protect your account"
                    .into(),
            ));
        }
    }
    Ok(())
}

/// Validate that a `web_fetch` target URL is public HTTP(S) and not a private/loopback/metadata IP.
///
/// Host names are additionally checked after DNS resolution by the client (`get_public`), which
/// is what stops decimal/hex IP spellings and DNS names pointing at private ranges.
pub fn check_public_fetch_url(url: &str) -> Result {
    check_outbound_url(url)?;
    let host =
        extract_host(url).ok_or_else(|| HrnError::Tool(format!("invalid URL host: {url}")))?;
    let host_lc = host.to_ascii_lowercase();
    // Last DNS label: `localhost`, `*.localhost`, `*.local` (mDNS) and `*.internal`.
    if matches!(
        host_lc.rsplit('.').next(),
        Some("localhost" | "local" | "internal")
    ) {
        return Err(HrnError::Tool(format!(
            "web_fetch blocked private host `{host}`"
        )));
    }
    let bare = host_lc.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>()
        && is_private_ip(ip)
    {
        return Err(HrnError::Tool(format!(
            "web_fetch blocked non-public IP `{ip}`"
        )));
    }
    Ok(())
}

/// Check whether a resolved socket IP is private, loopback, link-local, reserved or unspecified.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => is_private_v6(v6),
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || a == 0 // 0.0.0.0/8 "this network"
        || a >= 240 // 240.0.0.0/4 reserved
        || (a == 100 && (64..=127).contains(&b)) // CGNAT 100.64.0.0/10
        || (a == 198 && (b == 18 || b == 19)) // benchmarking 198.18.0.0/15
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments 192.0.0.0/24
}

fn is_private_v6(v6: Ipv6Addr) -> bool {
    // Covers IPv4-mapped (`::ffff:a.b.c.d`) and the deprecated IPv4-compatible (`::a.b.c.d`) forms.
    if let Some(v4) = v6.to_ipv4() {
        return is_private_v4(v4);
    }
    let seg = v6.segments();
    let [first, second, ..] = seg;
    // NAT64 well-known prefix 64:ff9b::/96 embeds an IPv4 address in the last 32 bits.
    if first == 0x0064
        && second == 0xff9b
        && seg.get(2..6).is_some_and(|s| s.iter().all(|x| *x == 0))
    {
        let [.., hi, lo] = seg;
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        return is_private_v4(Ipv4Addr::new(a, b, c, d));
    }
    // 6to4 2002::/16 embeds the IPv4 address in bits 16..48.
    if first == 0x2002 {
        let [a, b] = second.to_be_bytes();
        let [c, d] = seg.get(2).copied().unwrap_or(0).to_be_bytes();
        return is_private_v4(Ipv4Addr::new(a, b, c, d));
    }
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || (first & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (first & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (first & 0xffc0) == 0xfec0 // deprecated site-local fec0::/10
}

/// Extract the host (without port, userinfo or trailing dot) from an HTTP(S) URL.
///
/// IPv6 literals keep their brackets. The authority ends at the first `/`, `?`, `#` or `\`, the
/// same way WHATWG-style parsers delimit it, so `https://host?x=1` yields `host`.
pub fn extract_host(url: &str) -> Option<&str> {
    let prefix_len = [("https://", 8), ("http://", 7)]
        .into_iter()
        .find(|(p, n)| {
            url.get(..*n)
                .is_some_and(|head| head.eq_ignore_ascii_case(p))
        })?
        .1;
    let rest = url.get(prefix_len..)?;
    let authority = rest.split(['/', '?', '#', '\\']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if host_port.starts_with('[') {
        let end = host_port.find(']')?;
        host_port.get(..=end)?
    } else {
        host_port.split(':').next()?
    };
    let host = host.trim_end_matches('.');
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_antigravity_hosts_models_and_cli_stores() {
        assert!(check_outbound_url("https://cloudcode-pa.googleapis.com/v1").is_err());
        assert!(check_outbound_url("https://api.anthropic.com/v1/messages").is_ok());
        assert!(check_model_id("google-antigravity/gemini-3").is_err());
        assert!(check_model_id("claude-opus-4-6").is_ok());
        assert!(check_gemini_base_url("https://generativelanguage.googleapis.com").is_ok());
        assert!(check_gemini_base_url("https://proxy.example.com").is_err());
        assert!(check_token_path(r"C:\Users\alice\.gemini\oauth_creds.json").is_err());
        assert!(check_token_path(r"C:\Users\alice\.claude\credentials.json").is_err());
        assert!(check_token_path(r"C:\Users\alice\AppData\Local\harness\provider.json").is_ok());
        assert!(
            check_token_path("C:/Users/alice/.omp/agent/auth-gateway-tokens/gateway.token").is_ok()
        );
        assert!(check_token_path("C:/Users/alice/.omp/agent/agent.db").is_err());
        assert!(
            check_token_path("C:/Users/alice/.omp/agent/auth-gateway-tokens/../agent.db").is_err()
        );
        assert!(check_oauth_client_id("my-own-oidc-client").is_ok());
    }

    #[test]
    fn oauth_client_guard_allows_custom_clients_and_blocks_google() {
        for allowed in ["my-own-oidc-client", "custom-enterprise-app"] {
            assert!(
                check_oauth_client_id(allowed).is_ok(),
                "{allowed} must pass"
            );
        }
        for blocked in [
            "681255809395-abc.apps.googleusercontent.com", // Gemini CLI
            "1071006060591-xyz.apps.googleusercontent.com", // Antigravity / Code Assist
            " 681255809395-ABC.apps.googleusercontent.com",
        ] {
            let err = check_oauth_client_id(blocked).expect_err(blocked);
            assert!(err.to_string().contains("ToS guard"), "{err}");
        }
    }

    #[test]
    fn blocks_ssrf_private_targets() {
        assert!(check_public_fetch_url("http://127.0.0.1:8080/secret").is_err());
        assert!(check_public_fetch_url("http://169.254.169.254/latest/meta-data").is_err());
        assert!(check_public_fetch_url("http://10.0.0.1/admin").is_err());
        assert!(check_public_fetch_url("https://docs.rs/tokio").is_ok());
    }

    #[test]
    fn authority_delimiters_cannot_hide_a_blocked_host() {
        for url in [
            "https://cloudcode-pa.googleapis.com?x=1",
            "https://cloudcode-pa.googleapis.com#frag",
            "https://cloudcode-pa.googleapis.com:443/v1",
            "https://cloudcode-pa.googleapis.com./v1",
            "HTTPS://CLOUDCODE-PA.GOOGLEAPIS.COM/v1",
            "https://user:pw@cloudcode-pa.googleapis.com/v1",
            "https://x.cloudcode-pa.googleapis.com/v1",
            r"https://cloudcode-pa.googleapis.com\@example.com/v1",
        ] {
            assert!(check_outbound_url(url).is_err(), "{url} must be blocked");
        }
        assert!(check_outbound_url("https://notcloudcode-pa.googleapis.com/").is_ok());
        assert_eq!(extract_host("https://a.b?x"), Some("a.b"));
        assert_eq!(extract_host("http://u@[::1]:80/p"), Some("[::1]"));
        assert_eq!(extract_host("ftp://a.b"), None);
    }

    #[test]
    fn rejects_reserved_and_embedded_ipv4_ranges() {
        for host in [
            "0.0.0.0",
            "0.1.2.3",
            "240.0.0.1",
            "198.18.0.1",
            "100.64.0.1",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "[::127.0.0.1]",
            "[64:ff9b::7f00:1]",
            "[2002:7f00:1::]",
            "[fec0::1]",
            "[fd00::1]",
        ] {
            let url = format!("http://{host}/");
            assert!(
                check_public_fetch_url(&url).is_err(),
                "{host} must be blocked"
            );
        }
        assert!(check_public_fetch_url("http://198.51.100.34/").is_ok());
        assert!(check_public_fetch_url("http://[2606:2800:220:1::1]/").is_ok());
    }
}
