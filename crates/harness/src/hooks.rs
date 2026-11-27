//! Deterministic built-in harness hooks: secret redaction, output capping, and loop guard.

use hrn_kernel::{BoxFuture, HarnessHook, HookVerdict, Result, ToolContext, ToolOutput};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const DEFAULT_OUTPUT_CAP_BYTES: usize = 24 * 1024;
/// Diffs only feed the UI; they get a larger allowance than the model-facing content.
const DIFF_CAP_FACTOR: usize = 3;
/// A call identical to its immediate predecessor is allowed this many times in a row.
const MAX_CONSECUTIVE_IDENTICAL_CALLS: usize = 3;
/// Runaway breaker per tool and turn. Deliberately generous: real sessions run `bash` or
/// `read` dozens of times; this only stops genuinely unbounded loops.
const MAX_TOOL_CALLS_PER_TURN: usize = 250;
/// Shortest token body (after the prefix) that is treated as a credential.
const MIN_TOKEN_BODY: usize = 16;

/// Token prefixes redacted when followed by at least [`MIN_TOKEN_BODY`] token characters.
const TOKEN_PREFIXES: [&str; 16] = [
    "sk-ant-",
    "sk-proj-",
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "AIza",
    "BSA",
    "AKIA",
    "hf_",
];

/// Redacts common API keys, bearer tokens, and private key blocks from tool outputs.
pub struct SecretRedactionHook;

impl SecretRedactionHook {
    /// Redact secret patterns from an arbitrary string.
    pub fn redact(input: &str) -> String {
        let mut out = redact_private_keys(input);
        for prefix in TOKEN_PREFIXES {
            out = redact_token_prefix(&out, prefix, MIN_TOKEN_BODY, is_plain_token_byte);
        }
        redact_token_prefix(&out, "Bearer ", 20, is_bearer_byte)
    }
}

/// Replace every `-----BEGIN ... PRIVATE KEY-----` block; an unterminated block (e.g. output cut
/// mid-key) is redacted to the end of the text rather than leaked.
fn redact_private_keys(input: &str) -> String {
    const END_MARK: &str = "PRIVATE KEY-----";
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("-----BEGIN ") {
        let after_begin = start + "-----BEGIN ".len();
        let Some(label_end) = rest
            .get(after_begin..)
            .and_then(|s| s.find(END_MARK))
            .filter(|&i| {
                rest.get(after_begin..after_begin + i)
                    .is_some_and(|l| !l.contains('\n'))
            })
        else {
            // Not a private-key header (e.g. a certificate): keep the marker and move on.
            out.push_str(rest.get(..after_begin).unwrap_or_default());
            rest = rest.get(after_begin..).unwrap_or_default();
            continue;
        };
        out.push_str(rest.get(..start).unwrap_or_default());
        out.push_str("[REDACTED_PRIVATE_KEY]");
        let body_start = after_begin + label_end + END_MARK.len();
        let body = rest.get(body_start..).unwrap_or_default();
        rest = match body.find("-----END ") {
            Some(end) => {
                let from_end = body.get(end + "-----END ".len()..).unwrap_or_default();
                match from_end.find("-----") {
                    Some(close) => from_end.get(close + 5..).unwrap_or_default(),
                    None => "",
                }
            }
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn is_plain_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_bearer_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
}

/// Redact `prefix` + body occurrences. The prefix must start at a token boundary, otherwise
/// ordinary words such as `disk-usage-report-2024` would match `sk-`.
fn redact_token_prefix(
    input: &str,
    prefix: &str,
    min_body: usize,
    body_byte: fn(u8) -> bool,
) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0usize;
    while let Some(rel) = input.get(cursor..).and_then(|s| s.find(prefix)) {
        let start = cursor + rel;
        let after_prefix = start + prefix.len();
        let at_boundary = start
            .checked_sub(1)
            .and_then(|i| input.as_bytes().get(i))
            .is_none_or(|b| !is_plain_token_byte(*b));
        let body_len = input
            .as_bytes()
            .get(after_prefix..)
            .unwrap_or_default()
            .iter()
            .take_while(|b| body_byte(**b))
            .count();
        if at_boundary && body_len >= min_body {
            out.push_str(input.get(cursor..start).unwrap_or_default());
            out.push_str(if prefix == "Bearer " {
                "Bearer [REDACTED]"
            } else {
                "[REDACTED]"
            });
            cursor = after_prefix + body_len;
        } else {
            out.push_str(input.get(cursor..after_prefix).unwrap_or_default());
            cursor = after_prefix;
        }
    }
    out.push_str(input.get(cursor..).unwrap_or_default());
    out
}

impl HarnessHook for SecretRedactionHook {
    fn after_tool(
        &self,
        _ctx: &ToolContext,
        _tool_name: &str,
        _arguments: &Value,
        mut output: ToolOutput,
    ) -> BoxFuture<Result<ToolOutput>> {
        output.content = Self::redact(&output.content);
        output.diff = output.diff.map(|d| Self::redact(&d));
        Box::pin(async move { Ok(output) })
    }
}

/// Caps oversized tool output: a UTF-8-safe head + tail window stays inline and the complete
/// text is spilled to a `local://N.log` the model can page with `read` selectors.
pub struct ToolOutputCapHook {
    max_bytes: usize,
}

impl ToolOutputCapHook {
    /// Create a cap hook enforcing `max_bytes` (minimum 1 KiB).
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes: max_bytes.max(1024),
        }
    }

    /// Head + tail of `text` plus a note; `spilled` is the outcome of saving the full text.
    fn cap_text(&self, text: &str, spilled: &Result<Option<String>>) -> String {
        let Some((head, omitted, tail)) = window(text, self.max_bytes) else {
            return text.to_owned();
        };
        let note = match spilled {
            Ok(Some(url)) => format!(
                "[Output truncated: {} bytes, {} lines; the middle {omitted} bytes are omitted. \
                 Full output saved to `{url}`; page it with `read`, e.g. `{url}:1-200`, \
                 `{url}:-100` (last 100 lines) or `{url}:raw:300-400`.]",
                text.len(),
                text.lines().count()
            ),
            Ok(None) => format!("[Output truncated: the middle {omitted} bytes are omitted.]"),
            Err(err) => format!(
                "[Output truncated: the middle {omitted} bytes are omitted; saving the full \
                 output failed: {err}]"
            ),
        };
        format!("{head}\n... [{omitted} bytes omitted] ...\n{tail}\n\n{note}")
    }
}

/// Split `text` into a ~60% head and ~35% tail of `max_bytes` (snapped to line starts when one
/// is close) and the number of bytes between them; `None` when it already fits.
fn window(text: &str, max_bytes: usize) -> Option<(&str, usize, &str)> {
    if text.len() <= max_bytes {
        return None;
    }
    let mut head_end = text.floor_char_boundary((max_bytes * 60) / 100);
    if let Some(nl) = text.get(..head_end).and_then(|h| h.rfind('\n'))
        && nl >= head_end / 2
    {
        head_end = nl + 1;
    }
    let tail_want = (max_bytes * 35) / 100;
    let mut tail_start = text.ceil_char_boundary(text.len().saturating_sub(tail_want));
    if let Some(nl) = text.get(tail_start..).and_then(|t| t.find('\n'))
        && nl <= tail_want / 2
    {
        tail_start += nl + 1;
    }
    let tail_start = tail_start.max(head_end);
    Some((
        text.get(..head_end).unwrap_or_default(),
        tail_start - head_end,
        text.get(tail_start..).unwrap_or_default(),
    ))
}

impl Default for ToolOutputCapHook {
    fn default() -> Self {
        Self::new(DEFAULT_OUTPUT_CAP_BYTES)
    }
}

impl HarnessHook for ToolOutputCapHook {
    fn after_tool(
        &self,
        ctx: &ToolContext,
        _tool_name: &str,
        _arguments: &Value,
        mut output: ToolOutput,
    ) -> BoxFuture<Result<ToolOutput>> {
        if output.content.len() > self.max_bytes {
            let spilled = ctx.urls.spill(ctx, &output.content);
            output.content = self.cap_text(&output.content, &spilled);
        }
        // Diffs only feed the UI: capped inline, never spilled.
        let diff_cap = self.max_bytes.saturating_mul(DIFF_CAP_FACTOR);
        output.diff = output.diff.map(|d| match window(&d, diff_cap) {
            Some((head, omitted, tail)) => {
                format!("{head}\n... [{omitted} bytes omitted] ...\n{tail}")
            }
            None => d,
        });
        Box::pin(async move { Ok(output) })
    }
}

/// Guards against runaway tool loops within a single turn. Create one per turn.
#[derive(Default)]
pub struct LoopGuardHook {
    state: Mutex<LoopState>,
}

#[derive(Default)]
struct LoopState {
    last_signature: Option<String>,
    consecutive: usize,
    per_tool: BTreeMap<String, usize>,
}

impl HarnessHook for LoopGuardHook {
    fn before_tool(
        &self,
        _ctx: &ToolContext,
        tool_name: &str,
        arguments: &Value,
    ) -> BoxFuture<Result<HookVerdict>> {
        let signature = format!("{tool_name}:{}", canonical_json(arguments));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.last_signature.as_deref() == Some(signature.as_str()) {
            state.consecutive += 1;
        } else {
            state.last_signature = Some(signature);
            state.consecutive = 1;
        }
        let consecutive = state.consecutive;
        let tool_n = {
            let n = state.per_tool.entry(tool_name.to_owned()).or_insert(0);
            *n += 1;
            *n
        };
        drop(state);
        let verdict = if consecutive > MAX_CONSECUTIVE_IDENTICAL_CALLS {
            HookVerdict::ShortCircuit(ToolOutput::err(format!(
                "loop guard: `{tool_name}` called {consecutive} times in a row with identical arguments; change approach or report the blocker"
            )))
        } else if tool_n > MAX_TOOL_CALLS_PER_TURN {
            HookVerdict::ShortCircuit(ToolOutput::err(format!(
                "loop guard: `{tool_name}` exceeded {MAX_TOOL_CALLS_PER_TURN} calls in a single turn; synthesize findings or narrow scope"
            )))
        } else {
            HookVerdict::Continue
        };
        Box::pin(async move { Ok(verdict) })
    }
}

/// Canonicalize a JSON value with lexicographically sorted object keys for deduplication.
pub fn canonical_json(val: &Value) -> String {
    canonical_value(val).to_string()
}

fn canonical_value(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, Value> =
                map.iter().map(|(k, v)| (k, canonical_value(v))).collect();
            Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonical_value).collect()),
        other => other.clone(),
    }
}

/// Build the default deterministic hook stack: redaction, then output cap, then a fresh loop guard.
///
/// Call once per turn: the loop guard keeps per-turn state.
pub fn builtin_hooks() -> Vec<Arc<dyn HarnessHook>> {
    vec![
        Arc::new(SecretRedactionHook),
        Arc::new(ToolOutputCapHook::default()),
        Arc::new(LoopGuardHook::default()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_tokens_and_caps_safely() {
        let leaked = "token=sk-ant-api03-1234567890abcdef1234567890 end";
        assert_eq!(SecretRedactionHook::redact(leaked), "token=[REDACTED] end");

        let cap = ToolOutputCapHook::new(1024);
        let big = "й".repeat(1200);
        let capped = cap.cap_text(&big, &Ok(None));
        assert!(capped.contains("bytes omitted"));
        assert!(capped.len() < big.len());
        let spilled = cap.cap_text(&big, &Ok(Some("local://3.log".to_owned())));
        assert!(spilled.contains("local://3.log:1-200"), "{spilled}");
    }

    #[test]
    fn redaction_respects_token_boundaries_and_finds_every_key_block() {
        let benign = "see disk-usage-report-20240101-final and task-abcdefghijklmnopqrstuvwxyz";
        assert_eq!(SecretRedactionHook::redact(benign), benign);

        let key = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n-----END PRIVATE KEY-----";
        let two_keys = format!("a {key} b {key} c");
        assert_eq!(
            SecretRedactionHook::redact(&two_keys),
            "a [REDACTED_PRIVATE_KEY] b [REDACTED_PRIVATE_KEY] c"
        );
        let cut = "x -----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg";
        assert_eq!(SecretRedactionHook::redact(cut), "x [REDACTED_PRIVATE_KEY]");

        let cert = "-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----";
        assert_eq!(SecretRedactionHook::redact(cert), cert);
        assert_eq!(
            SecretRedactionHook::redact(
                "Authorization: Bearer abcdefghij0123456789.XYZ_token-value"
            ),
            "Authorization: Bearer [REDACTED]"
        );
    }

    #[test]
    fn loop_guard_only_trips_on_consecutive_identical_calls() {
        use crate::testutil::hook_ctx;
        let guard = LoopGuardHook::default();
        let verdict = |name: &str, args: Value| {
            let fut = guard.before_tool(&hook_ctx(), name, &args);
            let mut fut = std::pin::pin!(fut);
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(v) => v.expect("hook ok"),
                std::task::Poll::Pending => panic!("hook future must be ready"),
            }
        };
        let tests = json!({"command": "cargo test"});
        for _ in 0..3 {
            assert!(matches!(
                verdict("bash", tests.clone()),
                HookVerdict::Continue
            ));
            // An intervening edit resets the streak: re-running tests after a fix is normal.
            assert!(matches!(
                verdict("edit", json!({"path": "a"})),
                HookVerdict::Continue
            ));
        }
        for _ in 0..3 {
            assert!(matches!(
                verdict("bash", tests.clone()),
                HookVerdict::Continue
            ));
        }
        assert!(matches!(
            verdict("bash", tests),
            HookVerdict::ShortCircuit(_)
        ));
    }
}
