//! Data-driven context-overflow classification (`data/overflow-patterns.txt`) and
//! context-window promotion target resolution (`context_promotion_target`).

use crate::candidates::expand_selector;
use crate::config::{AuthMode, LlmConfig, ProviderEntry, ProviderKind};
use hrn_kernel::{HrnError, LevelSuffix};
use std::path::Path;

const OVERFLOW_PATTERNS_DATA: &str = include_str!("../data/overflow-patterns.txt");
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// True when `err` matches any context-window overflow pattern in `data/overflow-patterns.txt`.
#[must_use]
pub fn is_context_overflow(err: &HrnError) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    OVERFLOW_PATTERNS_DATA
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .any(|pat| msg.contains(&pat.to_ascii_lowercase()))
}

/// Resolve a preset's `context_promotion_target` via [`expand_selector`], verifying that the target
/// is available/enabled, distinct from the current preset, has a strictly larger `context_window`,
/// and has a usable credential.
#[must_use]
pub(crate) fn resolve_promotion_target(cfg: &LlmConfig, model: Option<&str>) -> Option<String> {
    let current_alias = crate::candidates::resolve_alias(cfg, model).ok()?;
    let current_preset = cfg.presets.get(&current_alias)?;
    let raw_target = current_preset
        .context_promotion_target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;

    let resolved = expand_selector(cfg, Some(raw_target)).ok().or_else(|| {
        if raw_target.contains('/') || raw_target.starts_with('@') || raw_target.contains(',') {
            return None;
        }
        let qualified = format!("{}/{raw_target}", current_preset.provider);
        expand_selector(cfg, Some(&qualified)).ok()
    })?;

    if resolved.alias == current_alias {
        return None;
    }
    let target_preset = cfg.presets.get(&resolved.alias)?;
    let current_window = current_preset
        .context_window
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let target_window = target_preset
        .context_window
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    if target_window <= current_window {
        return None;
    }
    let target_prov = cfg.providers.get(&target_preset.provider)?;
    if !has_usable_credential(target_prov) {
        return None;
    }
    match resolved.level {
        LevelSuffix::Level(lvl) => Some(format!("{}:{}", resolved.alias, lvl.as_str())),
        LevelSuffix::None | LevelSuffix::Inherit => Some(resolved.alias),
    }
}

/// Synchronous usability check mirroring [`crate::LlmRouter::resolve_credential`] without network I/O.
fn has_usable_credential(prov: &ProviderEntry) -> bool {
    if prov.auth_mode.subscription().is_some() {
        return prov
            .token_file
            .as_deref()
            .is_some_and(|p| Path::new(p).is_file());
    }
    match prov.auth_mode {
        AuthMode::Agy => crate::agy_acp::discover_agy_binary(prov.agy.binary.as_deref()).is_some(),
        AuthMode::OauthJson => prov
            .oauth
            .as_ref()
            .is_some_and(|o| Path::new(&o.token_file).is_file()),
        AuthMode::Default
        | AuthMode::ApiKey
        | AuthMode::Bearer
        | AuthMode::Claude
        | AuthMode::Chatgpt
        | AuthMode::KimiCode => {
            if prov.resolve_static_token().ok().flatten().is_some() {
                return true;
            }
            // Keyless local OpenAI-compatible servers (e.g. Ollama) are legitimately usable.
            prov.token_env.is_none()
                && prov.token_envs.is_empty()
                && prov.token_file.is_none()
                && prov.kind == ProviderKind::OpenaiChat
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmRouter;
    use crate::config::ModelPreset;
    use hrn_edge_http::HttpClient;
    use hrn_edge_oauth::OAuthManager;
    use hrn_kernel::{
        BoxFuture, CancelFlag, ChunkSink, LlmChunk, LlmProvider, LlmRequest, Msg, ThinkingLevel,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct NullSink;
    impl ChunkSink for NullSink {
        fn on_chunk(&self, _chunk: LlmChunk) -> BoxFuture<()> {
            Box::pin(async {})
        }
    }

    #[test]
    fn classifies_provider_overflow_patterns() {
        for msg in [
            "context_length_exceeded: max 128000",
            "Please reduce the length of the messages or completion.",
            "prompt is too long: 210000 tokens > 200000 maximum",
            "This model's maximum context length is 128000 tokens",
            "Request contains too many tokens for this model",
            "Input exceeds the context window",
            "ollama empty_length_finish detected",
        ] {
            let err = HrnError::Provider {
                message: msg.into(),
                retryable: false,
                retry_after_secs: None,
            };
            assert!(is_context_overflow(&err), "expected overflow for: {msg}");
        }
        let non_overflow = HrnError::Provider {
            message: "HTTP 503 service temporarily unavailable".into(),
            retryable: true,
            retry_after_secs: Some(5),
        };
        assert!(!is_context_overflow(&non_overflow));
    }

    #[test]
    fn promotion_target_verifies_window_distinctness_and_credentials() {
        let mut cfg = LlmConfig::default();
        cfg.providers.insert(
            "local".into(),
            ProviderEntry {
                kind: ProviderKind::OpenaiChat,
                base_url: Some("http://127.0.0.1:1/v1".into()),
                ..ProviderEntry::default()
            },
        );
        cfg.providers.insert(
            "missing_key".into(),
            ProviderEntry {
                kind: ProviderKind::OpenaiChat,
                base_url: Some("http://127.0.0.1:2/v1".into()),
                token_env: Some("HRN_TEST_DEFINITELY_UNSET_KEY_10D".into()),
                ..ProviderEntry::default()
            },
        );
        cfg.presets.insert(
            "small".into(),
            ModelPreset {
                provider: "local".into(),
                id: "m-small".into(),
                context_window: Some(64_000),
                context_promotion_target: Some("big".into()),
                ..ModelPreset::default()
            },
        );
        cfg.presets.insert(
            "big".into(),
            ModelPreset {
                provider: "local".into(),
                id: "m-big".into(),
                context_window: Some(256_000),
                ..ModelPreset::default()
            },
        );
        cfg.presets.insert(
            "same_size".into(),
            ModelPreset {
                provider: "local".into(),
                id: "m-same".into(),
                context_window: Some(64_000),
                ..ModelPreset::default()
            },
        );
        cfg.presets.insert(
            "unauthed".into(),
            ModelPreset {
                provider: "missing_key".into(),
                id: "m-huge".into(),
                context_window: Some(1_000_000),
                ..ModelPreset::default()
            },
        );
        cfg.roles.insert("default".into(), "small".into());

        // Valid larger keyless target succeeds.
        assert_eq!(
            resolve_promotion_target(&cfg, Some("small")).as_deref(),
            Some("big")
        );

        // Equal/smaller window is rejected.
        cfg.presets
            .get_mut("small")
            .expect("small")
            .context_promotion_target = Some("same_size".into());
        assert_eq!(resolve_promotion_target(&cfg, Some("small")), None);

        // Missing credential is rejected.
        cfg.presets
            .get_mut("small")
            .expect("small")
            .context_promotion_target = Some("unauthed".into());
        assert_eq!(resolve_promotion_target(&cfg, Some("small")), None);

        // Disabled target is rejected.
        cfg.presets
            .get_mut("small")
            .expect("small")
            .context_promotion_target = Some("big".into());
        cfg.enabled_models = vec!["small".into()];
        assert_eq!(resolve_promotion_target(&cfg, Some("small")), None);
    }

    #[tokio::test]
    async fn overflow_not_in_fallback_chain() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let n = hits_srv.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let (status, body) = if n == 0 {
                    // Use HTTP 500 with `context_length_exceeded` (normally retryable!) to prove
                    // overflow bypasses `fallback_chains` unconditionally.
                    (
                        "500 Internal Server Error",
                        r#"{"error":{"message":"context_length_exceeded: prompt is too long","type":"server_error"}}"#,
                    )
                } else {
                    (
                        "200 OK",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"fallback\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    )
                };
                let content_type = if n == 0 {
                    "application/json"
                } else {
                    "text/event-stream"
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        let mut cfg = LlmConfig::default();
        cfg.providers.insert(
            "local".into(),
            ProviderEntry {
                kind: ProviderKind::OpenaiChat,
                base_url: Some(format!("http://{addr}/v1")),
                ..ProviderEntry::default()
            },
        );
        cfg.presets.insert(
            "primary".into(),
            ModelPreset {
                provider: "local".into(),
                id: "m1".into(),
                context_window: Some(64_000),
                ..ModelPreset::default()
            },
        );
        cfg.presets.insert(
            "backup".into(),
            ModelPreset {
                provider: "local".into(),
                id: "m2".into(),
                context_window: Some(64_000),
                ..ModelPreset::default()
            },
        );
        cfg.roles.insert("default".into(), "primary".into());
        cfg.fallback_chains
            .insert("primary".into(), vec!["backup".into()]);

        let router = LlmRouter::try_new(
            cfg,
            HttpClient::new().expect("http"),
            OAuthManager::default(),
        )
        .expect("router");
        let req = LlmRequest {
            model: Some("primary".into()),
            messages: vec![Msg::user("hello")],
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            thinking_budgets: None,
            temperature: None,
            max_tokens: None,
            session_id: None,
            service_tier: None,
        };
        let err = router
            .stream_chat(&req, &CancelFlag::new(), &NullSink)
            .await
            .expect_err("overflow must return immediately");
        assert!(router.is_context_overflow(&err), "unexpected error: {err}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "context overflow must NOT enter fallback_chains"
        );
        server.abort();
    }
}
