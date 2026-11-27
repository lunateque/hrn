//! Authoritative DNS and HTTPS `/healthz` readiness probing for Cloudflare Quick Tunnels.
//!
//! `cloudflared` prints `Registered tunnel connection` several seconds before `*.trycloudflare.com`
//! finishes publishing in Cloudflare's authoritative DNS. Probing a recursive resolver (`1.1.1.1`,
//! `8.8.8.8`, OS DNS, or a phone carrier resolver) before publication poisons its negative cache
//! for up to the zone's 60 s SOA minimum TTL, causing phones scanning the QR code to see "page
//! unavailable".
//!
//! To avoid negative-cache poisoning completely, we first send non-recursive (`RD=0`) UDP DNS `A`
//! queries directly to `trycloudflare.com`'s authoritative nameservers until the record is live,
//! and only then verify `GET https://<slug>.trycloudflare.com/healthz` returns `200 OK`.

use hrn_edge_http::HttpClient;
use hrn_kernel::{HrnError, Result};
use http::StatusCode;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::Instant;

const AUTH_NS_HOSTS: [&str; 2] = ["kevin.ns.cloudflare.com", "marjory.ns.cloudflare.com"];
const FALLBACK_AUTH_NS: [SocketAddr; 2] = [
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 64, 33, 191)), 53),
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 64, 32, 199)), 53),
];
const NS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const DNS_PROPAGATION_TIMEOUT: Duration = Duration::from_secs(22);
const DNS_POLL_INTERVAL: Duration = Duration::from_millis(400);
const UDP_QUERY_TIMEOUT: Duration = Duration::from_millis(1200);
const DNS_PROPAGATED_SETTLE: Duration = Duration::from_millis(450);
const UDP_BLOCKED_SETTLE: Duration = Duration::from_secs(3);
const HEALTHZ_READY_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTHZ_REQ_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTHZ_POLL_INTERVAL: Duration = Duration::from_millis(700);
const HEALTHZ_MAX_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DnsProbeReply {
    Resolved,
    NotYet,
    Invalid,
}

/// Wait until a freshly registered `https://<slug>.trycloudflare.com` quick tunnel has published
/// its authoritative DNS record and responds `200 OK` on `/healthz`.
pub(super) async fn wait_trycloudflare_ready(public_url: &str) -> Result<()> {
    let host = extract_https_host(public_url).ok_or_else(|| {
        HrnError::Io(format!(
            "cannot probe readiness for malformed tunnel URL `{public_url}`"
        ))
    })?;
    wait_authoritative_dns(host).await;
    wait_https_healthz(public_url).await
}

fn extract_https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split('/').next()?;
    if host.is_empty() || host.contains(':') {
        return None;
    }
    Some(host)
}

async fn resolve_auth_ns() -> Vec<SocketAddr> {
    let mut addrs = Vec::new();
    for ns in AUTH_NS_HOSTS {
        let lookup = tokio::time::timeout(NS_LOOKUP_TIMEOUT, tokio::net::lookup_host((ns, 53)))
            .await
            .ok()
            .and_then(std::result::Result::ok);
        if let Some(iter) = lookup
            && let Some(v4) = iter.into_iter().find(SocketAddr::is_ipv4)
            && !addrs.contains(&v4)
        {
            addrs.push(v4);
        }
    }
    if addrs.is_empty() {
        addrs.extend_from_slice(&FALLBACK_AUTH_NS);
    }
    addrs
}

async fn wait_authoritative_dns(host: &str) {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else {
        tokio::time::sleep(UDP_BLOCKED_SETTLE).await;
        return;
    };
    let servers = resolve_auth_ns().await;
    let deadline = Instant::now() + DNS_PROPAGATION_TIMEOUT;
    let mut tx_id: u16 = 0x4852;
    let mut saw_any_reply = false;
    let mut silent_rounds: u8 = 0;
    let mut first_resolved: Option<Instant> = None;

    while Instant::now() < deadline {
        let mut resolved = 0usize;
        let mut replied = 0usize;
        for &ns in &servers {
            tx_id = tx_id.wrapping_add(1);
            match query_ns_once(&socket, ns, host, tx_id).await {
                Some(DnsProbeReply::Resolved) => {
                    resolved += 1;
                    replied += 1;
                }
                Some(DnsProbeReply::NotYet) => replied += 1,
                Some(DnsProbeReply::Invalid) | None => {}
            }
        }
        if replied > 0 {
            saw_any_reply = true;
            silent_rounds = 0;
        }
        if resolved == servers.len() {
            tokio::time::sleep(DNS_PROPAGATED_SETTLE).await;
            return;
        }
        if resolved > 0 {
            let since = *first_resolved.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(1) {
                return;
            }
        } else if !saw_any_reply {
            silent_rounds = silent_rounds.saturating_add(1);
            if silent_rounds >= 2 {
                // Outbound UDP/53 appears filtered; fall back to a brief settle before /healthz.
                tokio::time::sleep(UDP_BLOCKED_SETTLE).await;
                return;
            }
        }
        tokio::time::sleep(DNS_POLL_INTERVAL).await;
    }
}

async fn query_ns_once(
    socket: &UdpSocket,
    ns: SocketAddr,
    host: &str,
    tx_id: u16,
) -> Option<DnsProbeReply> {
    let packet = encode_dns_a_query(host, tx_id)?;
    socket.send_to(&packet, ns).await.ok()?;
    let deadline = Instant::now() + UDP_QUERY_TIMEOUT;
    let mut buf = [0u8; 512];
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let recv = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await;
        let Ok(Ok((len, peer))) = recv else {
            return None;
        };
        if peer != ns {
            continue;
        }
        let slice = buf.get(..len)?;
        let reply = parse_dns_a_reply(slice, tx_id);
        if reply != DnsProbeReply::Invalid {
            return Some(reply);
        }
    }
    None
}

async fn wait_https_healthz(public_url: &str) -> Result<()> {
    let client = HttpClient::new()?;
    let url = format!("{}/healthz", public_url.trim_end_matches('/'));
    let deadline = Instant::now() + HEALTHZ_READY_TIMEOUT;
    let mut last_err = "timed out".to_owned();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let req_timeout = remaining.min(HEALTHZ_REQ_TIMEOUT);
        match client.get(&url, &[], req_timeout, HEALTHZ_MAX_BYTES).await {
            Ok(resp) if resp.status == StatusCode::OK => return Ok(()),
            Ok(resp) => last_err = format!("HTTP {}", resp.status.as_u16()),
            Err(err) => last_err = err.to_string(),
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(HEALTHZ_POLL_INTERVAL).await;
    }

    Err(HrnError::Io(format!(
        "quick tunnel `{public_url}` did not pass /healthz: {last_err}"
    )))
}

/// Encode a minimal RFC 1035 non-recursive (`RD=0`) `IN A` query for `host`.
fn encode_dns_a_query(host: &str, tx_id: u16) -> Option<Vec<u8>> {
    let trimmed = host.trim_end_matches('.');
    if trimmed.is_empty() || trimmed.len() > 253 {
        return None;
    }
    let mut out = Vec::with_capacity(12 + trimmed.len() + 6);
    out.extend_from_slice(&tx_id.to_be_bytes());
    out.extend_from_slice(&[0x00, 0x00]); // Standard query, RD=0 (authoritative)
    out.extend_from_slice(&[0x00, 0x01]); // QDCOUNT = 1
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // AN/NS/AR = 0
    for label in trimmed.split('.') {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
        let len = u8::try_from(label.len()).ok()?;
        out.push(len);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0x00); // Root label terminator
    out.extend_from_slice(&[0x00, 0x01]); // QTYPE = A
    out.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN
    Some(out)
}

/// Inspect a DNS wire reply header for `expected_tx_id` to check whether `ANCOUNT > 0`.
fn parse_dns_a_reply(packet: &[u8], expected_tx_id: u16) -> DnsProbeReply {
    let Some(hdr) = packet.get(..12) else {
        return DnsProbeReply::Invalid;
    };
    let (Some(&b0), Some(&b1), Some(&b2), Some(&b3), Some(&b6), Some(&b7)) = (
        hdr.first(),
        hdr.get(1),
        hdr.get(2),
        hdr.get(3),
        hdr.get(6),
        hdr.get(7),
    ) else {
        return DnsProbeReply::Invalid;
    };
    let tx_id = u16::from_be_bytes([b0, b1]);
    if tx_id != expected_tx_id {
        return DnsProbeReply::Invalid;
    }
    let flags = u16::from_be_bytes([b2, b3]);
    let is_response = (flags & 0x8000) != 0;
    if !is_response {
        return DnsProbeReply::Invalid;
    }
    let rcode = flags & 0x000F;
    let ancount = u16::from_be_bytes([b6, b7]);
    if rcode == 0 && ancount > 0 {
        DnsProbeReply::Resolved
    } else {
        DnsProbeReply::NotYet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_non_recursive_dns_a_query() {
        let pkt = encode_dns_a_query("a-b.trycloudflare.com", 0x1234).expect("query");
        assert_eq!(&pkt[0..2], &[0x12, 0x34]);
        assert_eq!(&pkt[2..4], &[0x00, 0x00], "RD must stay 0");
        assert_eq!(&pkt[4..6], &[0x00, 0x01]);
        assert_eq!(pkt[12], 3);
        assert_eq!(&pkt[13..16], b"a-b");
        assert_eq!(pkt[16], 13);
        assert_eq!(&pkt[17..30], b"trycloudflare");
        assert_eq!(pkt[30], 3);
        assert_eq!(&pkt[31..34], b"com");
        assert_eq!(&pkt[34..], &[0x00, 0x00, 0x01, 0x00, 0x01]);

        assert!(encode_dns_a_query("", 1).is_none());
        assert!(encode_dns_a_query("bad..host", 1).is_none());
        assert!(encode_dns_a_query("bad host.com", 1).is_none());
    }

    #[test]
    fn parses_dns_reply_status_and_validates_tx_id() {
        // Response (QR=1, AA=1, RCODE=0), ANCOUNT=2
        let ok_pkt = [0x12, 0x34, 0x84, 0x00, 0, 1, 0, 2, 0, 0, 0, 0];
        assert_eq!(parse_dns_a_reply(&ok_pkt, 0x1234), DnsProbeReply::Resolved);
        assert_eq!(parse_dns_a_reply(&ok_pkt, 0x9999), DnsProbeReply::Invalid);

        // NXDOMAIN (QR=1, AA=1, RCODE=3), ANCOUNT=0
        let nx_pkt = [0x12, 0x34, 0x84, 0x03, 0, 1, 0, 0, 0, 1, 0, 0];
        assert_eq!(parse_dns_a_reply(&nx_pkt, 0x1234), DnsProbeReply::NotYet);

        // NODATA (QR=1, RCODE=0), ANCOUNT=0
        let nodata = [0x12, 0x34, 0x84, 0x00, 0, 1, 0, 0, 0, 1, 0, 0];
        assert_eq!(parse_dns_a_reply(&nodata, 0x1234), DnsProbeReply::NotYet);

        // Query packet (QR=0) or truncated packet
        let query = [0x12, 0x34, 0x00, 0x00, 0, 1, 0, 1, 0, 0, 0, 0];
        assert_eq!(parse_dns_a_reply(&query, 0x1234), DnsProbeReply::Invalid);
        assert_eq!(
            parse_dns_a_reply(&[0x12, 0x34], 0x1234),
            DnsProbeReply::Invalid
        );
    }

    #[test]
    fn extracts_https_host_strictly() {
        assert_eq!(
            extract_https_host("https://quiet-river-42.trycloudflare.com"),
            Some("quiet-river-42.trycloudflare.com")
        );
        assert_eq!(
            extract_https_host("https://quiet-river-42.trycloudflare.com/healthz"),
            Some("quiet-river-42.trycloudflare.com")
        );
        assert_eq!(extract_https_host("http://127.0.0.1:8787"), None);
        assert_eq!(extract_https_host("https://"), None);
    }
}
