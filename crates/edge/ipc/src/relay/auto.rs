//! Zero-config in-process `hrn-relay` with optional automatic Cloudflare Quick Tunnel (`HTTPS`).
//!
//! When `[collab] relay` is unset, [`AutoRelay::start`] spins up an embedded `hrn-relay` in the
//! current process and, if `cloudflared` is on `PATH` (and `HRN_RELAY_NO_TUNNEL` is not set),
//! opens an ephemeral `https://<slug>.trycloudflare.com` quick tunnel to loopback so phone and
//! desktop browsers get a real HTTPS origin (required by `WebCrypto` AES-256-GCM) over 4G/5G with
//! zero accounts, zero VPS, and end-to-end encryption (the `#key` fragment never leaves the
//! client). If `cloudflared` is absent or unreachable, it falls back cleanly to a LAN/Tailscale
//! embedded relay.

use super::auto_probe::wait_trycloudflare_ready;
use super::{RelayConfig, RelayHandle, serve};
use crate::discover_lan_addrs;
use hrn_edge_proc::{ChildWrapper, build_wrapped_command, resolve_on_path};
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const TUNNEL_URL_TIMEOUT: Duration = Duration::from_secs(15);
const TUNNEL_READY_GRACE: Duration = Duration::from_secs(8);
const TRYCLOUDFLARE_SUFFIX: &str = ".trycloudflare.com";

/// Whether `cloudflared` is discoverable on `PATH` and automatic quick tunnels are not disabled
/// via `HRN_RELAY_NO_TUNNEL`.
pub fn cloudflared_available() -> bool {
    !tunnel_disabled_by_env() && resolve_on_path("cloudflared").is_some()
}

/// Transport mode selected by [`AutoRelay`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoRelayMode {
    /// Embedded loopback relay exposed via an ephemeral Cloudflare Quick Tunnel (`https://`).
    CloudflareHttps,
    /// Embedded LAN/Tailscale/loopback relay without an external HTTPS tunnel.
    Local,
}

impl AutoRelayMode {
    /// Human-readable transport badge shown in the `/remote` QR overlay.
    pub fn label(self) -> &'static str {
        match self {
            Self::CloudflareHttps => "E2E Relay · Cloudflare HTTPS",
            Self::Local => "E2E Relay · Local",
        }
    }

    /// Short suffix shown in `/collab` status notices.
    pub fn short_label(self) -> &'static str {
        match self {
            Self::CloudflareHttps => "Cloudflare HTTPS",
            Self::Local => "local relay",
        }
    }
}

struct TunnelGuard {
    child: Box<dyn ChildWrapper>,
    drains: Vec<JoinHandle<()>>,
}

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        for task in self.drains.drain(..) {
            task.abort();
        }
    }
}

/// Running zero-config embedded relay plus optional background HTTPS tunnel.
pub struct AutoRelay {
    _relay: RelayHandle,
    connect_url: String,
    public_url: String,
    alt_bases: Vec<String>,
    mode: AutoRelayMode,
    _tunnel: Option<TunnelGuard>,
}

impl AutoRelay {
    /// Start an embedded `hrn-relay` and automatically open a Cloudflare Quick Tunnel when
    /// `cloudflared` is installed and `HRN_RELAY_NO_TUNNEL` is unset; otherwise bind locally on
    /// LAN/Tailscale.
    pub async fn start() -> Result<Self> {
        if cloudflared_available()
            && let Ok(tunneled) = Self::start_cloudflared().await
        {
            return Ok(tunneled);
        }
        Self::start_local_only().await
    }

    /// Start an embedded loopback `hrn-relay` exposed over an ephemeral Cloudflare Quick Tunnel
    /// (`https://<slug>.trycloudflare.com`), waiting until authoritative DNS has published the
    /// hostname and `GET /healthz` returns `200 OK`.
    pub async fn start_cloudflared() -> Result<Self> {
        if !cloudflared_available() {
            return Err(HrnError::Io(
                "cloudflared is unavailable or disabled by HRN_RELAY_NO_TUNNEL".to_owned(),
            ));
        }
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| HrnError::Io(format!("bind loopback relay: {e}")))?;
        let relay = serve(listener, RelayConfig::default()).await?;
        let connect_url = relay.base_url();
        let (public_url, guard) = spawn_cloudflared(&connect_url).await?;
        wait_trycloudflare_ready(&public_url).await?;
        Ok(Self {
            _relay: relay,
            connect_url,
            public_url,
            alt_bases: Vec::new(),
            mode: AutoRelayMode::CloudflareHttps,
            _tunnel: Some(guard),
        })
    }

    /// Start an embedded `hrn-relay` bound for LAN/Tailscale/loopback without opening any
    /// external tunnel (deterministic and offline-safe).
    pub async fn start_local_only() -> Result<Self> {
        let listener = match TcpListener::bind("0.0.0.0:0").await {
            Ok(l) => l,
            Err(_) => TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| HrnError::Io(format!("bind embedded relay: {e}")))?,
        };
        let relay = serve(listener, RelayConfig::default()).await?;
        let port = relay.addr().port();
        let connect_url = format!("http://127.0.0.1:{port}");
        let mut candidates: Vec<String> = discover_lan_addrs()
            .into_iter()
            .map(|ip| format!("http://{}", SocketAddr::new(ip, port)))
            .collect();
        let public_url = if candidates.is_empty() {
            connect_url.clone()
        } else {
            candidates.remove(0)
        };
        let alt_bases: Vec<String> = candidates.into_iter().take(3).collect();
        Ok(Self {
            _relay: relay,
            connect_url,
            public_url,
            alt_bases,
            mode: AutoRelayMode::Local,
            _tunnel: None,
        })
    }

    /// Loopback URL (`http://127.0.0.1:<port>`) the host uses to connect directly to the relay.
    pub fn connect_url(&self) -> &str {
        &self.connect_url
    }

    /// Shareable base URL embedded in `/remote relay` QR codes and `/collab` invite links.
    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    /// Alternate LAN/Tailscale base URLs (populated in [`AutoRelayMode::Local`] mode).
    pub fn alt_bases(&self) -> &[String] {
        &self.alt_bases
    }

    /// Selected transport mode (`CloudflareHttps` or `Local`).
    pub fn mode(&self) -> AutoRelayMode {
        self.mode
    }
}

fn tunnel_disabled_by_env() -> bool {
    std::env::var("HRN_RELAY_NO_TUNNEL")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

async fn spawn_cloudflared(local_url: &str) -> Result<(String, TunnelGuard)> {
    let args = vec![
        "tunnel".to_owned(),
        "--no-autoupdate".to_owned(),
        "--url".to_owned(),
        local_url.to_owned(),
    ];
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut wrap = build_wrapped_command("cloudflared", &args, &cwd, &BTreeMap::new());
    let child = wrap
        .spawn()
        .map_err(|e| HrnError::Io(format!("spawn cloudflared: {e}")))?;
    // Wrap immediately so dropping or cancelling this future always kills `cloudflared`.
    let mut guard = TunnelGuard {
        child,
        drains: Vec::new(),
    };

    if let Some(mut stdout) = guard.child.stdout().take() {
        guard.drains.push(tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await;
        }));
    }
    let Some(stderr) = guard.child.stderr().take() else {
        return Err(HrnError::Io("cloudflared stderr unavailable".to_owned()));
    };
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    let mut found_url: Option<String> = None;
    let url_deadline = tokio::time::Instant::now() + TUNNEL_URL_TIMEOUT;

    while tokio::time::Instant::now() < url_deadline {
        line.clear();
        let remaining = url_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, reader.read_line(&mut line)).await {
            Ok(Ok(0) | Err(_)) | Err(_) => break,
            Ok(Ok(_)) => {
                if let Some(url) = parse_trycloudflare_url(&line) {
                    found_url = Some(url);
                    break;
                }
            }
        }
    }

    let Some(public_url) = found_url else {
        return Err(HrnError::Io(
            "cloudflared did not publish a trycloudflare.com URL in time".to_owned(),
        ));
    };

    // Wait for edge tunnel registration before probing authoritative DNS and `/healthz`.
    let ready_deadline = tokio::time::Instant::now() + TUNNEL_READY_GRACE;
    while tokio::time::Instant::now() < ready_deadline {
        line.clear();
        let remaining = ready_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, reader.read_line(&mut line)).await {
            Ok(Ok(0) | Err(_)) | Err(_) => break,
            Ok(Ok(_)) => {
                if line.contains("Registered tunnel connection") {
                    break;
                }
            }
        }
    }

    // Keep draining stderr in the background so the OS pipe never fills and stalls cloudflared.
    guard.drains.push(tokio::spawn(async move {
        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    }));

    Ok((public_url, guard))
}

/// Extract a strictly validated `https://<slug>.trycloudflare.com` quick-tunnel origin from a
/// `cloudflared` log line, ignoring documentation links or malformed hosts.
pub fn parse_trycloudflare_url(line: &str) -> Option<String> {
    for raw in line.split(|c: char| c.is_whitespace() || matches!(c, '|' | '"' | '\'' | '<' | '>'))
    {
        let trimmed = raw.trim_end_matches('/');
        let Some(host) = trimmed.strip_prefix("https://") else {
            continue;
        };
        let Some(slug) = host.strip_suffix(TRYCLOUDFLARE_SUFFIX) else {
            continue;
        };
        if slug.is_empty()
            || slug.len() > 100
            || slug.starts_with('-')
            || slug.ends_with('-')
            || !slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            continue;
        }
        return Some(format!("https://{host}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collab::{CollabHost, HostSnapshot};
    use std::sync::Arc;

    #[test]
    fn trycloudflare_url_parser_validates_strict_origin() {
        assert_eq!(
            parse_trycloudflare_url(
                "2026-11-26T13:30:00Z INF |  https://quiet-river-42.trycloudflare.com  |"
            )
            .as_deref(),
            Some("https://quiet-river-42.trycloudflare.com")
        );
        assert_eq!(
            parse_trycloudflare_url("url=https://foo-bar.trycloudflare.com/"),
            None
        );
        assert_eq!(
            parse_trycloudflare_url("https://foo-bar.trycloudflare.com/").as_deref(),
            Some("https://foo-bar.trycloudflare.com")
        );
        assert_eq!(
            parse_trycloudflare_url("See https://developers.cloudflare.com/cloudflare-one/"),
            None
        );
        assert_eq!(
            parse_trycloudflare_url("http://insecure.trycloudflare.com"),
            None
        );
        assert_eq!(
            parse_trycloudflare_url("https://evil.sub.trycloudflare.com"),
            None
        );
        assert_eq!(
            parse_trycloudflare_url("https://bad.trycloudflare.com/path?x=1"),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_auto_relay_serves_collab_host_with_custom_public_base() {
        let auto = AutoRelay::start_local_only().await.expect("auto relay");
        assert_eq!(auto.mode(), AutoRelayMode::Local);
        assert!(auto.connect_url().starts_with("http://127.0.0.1:"));

        let (host, _in_rx) = CollabHost::start_with_public_base(
            auto.connect_url(),
            "https://demo-room-99.trycloudflare.com",
            Arc::new(|| HostSnapshot {
                session_id: "s_auto".to_owned(),
                title: "Zero Config".to_owned(),
                ..HostSnapshot::default()
            }),
        )
        .await
        .expect("start host on embedded relay");

        assert!(
            host.interactive_link()
                .starts_with("https://demo-room-99.trycloudflare.com/r/"),
            "{}",
            host.interactive_link()
        );
        assert!(
            host.view_only_link()
                .starts_with("https://demo-room-99.trycloudflare.com/r/"),
            "{}",
            host.view_only_link()
        );
        let alt = host
            .link_with_base(auto.connect_url(), false)
            .expect("alt link");
        assert!(alt.starts_with(auto.connect_url()), "{alt}");

        host.close_and_wait("done").await;
        drop(auto);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "opens a live Cloudflare Quick Tunnel; run with --ignored"]
    async fn live_cloudflared_tunnel_passes_healthz_and_wss_relay_immediately() {
        use crate::collab::CollabGuest;
        use hrn_edge_http::HttpClient;
        use hrn_edge_http::seal::CollabLink;

        let started = std::time::Instant::now();
        let auto = AutoRelay::start_cloudflared()
            .await
            .expect("AutoRelay::start_cloudflared");
        assert_eq!(auto.mode(), AutoRelayMode::CloudflareHttps);
        let public = auto.public_url().to_owned();
        println!(
            "AutoRelay ready in {:.2}s: {public}",
            started.elapsed().as_secs_f64()
        );

        let client = HttpClient::new().expect("http client");
        let hz = client
            .get(
                &format!("{public}/healthz"),
                &[],
                Duration::from_secs(6),
                4096,
            )
            .await
            .expect("immediate /healthz must succeed");
        assert_eq!(hz.status, http::StatusCode::OK);

        let (host, mut host_in) = CollabHost::start_with_public_base(
            auto.connect_url(),
            &public,
            Arc::new(|| HostSnapshot {
                session_id: "s_live".to_owned(),
                title: "Live CF Test".to_owned(),
                ..HostSnapshot::default()
            }),
        )
        .await
        .expect("start host");

        let pwa_url = format!("{public}/r/{}", host.room());
        let pwa = client
            .get(&pwa_url, &[], Duration::from_secs(6), 256 * 1024)
            .await
            .expect("immediate GET /r/<room> must succeed");
        assert_eq!(pwa.status, http::StatusCode::OK);
        assert!(pwa.text().contains("startRelayMode"));

        let link = CollabLink::parse(host.interactive_link()).expect("parse link");
        let (guest, mut guest_ev) = CollabGuest::connect(link, None, "phone-test", None)
            .await
            .expect("guest WSS over trycloudflare.com");
        let joined = tokio::time::timeout(Duration::from_secs(8), host_in.recv())
            .await
            .expect("host join event timeout")
            .expect("host join event");
        println!("Host observed over Cloudflare WSS: {joined:?}");
        let welcome = tokio::time::timeout(Duration::from_secs(8), guest_ev.recv())
            .await
            .expect("guest welcome timeout")
            .expect("guest welcome");
        println!("Guest observed over Cloudflare WSS: {welcome:?}");

        guest.leave();
        host.close_and_wait("done").await;
        drop(auto);
    }
}
