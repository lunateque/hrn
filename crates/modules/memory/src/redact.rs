//! Scrub recognisable credentials before a memory is written: the database outlives the
//! session, so a pasted token must not become a permanent, recalled fact.

const MASK: &str = "[redacted]";
const PEM_BEGIN: &str = "-----BEGIN ";
const PEM_END: &str = "-----END ";
const PEM_RULE: &str = "-----";
/// Word prefixes of well-known credential formats.
const TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "AIza",
    "npm_",
];
/// Shortest word that can be a real credential with one of the prefixes above.
const MIN_TOKEN_LEN: usize = 20;
/// Key names whose `key=value` value is a secret.
const SECRET_KEYS: &[&str] = &[
    "token", "secret", "password", "passwd", "apikey", "api_key", "api-key", "auth",
];
/// Shortest value worth masking after a secret key (shorter ones are flags like `auth=none`).
const MIN_VALUE_LEN: usize = 8;

/// `text` with private-key blocks, known token formats and `secret_key=value` values masked.
pub(crate) fn redact(text: &str) -> String {
    let text = mask_private_keys(text);
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

fn flush(word: &mut String, out: &mut String) {
    if !word.is_empty() {
        out.push_str(&mask_word(word));
        word.clear();
    }
}

fn mask_private_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(PEM_BEGIN) {
        out.push_str(rest.get(..start).unwrap_or_default());
        let tail = rest.get(start..).unwrap_or_default();
        if let Some(len) = private_key_len(tail) {
            out.push_str(MASK);
            rest = tail.get(len..).unwrap_or_default();
        } else {
            out.push_str(PEM_BEGIN);
            rest = tail.get(PEM_BEGIN.len()..).unwrap_or_default();
        }
    }
    out.push_str(rest);
    out
}

/// Byte length of the PEM private-key block that starts `tail`, if it is one.
fn private_key_len(tail: &str) -> Option<usize> {
    let after_end = tail.find(PEM_END)? + PEM_END.len();
    let len = after_end + tail.get(after_end..)?.find(PEM_RULE)? + PEM_RULE.len();
    tail.get(..len)?.contains("PRIVATE KEY").then_some(len)
}

fn mask_word(word: &str) -> String {
    let trimmed =
        word.trim_matches(|c: char| matches!(c, ',' | '.' | ';' | '"' | '\'' | '(' | ')'));
    if trimmed.is_empty() {
        return word.to_owned();
    }
    if is_token(trimmed) {
        return word.replacen(trimmed, MASK, 1);
    }
    if let Some((key, value)) = trimmed.split_once(['=', ':'])
        && value.len() >= MIN_VALUE_LEN
        && SECRET_KEYS
            .iter()
            .any(|k| key.to_ascii_lowercase().contains(k))
    {
        return word.replacen(value, MASK, 1);
    }
    word.to_owned()
}

fn is_token(word: &str) -> bool {
    if word.len() < MIN_TOKEN_LEN {
        return false;
    }
    let aws = (word.starts_with("AKIA") || word.starts_with("ASIA"))
        && word.len() == 20
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    // A JWT: three dot-separated base64url parts, the first being `{"…` encoded.
    let jwt = word.starts_with("eyJ") && word.matches('.').count() == 2;
    aws || jwt || TOKEN_PREFIXES.iter().any(|p| word.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn masks_credentials_and_keeps_prose() {
        // Built at run time so no credential-shaped literal sits in the source.
        let api = format!("sk-{}", "x".repeat(24));
        let aws = format!("AKIA{}", "7".repeat(16));
        let (body, rule) = ("q".repeat(20), "-".repeat(5));
        let pem =
            format!("{rule}BEGIN RSA PRIVATE KEY{rule}\n{body}\n{rule}END RSA PRIVATE KEY{rule}");
        let text =
            format!("use {api} (and API_TOKEN=hunter2hunter2) then {pem}.\nline 2: {aws} ok");
        assert_eq!(
            redact(&text),
            "use [redacted] (and API_TOKEN=[redacted]) then [redacted].\nline 2: [redacted] ok"
        );
        let prose = "deploys go through `just ci`; auth=none is fine";
        assert_eq!(redact(prose), prose);
    }
}
