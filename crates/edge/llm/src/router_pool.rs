//! Intra-provider multi-account pool rotation for [`LlmRouter`] and acceptance tests for pool
//! rotation + extended-context window switching.

use crate::config::{ModelPreset, ProviderEntry, ProviderKind};
use crate::router::{GuardSink, LlmRouter, subscription_file};
use crate::sub_auth::SubscriptionAuth;
use crate::tier::RouterOverrides;
use hrn_edge_oauth::{CredentialPool, Subscription, read_subscription_accounts};
use hrn_kernel::{CancelFlag, ChunkSink, HrnError, LlmRequest, LlmResponse, Tier};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default block duration (seconds) when a 429 / usage-limit error carries no `Retry-After`.
const DEFAULT_POOL_BLOCK_SECS: u64 = 60;

/// Result of attempting one subscription candidate across its available account pool.
pub(crate) enum PoolOutcome {
    /// One account succeeded.
    Succeeded(LlmResponse),
    /// Non-failoverable error (cancellation, context overflow, mid-stream failure, non-retryable).
    Fatal(HrnError),
    /// Pool exhausted or retryable non-pool failure; caller may advance `resolve_chain`.
    Exhausted(HrnError),
}

pub(crate) fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// True when `err` represents a 429 rate-limit or subscription usage-limit response.
pub(crate) fn is_rate_or_usage_limit(err: &HrnError) -> bool {
    let HrnError::Provider {
        message,
        retryable: true,
        ..
    } = err
    else {
        return false;
    };
    let lc = message.to_ascii_lowercase();
    lc.contains("429")
        || lc.contains("rate limit")
        || lc.contains("rate_limit")
        || lc.contains("usage limit")
        || lc.contains("usage_limit")
        || lc.contains("too many requests")
        || lc.contains("quota")
}

impl LlmRouter {
    /// Attach a shared [`CredentialPool`] (e.g. shared with `/usage` and hot config reloads).
    #[must_use]
    pub fn with_pool(mut self, pool: Arc<CredentialPool>) -> Self {
        pool.set_default_reserve_pct(self.config.retry.usage_reserve_pct);
        self.pool = pool;
        self
    }

    /// Attach shared session-scoped runtime overrides (`/fast`, `/slow`, `/extended-context`).
    #[must_use]
    pub fn with_overrides(mut self, overrides: RouterOverrides) -> Self {
        self.overrides = overrides;
        self
    }

    /// Shared subscription account pool.
    #[must_use]
    pub fn pool(&self) -> Arc<CredentialPool> {
        Arc::clone(&self.pool)
    }

    /// Shared session-scoped runtime overrides handle.
    #[must_use]
    pub fn overrides(&self) -> RouterOverrides {
        self.overrides.clone()
    }

    /// Whether extended context (`max_context_window`) is currently active.
    #[must_use]
    pub fn extended_context(&self) -> bool {
        self.overrides
            .extended_context()
            .unwrap_or(self.config.extended_context)
    }

    /// Toggle extended context for the current session (non-persisted).
    pub fn set_extended_context(&self, enabled: bool) {
        self.overrides.set_extended_context(Some(enabled));
    }

    /// Whether Anthropic subscription slow mode is currently active globally.
    #[must_use]
    pub fn slow_mode(&self) -> bool {
        self.overrides
            .slow_mode()
            .unwrap_or(self.config.anthropic.slow_mode)
    }

    /// Effective slow-mode setting for a specific provider entry.
    pub(crate) fn effective_slow_mode(&self, prov: &ProviderEntry) -> bool {
        self.overrides
            .slow_mode()
            .unwrap_or(self.config.anthropic.slow_mode || prov.slow_mode)
    }

    /// Toggle Anthropic subscription slow mode for the current session (non-persisted).
    pub fn set_slow_mode(&self, enabled: bool) {
        self.overrides.set_slow_mode(Some(enabled));
    }

    /// Set or clear the session-scoped service-tier override (non-persisted).
    pub fn set_service_tier(&self, tier: Option<Tier>) {
        self.overrides.set_service_tier(tier);
    }

    /// Whether `model` supports Anthropic subscription slow mode.
    #[must_use]
    pub fn supports_slow_mode(&self, model: Option<&str>) -> bool {
        crate::tier::supports_slow_mode(&self.config, model)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "dispatch passes provider, preset, kind, file, cred, model_id, req, cancel, sink"
    )]
    async fn dispatch_subscription(
        &self,
        prov: &ProviderEntry,
        preset: &ModelPreset,
        kind: Subscription,
        file: &std::path::Path,
        cred: hrn_edge_oauth::SubscriptionCredential,
        effective_id: &str,
        effective_req: &LlmRequest,
        cancel: &CancelFlag,
        sink: &dyn ChunkSink,
    ) -> hrn_kernel::Result<LlmResponse> {
        let mut auth = SubscriptionAuth::new(&self.http, &self.oauth, kind, file, cred);
        if kind == Subscription::KimiCode {
            return crate::kimi_sub::stream(
                &mut auth,
                prov,
                preset,
                self.config.kimi.api_format,
                effective_id,
                effective_req,
                cancel,
                sink,
            )
            .await;
        }
        match prov.kind {
            ProviderKind::Anthropic => {
                let slow = self.effective_slow_mode(prov);
                crate::claude_sub::stream(
                    &mut auth,
                    prov,
                    effective_id,
                    effective_req,
                    slow,
                    cancel,
                    sink,
                )
                .await
            }
            ProviderKind::OpenaiResponses => {
                crate::codex::stream(&mut auth, prov, effective_id, effective_req, cancel, sink)
                    .await
            }
            ProviderKind::OpenaiChat | ProviderKind::Gemini | ProviderKind::AgyAcp => {
                Err(HrnError::Config(
                    "subscription logins serve only anthropic, openai_responses, and kimi-code"
                        .into(),
                ))
            }
        }
    }

    /// Try a subscription preset across healthy accounts in `self.pool`, rotating on 429 usage
    /// limits and 401 auth errors on the **same preset** before returning [`PoolOutcome::Exhausted`].
    #[expect(
        clippy::too_many_arguments,
        reason = "pool attempt needs alias, provider, preset, kind, model_id, request, cancel, sink"
    )]
    pub(crate) async fn attempt_subscription_pool(
        &self,
        alias: &str,
        prov: &ProviderEntry,
        preset: &ModelPreset,
        kind: Subscription,
        effective_id: &str,
        effective_req: &LlmRequest,
        cancel: &CancelFlag,
        sink: &dyn ChunkSink,
    ) -> PoolOutcome {
        let file = match subscription_file(prov) {
            Ok(f) => f,
            Err(e) => return PoolOutcome::Exhausted(e),
        };
        let accounts = match read_subscription_accounts(kind, file) {
            Ok(a) if !a.is_empty() => a,
            Ok(_) => {
                return PoolOutcome::Exhausted(HrnError::Auth(format!(
                    "not logged in to {}: run `hrn login {}`",
                    kind.label(),
                    kind.id()
                )));
            }
            Err(e) => return PoolOutcome::Exhausted(e),
        };
        let mut last_err: Option<HrnError> = None;
        for _ in 0..accounts.len().max(1) {
            if cancel.is_cancelled() {
                return PoolOutcome::Fatal(HrnError::Cancelled);
            }
            let now = now_ms();
            let usage_map = self.pool.cached_usage_map(kind.id(), now);
            let Some(selected) =
                self.pool
                    .select(kind.id(), &accounts, &self.config.accounts, &usage_map, now)
            else {
                break;
            };
            let acct_key = selected.identity_key();
            let cred = match self
                .oauth
                .subscription_account(&self.http, kind, file, &acct_key)
                .await
            {
                Ok(c) => c,
                Err(err) => {
                    if matches!(err, HrnError::Auth(_)) {
                        self.pool.invalidate(kind.id(), &acct_key);
                    }
                    last_err = Some(err);
                    continue;
                }
            };
            let attempt_cancel = cancel.child();
            let guard_sink = GuardSink::new(sink, attempt_cancel.clone());
            let res = self
                .dispatch_subscription(
                    prov,
                    preset,
                    kind,
                    file,
                    cred,
                    effective_id,
                    effective_req,
                    &attempt_cancel,
                    &guard_sink,
                )
                .await;
            if attempt_cancel.is_cancelled() && !cancel.is_cancelled() {
                return PoolOutcome::Exhausted(HrnError::Provider {
                    message: format!("thinking loop detected on {alias}"),
                    retryable: true,
                    retry_after_secs: None,
                });
            }
            match self.classify_pool_result(kind, &acct_key, res, guard_sink.emitted_any()) {
                Ok(outcome) => return outcome,
                Err(err) => last_err = Some(err),
            }
        }
        PoolOutcome::Exhausted(last_err.unwrap_or_else(|| HrnError::Provider {
            message: format!(
                "all {} accounts in pool are currently blocked",
                kind.label()
            ),
            retryable: true,
            retry_after_secs: Some(DEFAULT_POOL_BLOCK_SECS),
        }))
    }

    /// Returns `Ok(outcome)` to finish the pool loop, or `Err(err)` to rotate to the next account.
    fn classify_pool_result(
        &self,
        kind: Subscription,
        acct_key: &str,
        res: hrn_kernel::Result<LlmResponse>,
        emitted_any: bool,
    ) -> std::result::Result<PoolOutcome, HrnError> {
        match res {
            Ok(resp) => Ok(PoolOutcome::Succeeded(resp)),
            Err(err) if crate::overflow::is_context_overflow(&err) || emitted_any => {
                Ok(PoolOutcome::Fatal(err))
            }
            Err(err @ HrnError::Auth(_)) => {
                self.pool.invalidate(kind.id(), acct_key);
                Err(err)
            }
            Err(err) if is_rate_or_usage_limit(&err) => {
                let secs = err.retry_after_secs().unwrap_or(DEFAULT_POOL_BLOCK_SECS);
                let until = now_ms().saturating_add(secs.saturating_mul(1000));
                self.pool.mark_blocked(kind.id(), acct_key, until);
                Err(err)
            }
            Err(err) if err.is_retryable() => Ok(PoolOutcome::Exhausted(err)),
            Err(err) => Ok(PoolOutcome::Fatal(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::{VecSink, header, http_reply, serve};
    use crate::config::{AuthMode, LlmConfig, ModelPreset, ProviderEntry, ProviderKind};
    use hrn_edge_http::HttpClient;
    use hrn_edge_oauth::{
        OAuthManager, SubscriptionCredential, SubscriptionStore, upsert_subscription_account,
    };
    use hrn_kernel::{LlmProvider, Msg, ThinkingLevel};

    const CLAUDE_SSE_OK: &str = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"from-acct-b\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    fn pool_test_config(base: String, cred_file: &std::path::Path) -> LlmConfig {
        let mut cfg = LlmConfig::default();
        cfg.providers.insert(
            "claude-sub".into(),
            ProviderEntry {
                kind: ProviderKind::Anthropic,
                base_url: Some(base),
                auth_mode: AuthMode::Claude,
                token_file: Some(cred_file.to_string_lossy().into_owned()),
                ..ProviderEntry::default()
            },
        );
        cfg.providers.insert(
            "fallback-prov".into(),
            ProviderEntry {
                kind: ProviderKind::OpenaiChat,
                base_url: Some("http://127.0.0.1:9/v1".into()),
                ..ProviderEntry::default()
            },
        );
        cfg.presets.insert(
            "sonnet".into(),
            ModelPreset {
                provider: "claude-sub".into(),
                id: "claude-sonnet-4-6".into(),
                ..ModelPreset::default()
            },
        );
        cfg.presets.insert(
            "backup".into(),
            ModelPreset {
                provider: "fallback-prov".into(),
                id: "backup-model".into(),
                ..ModelPreset::default()
            },
        );
        cfg.roles.insert("default".into(), "sonnet".into());
        cfg.fallback_chains
            .insert("sonnet".into(), vec!["backup".into()]);
        cfg
    }

    #[tokio::test]
    async fn pool_rotates_on_429_same_preset() {
        let dir = std::env::temp_dir().join(format!("hrn-pool-429-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cred_file = dir.join("claude.json");

        for (id, tok) in [("acct-a", "bearer-a"), ("acct-b", "bearer-b")] {
            let cred = SubscriptionCredential {
                provider: "claude".into(),
                access: tok.into(),
                refresh: format!("ref-{id}"),
                expires: u64::MAX / 2,
                account_id: Some(id.into()),
                ..SubscriptionCredential::default()
            };
            upsert_subscription_account(Subscription::Claude, &cred_file, cred).expect("seed");
        }
        let store: SubscriptionStore =
            serde_json::from_str(&std::fs::read_to_string(&cred_file).expect("read"))
                .expect("plural store");
        assert_eq!(store.accounts.len(), 2);

        let (base, seen) = serve(2, |i, _| {
            if i == 0 {
                http_reply(
                    "429 Too Many Requests",
                    "application/json",
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limit exceeded"}}"#,
                )
            } else {
                http_reply("200 OK", "text/event-stream", CLAUDE_SSE_OK)
            }
        })
        .await;

        let router = LlmRouter::try_new(
            pool_test_config(base, &cred_file),
            HttpClient::new().expect("http"),
            OAuthManager::default(),
        )
        .expect("router");
        let req = LlmRequest {
            model: Some("sonnet".into()),
            messages: vec![Msg::user("hi there")],
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            thinking_budgets: None,
            temperature: None,
            max_tokens: None,
            session_id: Some("ses_pool".into()),
            service_tier: None,
        };
        let sink = VecSink::default();
        let resp = router
            .stream_chat(&req, &CancelFlag::new(), &sink)
            .await
            .expect("rotated to account B");

        assert_eq!(resp.content, "from-acct-b");
        assert_eq!(resp.model_used, "claude-sonnet-4-6");
        assert_eq!(
            resp.fallback_from, None,
            "rotation stayed on the same preset without falling back"
        );
        let reqs = seen.lock().expect("seen").clone();
        assert_eq!(reqs.len(), 2, "two POSTs to the same model");
        let auths: Vec<&str> = reqs
            .iter()
            .filter_map(|r| header(r, "authorization"))
            .collect();
        assert_eq!(auths, ["Bearer bearer-a", "Bearer bearer-b"]);
        assert!(
            router.pool().is_blocked("claude", "acct-a", now_ms()),
            "account A must be marked blocked after 429"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn extended_context_switches_window() {
        let mut cfg = LlmConfig::default();
        cfg.providers.insert(
            "openai".into(),
            ProviderEntry {
                kind: ProviderKind::OpenaiResponses,
                ..ProviderEntry::default()
            },
        );
        cfg.presets.insert(
            "codex".into(),
            ModelPreset {
                provider: "openai".into(),
                id: "gpt-5.3-codex".into(),
                context_window: Some(128_000),
                max_context_window: Some(256_000),
                ..ModelPreset::default()
            },
        );
        cfg.roles.insert("default".into(), "codex".into());

        let router = LlmRouter::try_new(
            cfg,
            HttpClient::new().expect("http"),
            OAuthManager::default(),
        )
        .expect("router");
        assert!(!router.extended_context());
        let off_caps = router.capabilities(Some("codex"));
        assert_eq!(off_caps.context_window, 128_000);
        assert_eq!(off_caps.max_context_window, Some(256_000));

        router.set_extended_context(true);
        assert!(router.extended_context());
        let on_caps = router.capabilities(Some("codex"));
        assert_eq!(on_caps.context_window, 256_000);
        assert_eq!(on_caps.max_context_window, Some(256_000));

        router.set_extended_context(false);
        assert_eq!(router.capabilities(Some("codex")).context_window, 128_000);
    }
}
