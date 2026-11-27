//! Intra-provider multi-key API-key rotation for static-credential providers.
//!
//! When a provider configures multiple keys (comma/newline-separated inside `token_env` /
//! `token_file`, or multiple populated env vars across `token_env` + `token_envs`),
//! [`LlmRouter::attempt_static_key_rotation`] rotates across healthy keys on HTTP 401/403
//! (`HrnError::Auth`) and HTTP 429 / quota / rate-limit errors on the **same preset** before
//! falling back to the next model in the fallback chain.

use crate::config::{AuthMode, ModelPreset, ProviderEntry};
use crate::router::{GuardSink, LlmRouter};
use crate::router_pool::{PoolOutcome, is_rate_or_usage_limit};
use hrn_edge_http::guard;
use hrn_kernel::{CancelFlag, ChunkSink, HrnError, LlmRequest, Result};
use std::collections::HashSet;

const RATE_KEY_COOLDOWN_SECS: u64 = 60;
const AUTH_KEY_COOLDOWN_SECS: u64 = 300;

/// One resolved static API key with a non-secret label used for per-key cooldown tracking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StaticKey {
    pub(crate) label: String,
    pub(crate) token: String,
}

/// Collect all distinct non-empty static API keys from `token_env`, `token_envs`, and `token_file`.
///
/// Supports multiple keys separated by commas or newlines inside any single env var or token file,
/// as well as multiple distinct env vars across `token_env` + `token_envs`.
pub(crate) fn resolve_static_keys(prov: &ProviderEntry) -> Result<Vec<StaticKey>> {
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    for env_name in prov
        .token_env
        .iter()
        .chain(prov.token_envs.iter())
        .map(String::as_str)
    {
        let from_env = std::env::var(env_name).ok();
        let value = from_env
            .filter(|v| !v.trim().is_empty())
            .or_else(|| prov.env_file_value(env_name));
        if let Some(raw) = value {
            push_split_keys(&raw, env_name, &mut seen, &mut keys);
        }
    }
    if let Some(path) = prov.token_file.as_deref() {
        guard::check_token_path(path)?;
        let raw = std::fs::read_to_string(path)
            .map_err(|e| HrnError::Auth(format!("read token_file `{path}`: {e}")))?;
        push_split_keys(&raw, "token_file", &mut seen, &mut keys);
    }
    Ok(keys)
}

fn push_split_keys(raw: &str, source: &str, seen: &mut HashSet<String>, out: &mut Vec<StaticKey>) {
    let parts: Vec<&str> = raw
        .split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let multi = parts.len() > 1;
    for (idx, part) in parts.into_iter().enumerate() {
        if !seen.insert(part.to_owned()) {
            continue;
        }
        let label = if multi {
            format!("{source}#{}", idx + 1)
        } else {
            source.to_owned()
        };
        out.push(StaticKey {
            label,
            token: part.to_owned(),
        });
    }
}

impl LlmRouter {
    /// Attempt one non-subscription candidate, rotating across all configured static API keys on
    /// auth (401/403) and rate-limit (429/quota) failures before exhausting the candidate.
    #[expect(
        clippy::too_many_arguments,
        reason = "candidate attempt passes alias, provider, preset, model_id, request, cancel, sink"
    )]
    pub(crate) async fn attempt_static_key_rotation(
        &self,
        alias: &str,
        prov: &ProviderEntry,
        preset: &ModelPreset,
        effective_id: &str,
        effective_req: &LlmRequest,
        cancel: &CancelFlag,
        sink: &dyn ChunkSink,
    ) -> PoolOutcome {
        if prov.auth_mode == AuthMode::OauthJson {
            return self
                .attempt_single_static(
                    alias,
                    prov,
                    preset,
                    effective_id,
                    effective_req,
                    cancel,
                    sink,
                )
                .await;
        }
        let keys = match resolve_static_keys(prov) {
            Ok(k) => k,
            Err(err) => return PoolOutcome::Exhausted(err),
        };
        if keys.len() <= 1 {
            return self
                .attempt_single_static(
                    alias,
                    prov,
                    preset,
                    effective_id,
                    effective_req,
                    cancel,
                    sink,
                )
                .await;
        }

        let mut last_err: Option<HrnError> = None;
        for key in &keys {
            if cancel.is_cancelled() {
                return PoolOutcome::Fatal(HrnError::Cancelled);
            }
            let cd_key = format!("{}::key::{}", preset.provider, key.label);
            if self.is_cooled_down(&cd_key) {
                continue;
            }
            let attempt_cancel = cancel.child();
            let guard_sink = GuardSink::new(sink, attempt_cancel.clone());
            let res = self
                .dispatch_static(
                    prov,
                    preset,
                    effective_id,
                    &key.token,
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
            match res {
                Ok(resp) => return PoolOutcome::Succeeded(resp),
                Err(err)
                    if crate::overflow::is_context_overflow(&err) || guard_sink.emitted_any() =>
                {
                    return PoolOutcome::Fatal(err);
                }
                Err(err @ HrnError::Auth(_)) => {
                    self.set_cooldown(&cd_key, AUTH_KEY_COOLDOWN_SECS);
                    last_err = Some(err);
                }
                Err(err) if is_rate_or_usage_limit(&err) => {
                    let secs = err.retry_after_secs().unwrap_or(RATE_KEY_COOLDOWN_SECS);
                    self.set_cooldown(&cd_key, secs);
                    last_err = Some(err);
                }
                Err(err) if err.is_retryable() => return PoolOutcome::Exhausted(err),
                Err(err) => return PoolOutcome::Fatal(err),
            }
        }
        PoolOutcome::Exhausted(last_err.unwrap_or_else(|| HrnError::Provider {
            message: format!(
                "all API keys for `{}` are currently cooled down",
                preset.provider
            ),
            retryable: true,
            retry_after_secs: Some(RATE_KEY_COOLDOWN_SECS),
        }))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "single static attempt passes alias, provider, preset, model_id, request, cancel, sink"
    )]
    async fn attempt_single_static(
        &self,
        alias: &str,
        prov: &ProviderEntry,
        preset: &ModelPreset,
        effective_id: &str,
        effective_req: &LlmRequest,
        cancel: &CancelFlag,
        sink: &dyn ChunkSink,
    ) -> PoolOutcome {
        let token = match self.resolve_static_credential(prov).await {
            Ok(t) => t,
            Err(e) => return PoolOutcome::Exhausted(e),
        };
        let attempt_cancel = cancel.child();
        let guard_sink = GuardSink::new(sink, attempt_cancel.clone());
        let res = self
            .dispatch_static(
                prov,
                preset,
                effective_id,
                &token,
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
        match res {
            Ok(resp) => PoolOutcome::Succeeded(resp),
            Err(err) => {
                if crate::overflow::is_context_overflow(&err) {
                    return PoolOutcome::Fatal(err);
                }
                if err.is_retryable() && !guard_sink.emitted_any() {
                    PoolOutcome::Exhausted(err)
                } else {
                    PoolOutcome::Fatal(err)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::{VecSink, header, http_reply, serve};
    use crate::config::{LlmConfig, ModelPreset, ProviderEntry, ProviderKind};
    use hrn_edge_http::HttpClient;
    use hrn_edge_oauth::OAuthManager;
    use hrn_kernel::{LlmProvider, Msg, ThinkingLevel};

    const OPENAI_SSE_OK: &str = concat!(
        "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"from-key-c\"}}]}\n\n",
        "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn resolve_static_keys_splits_and_deduplicates_dotenv_and_token_file() {
        let dir = std::env::temp_dir().join(format!("hrn-keys-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let env_path = dir.join("test.env");
        let tok_path = dir.join("keys.txt");
        std::fs::write(&env_path, "PRIMARY_K=key-a, key-b\nBACKUP_K=key-b, key-c\n").expect("env");
        std::fs::write(&tok_path, "key-c\nkey-d\n").expect("tok");

        let prov = ProviderEntry {
            kind: ProviderKind::OpenaiChat,
            token_env: Some("PRIMARY_K".into()),
            token_envs: vec!["BACKUP_K".into()],
            env_file: Some(env_path.to_string_lossy().into_owned()),
            token_file: Some(tok_path.to_string_lossy().into_owned()),
            ..ProviderEntry::default()
        };
        let keys = resolve_static_keys(&prov).expect("keys");
        let toks: Vec<&str> = keys.iter().map(|k| k.token.as_str()).collect();
        assert_eq!(toks, ["key-a", "key-b", "key-c", "key-d"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn static_api_key_rotates_on_401_and_429_staying_on_same_preset() {
        let dir = std::env::temp_dir().join(format!("hrn-keys-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let env_path = dir.join("keys.env");
        std::fs::write(&env_path, "ROT_KEYS=key-a,key-b,key-c\n").expect("env");

        let (base, seen) = serve(3, |i, _| match i {
            0 => http_reply(
                "401 Unauthorized",
                "application/json",
                r#"{"error":"bad key"}"#,
            ),
            1 => http_reply(
                "429 Too Many Requests",
                "application/json",
                r#"{"error":{"message":"Rate limit exceeded"}}"#,
            ),
            _ => http_reply("200 OK", "text/event-stream", OPENAI_SSE_OK),
        })
        .await;

        let mut cfg = LlmConfig::default();
        cfg.providers.insert(
            "pool-api".into(),
            ProviderEntry {
                kind: ProviderKind::OpenaiChat,
                base_url: Some(base),
                token_env: Some("ROT_KEYS".into()),
                env_file: Some(env_path.to_string_lossy().into_owned()),
                ..ProviderEntry::default()
            },
        );
        cfg.providers.insert(
            "fallback".into(),
            ProviderEntry {
                kind: ProviderKind::OpenaiChat,
                base_url: Some("http://127.0.0.1:9/v1".into()),
                ..ProviderEntry::default()
            },
        );
        cfg.presets.insert(
            "primary".into(),
            ModelPreset {
                provider: "pool-api".into(),
                id: "test-model".into(),
                ..ModelPreset::default()
            },
        );
        cfg.presets.insert(
            "backup".into(),
            ModelPreset {
                provider: "fallback".into(),
                id: "backup-model".into(),
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
        let sink = VecSink::default();
        let resp = router
            .stream_chat(&req, &CancelFlag::new(), &sink)
            .await
            .expect("rotated to key-c");

        assert_eq!(resp.content, "from-key-c");
        assert_eq!(resp.fallback_from, None, "must stay on primary preset");
        let reqs = seen.lock().expect("seen").clone();
        let auths: Vec<&str> = reqs
            .iter()
            .filter_map(|r| header(r, "authorization"))
            .collect();
        assert_eq!(auths, ["Bearer key-a", "Bearer key-b", "Bearer key-c"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
