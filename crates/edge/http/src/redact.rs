//! Data-driven secret redaction powered by `data/redact-patterns.txt` and `grep-regex`.

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use serde_json::Value;
use std::sync::OnceLock;

const PATTERNS_TXT: &str = include_str!("../data/redact-patterns.txt");

struct RedactRule {
    label: String,
    matcher: RegexMatcher,
}

static RULES: OnceLock<Vec<RedactRule>> = OnceLock::new();

fn rules() -> &'static [RedactRule] {
    RULES
        .get_or_init(|| compile_patterns(PATTERNS_TXT))
        .as_slice()
}

fn compile_patterns(raw: &str) -> Vec<RedactRule> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((label, pattern)) = trimmed.split_once('\t') else {
            continue;
        };
        let label = label.trim();
        let pattern = pattern.trim();
        if label.is_empty() || pattern.is_empty() {
            continue;
        }
        if let Ok(matcher) = RegexMatcherBuilder::new()
            .multi_line(true)
            .dot_matches_new_line(false)
            .build(pattern)
        {
            out.push(RedactRule {
                label: label.to_owned(),
                matcher,
            });
        }
    }
    out
}

/// Mask every known secret shape in `input` with `[REDACTED:<LABEL>]`.
pub fn redact_text(input: &str) -> String {
    let mut current = input.to_owned();
    for rule in rules() {
        current = apply_rule(&current, rule);
    }
    current
}

fn apply_rule(text: &str, rule: &RedactRule) -> String {
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let _ = rule.matcher.find_iter(bytes, |mat| {
        spans.push((mat.start(), mat.end()));
        true
    });
    if spans.is_empty() {
        return text.to_owned();
    }
    let replacement = format!("[REDACTED:{}]", rule.label);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end) in spans {
        if start < cursor || end > text.len() {
            continue;
        }
        if let Some(prefix) = text.get(cursor..start) {
            out.push_str(prefix);
        }
        out.push_str(&replacement);
        cursor = end;
    }
    if let Some(tail) = text.get(cursor..) {
        out.push_str(tail);
    }
    out
}

/// Recursively redact every string leaf inside a JSON value in place.
pub fn redact_json(val: &mut Value) {
    match val {
        Value::String(s) => {
            *s = redact_text(s);
        }
        Value::Array(items) => {
            for item in items {
                redact_json(item);
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                redact_json(v);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_masks_known_shapes() {
        assert!(!rules().is_empty(), "redact-patterns.txt must compile");
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA1234567890abcdef\n-----END RSA PRIVATE KEY-----";
        let gh = ["ghp", "1234567890abcdefghijklmnopqrstuvwxyzABCD"].join("_");
        let ghpat = [
            "github",
            "pat",
            "11ABCDEFG",
            "0123456789abcdefghijklmnopqrstuvwxyz",
        ]
        .join("_");
        let stripe = ["sk", "live", "51H7abcDEF1234567890abcdef123456"].join("_");
        let slack = ["xoxb", "123456789012", "123456789012", "abcdefghijklmnop"].join("-");
        let google = ["AIza", "SyDaGmWKa4JsXZ-HjGw7ISLn_3namBGewQe"].join("");
        let sample = format!(
            "anthropic: sk-ant-api03-abcdefghijklmnopqrstuvwx1234567890\n\
             openai: sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890\n\
             gh: {gh}\n\
             ghpat: {ghpat}\n\
             aws: AKIAIOSFODNN7EXAMPLE\n\
             aws_sec: aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
             stripe: {stripe}\n\
             slack: {slack}\n\
             google: {google}\n\
             jwt: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U\n\
             auth: Authorization: Bearer ey0123456789abcdefGHIJKLMNOPQRSTUV\n\
             url: https://alice:supersecretpass@db.example.com/app\n\
             {pem}\n\
             safe_line: fn main() {{ println!(\"hello world\"); }}\n"
        );
        let masked = redact_text(&sample);
        let leaked_stripe = ["sk", "live", "51H7"].join("_");
        let leaked_slack = ["xoxb", "123456789012"].join("-");
        for leaked in [
            "sk-ant-api03",
            "sk-proj-ABCD",
            "1234567890abcdefghij",
            "11ABCDEFG_0123456789",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI",
            leaked_stripe.as_str(),
            leaked_slack.as_str(),
            "SyDaGmWKa4",
            "eyJhbGciOiJIUzI1NiIs",
            "ey0123456789abcdef",
            "supersecretpass",
            "MIIEpAIBAAKCAQEA",
        ] {
            assert!(
                !masked.contains(leaked),
                "secret `{leaked}` leaked:\n{masked}"
            );
        }
        assert!(masked.contains("[REDACTED:ANTHROPIC_KEY]"), "{masked}");
        assert!(masked.contains("[REDACTED:OPENAI_KEY]"), "{masked}");
        assert!(masked.contains("[REDACTED:GITHUB_TOKEN]"), "{masked}");
        assert!(masked.contains("[REDACTED:AWS_ACCESS_KEY]"), "{masked}");
        assert!(masked.contains("[REDACTED:STRIPE_KEY]"), "{masked}");
        assert!(masked.contains("[REDACTED:SLACK_TOKEN]"), "{masked}");
        assert!(masked.contains("[REDACTED:PRIVATE_KEY]"), "{masked}");
        assert!(
            masked.contains("safe_line: fn main() { println!(\"hello world\"); }"),
            "benign code must survive redaction: {masked}"
        );
    }

    #[test]
    fn all_redact_patterns_compile() {
        let mut expected = 0usize;
        for (idx, line) in PATTERNS_TXT.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            expected += 1;
            let (label, pattern) = trimmed
                .split_once('\t')
                .unwrap_or_else(|| panic!("line {} missing TAB separator: `{trimmed}`", idx + 1));
            assert!(!label.trim().is_empty(), "empty label on line {}", idx + 1);
            assert!(
                !pattern.trim().is_empty(),
                "empty pattern on line {}",
                idx + 1
            );
            RegexMatcherBuilder::new()
                .multi_line(true)
                .dot_matches_new_line(false)
                .build(pattern.trim())
                .unwrap_or_else(|e| panic!("pattern `{label}` failed to compile: {e}"));
        }
        assert!(expected >= 18, "expected at least 18 redaction rules");
        assert_eq!(rules().len(), expected);
    }

    #[test]
    fn redaction_covers_prefixed_env_json_fields_and_basic_auth() {
        let sample = concat!(
            "DB_PASSWORD=supersecretdbpass\n",
            "MY_API_KEY=custom_key_9876543210\n",
            "STRIPE_SECRET=my_stripe_secret_val\n",
            "{\"api_key\": \"json_api_secret_123\", \"password\": \"json_pw_secret_456\", ",
            "\"token\": \"json_tok_secret_789\", \"secret\": \"json_sec_value_abc\", ",
            "\"aws_secret_access_key\": \"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\", ",
            "\"ok\": true, \"max_tokens\": 4096}\n",
            "Authorization: Basic dXNlcjpzZWNyZXRwYXNz\n",
            "The token is valid and password policy is documented.\n",
        );
        let masked = redact_text(sample);
        for leaked in [
            "supersecretdbpass",
            "custom_key_9876543210",
            "my_stripe_secret_val",
            "json_api_secret_123",
            "json_pw_secret_456",
            "json_tok_secret_789",
            "json_sec_value_abc",
            "wJalrXUtnFEMI",
            "dXNlcjpzZWNyZXRwYXNz",
        ] {
            assert!(
                !masked.contains(leaked),
                "secret `{leaked}` leaked:\n{masked}"
            );
        }
        assert!(masked.contains("[REDACTED:BASIC_AUTH_HEADER]"), "{masked}");
        assert!(masked.contains("[REDACTED:AWS_SECRET_ASSIGN]"), "{masked}");
        assert!(
            masked.contains("[REDACTED:GENERIC_SECRET_ASSIGN]"),
            "{masked}"
        );
        assert!(masked.contains("\"ok\": true"), "{masked}");
        assert!(masked.contains("\"max_tokens\": 4096"), "{masked}");
        assert!(
            masked.contains("The token is valid and password policy is documented."),
            "benign prose must survive redaction:\n{masked}"
        );
    }
}
