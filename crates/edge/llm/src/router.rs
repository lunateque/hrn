//! Usage-aware multi-provider router with cooldowns, fallback chains, and `ThinkingLoopDetector`.

use crate::config::{
    AuthMode, LlmConfig, ModelPreset, ProviderEntry, ProviderKind, validate_config,
};
use crate::router_pool::PoolOutcome;
use crate::tier::RouterOverrides;
use hrn_edge_http::HttpClient;
use hrn_edge_oauth::{CredentialPool, OAuthManager};
use hrn_kernel::{
    BoxFuture, CancelFlag, ChunkSink, HrnError, LlmChunk, LlmProvider, LlmRequest, LlmResponse,
    ModelInfo, ProviderCapabilities, Result, ScopedFuture,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Cooldown applied to a provider after a retryable failure that carried no `Retry-After`.
const DEFAULT_COOLDOWN_SECS: u64 = 10;

/// Detects exact suffix cycles in streamed reasoning deltas (4096-byte rolling tail).
#[derive(Default)]
pub struct ThinkingLoopDetector {
    tail: String,
    disarmed: bool,
    tripped: bool,
}

impl ThinkingLoopDetector {
    /// Disarm once visible output or tool calls begin.
    pub fn disarm(&mut self) {
        self.disarmed = true;
    }

    /// Push a reasoning delta; returns `true` when an exact repeating suffix cycle is detected.
    pub fn push_reasoning(&mut self, delta: &str) -> bool {
        if self.disarmed || self.tripped {
            return self.tripped;
        }
        self.tail.push_str(delta);
        if self.tail.len() > 4096 {
            let start = self.tail.ceil_char_boundary(self.tail.len() - 4096);
            self.tail = self.tail.get(start..).unwrap_or("").to_owned();
        }
        let bytes = self.tail.as_bytes();
        let len = bytes.len();
        // Check for 3 consecutive identical blocks of length 40..=300 bytes at the tail.
        for period in 40..=300 {
            if len < period * 3 {
                break;
            }
            let a = bytes.get(len - period..len);
            let b = bytes.get(len - period * 2..len - period);
            let c = bytes.get(len - period * 3..len - period * 2);
            if a.is_some() && a == b && b == c {
                self.tripped = true;
                return true;
            }
        }
        false
    }
}

pub(crate) struct GuardSink<'a> {
    inner: &'a dyn ChunkSink,
    emitted_any: AtomicBool,
    detector: Mutex<ThinkingLoopDetector>,
    loop_cancel: CancelFlag,
}

impl<'a> GuardSink<'a> {
    pub(crate) fn new(inner: &'a dyn ChunkSink, loop_cancel: CancelFlag) -> Self {
        Self {
            inner,
            emitted_any: AtomicBool::new(false),
            detector: Mutex::new(ThinkingLoopDetector::default()),
            loop_cancel,
        }
    }

    pub(crate) fn emitted_any(&self) -> bool {
        self.emitted_any.load(Ordering::SeqCst)
    }
}

impl ChunkSink for GuardSink<'_> {
    fn on_chunk(&self, chunk: LlmChunk) -> BoxFuture<()> {
        match &chunk {
            LlmChunk::ContentDelta(_) => {
                self.emitted_any.store(true, Ordering::SeqCst);
                if let Ok(mut det) = self.detector.lock() {
                    det.disarm();
                }
            }
            LlmChunk::ReasoningDelta(delta) => {
                if let Ok(mut det) = self.detector.lock()
                    && det.push_reasoning(delta)
                {
                    self.loop_cancel.cancel();
                }
            }
        }
        self.inner.on_chunk(chunk)
    }
}

/// Multi-provider [`LlmProvider`] implementing roles, presets, account pools, service tiers,
/// extended context, cooldowns, and fallback chains.
pub struct LlmRouter {
    pub(crate) config: LlmConfig,
    pub(crate) http: HttpClient,
    pub(crate) oauth: OAuthManager,
    pub(crate) pool: Arc<CredentialPool>,
    pub(crate) overrides: RouterOverrides,
    pub(crate) agy_runtimes: Arc<Mutex<HashMap<String, crate::router_agy::AgyRuntimeEntry>>>,
    cooldowns: Mutex<HashMap<String, Instant>>,
}

impl LlmRouter {
    /// A router over the same http/oauth/pool/override state with a different (validated) config:
    /// the `modelpreset switch` and live-reload rebuild path.
    #[must_use]
    pub fn with_config(&self, config: LlmConfig) -> Self {
        self.pool
            .set_default_reserve_pct(config.retry.usage_reserve_pct);
        Self {
            config,
            http: self.http.clone(),
            oauth: self.oauth.clone(),
            pool: Arc::clone(&self.pool),
            overrides: self.overrides.clone(),
            agy_runtimes: Arc::clone(&self.agy_runtimes),
            cooldowns: Mutex::new(HashMap::new()),
        }
    }

    /// Validate configuration and build a new router.
    pub fn try_new(config: LlmConfig, http: HttpClient, oauth: OAuthManager) -> Result<Self> {
        validate_config(&config)?;
        let pool = Arc::new(CredentialPool::new(config.retry.usage_reserve_pct));
        Ok(Self {
            config,
            http,
            oauth,
            pool,
            overrides: RouterOverrides::default(),
            agy_runtimes: Arc::new(Mutex::new(HashMap::new())),
            cooldowns: Mutex::new(HashMap::new()),
        })
    }

    /// Resolve a requested model/role/preset alias into a concrete ordered candidate chain.
    pub(crate) fn resolve_chain(
        &self,
        requested: Option<&str>,
    ) -> Result<(Vec<(String, ModelPreset)>, hrn_kernel::LevelSuffix)> {
        crate::candidates::resolve_chain(&self.config, requested)
    }

    pub(crate) fn is_cooled_down(&self, provider_name: &str) -> bool {
        let guard = self
            .cooldowns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .get(provider_name)
            .is_some_and(|until| Instant::now() < *until)
    }

    pub(crate) fn set_cooldown(&self, provider_name: &str, secs: u64) {
        let mut guard = self
            .cooldowns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(
            provider_name.to_owned(),
            Instant::now() + Duration::from_secs(secs.clamp(5, 300)),
        );
    }

    pub(crate) async fn resolve_static_credential(&self, prov: &ProviderEntry) -> Result<String> {
        match prov.auth_mode {
            AuthMode::Agy => Err(HrnError::Config(
                "auth_mode = agy delegates credentials to the `agy` CLI runtime".into(),
            )),
            AuthMode::OauthJson => {
                let oauth_cfg = prov.oauth.as_ref().ok_or_else(|| {
                    HrnError::Config("auth_mode = oauth_json requires [oauth] table".into())
                })?;
                self.oauth.access_token(&self.http, oauth_cfg).await
            }
            AuthMode::Default
            | AuthMode::ApiKey
            | AuthMode::Bearer
            | AuthMode::Claude
            | AuthMode::Chatgpt
            | AuthMode::KimiCode => {
                if let Some(token) = prov.resolve_static_token()? {
                    return Ok(token);
                }
                if let Some(env) = prov.token_env.as_deref() {
                    return Err(HrnError::Auth(format!(
                        "no API key: environment variable `{env}` is unset or empty"
                    )));
                }
                if prov.token_file.is_some() {
                    return Err(HrnError::Auth("token_file is empty".into()));
                }
                // Keyless local OpenAI-compatible servers are legitimate; hosted APIs are not.
                if prov.kind == ProviderKind::OpenaiChat {
                    Ok(String::new())
                } else {
                    Err(HrnError::Auth(
                        "provider has no token_env/token_file configured".into(),
                    ))
                }
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "wire dispatch passes provider, preset, model_id, token, req, cancel, sink"
    )]
    pub(crate) async fn dispatch_static(
        &self,
        prov: &ProviderEntry,
        preset: &ModelPreset,
        model_id: &str,
        token: &str,
        req: &LlmRequest,
        cancel: &CancelFlag,
        sink: &dyn ChunkSink,
    ) -> Result<LlmResponse> {
        let http = &self.http;
        match prov.kind {
            ProviderKind::Anthropic => {
                crate::anthropic::stream(http, prov, model_id, token, req, cancel, sink).await
            }
            ProviderKind::OpenaiChat => {
                crate::openai_chat::stream(http, prov, preset, model_id, token, req, cancel, sink)
                    .await
            }
            ProviderKind::OpenaiResponses => {
                crate::openai_responses::stream(
                    http, prov, preset, model_id, token, req, cancel, sink,
                )
                .await
            }
            ProviderKind::Gemini => {
                crate::gemini::stream(http, prov, model_id, token, req, cancel, sink).await
            }
            ProviderKind::AgyAcp => Err(HrnError::Config(
                "provider kind `agy_acp` is dispatched through the ACP runtime".into(),
            )),
        }
    }
}

/// Effective upstream model id, appending any `OpenRouter` routing variant suffix (`:nitro`,
/// `:floor`, `:online`, `:exacto`, `:extended`) requested by the caller.
fn effective_model_id(
    requested: Option<&str>,
    prov: &ProviderEntry,
    preset: &ModelPreset,
) -> String {
    if let Some((_, suffix)) = requested.and_then(crate::candidates::strip_openrouter_variant)
        && crate::openai_chat::is_openrouter_url(prov.effective_base_url())
        && !preset.id.rsplit('/').next().unwrap_or("").contains(':')
    {
        return format!("{}{suffix}", preset.id);
    }
    preset.id.clone()
}

/// Build the per-candidate request with thinking resolution and failover trace stripping.
fn prepare_attempt_request(
    request: &LlmRequest,
    preset: &ModelPreset,
    selector_level: Option<hrn_kernel::ThinkingLevel>,
    is_failover: bool,
) -> LlmRequest {
    let mut effective_req = request.clone();
    if is_failover {
        for m in &mut effective_req.messages {
            m.reasoning = None;
            m.thinking_blocks.clear();
        }
    }
    effective_req
        .thinking_budgets
        .clone_from(&preset.thinking_budgets);
    if preset.reasoning == Some(false) {
        effective_req.thinking = hrn_kernel::ThinkingLevel::Off;
    } else {
        if let Some(suffix) = selector_level {
            effective_req.thinking = suffix;
        } else if let Some(default_think) = preset.thinking
            && effective_req.thinking == hrn_kernel::ThinkingLevel::Medium
        {
            effective_req.thinking = default_think;
        }
        if let Some(budgets) = &preset.thinking_budgets {
            effective_req.thinking = budgets.resolve(effective_req.thinking);
        } else if effective_req.thinking == hrn_kernel::ThinkingLevel::Auto {
            effective_req.thinking = hrn_kernel::ThinkingLevel::Medium;
        }
    }
    effective_req
}

/// The `hrn login` credential file of a subscription provider.
pub(crate) fn subscription_file(prov: &ProviderEntry) -> Result<&Path> {
    prov.token_file
        .as_deref()
        .map(Path::new)
        .ok_or_else(|| HrnError::Config("subscription provider has no credential file".into()))
}

fn finalize_response(
    mut resp: LlmResponse,
    preset: &ModelPreset,
    idx: usize,
    primary_alias: Option<&str>,
) -> LlmResponse {
    if let (Some(price), Some(usage)) = (&preset.price, resp.usage.as_mut()) {
        usage.cost_micros = Some(price.cost_micros(usage));
    }
    if idx > 0 {
        resp.fallback_from = primary_alias.map(str::to_owned);
    }
    resp
}

impl LlmRouter {
    /// Attempt one non-subscription candidate and convert its outcome into [`PoolOutcome`].
    #[expect(
        clippy::too_many_arguments,
        reason = "candidate attempt passes alias, provider, preset, model_id, request, cancel, sink"
    )]
    async fn attempt_static_candidate(
        &self,
        alias: &str,
        prov: &ProviderEntry,
        preset: &ModelPreset,
        effective_id: &str,
        effective_req: &LlmRequest,
        cancel: &CancelFlag,
        sink: &dyn ChunkSink,
    ) -> PoolOutcome {
        self.attempt_static_key_rotation(
            alias,
            prov,
            preset,
            effective_id,
            effective_req,
            cancel,
            sink,
        )
        .await
    }
}

impl LlmProvider for LlmRouter {
    fn stream_chat<'a>(
        &'a self,
        request: &'a LlmRequest,
        cancel: &'a CancelFlag,
        sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>> {
        Box::pin(async move {
            let (candidates, resolved_level) = self.resolve_chain(request.model.as_deref())?;
            let selector_level = match resolved_level {
                hrn_kernel::LevelSuffix::Level(level) => Some(level),
                hrn_kernel::LevelSuffix::None | hrn_kernel::LevelSuffix::Inherit => None,
            };
            let primary_alias = candidates.first().map(|(a, _)| a.clone());
            let mut last_err: Option<HrnError> = None;

            for (idx, (alias, preset)) in candidates.iter().enumerate() {
                if cancel.is_cancelled() {
                    return Err(HrnError::Cancelled);
                }
                if idx + 1 < candidates.len() && self.is_cooled_down(&preset.provider) {
                    continue;
                }
                let Some(prov) = self.config.providers.get(&preset.provider) else {
                    continue;
                };
                let effective_id = effective_model_id(request.model.as_deref(), prov, preset);
                let mut effective_req =
                    prepare_attempt_request(request, preset, selector_level, idx > 0);
                effective_req.service_tier = crate::tier::resolve_service_tier(
                    request.service_tier,
                    self.overrides.service_tier(),
                    &self.config.tier,
                    prov,
                    &effective_id,
                );

                let outcome = if prov.kind == ProviderKind::AgyAcp {
                    self.attempt_agy_candidate(
                        alias,
                        &preset.provider,
                        prov,
                        &effective_id,
                        &effective_req,
                        cancel,
                        sink,
                    )
                    .await
                } else if let Some(kind) = prov.auth_mode.subscription() {
                    self.attempt_subscription_pool(
                        alias,
                        prov,
                        preset,
                        kind,
                        &effective_id,
                        &effective_req,
                        cancel,
                        sink,
                    )
                    .await
                } else {
                    self.attempt_static_candidate(
                        alias,
                        prov,
                        preset,
                        &effective_id,
                        &effective_req,
                        cancel,
                        sink,
                    )
                    .await
                };

                match outcome {
                    PoolOutcome::Succeeded(resp) => {
                        return Ok(finalize_response(
                            resp,
                            preset,
                            idx,
                            primary_alias.as_deref(),
                        ));
                    }
                    PoolOutcome::Fatal(err) => {
                        if let Some(secs) = err
                            .retry_after_secs()
                            .or_else(|| err.is_retryable().then_some(DEFAULT_COOLDOWN_SECS))
                        {
                            self.set_cooldown(&preset.provider, secs);
                        }
                        return Err(err);
                    }
                    PoolOutcome::Exhausted(err) => {
                        if let Some(secs) = err
                            .retry_after_secs()
                            .or_else(|| err.is_retryable().then_some(DEFAULT_COOLDOWN_SECS))
                        {
                            self.set_cooldown(&preset.provider, secs);
                        }
                        last_err = Some(err);
                    }
                }
            }

            Err(last_err.unwrap_or_else(|| HrnError::Internal("all LLM candidates failed".into())))
        })
    }

    fn models(&self) -> Vec<ModelInfo> {
        let default_alias = crate::candidates::resolve_alias(&self.config, None).ok();
        self.config
            .presets
            .iter()
            .filter(|(alias, preset)| {
                crate::candidates::is_preset_enabled(&self.config, alias, preset)
            })
            .map(|(alias, preset)| ModelInfo {
                alias: alias.clone(),
                id: preset.id.clone(),
                provider: preset.provider.clone(),
                default: default_alias.as_deref() == Some(alias.as_str()),
            })
            .collect()
    }

    fn capabilities(&self, model: Option<&str>) -> ProviderCapabilities {
        let alias = crate::candidates::resolve_alias(&self.config, model).ok();
        let preset = alias.as_deref().and_then(|a| self.config.presets.get(a));
        let prov = preset.and_then(|p| self.config.providers.get(&p.provider));
        let base_window = preset.and_then(|p| p.context_window).unwrap_or(200_000);
        let max_window = preset.and_then(|p| p.max_context_window);
        let context_window = if self.extended_context() {
            max_window.unwrap_or(base_window)
        } else {
            base_window
        };
        ProviderCapabilities {
            tools: true,
            thinking: preset.is_none_or(|p| p.reasoning != Some(false)),
            prompt_cache: prov.is_some_and(|p| {
                p.prompt_cache
                    || matches!(p.kind, ProviderKind::OpenaiResponses | ProviderKind::AgyAcp)
            }),
            context_window,
            max_context_window: max_window,
        }
    }

    fn prompt_variant(&self, model: Option<&str>) -> Option<String> {
        crate::prompt_variant::resolve(&self.config, model)
    }

    fn is_context_overflow(&self, err: &HrnError) -> bool {
        crate::overflow::is_context_overflow(err)
    }

    fn context_promotion_target(&self, model: Option<&str>) -> Option<String> {
        crate::overflow::resolve_promotion_target(&self.config, model)
    }

    fn supports_service_tier(&self, model: Option<&str>) -> bool {
        crate::tier::supports_service_tier(&self.config, model)
    }

    fn supports_vision(&self, model: Option<&str>) -> bool {
        crate::vision::supports_vision(&self.config, model)
    }

    fn compact_remote<'a>(
        &'a self,
        model: Option<&'a str>,
        messages: &'a [hrn_kernel::Msg],
        instructions: Option<&'a str>,
        cancel: &'a CancelFlag,
    ) -> ScopedFuture<'a, Result<Option<hrn_kernel::RemoteCompaction>>> {
        Box::pin(crate::remote_compact::compact_remote(
            self,
            model,
            messages,
            instructions,
            cancel,
        ))
    }
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
