//! `file_read`: one file of a GitHub repository through the contents API.

use crate::args;
use crate::gh::{Gh, reason};
use crate::urls::api_args;
use hrn_kernel::serde_json::Value;
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::fmt::Write as _;
use std::path::Path;

/// Percent-encode one path segment (RFC 3986 unreserved characters stay).
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// RFC 4648 base64 decoding; whitespace (GitHub wraps lines) is skipped.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in text.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

/// Read `path` at `branch` (default branch when omitted) of `repo` (the checkout's when omitted).
pub(crate) async fn read(gh: &Gh, a: &Value, cwd: &Path, cancel: &CancelFlag) -> Result<String> {
    let path = args::required(a, "path", "file_read")?;
    if path.starts_with('/') {
        return Err(HrnError::Tool(
            "`path` is repository-relative; remove the leading `/`".to_owned(),
        ));
    }
    let repo = args::opt(a, "repo");
    let branch = args::opt(a, "branch");
    let encoded = path
        .split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/");
    let fetch = |accept: &str| {
        let mut cmd = api_args(repo, false, |slug| {
            format!("repos/{slug}/contents/{encoded}")
        });
        cmd.extend(
            ["-X", "GET", "-H", accept, "-H", "Accept-Encoding: identity"].map(str::to_owned),
        );
        if let Some(branch) = branch {
            cmd.extend(["-f".to_owned(), format!("ref={branch}")]);
        }
        cmd
    };
    let wrap = |e: HrnError| {
        HrnError::Tool(format!(
            "cannot read `{path}` at {} in {}: {}",
            branch.unwrap_or("the default branch"),
            repo.unwrap_or("the current repository"),
            reason(&e)
        ))
    };
    let meta = gh
        .json(cwd, fetch("Accept: application/vnd.github+json"), cancel)
        .await
        .map_err(wrap)?;
    if meta.is_array() {
        return Err(HrnError::Tool(format!(
            "`{path}` is a directory, not a file"
        )));
    }
    let kind = meta.get("type").and_then(Value::as_str).unwrap_or_default();
    if kind != "file" {
        return Err(HrnError::Tool(format!(
            "`{path}` is a {kind}, not a regular file"
        )));
    }
    let encoded_body = meta
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let bytes = if encoded_body.is_empty() {
        // Past 1 MB the contents API sends no body; the raw media type does.
        let raw = fetch("Accept: application/vnd.github.raw+json");
        let out = gh.exec(cwd, &raw, cancel).await.map_err(wrap)?;
        if out.code != Some(0) {
            return Err(wrap(out.failure(&raw, "gh")));
        }
        out.stdout.into_bytes()
    } else {
        base64_decode(encoded_body)
            .ok_or_else(|| HrnError::Tool(format!("GitHub returned invalid base64 for `{path}`")))?
    };
    let url = meta
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(text),
        Ok(text) => Ok(format!(
            "[binary file `{path}` ({} bytes); not shown]\n{url}",
            text.len()
        )),
        Err(e) => Ok(format!(
            "[binary file `{path}` ({} bytes); not shown]\n{url}",
            e.as_bytes().len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_vectors() {
        for (enc, dec) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v\nYmFy", "foobar"),
        ] {
            assert_eq!(base64_decode(enc).expect("valid"), dec.as_bytes());
        }
        assert_eq!(base64_decode("a$b"), None);
    }

    #[test]
    fn segments_are_encoded_individually() {
        assert_eq!(encode_segment("a b#c.rs"), "a%20b%23c.rs");
        assert_eq!(encode_segment("é"), "%C3%A9");
    }
}
