//! Authenticated gzip + AES-256-GCM sealing, URL fragment link codecs, and constant-time
//! comparison for sealed share links and live collaboration rooms.
//!
//! Wire format of [`seal`]: `[12-byte CSPRNG IV][AES-256-GCM ciphertext + 16-byte auth tag]`.
//! Encryption keys and write tokens live strictly inside URL `#fragment` components and are
//! never placed in HTTP paths, query strings, headers, or unencrypted relay metadata.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use hrn_kernel::{HrnError, Result};
use std::io::{Read, Write};

/// AES-256-GCM key length in bytes.
pub const KEY_LEN: usize = 32;
/// AES-256-GCM nonce/IV length in bytes.
pub const NONCE_LEN: usize = 12;
/// AES-256-GCM authentication tag length in bytes.
pub const TAG_LEN: usize = 16;
/// Strict decompression bomb ceiling enforced by [`unseal`] (16 MiB).
pub const MAX_UNSEALED_BYTES: usize = 16 * 1024 * 1024;
/// Minimum length of a validated room or blob identifier.
pub const MIN_ID_LEN: usize = 8;
/// Maximum length of a validated room or blob identifier.
pub const MAX_ID_LEN: usize = 64;

/// Generate a fresh 256-bit random key or write token from the OS CSPRNG.
pub fn generate_key() -> Result<[u8; KEY_LEN]> {
    let mut key = [0u8; KEY_LEN];
    getrandom::getrandom(&mut key)
        .map_err(|e| HrnError::Internal(format!("OS CSPRNG failure: {e}")))?;
    Ok(key)
}

/// Generate a fresh URL-safe random room or blob identifier (128 bits, 22 base64url chars).
pub fn generate_room_id() -> Result<String> {
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw)
        .map_err(|e| HrnError::Internal(format!("OS CSPRNG failure: {e}")))?;
    Ok(B64URL.encode(raw))
}

/// Validate a room or blob identifier against `[A-Za-z0-9_-]{8,64}`.
pub fn is_valid_id(id: &str) -> bool {
    (MIN_ID_LEN..=MAX_ID_LEN).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Constant-time byte-slice equality check (bytewise XOR fold + length fold).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    let mut diff: u8 = std::hint::black_box(u8::from(a.len() != b.len()));
    for i in 0..max_len {
        let ba = std::hint::black_box(a.get(i).copied().unwrap_or(0));
        let bb = std::hint::black_box(b.get(i).copied().unwrap_or(0));
        diff = std::hint::black_box(diff | (ba ^ bb));
    }
    std::hint::black_box(diff) == 0
}

/// Encode a 32-byte key or write token as unpadded URL-safe Base64.
pub fn encode_key(key: &[u8; KEY_LEN]) -> String {
    B64URL.encode(key)
}

/// Decode an unpadded URL-safe Base64 string into a 32-byte key or write token.
pub fn decode_key(raw: &str) -> Result<[u8; KEY_LEN]> {
    let bytes = B64URL
        .decode(raw.trim().as_bytes())
        .map_err(|_| HrnError::Auth("invalid base64url key material".to_owned()))?;
    let key: [u8; KEY_LEN] = bytes
        .try_into()
        .map_err(|_| HrnError::Auth(format!("expected {KEY_LEN}-byte key material")))?;
    Ok(key)
}

/// Gzip-compress `plaintext` and encrypt with AES-256-GCM using a fresh 12-byte CSPRNG IV.
///
/// Output layout: `[12B IV][ciphertext + 16B GCM tag]`.
pub fn seal(plaintext: &[u8], key: &[u8; KEY_LEN]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(plaintext)
        .map_err(|e| HrnError::Internal(format!("gzip compress write failed: {e}")))?;
    let compressed = encoder
        .finish()
        .map_err(|e| HrnError::Internal(format!("gzip compress finish failed: {e}")))?;

    let mut iv = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut iv)
        .map_err(|e| HrnError::Internal(format!("CSPRNG IV generation failed: {e}")))?;

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&iv), compressed.as_slice())
        .map_err(|_| HrnError::Internal("AES-256-GCM encryption failed".to_owned()))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&iv);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Authenticate and decrypt `blob` with AES-256-GCM, then gzip-decompress with a strict 16 MiB
/// output ceiling ([`MAX_UNSEALED_BYTES`]).
pub fn unseal(blob: &[u8], key: &[u8; KEY_LEN]) -> Result<Vec<u8>> {
    unseal_with_limit(blob, key, MAX_UNSEALED_BYTES)
}

/// Like [`unseal`], enforcing a custom maximum decompressed byte cap `max_bytes`.
pub fn unseal_with_limit(blob: &[u8], key: &[u8; KEY_LEN], max_bytes: usize) -> Result<Vec<u8>> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err(HrnError::Auth(
            "sealed payload too short or corrupted".to_owned(),
        ));
    }
    let (iv, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let compressed = cipher
        .decrypt(Nonce::from_slice(iv), ciphertext)
        .map_err(|_| HrnError::Auth("decryption failed: bad key or tampered payload".to_owned()))?;

    let mut decoder = GzDecoder::new(compressed.as_slice());
    let limit_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let mut limited = (&mut decoder).take(limit_u64.saturating_add(1));
    let mut out = Vec::new();
    limited
        .read_to_end(&mut out)
        .map_err(|e| HrnError::Config(format!("gzip decompress failed: {e}")))?;
    if out.len() > max_bytes {
        return Err(HrnError::Config(format!(
            "decompressed payload exceeds {max_bytes}-byte safety cap"
        )));
    }
    Ok(out)
}

/// Format a sealed share link `<server>/s/<id>#<b64url_key>`.
pub fn format_share_url(server_base: &str, blob_id: &str, key: &[u8; KEY_LEN]) -> Result<String> {
    if !is_valid_id(blob_id) {
        return Err(HrnError::Config(format!("invalid blob id `{blob_id}`")));
    }
    let base = server_base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(HrnError::Config("share server URL is empty".to_owned()));
    }
    Ok(format!("{base}/s/{blob_id}#{}", encode_key(key)))
}

/// Parse a sealed share link `<server>/s/<id>#<b64url_key>` into `(server_base, blob_id, key)`.
pub fn parse_share_url(url: &str) -> Result<(String, String, [u8; KEY_LEN])> {
    let trimmed = url.trim();
    let (base_and_path, fragment) = trimmed
        .split_once('#')
        .ok_or_else(|| HrnError::Config("share link missing `#<key>` fragment".to_owned()))?;
    let key = decode_key(fragment)?;
    let (base, id) = base_and_path
        .rsplit_once("/s/")
        .ok_or_else(|| HrnError::Config("share link missing `/s/<id>` path".to_owned()))?;
    let id = id.trim_end_matches('/');
    if !is_valid_id(id) {
        return Err(HrnError::Config(format!("invalid share blob id `{id}`")));
    }
    if base.is_empty() {
        return Err(HrnError::Config(
            "share link missing server base".to_owned(),
        ));
    }
    Ok((base.to_owned(), id.to_owned(), key))
}

/// Parsed live collaboration room link carrying room ID, encryption key, and optional write token.
///
/// Supports both:
/// - Full URL form: `<relay>/r/<room>#<b64url_key>` (view-only) or
///   `<relay>/r/<room>#<b64url_key>.<b64url_write_token>` (interactive)
/// - Compact token form: `<room>.<b64url_key>` (view-only) or
///   `<room>.<b64url_key>.<b64url_write_token>` (interactive)
#[derive(Clone, PartialEq, Eq)]
pub struct CollabLink {
    /// Relay HTTP(S)/WS(S) base URL when parsed from a full link.
    pub relay_base: Option<String>,
    /// Validated room identifier (`[A-Za-z0-9_-]{8,64}`).
    pub room: String,
    /// 32-byte AES-256-GCM room key (kept exclusively in `#fragment`).
    pub key: [u8; KEY_LEN],
    /// Optional 32-byte write token granting interactive prompt/abort permission.
    pub write_token: Option<[u8; KEY_LEN]>,
}

impl std::fmt::Debug for CollabLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollabLink")
            .field("relay_base", &self.relay_base)
            .field("room", &self.room)
            .field("key", &"<redacted>")
            .field(
                "write_token",
                &if self.write_token.is_some() {
                    "Some(<redacted>)"
                } else {
                    "None"
                },
            )
            .finish()
    }
}

impl CollabLink {
    /// Whether this link grants write (prompt/abort) permission.
    pub fn can_write(&self) -> bool {
        self.write_token.is_some()
    }

    /// Return a view-only copy of this link with `write_token` stripped.
    #[must_use]
    pub fn view_only(&self) -> Self {
        Self {
            relay_base: self.relay_base.clone(),
            room: self.room.clone(),
            key: self.key,
            write_token: None,
        }
    }

    /// Encode as the compact `<room>.<b64url_key>[.<b64url_write_token>]` form.
    pub fn to_compact(&self) -> String {
        let key_b64 = encode_key(&self.key);
        match &self.write_token {
            Some(tok) => format!("{}.{key_b64}.{}", self.room, encode_key(tok)),
            None => format!("{}.{key_b64}", self.room),
        }
    }

    /// Encode as `<relay>/r/<room>#<b64url_key>[.<b64url_write_token>]`.
    pub fn to_url(&self, default_relay: Option<&str>) -> Result<String> {
        let base = self
            .relay_base
            .as_deref()
            .or(default_relay)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| HrnError::Config("collab link has no relay URL".to_owned()))?
            .trim_end_matches('/');
        let key_b64 = encode_key(&self.key);
        Ok(match &self.write_token {
            Some(tok) => format!("{base}/r/{}#{key_b64}.{}", self.room, encode_key(tok)),
            None => format!("{base}/r/{}#{key_b64}", self.room),
        })
    }

    /// Build the WebSocket connect URL `<ws(s)://...>/r/<room>?role=<role>`.
    ///
    /// Never includes `#fragment`, encryption key, or write token on the wire.
    pub fn to_ws_url(&self, role: &str, default_relay: Option<&str>) -> Result<String> {
        if role != "host" && role != "guest" {
            return Err(HrnError::Config(format!("invalid collab role `{role}`")));
        }
        if !is_valid_id(&self.room) {
            return Err(HrnError::Config(format!("invalid room id `{}`", self.room)));
        }
        let base = self
            .relay_base
            .as_deref()
            .or(default_relay)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                HrnError::Config(
                    "no collab relay configured (set `[collab] relay` or pass a full URL)"
                        .to_owned(),
                )
            })?
            .trim_end_matches('/');
        let ws_base = http_base_to_ws_base(base)?;
        Ok(format!("{ws_base}/r/{}?role={role}", self.room))
    }

    /// Parse either a full URL or compact `<room>.<key>[.<write_token>]` collab link.
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.contains("://") {
            parse_collab_url(trimmed)
        } else {
            parse_collab_compact(trimmed)
        }
    }
}

fn http_base_to_ws_base(base: &str) -> Result<String> {
    if let Some(rest) = strip_scheme_ci(base, "https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = strip_scheme_ci(base, "http://") {
        Ok(format!("ws://{rest}"))
    } else if let Some(rest) = strip_scheme_ci(base, "wss://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = strip_scheme_ci(base, "ws://") {
        Ok(format!("ws://{rest}"))
    } else {
        Err(HrnError::Config(format!(
            "unsupported relay URL scheme in `{base}` (expected http://, https://, ws://, or wss://)"
        )))
    }
}

fn strip_scheme_ci<'a>(input: &'a str, scheme: &str) -> Option<&'a str> {
    let head = input.get(..scheme.len())?;
    head.eq_ignore_ascii_case(scheme)
        .then(|| input.get(scheme.len()..))
        .flatten()
}

fn parse_collab_url(url: &str) -> Result<SelfLink> {
    let (before_frag, fragment) = url
        .split_once('#')
        .ok_or_else(|| HrnError::Config("collab link missing `#<key>` fragment".to_owned()))?;
    let before_query = before_frag.split('?').next().unwrap_or(before_frag);
    let (base, room_raw) = before_query
        .rsplit_once("/r/")
        .ok_or_else(|| HrnError::Config("collab link missing `/r/<room>` path".to_owned()))?;
    let room = room_raw.trim_end_matches('/');
    if !is_valid_id(room) {
        return Err(HrnError::Config(format!("invalid collab room id `{room}`")));
    }
    let _ = http_base_to_ws_base(base)?;
    let parts: Vec<&str> = fragment.trim().split('.').collect();
    let (key, write_token) = match parts.as_slice() {
        [k] => (decode_key(k)?, None),
        [k, wt] => (decode_key(k)?, Some(decode_key(wt)?)),
        [frag_room, k, wt] if *frag_room == room => (decode_key(k)?, Some(decode_key(wt)?)),
        _ => {
            return Err(HrnError::Config(
                "malformed collab link fragment (expected `#<key>` or `#<key>.<write_token>`)"
                    .to_owned(),
            ));
        }
    };
    Ok(CollabLink {
        relay_base: Some(base.to_owned()),
        room: room.to_owned(),
        key,
        write_token,
    })
}

type SelfLink = CollabLink;

fn parse_collab_compact(compact: &str) -> Result<CollabLink> {
    let parts: Vec<&str> = compact.split('.').collect();
    let (room, key, write_token) = match parts.as_slice() {
        [room, k] => (*room, decode_key(k)?, None),
        [room, k, wt] => (*room, decode_key(k)?, Some(decode_key(wt)?)),
        _ => {
            return Err(HrnError::Config(
                "malformed collab link (expected `<url>/r/<room>#<key>[.<token>]` or `<room>.<key>[.<token>]`)"
                    .to_owned(),
            ));
        }
    };
    if !is_valid_id(room) {
        return Err(HrnError::Config(format!("invalid collab room id `{room}`")));
    }
    Ok(CollabLink {
        relay_base: None,
        room: room.to_owned(),
        key,
        write_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip() {
        let key = generate_key().expect("key");
        let wrong_key = generate_key().expect("wrong key");
        let payload = b"hello sealed world - repeated repeated repeated payload!".repeat(32);

        let sealed1 = seal(&payload, &key).expect("seal1");
        let sealed2 = seal(&payload, &key).expect("seal2");
        assert!(sealed1.len() >= NONCE_LEN + TAG_LEN);
        // Fresh 12-byte IV on every call -> ciphertexts must differ.
        assert_ne!(sealed1.get(..NONCE_LEN), sealed2.get(..NONCE_LEN));
        assert_ne!(sealed1, sealed2);

        // Plaintext must not appear in ciphertext.
        assert!(
            !sealed1
                .windows(b"hello sealed".len())
                .any(|w| w == b"hello sealed")
        );

        assert_eq!(unseal(&sealed1, &key).expect("unseal1"), payload);
        assert_eq!(unseal(&sealed2, &key).expect("unseal2"), payload);

        // Wrong key rejected.
        assert!(unseal(&sealed1, &wrong_key).is_err());

        // 1-bit tamper in IV, ciphertext, or tag rejected.
        for idx in [0usize, NONCE_LEN, sealed1.len() - 1] {
            let mut tampered = sealed1.clone();
            tampered[idx] ^= 0x01;
            assert!(unseal(&tampered, &key).is_err(), "tamper at byte {idx}");
        }

        // Truncated blob rejected.
        assert!(unseal(&sealed1[..NONCE_LEN + TAG_LEN - 1], &key).is_err());

        // Decompression bomb rejected when expanded output exceeds limit.
        let bomb = vec![b'A'; 128 * 1024];
        let sealed_bomb = seal(&bomb, &key).expect("seal bomb");
        assert!(sealed_bomb.len() < 4096, "bomb compresses tiny");
        assert!(unseal_with_limit(&sealed_bomb, &key, 4096).is_err());
        assert_eq!(
            unseal_with_limit(&sealed_bomb, &key, 128 * 1024).expect("exact cap"),
            bomb
        );
    }

    #[test]
    fn ct_eq_and_link_codecs_roundtrip() {
        let key = generate_key().expect("key");
        let token = generate_key().expect("token");
        let room = generate_room_id().expect("room");
        assert!(is_valid_id(&room));
        assert!(!is_valid_id("short"));
        assert!(!is_valid_id("../etc/passwd"));
        assert!(ct_eq(&key, &key));
        assert!(!ct_eq(&key, &token));
        assert!(!ct_eq(&key, &key[..16]));

        let share = format_share_url("http://127.0.0.1:8787/", &room, &key).expect("share url");
        let (base, id, parsed_key) = parse_share_url(&share).expect("parse share");
        assert_eq!(base, "http://127.0.0.1:8787");
        assert_eq!(id, room);
        assert_eq!(parsed_key, key);

        let full = CollabLink {
            relay_base: Some("https://relay.local:8443".to_owned()),
            room: room.clone(),
            key,
            write_token: Some(token),
        };
        let url = full.to_url(None).expect("url");
        let parsed = CollabLink::parse(&url).expect("parse url");
        assert_eq!(parsed, full);
        assert!(parsed.can_write());

        let ws = parsed.to_ws_url("guest", None).expect("ws url");
        assert_eq!(ws, format!("wss://relay.local:8443/r/{room}?role=guest"));
        assert!(!ws.contains('#'));
        assert!(!ws.contains(&encode_key(&key)));
        assert!(!ws.contains(&encode_key(&token)));

        let ro = full.view_only();
        assert!(!ro.can_write());
        let ro_compact = ro.to_compact();
        let ro_parsed = CollabLink::parse(&ro_compact).expect("parse compact");
        assert_eq!(ro_parsed.room, room);
        assert_eq!(ro_parsed.key, key);
        assert_eq!(ro_parsed.write_token, None);

        // Debug output must redact raw key and write_token material.
        let dbg = format!("{full:?}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        assert!(!dbg.contains(&format!("{key:?}")), "{dbg}");
        assert!(!dbg.contains(&format!("{token:?}")), "{dbg}");
        assert!(!dbg.contains(&encode_key(&key)), "{dbg}");
        assert!(!dbg.contains(&encode_key(&token)), "{dbg}");
    }
}
