//! Error sanitization for MCP client and `mcp://` URL surfaces so auth headers, bearer tokens,
//! and configured secret values are never leaked back to the model or user logs.

/// Maximum characters preserved in a sanitized MCP error message.
const MAX_ERROR_CHARS: usize = 512;

/// Sanitize an MCP error message so auth headers, bearer tokens, and raw secret values are never
/// leaked back to the model or user logs.
#[must_use]
pub fn sanitize_mcp_error(raw: &str, secrets: &[String]) -> String {
    let mut cleaned: String = raw
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    for secret in secrets {
        let trimmed = secret.trim();
        if trimmed.len() >= 6 {
            cleaned = cleaned.replace(trimmed, "<redacted>");
        }
    }
    cleaned = redact_auth_schemes(&cleaned);
    cleaned = redact_sensitive_headers(&cleaned);
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_ERROR_CHARS {
        collapsed
    } else {
        let clipped: String = collapsed.chars().take(MAX_ERROR_CHARS).collect();
        format!("{clipped}…")
    }
}

fn redact_auth_schemes(input: &str) -> String {
    const WRAP: &[char] = &['"', '\'', '(', ')', '[', ']', '{', '}', ',', ';'];
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for token in input.split_whitespace() {
        if redact_next {
            let tail_start = token.trim_end_matches(WRAP).len();
            let tail = token.get(tail_start..).unwrap_or_default();
            out.push(format!("<redacted>{tail}"));
            redact_next = false;
            continue;
        }
        let core = token.trim_matches(WRAP);
        let lower = core.to_ascii_lowercase();
        if lower == "bearer" || lower == "basic" {
            out.push(token.to_owned());
            redact_next = true;
        } else if lower.starts_with("bearer:")
            || lower.starts_with("basic:")
            || is_inline_secret_param(&lower)
        {
            let tail_start = token.trim_end_matches(WRAP).len();
            let tail = token.get(tail_start..).unwrap_or_default();
            out.push(format!("<redacted>{tail}"));
        } else {
            out.push(token.to_owned());
        }
    }
    out.join(" ")
}

fn is_inline_secret_param(lower: &str) -> bool {
    for prefix in ["token=", "api_key=", "apikey=", "secret=", "password="] {
        if lower.contains(prefix) {
            return true;
        }
    }
    lower.starts_with("sk-") || lower.starts_with("ghp_") || lower.starts_with("gho_")
}

fn redact_sensitive_headers(input: &str) -> String {
    const HEADERS: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "x-api-key",
        "api-key",
        "x-auth-token",
        "cookie",
        "set-cookie",
    ];
    let mut cur = input.to_owned();
    for header in HEADERS {
        let mut search_from = 0usize;
        while search_from < cur.len() {
            let lower = cur.to_ascii_lowercase();
            let Some(rel) = lower.get(search_from..).and_then(|s| s.find(header)) else {
                break;
            };
            let pos = search_from + rel;
            let prev_ok = pos == 0
                || cur
                    .get(..pos)
                    .and_then(|s| s.chars().next_back())
                    .is_some_and(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_')));
            let after_name = pos + header.len();
            let Some(colon_end) = header_colon_end(&cur, after_name).filter(|_| prev_ok) else {
                search_from = after_name;
                continue;
            };
            let rest = cur.get(colon_end..).unwrap_or_default();
            let trimmed_start = colon_end + (rest.len() - rest.trim_start().len());
            let after = cur.get(trimmed_start..).unwrap_or_default();
            let (replacement, val_end) = redacted_header_span(after, trimmed_start, cur.len());
            if after.starts_with(replacement) {
                search_from = trimmed_start + replacement.len();
                continue;
            }
            let mut next = String::with_capacity(cur.len());
            next.push_str(cur.get(..colon_end).unwrap_or_default());
            next.push(' ');
            next.push_str(replacement);
            next.push_str(cur.get(val_end..).unwrap_or_default());
            search_from = colon_end + 1 + replacement.len();
            cur = next;
        }
    }
    cur
}

/// Match optional closing quote (`"` / `'`), optional spaces, then `:`.
fn header_colon_end(text: &str, mut idx: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if matches!(bytes.get(idx), Some(b'"' | b'\'')) {
        idx += 1;
    }
    while bytes.get(idx) == Some(&b' ') {
        idx += 1;
    }
    (bytes.get(idx) == Some(&b':')).then_some(idx + 1)
}

/// Compute `(replacement, end_offset)` for a header value starting at `trimmed_start`, handling
/// quoted (`"..."`, `'...'`), parenthesized (`(...)`), and bare values.
fn redacted_header_span(
    after: &str,
    trimmed_start: usize,
    total_len: usize,
) -> (&'static str, usize) {
    let Some(first) = after.as_bytes().first().copied() else {
        return ("<redacted>", total_len);
    };
    let (close, repl) = match first {
        b'"' => (b'"', "\"<redacted>\""),
        b'\'' => (b'\'', "'<redacted>'"),
        b'(' => (b')', "(<redacted>)"),
        _ => {
            let end = after
                .find([',', ';', '"', '\'', '}', ')', '\n'])
                .map_or(total_len, |off| trimmed_start + off);
            return ("<redacted>", end);
        }
    };
    let bytes = after.as_bytes();
    let mut i = 1usize;
    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else { break };
        if b == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if b == close {
            return (repl, trimmed_start + i + 1);
        }
        i += 1;
    }
    (repl, total_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_mcp_error_redacts_quoted_and_json_headers() {
        for (raw, secret) in [
            (
                "Authorization: \"Bearer eyJsecret_jwt_123\"",
                "eyJsecret_jwt_123",
            ),
            (
                "Cookie: 'sid=super_secret_cookie; path=/'",
                "super_secret_cookie",
            ),
            ("X-Api-Key: \"abc123456secret\"", "abc123456secret"),
            (
                "{\"Authorization\": \"Bearer json_secret_999\", \"status\": 401}",
                "json_secret_999",
            ),
            (
                "Authorization: (Basic dXNlcjpwYXNzMTIz)",
                "dXNlcjpwYXNzMTIz",
            ),
            (
                "rejected \"Bearer standalone_tok_777\"",
                "standalone_tok_777",
            ),
            ("failed with x-api-key: raw_key_xyz, retry", "raw_key_xyz"),
        ] {
            let cleaned = sanitize_mcp_error(raw, &[]);
            assert!(
                !cleaned.contains(secret),
                "leaked `{secret}` in `{cleaned}`"
            );
            assert!(
                cleaned.contains("<redacted>"),
                "missing redaction in `{cleaned}`"
            );
        }
        let json_out = sanitize_mcp_error(
            "{\"Authorization\": \"Bearer tok_abc\", \"status\": 401}",
            &[],
        );
        assert!(json_out.contains("\"status\": 401"), "{json_out}");
    }
}
