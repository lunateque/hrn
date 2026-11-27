//! `ChatGPT` Plus/Pro subscription transport (the Codex backend), mirrored from oh-my-pi:
//! `/codex/responses` with the Codex request shape, `ChatGPT` usage-limit messages and one 401
//! recovery per request.

use crate::common::fatal;
use crate::config::ProviderEntry;
use crate::openai_responses::{encode_request, read_stream};
use crate::sub_auth::{SubscriptionAuth, session_uuid};
use bytes::Bytes;
use hrn_edge_http::{HttpResponse, StreamReply};
use hrn_edge_oauth::SubscriptionCredential;
use hrn_kernel::{CancelFlag, ChunkSink, HrnError, LlmRequest, LlmResponse, Result, Role};
use serde_json::{Map, Value, json};
use std::time::Duration;

/// Codex CLI version presented as `version`.
const CLIENT_VERSION: &str = "0.169.0";
const ORIGINATOR: &str = "hrn";
const USER_AGENT: &str = concat!("hrn/", env!("CARGO_PKG_VERSION"));
/// Routing fields the backend reads before `input`.
const LEADING_KEYS: [&str; 5] = [
    "type",
    "model",
    "stream",
    "service_tier",
    "previous_response_id",
];
/// The backend rejects sampling parameters and output caps with 400 `Unsupported parameter`.
const UNSUPPORTED_KEYS: [&str; 5] = [
    "temperature",
    "top_p",
    "max_output_tokens",
    "max_completion_tokens",
    "instructions",
];

/// Stream one request with the logged-in `ChatGPT` subscription.
pub(crate) async fn stream(
    auth: &mut SubscriptionAuth<'_>,
    provider: &ProviderEntry,
    model_id: &str,
    req: &LlmRequest,
    cancel: &CancelFlag,
    sink: &dyn ChunkSink,
) -> Result<LlmResponse> {
    let url = format!("{}/codex/responses", provider.effective_base_url());
    let session = session_uuid(req.session_id.as_deref());
    let body = Bytes::from(encode_body(model_id, req)?);
    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(180));
    loop {
        let mut headers = account_headers(&auth.cred)?;
        headers.extend([
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            ("session-id".to_owned(), session.clone()),
            ("x-client-request-id".to_owned(), session.clone()),
            (
                "x-codex-routing-hint".to_owned(),
                format!("model={model_id}"),
            ),
        ]);
        for (name, value) in provider.resolve_env_headers() {
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name)) {
                headers.push((name, value));
            }
        }
        let reply = crate::common::post_stream_cancellable(
            auth.http,
            &url,
            &headers,
            body.clone(),
            timeout,
            cancel,
        )
        .await?;
        let failed = match reply {
            StreamReply::Stream(mut sse) => {
                return read_stream(&mut sse, model_id, cancel, sink).await;
            }
            StreamReply::Failed(failed) => failed,
        };
        if failed.status.as_u16() == 401 && auth.recover_unauthorized().await? {
            continue;
        }
        return Err(reply_error(&failed, auth.cred.plan.as_deref(), now_secs()));
    }
}

/// Identity headers every Codex backend call carries.
pub(crate) fn account_headers(cred: &SubscriptionCredential) -> Result<Vec<(String, String)>> {
    let account = cred
        .chatgpt_account_id
        .as_deref()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            HrnError::Auth("the ChatGPT login has no account id: run `hrn login chatgpt`".into())
        })?;
    let mut headers = vec![
        (
            "authorization".to_owned(),
            format!("Bearer {}", cred.access),
        ),
        ("chatgpt-account-id".to_owned(), account.to_owned()),
        ("originator".to_owned(), ORIGINATOR.to_owned()),
        ("version".to_owned(), CLIENT_VERSION.to_owned()),
        ("user-agent".to_owned(), USER_AGENT.to_owned()),
    ];
    if let Some(residency) = cred
        .residency
        .as_deref()
        .filter(|r| !r.is_empty() && *r != "no_constraint")
    {
        headers.push((
            "x-openai-internal-codex-residency".to_owned(),
            residency.to_owned(),
        ));
    }
    Ok(headers)
}

/// The Codex request body: system prompts become leading developer items (the first tagged as
/// the base instructions), unsupported parameters are dropped, call ids sanitized, encrypted
/// reasoning requested and the routing fields serialized first.
pub(crate) fn encode_body(model_id: &str, req: &LlmRequest) -> Result<Vec<u8>> {
    let mut wire = encode_request(model_id, req, true)?;
    let Some(obj) = wire.as_object_mut() else {
        return Err(fatal("request body is not an object"));
    };
    for key in UNSUPPORTED_KEYS {
        obj.remove(key);
    }
    obj.insert("store".into(), Value::Bool(false));
    obj.insert("stream".into(), Value::Bool(true));
    obj.insert("include".into(), json!(["reasoning.encrypted_content"]));

    let mut turns = match obj.remove("input") {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    for item in &mut turns {
        sanitize_call_id(item);
    }
    let instructions: Vec<&str> = req
        .messages
        .iter()
        .filter(|m| m.role == Role::System && !m.content.trim().is_empty())
        .map(|m| m.content.as_str())
        .collect();
    let mut input = Vec::with_capacity(instructions.len() + turns.len() + 1);
    for (i, text) in instructions.iter().enumerate() {
        input.push(developer_item(text, i == 0));
    }
    // The backend needs one non-developer item: echo the last instruction as the user turn.
    if turns.is_empty()
        && let Some(last) = instructions.last()
    {
        turns.push(json!({"role": "user", "content": last}));
    }
    input.append(&mut turns);
    obj.insert("input".into(), Value::Array(input));
    write_ordered(obj)
}

fn developer_item(text: &str, base_instructions: bool) -> Value {
    let mut item = json!({
        "type": "message",
        "role": "developer",
        "content": [{"type": "input_text", "text": text}],
    });
    if base_instructions && let Some(obj) = item.as_object_mut() {
        obj.insert(
            "internal_chat_message_metadata_passthrough".into(),
            json!({"content_item_kinds": ["model.base_instructions"]}),
        );
    }
    item
}

/// Call ids must be at most 64 characters of `[A-Za-z0-9_-]`.
fn sanitize_call_id(item: &mut Value) {
    let ok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if let Some(Value::String(id)) = item.get_mut("call_id")
        && (id.len() > 64 || !id.chars().all(ok))
    {
        *id = id
            .chars()
            .map(|c| if ok(c) { c } else { '_' })
            .take(64)
            .collect();
    }
}

/// Serialize `obj` with [`LEADING_KEYS`] first, then the rest in map order.
fn write_ordered(obj: &Map<String, Value>) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(16 * 1024);
    out.push(b'{');
    let leading = LEADING_KEYS.iter().filter_map(|k| obj.get_key_value(*k));
    let rest = obj
        .iter()
        .filter(|(k, _)| !LEADING_KEYS.contains(&k.as_str()));
    for (i, (key, value)) in leading.chain(rest).enumerate() {
        if i > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, key)?;
        out.push(b':');
        serde_json::to_writer(&mut out, value)?;
    }
    out.push(b'}');
    Ok(out)
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Map a failed Codex reply: usage and rate limits get `ChatGPT`'s own wording and stay
/// retryable (with the reset time) so a fallback chain can take over.
pub(crate) fn reply_error(reply: &HttpResponse, plan: Option<&str>, now_secs: u64) -> HrnError {
    let parsed: Value = serde_json::from_slice(&reply.body).unwrap_or_default();
    let err = parsed.get("error").unwrap_or(&Value::Null);
    let code = ["code", "type"]
        .iter()
        .find_map(|k| err.get(*k).and_then(Value::as_str))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let wait = reset_secs(err, reply, now_secs);
    let try_again = wait
        .map(|s| format!(" Try again in ~{} min.", s.div_ceil(60).max(1)))
        .unwrap_or_default();
    let message = if code.contains("usage_limit_reached") || code.contains("usage_not_included") {
        let plan = plan.map(|p| format!(" ({p} plan)")).unwrap_or_default();
        format!("You have hit your ChatGPT usage limit{plan}.{try_again}")
    } else if code.contains("rate_limit_exceeded") || reply.status.as_u16() == 429 {
        format!("ChatGPT rate limit exceeded.{try_again}")
    } else {
        return reply.status_error();
    };
    HrnError::Provider {
        message,
        retryable: true,
        retry_after_secs: wait.or_else(|| reply.retry_after_secs()),
    }
}

/// Seconds until the exhausted window resets: the body's `resets_in_seconds` / `resets_at`, else
/// the `x-codex-{primary,secondary}-reset-at` header of a window used to 100 %.
fn reset_secs(err: &Value, reply: &HttpResponse, now_secs: u64) -> Option<u64> {
    if let Some(secs) = err.get("resets_in_seconds").and_then(Value::as_u64) {
        return Some(secs);
    }
    if let Some(at) = err.get("resets_at").and_then(Value::as_u64) {
        return Some(at.saturating_sub(now_secs));
    }
    let header = |name: String| reply.headers.get(name)?.to_str().ok();
    ["primary", "secondary"]
        .iter()
        .filter_map(|window| {
            let used: f64 = header(format!("x-codex-{window}-used-percent"))?
                .trim()
                .parse()
                .ok()?;
            if used < 100.0 {
                return None;
            }
            let at: u64 = header(format!("x-codex-{window}-reset-at"))?
                .trim()
                .parse()
                .ok()?;
            Some(at.saturating_sub(now_secs))
        })
        .max()
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;
