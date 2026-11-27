//! Provider-native server-side remote compaction (`POST {base}/responses/compact` for OpenAI
//! Responses and Codex backends).

use crate::codex;
use crate::common::fatal;
use crate::config::{AuthMode, ModelPreset, ProviderEntry, ProviderKind};
use crate::openai_chat::build_headers;
use crate::openai_responses::encode_input_items;
use crate::router::{LlmRouter, subscription_file};
use crate::router_pool::{is_rate_or_usage_limit, now_ms};
use crate::sub_auth::{SubscriptionAuth, session_uuid};
use bytes::Bytes;
use hrn_edge_http::{HttpClient, HttpResponse, guard};
use hrn_edge_oauth::{Subscription, read_subscription_accounts};
use hrn_kernel::{CancelFlag, HrnError, Msg, RemoteCompaction, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// Cancellation poll period while awaiting a remote compaction HTTP response.
const CANCEL_POLL: Duration = Duration::from_millis(100);
/// Default block duration (seconds) when a subscription account hits 429 without `Retry-After`.
const DEFAULT_POOL_BLOCK_SECS: u64 = 60;

/// Execute provider-native remote compaction for `model` if supported by its wire protocol.
///
/// Returns `Ok(None)` when the resolved provider is neither `ProviderKind::OpenaiResponses` nor
/// `AuthMode::Chatgpt`, or when the upstream endpoint responds with `404`/`405`/`501`.
pub(crate) async fn compact_remote(
    router: &LlmRouter,
    model: Option<&str>,
    messages: &[Msg],
    instructions: Option<&str>,
    cancel: &CancelFlag,
) -> Result<Option<RemoteCompaction>> {
    if cancel.is_cancelled() {
        return Err(HrnError::Cancelled);
    }
    let (candidates, _) = router.resolve_chain(model)?;
    let Some((_, preset)) = candidates.first() else {
        return Ok(None);
    };
    let Some(prov) = router.config.providers.get(&preset.provider) else {
        return Ok(None);
    };
    if prov.kind != ProviderKind::OpenaiResponses && prov.auth_mode != AuthMode::Chatgpt {
        return Ok(None);
    }

    let url = compact_endpoint_url(prov);
    guard::check_outbound_url(&url)?;
    let payload = encode_compact_payload(&preset.id, messages, instructions)?;
    let body = Bytes::from(serde_json::to_vec(&payload)?);
    let timeout = Duration::from_secs(prov.timeout_secs.unwrap_or(180));

    let resp = if prov.auth_mode == AuthMode::Chatgpt {
        post_chatgpt_compact(router, prov, &preset.id, &url, body, timeout, cancel).await?
    } else {
        post_static_compact(router, prov, preset, &url, body, timeout, cancel).await?
    };

    if matches!(resp.status.as_u16(), 404 | 405 | 501) {
        return Ok(None);
    }
    if !resp.status.is_success() {
        return Err(resp.status_error());
    }
    parse_compact_response(&preset.id, &resp.body).map(Some)
}

/// Resolve the compact endpoint URL for an OpenAI Responses / Codex provider.
fn compact_endpoint_url(prov: &ProviderEntry) -> String {
    let base = prov.effective_base_url();
    if prov.auth_mode == AuthMode::Chatgpt
        && guard::extract_host(base).is_some_and(|h| h.eq_ignore_ascii_case("chatgpt.com"))
        && !base.ends_with("/codex")
    {
        format!("{base}/codex/responses/compact")
    } else {
        format!("{base}/responses/compact")
    }
}

/// Build `{ model, input, instructions? }` for `POST /responses/compact`.
pub(crate) fn encode_compact_payload(
    model_id: &str,
    messages: &[Msg],
    extra_instructions: Option<&str>,
) -> Result<Value> {
    let (mut sys_parts, input) = encode_input_items(messages)?;
    if let Some(extra) = extra_instructions.map(str::trim).filter(|s| !s.is_empty()) {
        sys_parts.push(extra.to_owned());
    }
    let mut body = json!({
        "model": model_id,
        "input": input,
    });
    if !sys_parts.is_empty()
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert("instructions".into(), json!(sys_parts.join("\n\n")));
    }
    Ok(body)
}

async fn post_static_compact(
    router: &LlmRouter,
    prov: &ProviderEntry,
    preset: &ModelPreset,
    url: &str,
    body: Bytes,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<HttpResponse> {
    let token = router.resolve_static_credential(prov).await?;
    let mut headers = build_headers(prov, preset, &token, None);
    for (k, v) in &mut headers {
        if k.eq_ignore_ascii_case("accept") {
            "application/json".clone_into(v);
        }
    }
    cancellable_post_raw(&router.http, url, &headers, body, timeout, cancel).await
}

async fn post_chatgpt_compact(
    router: &LlmRouter,
    prov: &ProviderEntry,
    model_id: &str,
    url: &str,
    body: Bytes,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<HttpResponse> {
    let file = subscription_file(prov)?;
    let accounts = read_subscription_accounts(Subscription::Chatgpt, file)?;
    if accounts.is_empty() {
        return Err(HrnError::Auth(
            "not logged in to ChatGPT: run `hrn login chatgpt`".into(),
        ));
    }
    let mut last_err: Option<HrnError> = None;
    for _ in 0..accounts.len().max(1) {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        let now = now_ms();
        let usage_map = router
            .pool
            .cached_usage_map(Subscription::Chatgpt.id(), now);
        let Some(selected) = router.pool.select(
            Subscription::Chatgpt.id(),
            &accounts,
            &router.config.accounts,
            &usage_map,
            now,
        ) else {
            break;
        };
        let acct_key = selected.identity_key();
        let cred = match router
            .oauth
            .subscription_account(&router.http, Subscription::Chatgpt, file, &acct_key)
            .await
        {
            Ok(c) => c,
            Err(err) => {
                if matches!(err, HrnError::Auth(_)) {
                    router
                        .pool
                        .invalidate(Subscription::Chatgpt.id(), &acct_key);
                }
                last_err = Some(err);
                continue;
            }
        };
        let mut auth = SubscriptionAuth::new(
            &router.http,
            &router.oauth,
            Subscription::Chatgpt,
            file,
            cred,
        );
        match send_codex_compact(&mut auth, prov, model_id, url, &body, timeout, cancel).await {
            Ok(resp) => return Ok(resp),
            Err(err @ HrnError::Auth(_)) => {
                router
                    .pool
                    .invalidate(Subscription::Chatgpt.id(), &acct_key);
                last_err = Some(err);
            }
            Err(err) if is_rate_or_usage_limit(&err) => {
                let secs = err.retry_after_secs().unwrap_or(DEFAULT_POOL_BLOCK_SECS);
                let until = now_ms().saturating_add(secs.saturating_mul(1000));
                router
                    .pool
                    .mark_blocked(Subscription::Chatgpt.id(), &acct_key, until);
                last_err = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_err.unwrap_or_else(|| HrnError::Provider {
        message: "all ChatGPT accounts in pool are currently blocked".into(),
        retryable: true,
        retry_after_secs: Some(DEFAULT_POOL_BLOCK_SECS),
    }))
}

async fn send_codex_compact(
    auth: &mut SubscriptionAuth<'_>,
    prov: &ProviderEntry,
    model_id: &str,
    url: &str,
    body: &Bytes,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<HttpResponse> {
    let session = session_uuid(None);
    loop {
        let mut headers = codex::account_headers(&auth.cred)?;
        headers.extend([
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "application/json".to_owned()),
            ("session-id".to_owned(), session.clone()),
            ("x-client-request-id".to_owned(), session.clone()),
            (
                "x-codex-routing-hint".to_owned(),
                format!("model={model_id}"),
            ),
        ]);
        for (name, value) in prov.resolve_env_headers() {
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name)) {
                headers.push((name, value));
            }
        }
        let resp =
            cancellable_post_raw(auth.http, url, &headers, body.clone(), timeout, cancel).await?;
        if resp.status.is_success() || matches!(resp.status.as_u16(), 404 | 405 | 501) {
            return Ok(resp);
        }
        if resp.status.as_u16() == 401 && auth.recover_unauthorized().await? {
            continue;
        }
        return Err(codex::reply_error(
            &resp,
            auth.cred.plan.as_deref(),
            codex::now_secs(),
        ));
    }
}

async fn cancellable_post_raw(
    http: &HttpClient,
    url: &str,
    headers: &[(String, String)],
    body: Bytes,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<HttpResponse> {
    if cancel.is_cancelled() {
        return Err(HrnError::Cancelled);
    }
    let fut = http.post_raw(url, headers, body, timeout);
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval(CANCEL_POLL);
    loop {
        tokio::select! {
            res = &mut fut => return res,
            _ = ticker.tick() => {
                if cancel.is_cancelled() {
                    return Err(HrnError::Cancelled);
                }
            }
        }
    }
}

/// Parse a `/responses/compact` JSON payload into [`RemoteCompaction`].
pub(crate) fn parse_compact_response(model_id: &str, raw_body: &[u8]) -> Result<RemoteCompaction> {
    let val: Value = serde_json::from_slice(raw_body)
        .map_err(|e| fatal(format!("remote compaction returned invalid JSON: {e}")))?;

    let items: Vec<Value> = if let Some(arr) = val
        .get("output")
        .or_else(|| val.get("items"))
        .and_then(Value::as_array)
    {
        arr.clone()
    } else if val.get("encrypted_content").is_some() {
        vec![val.clone()]
    } else {
        Vec::new()
    };

    if items.is_empty() {
        return Err(fatal(
            "remote compaction response contained no output items",
        ));
    }

    let summary_text = extract_summary_text(&val, &items).unwrap_or_else(|| {
        let count = items.len();
        format!(
            "Remote compaction preserved {count} opaque state item{} via {model_id}.",
            if count == 1 { "" } else { "s" }
        )
    });

    let preserve = json!({
        "provider": "openai_responses",
        "model": model_id,
        "output": items,
    });

    Ok(RemoteCompaction {
        summary_text,
        preserve,
    })
}

fn extract_summary_text(root: &Value, items: &[Value]) -> Option<String> {
    for key in ["summary", "output_text", "text"] {
        if let Some(s) = root.get(key).and_then(Value::as_str)
            && !s.trim().is_empty()
        {
            return Some(s.trim().to_owned());
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        if let Some(s) = item.get("summary").and_then(Value::as_str)
            && !s.trim().is_empty()
        {
            parts.push(s.trim().to_owned());
        }
        if let Some(content) = item.get("content") {
            collect_content_text(content, &mut parts);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn collect_content_text(content: &Value, out: &mut Vec<String>) {
    match content {
        Value::String(s) if !s.trim().is_empty() => out.push(s.trim().to_owned()),
        Value::Array(arr) => {
            for part in arr {
                if let Some(text) = part.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    out.push(text.trim().to_owned());
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::{http_reply, serve};
    use crate::config::LlmConfig;
    use hrn_edge_oauth::OAuthManager;
    use hrn_kernel::LlmProvider;

    #[tokio::test]
    async fn non_responses_providers_return_none() {
        let mut cfg = LlmConfig::default();
        cfg.providers.insert(
            "anth".into(),
            ProviderEntry {
                kind: ProviderKind::Anthropic,
                ..ProviderEntry::default()
            },
        );
        cfg.presets.insert(
            "claude".into(),
            ModelPreset {
                provider: "anth".into(),
                id: "claude-sonnet-4-6".into(),
                ..ModelPreset::default()
            },
        );
        let router = LlmRouter::try_new(
            cfg,
            HttpClient::new().expect("http"),
            OAuthManager::default(),
        )
        .expect("router");
        let res = router
            .compact_remote(Some("claude"), &[Msg::user("hi")], None, &CancelFlag::new())
            .await
            .expect("ok");
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn openai_responses_compact_parses_opaque_and_text_items() {
        let body = r#"{
            "id": "cmp_resp_1",
            "object": "response.compaction",
            "output": [
                {"type": "compaction", "id": "cmp_1", "encrypted_content": "enc_opaque_blob"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Compacted state summary"}]}
            ]
        }"#;
        let (base, seen) = serve(1, move |_, _| {
            http_reply("200 OK", "application/json", body)
        })
        .await;

        let token_path =
            std::env::temp_dir().join(format!("hrn-remote-compact-tok-{}.txt", std::process::id()));
        std::fs::write(&token_path, "test-openai-key").expect("token file");

        let mut cfg = LlmConfig::default();
        cfg.providers.insert(
            "openai".into(),
            ProviderEntry {
                kind: ProviderKind::OpenaiResponses,
                base_url: Some(base),
                token_file: Some(token_path.to_string_lossy().into_owned()),
                ..ProviderEntry::default()
            },
        );
        cfg.presets.insert(
            "codex".into(),
            ModelPreset {
                provider: "openai".into(),
                id: "gpt-5.3-codex".into(),
                ..ModelPreset::default()
            },
        );
        let router = LlmRouter::try_new(
            cfg,
            HttpClient::new().expect("http"),
            OAuthManager::default(),
        )
        .expect("router");
        let result = router
            .compact_remote(
                Some("codex"),
                &[Msg::user("earlier turn"), Msg::assistant("earlier reply")],
                Some("keep file paths"),
                &CancelFlag::new(),
            )
            .await
            .expect("compact ok")
            .expect("some compaction");

        assert_eq!(result.summary_text, "Compacted state summary");
        assert_eq!(
            result.preserve["output"][0]["encrypted_content"],
            "enc_opaque_blob"
        );
        let heads = seen.lock().expect("seen");
        assert!(heads[0].starts_with("POST /responses/compact HTTP/1.1"));
        let _ = std::fs::remove_file(&token_path);
    }
}
