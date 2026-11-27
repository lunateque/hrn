//! FIPS 180-4 SHA-256 (`sha2`), lowercase hex, and unpadded base64url (`base64`, RFC 4648 §5)
//! used by PKCE `S256`, JWT claim reading, and subscription request shaping.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest as _, Sha256};

/// Lowercase hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        for nibble in [byte >> 4, byte & 0x0f] {
            out.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
    }
    out
}

pub(crate) fn base64url_encode(input: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(input)
}

/// Decode base64url (RFC 4648 §5), tolerating `=` padding and the standard `+/` alphabet that
/// some issuers still emit. `None` on any other byte or an impossible length.
pub(crate) fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let trimmed = input.trim_end_matches('=');
    let normalized: String = trimmed
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect();
    URL_SAFE_NO_PAD.decode(normalized.as_bytes()).ok()
}

/// FIPS 180-4 SHA-256 digest; also used by subscription request shaping in `hrn-edge-llm`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut acc, b| {
            write!(acc, "{b:02x}").expect("writing to a String cannot fail");
            acc
        })
    }

    #[test]
    fn rfc7636_s256_vector_matches() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64url_encode(&sha256(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn sha256_nist_vectors_cover_padding_boundaries() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 56 bytes: the length field no longer fits in the first block, forcing a second one.
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 64 bytes: padding spills into a whole extra block.
        assert_eq!(
            hex(&sha256(&[b'a'; 64])),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn base64url_is_unpadded_and_url_safe() {
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn base64url_decode_round_trips_and_rejects_garbage() {
        for raw in [&b""[..], b"f", b"fo", b"foo", b"foob", &[0xfb, 0xff, 0x00]] {
            assert_eq!(
                base64url_decode(&base64url_encode(raw)).as_deref(),
                Some(raw)
            );
        }
        assert_eq!(base64url_decode("Zm8=").as_deref(), Some(&b"fo"[..]));
        assert_eq!(base64url_decode("+/8").as_deref(), Some(&[0xfb, 0xff][..]));
        assert!(base64url_decode("Zm9v!").is_none());
        assert!(base64url_decode("Z").is_none());
    }
}
