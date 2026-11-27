//! Pairing code lifecycle, bearer session authentication, and LAN/Tailscale address discovery
//! for the opt-in Mobile Pocket Supervision web server.

use hrn_edge_http::{ct_eq, encode_key, generate_key};
use hrn_kernel::Result;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_ACTIVE_PAIRINGS: usize = 32;
const MAX_ACTIVE_SESSIONS: usize = 64;
const DEFAULT_SESSION_TTL: Duration = Duration::from_hours(24);

/// Authorization level granted to a paired mobile browser session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileRole {
    /// Read-only observer: can inspect host info, sessions, transcripts, and live SSE streams.
    View,
    /// Full controller: can also create sessions, send prompts, steer, interrupt, approve, and ask.
    Control,
}

impl MobileRole {
    /// Whether this role permits mutating operations (`turns`, `steer`, `interrupt`, `approve`, `ask`).
    pub const fn can_control(self) -> bool {
        matches!(self, Self::Control)
    }

    /// Canonical lowercase role label (`"view"` or `"control"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Control => "control",
        }
    }
}

struct PairingEntry {
    code: String,
    role: MobileRole,
    expires_at: Instant,
}

struct SessionEntry {
    token: String,
    role: MobileRole,
    expires_at: Instant,
}

#[derive(Default)]
struct PairingState {
    pairings: Vec<PairingEntry>,
    sessions: Vec<SessionEntry>,
}

/// Thread-safe, bounded store for single-use pairing codes and 24-hour mobile bearer sessions.
pub struct PairingStore {
    state: Mutex<PairingState>,
    session_ttl: Duration,
}

impl Default for PairingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingStore {
    /// Create a new empty store with the default 24-hour session TTL.
    pub fn new() -> Self {
        Self::with_session_ttl(DEFAULT_SESSION_TTL)
    }

    /// Create a new empty store with a custom bearer session TTL.
    pub fn with_session_ttl(session_ttl: Duration) -> Self {
        Self {
            state: Mutex::new(PairingState::default()),
            session_ttl,
        }
    }

    /// Mint a fresh 256-bit base64url single-use pairing code valid for `ttl`.
    ///
    /// Keeps at most 32 active pairing codes, evicting expired or oldest entries first.
    pub fn create_pairing(&self, role: MobileRole, ttl: Duration) -> Result<String> {
        let code = encode_key(&generate_key()?);
        let now = Instant::now();
        let mut guard = self.lock_state();
        prune_expired(&mut guard, now);
        while guard.pairings.len() >= MAX_ACTIVE_PAIRINGS {
            guard.pairings.remove(0);
        }
        guard.pairings.push(PairingEntry {
            code: code.clone(),
            role,
            expires_at: now + ttl,
        });
        Ok(code)
    }

    /// Redeem a single-use pairing code in constant time and mint a fresh 256-bit session bearer
    /// token if valid and unexpired.
    pub fn redeem_pairing(&self, code: &str) -> Option<(String, MobileRole)> {
        let candidate = code.trim().as_bytes();
        if candidate.is_empty() {
            return None;
        }
        let now = Instant::now();
        let mut guard = self.lock_state();
        prune_expired(&mut guard, now);

        let mut matched: Option<usize> = None;
        for (idx, entry) in guard.pairings.iter().enumerate() {
            if ct_eq(entry.code.as_bytes(), candidate) {
                matched = Some(idx);
            }
        }
        let idx = matched?;
        let entry = guard.pairings.remove(idx);

        let token = encode_key(&generate_key().ok()?);
        while guard.sessions.len() >= MAX_ACTIVE_SESSIONS {
            guard.sessions.remove(0);
        }
        guard.sessions.push(SessionEntry {
            token: token.clone(),
            role: entry.role,
            expires_at: now + self.session_ttl,
        });
        Some((token, entry.role))
    }

    /// Verify an active session bearer token in constant time and return its [`MobileRole`].
    pub fn verify_session(&self, bearer: &str) -> Option<MobileRole> {
        let candidate = bearer.trim().as_bytes();
        if candidate.is_empty() {
            return None;
        }
        let now = Instant::now();
        let mut guard = self.lock_state();
        prune_expired(&mut guard, now);

        let mut role: Option<MobileRole> = None;
        for entry in &guard.sessions {
            if ct_eq(entry.token.as_bytes(), candidate) {
                role = Some(entry.role);
            }
        }
        role
    }

    /// Number of currently active non-expired paired mobile sessions.
    pub fn connected_count(&self) -> usize {
        let now = Instant::now();
        let mut guard = self.lock_state();
        prune_expired(&mut guard, now);
        guard.sessions.len()
    }

    /// Immediately invalidate all outstanding pairing codes and active mobile sessions.
    pub fn revoke_all(&self) {
        let mut guard = self.lock_state();
        guard.pairings.clear();
        guard.sessions.clear();
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PairingState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn prune_expired(state: &mut PairingState, now: Instant) {
    state.pairings.retain(|p| p.expires_at > now);
    state.sessions.retain(|s| s.expires_at > now);
}

/// Return `true` when `ip` is an IPv4 address in the Tailscale CGNAT range (`100.64.0.0/10`).
pub const fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127
        }
        IpAddr::V6(_) => false,
    }
}

/// Discover reachable Tailscale and primary LAN IPv4 addresses using non-sending UDP route probes.
///
/// Tailscale addresses (`100.64.0.0/10`) are ordered first, followed by private/routable LAN
/// interface addresses, deduplicated and excluding loopback, unspecified, and link-local ranges.
pub fn discover_lan_addrs() -> Vec<IpAddr> {
    const PROBES: &[&str] = &[
        "100.100.100.100:80",
        "1.1.1.1:80",
        "8.8.8.8:80",
        "192.168.1.1:80",
        "10.0.0.1:80",
    ];
    let mut tailscale = Vec::new();
    let mut lan = Vec::new();

    for target in PROBES {
        let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
            continue;
        };
        if sock.connect(target).is_err() {
            continue;
        }
        let Ok(local) = sock.local_addr() else {
            continue;
        };
        let ip = local.ip();
        if !is_usable_lan_ip(ip) {
            continue;
        }
        if is_tailscale_ip(ip) {
            if !tailscale.contains(&ip) {
                tailscale.push(ip);
            }
        } else if !lan.contains(&ip) {
            lan.push(ip);
        }
    }

    tailscale.extend(lan);
    tailscale
}

fn is_usable_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback() && !v4.is_unspecified() && !v4.is_link_local() && !v4.is_multicast()
        }
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified() && !v6.is_multicast(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_is_single_use_and_respects_roles() {
        let store = PairingStore::new();
        let code_ctrl = store
            .create_pairing(MobileRole::Control, Duration::from_mins(1))
            .expect("ctrl code");
        let code_view = store
            .create_pairing(MobileRole::View, Duration::from_mins(1))
            .expect("view code");
        assert_eq!(code_ctrl.len(), 43);
        assert_ne!(code_ctrl, code_view);

        let (tok_ctrl, role_ctrl) = store.redeem_pairing(&code_ctrl).expect("redeem ctrl");
        assert_eq!(role_ctrl, MobileRole::Control);
        assert!(role_ctrl.can_control());
        assert_eq!(tok_ctrl.len(), 43);
        // Single-use: second redemption must fail.
        assert!(store.redeem_pairing(&code_ctrl).is_none());

        let (tok_view, role_view) = store.redeem_pairing(&code_view).expect("redeem view");
        assert_eq!(role_view, MobileRole::View);
        assert!(!role_view.can_control());

        assert_eq!(store.verify_session(&tok_ctrl), Some(MobileRole::Control));
        assert_eq!(store.verify_session(&tok_view), Some(MobileRole::View));
        assert_eq!(store.verify_session("wrong-token"), None);
        assert_eq!(store.connected_count(), 2);

        store.revoke_all();
        assert_eq!(store.connected_count(), 0);
        assert_eq!(store.verify_session(&tok_ctrl), None);
    }

    #[test]
    fn expired_pairing_and_eviction_bounds_enforced() {
        let store = PairingStore::new();
        let expired = store
            .create_pairing(MobileRole::Control, Duration::ZERO)
            .expect("zero ttl");
        assert!(store.redeem_pairing(&expired).is_none());

        // Creating more than MAX_ACTIVE_PAIRINGS evicts oldest codes cleanly.
        let mut codes = Vec::new();
        for _ in 0..(MAX_ACTIVE_PAIRINGS + 5) {
            codes.push(
                store
                    .create_pairing(MobileRole::View, Duration::from_mins(1))
                    .expect("code"),
            );
        }
        assert!(store.redeem_pairing(&codes[0]).is_none());
        assert!(store.redeem_pairing(codes.last().expect("last")).is_some());
    }

    #[test]
    fn tailscale_ip_classification_and_lan_discovery_are_safe() {
        assert!(is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(
            100, 127, 255, 254
        ))));
        assert!(!is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(100, 63, 0, 1))));
        assert!(!is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
        assert!(!is_usable_lan_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!is_usable_lan_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 2))));

        let addrs = discover_lan_addrs();
        for ip in addrs {
            assert!(!ip.is_loopback());
            assert!(!ip.is_unspecified());
        }
    }
}
