//! Claude Pro/Max subscription transport: `/v1/messages?beta=true` with the Claude Code request
//! shape, one 401 recovery and one `claude_code_version_too_old` self-heal per request.

use crate::anthropic::read_stream;
use crate::claude_code::{self, VERSION_ENV};
use crate::config::ProviderEntry;
use crate::sub_auth::{SubscriptionAuth, session_uuid};
use bytes::Bytes;
use hrn_edge_http::StreamReply;
use hrn_kernel::{CancelFlag, ChunkSink, LlmRequest, LlmResponse, Result};
use std::time::Duration;

/// Next to the credentials: the Claude Code version a rejection required.
const VERSION_FILE: &str = "claude-code-version.txt";

/// Stream one request with the logged-in Claude subscription.
pub(crate) async fn stream(
    auth: &mut SubscriptionAuth<'_>,
    provider: &ProviderEntry,
    model_id: &str,
    req: &LlmRequest,
    slow_mode: bool,
    cancel: &CancelFlag,
    sink: &dyn ChunkSink,
) -> Result<LlmResponse> {
    let url = format!("{}/v1/messages?beta=true", provider.effective_base_url());
    let store = auth.file.with_file_name(VERSION_FILE);
    let mut version = claude_code::resolve_version(std::env::var(VERSION_ENV).ok(), &store);
    let session = session_uuid(req.session_id.as_deref());
    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(180));
    let mut version_healed = false;
    loop {
        let shaped = claude_code::encode(
            model_id,
            req,
            &auth.cred,
            &session,
            &version,
            provider.strip_temperature,
        )?;
        let mut headers =
            claude_code::headers(&version, &auth.cred.access, &session, &shaped.betas);
        if slow_mode {
            headers.push(("anthropic-slow-mode".to_owned(), "true".to_owned()));
        }
        // Claude Code's own headers win over configured extras.
        for (name, value) in provider.resolve_env_headers() {
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name)) {
                headers.push((name, value));
            }
        }
        let reply = crate::common::post_stream_cancellable(
            auth.http,
            &url,
            &headers,
            Bytes::from(shaped.body),
            timeout,
            cancel,
        )
        .await?;
        let failed = match reply {
            StreamReply::Stream(mut sse) => {
                return read_stream(&mut sse, model_id, true, cancel, sink).await;
            }
            StreamReply::Failed(failed) => failed,
        };
        if failed.status.as_u16() == 401 && auth.recover_unauthorized().await? {
            continue;
        }
        if !version_healed
            && let Some(required) = claude_code::required_version(&failed.text(), &version)
        {
            version_healed = true;
            // Best effort: the retry uses the new version even if it cannot be remembered.
            let _ = tokio::fs::write(&store, &required).await;
            version = required;
            continue;
        }
        return Err(failed.status_error());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::{VecSink, header, http_reply, serve};
    use crate::config::{AuthMode, ProviderKind};
    use hrn_edge_http::HttpClient;
    use hrn_edge_oauth::{OAuthManager, Subscription, SubscriptionCredential};
    use hrn_kernel::{HrnError, Msg};
    use std::path::{Path, PathBuf};

    const SSE_OK: &str = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    const UNAUTHORIZED: &str =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid bearer"}}"#;

    fn login_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hrn-claude-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir.join("claude.json")
    }

    fn store_login(file: &Path, access: &str) {
        let cred = SubscriptionCredential {
            provider: "claude".into(),
            access: access.into(),
            refresh: "rt".into(),
            expires: u64::MAX / 2,
            account_id: Some("acct-1".into()),
            ..SubscriptionCredential::default()
        };
        std::fs::write(file, serde_json::to_vec(&cred).expect("json")).expect("write");
    }

    fn provider(base: &str, file: &Path) -> ProviderEntry {
        ProviderEntry {
            kind: ProviderKind::Anthropic,
            base_url: Some(base.into()),
            auth_mode: AuthMode::Claude,
            token_file: Some(file.to_string_lossy().into_owned()),
            ..ProviderEntry::default()
        }
    }

    async fn run(base: &str, file: &Path) -> Result<LlmResponse> {
        let http = HttpClient::new().expect("http");
        let oauth = OAuthManager::default();
        let cred = oauth
            .subscription(&http, Subscription::Claude, file)
            .await
            .expect("login");
        let mut auth = SubscriptionAuth::new(&http, &oauth, Subscription::Claude, file, cred);
        let req = LlmRequest {
            model: None,
            messages: vec![Msg::user("hello there")],
            tools: Vec::new(),
            thinking: hrn_kernel::ThinkingLevel::Off,
            thinking_budgets: None,
            temperature: None,
            max_tokens: None,
            session_id: None,
            service_tier: None,
        };
        let prov = provider(base, file);
        let sink = VecSink::default();
        stream(
            &mut auth,
            &prov,
            "claude-x",
            &req,
            false,
            &CancelFlag::new(),
            &sink,
        )
        .await
    }

    #[tokio::test]
    async fn unauthorized_adopts_the_refreshed_login_and_retries_once() {
        let file = login_file("401");
        store_login(&file, "stale");
        let rotated = file.clone();
        let (base, seen) = serve(2, move |i, _| {
            if i == 0 {
                // Another hrn refreshed the login meanwhile.
                store_login(&rotated, "fresh");
                http_reply("401 Unauthorized", "application/json", UNAUTHORIZED)
            } else {
                http_reply("200 OK", "text/event-stream", SSE_OK)
            }
        })
        .await;
        let resp = run(&base, &file).await.expect("retried");
        assert_eq!(resp.content, "ok");
        let seen = seen.lock().expect("seen").clone();
        let auths: Vec<&str> = seen
            .iter()
            .filter_map(|h| header(h, "authorization"))
            .collect();
        assert_eq!(auths, ["Bearer stale", "Bearer fresh"]);
        assert!(seen[0].starts_with("POST /v1/messages?beta=true HTTP/1.1"));
        assert_eq!(header(&seen[0], "x-app"), Some("cli"));
        assert!(header(&seen[0], "x-api-key").is_none());
    }

    #[tokio::test]
    async fn second_unauthorized_is_an_auth_error() {
        let file = login_file("401x2");
        store_login(&file, "t0");
        let rotated = file.clone();
        let (base, seen) = serve(2, move |i, _| {
            store_login(&rotated, &format!("t{}", i + 1));
            http_reply("401 Unauthorized", "application/json", UNAUTHORIZED)
        })
        .await;
        let err = run(&base, &file).await.expect_err("rejected twice");
        assert!(matches!(err, HrnError::Auth(_)), "{err}");
        assert_eq!(seen.lock().expect("seen").len(), 2);
    }

    #[tokio::test]
    async fn too_old_rejection_adopts_and_remembers_the_required_version() {
        let file = login_file("version");
        store_login(&file, "tok");
        let (base, seen) = serve(2, |i, _| {
            if i == 0 {
                http_reply(
                    "400 Bad Request",
                    "application/json",
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"claude_code_version_too_old: version 9.9.9 or newer is required"}}"#,
                )
            } else {
                http_reply("200 OK", "text/event-stream", SSE_OK)
            }
        })
        .await;
        run(&base, &file).await.expect("healed");
        let seen = seen.lock().expect("seen").clone();
        assert_eq!(
            header(&seen[1], "user-agent"),
            Some("claude-cli/9.9.9 (external, cli)")
        );
        let stored = std::fs::read_to_string(file.with_file_name(VERSION_FILE)).expect("stored");
        assert_eq!(stored, "9.9.9");
    }
}
