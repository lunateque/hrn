//! Service-tier resolution, provider-family classification, Anthropic direct-host gating, and
//! session-scoped runtime overrides (`/fast`, `/slow`, `/extended-context`, `--service-tier`).

use crate::config::{AuthMode, LlmConfig, ProviderEntry, ProviderKind};
use crate::config_pool::LlmTierConfig;
use crate::openai_chat::is_openrouter_url;
use hrn_edge_http::guard;
use hrn_kernel::Tier;
use std::sync::{Arc, RwLock};

/// Provider family used to map `[llm.tier]` settings and validate `/fast` availability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderFamily {
    /// Direct `OpenAI`, ChatGPT Codex, or `OpenAI`-family models via `OpenRouter`.
    Openai,
    /// Direct Anthropic API (`api.anthropic.com`) or Claude Pro/Max subscription.
    Anthropic,
    /// Official Gemini Developer API (`generativelanguage.googleapis.com`).
    Google,
    /// Providers/gateways without service-tier support (Ollama, Groq, Bedrock, Vertex, …).
    Other,
}

/// True when `base_url` targets direct Anthropic (never Bedrock, Vertex, or `OpenRouter`).
#[must_use]
pub(crate) fn is_direct_anthropic_url(base_url: &str) -> bool {
    let Some(host) = guard::extract_host(base_url) else {
        return false;
    };
    let lower = host.to_ascii_lowercase();
    lower == "api.anthropic.com"
        || lower.ends_with(".anthropic.com")
        || matches!(lower.as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

fn is_openai_tier_url(base_url: &str, model_id: &str) -> bool {
    if is_openrouter_url(base_url) {
        return model_id.to_ascii_lowercase().starts_with("openai/");
    }
    let Some(host) = guard::extract_host(base_url) else {
        return false;
    };
    let lower = host.to_ascii_lowercase();
    lower == "api.openai.com" || lower == "chatgpt.com"
}

/// Classify `prov` + `model_id` into its service-tier provider family.
pub(crate) fn provider_family(prov: &ProviderEntry, model_id: &str) -> ProviderFamily {
    let base = prov.effective_base_url();
    match prov.kind {
        ProviderKind::Gemini => ProviderFamily::Google,
        ProviderKind::Anthropic => {
            if is_direct_anthropic_url(base) {
                ProviderFamily::Anthropic
            } else {
                ProviderFamily::Other
            }
        }
        ProviderKind::OpenaiResponses | ProviderKind::OpenaiChat => {
            if is_openai_tier_url(base, model_id) {
                ProviderFamily::Openai
            } else {
                ProviderFamily::Other
            }
        }
        ProviderKind::AgyAcp => ProviderFamily::Other,
    }
}

/// Whether `model` (or the default role when `None`) supports service tiers (`/fast`).
#[must_use]
pub fn supports_service_tier(cfg: &LlmConfig, model: Option<&str>) -> bool {
    let Ok(alias) = crate::candidates::resolve_alias(cfg, model) else {
        return false;
    };
    let Some(preset) = cfg.presets.get(&alias) else {
        return false;
    };
    let Some(prov) = cfg.providers.get(&preset.provider) else {
        return false;
    };
    provider_family(prov, &preset.id) != ProviderFamily::Other
}

/// Whether `model` (or the default role when `None`) supports Anthropic subscription slow mode.
#[must_use]
pub fn supports_slow_mode(cfg: &LlmConfig, model: Option<&str>) -> bool {
    let Ok(alias) = crate::candidates::resolve_alias(cfg, model) else {
        return false;
    };
    let Some(preset) = cfg.presets.get(&alias) else {
        return false;
    };
    let Some(prov) = cfg.providers.get(&preset.provider) else {
        return false;
    };
    prov.auth_mode == AuthMode::Claude
}

/// Resolve the effective wire [`Tier`] for one request against `(prov, model_id)`.
///
/// Precedence: explicit `request_tier` -> session `override_tier` -> `[llm.tier]` family default.
/// Returns `None` when the effective tier is [`Tier::None`] or unsupported by the provider family.
#[must_use]
pub(crate) fn resolve_service_tier(
    request_tier: Option<Tier>,
    override_tier: Option<Tier>,
    config: &LlmTierConfig,
    prov: &ProviderEntry,
    model_id: &str,
) -> Option<Tier> {
    let family = provider_family(prov, model_id);
    if family == ProviderFamily::Other {
        return None;
    }
    let raw = request_tier.or(override_tier).unwrap_or(match family {
        ProviderFamily::Openai => config.openai,
        ProviderFamily::Anthropic => config.anthropic,
        ProviderFamily::Google => config.google,
        ProviderFamily::Other => Tier::None,
    });
    match (family, raw) {
        (ProviderFamily::Openai, tier) if tier != Tier::None => Some(tier),
        (ProviderFamily::Anthropic | ProviderFamily::Google, Tier::Priority | Tier::Ultrafast) => {
            Some(Tier::Priority)
        }
        (ProviderFamily::Google, Tier::Flex) => Some(Tier::Flex),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct OverrideState {
    service_tier: Option<Tier>,
    slow_mode: Option<bool>,
    extended_context: Option<bool>,
}

/// Shared session-scoped runtime overrides (non-persisted): `/fast`, `/slow`, `/extended-context`,
/// `--service-tier`, and RPC `set_fast_mode` / `set_slow_mode`.
#[derive(Clone, Debug, Default)]
pub struct RouterOverrides {
    inner: Arc<RwLock<OverrideState>>,
}

impl RouterOverrides {
    /// Active session service-tier override, if any (`Some(Tier::None)` disables config defaults).
    #[must_use]
    pub fn service_tier(&self) -> Option<Tier> {
        self.read().service_tier
    }

    /// Set or clear the session service-tier override.
    pub fn set_service_tier(&self, tier: Option<Tier>) {
        self.write().service_tier = tier;
    }

    /// Active session slow-mode override, if any.
    #[must_use]
    pub fn slow_mode(&self) -> Option<bool> {
        self.read().slow_mode
    }

    /// Set or clear the session slow-mode override.
    pub fn set_slow_mode(&self, enabled: Option<bool>) {
        self.write().slow_mode = enabled;
    }

    /// Active session extended-context override, if any.
    #[must_use]
    pub fn extended_context(&self) -> Option<bool> {
        self.read().extended_context
    }

    /// Set or clear the session extended-context override.
    pub fn set_extended_context(&self, enabled: Option<bool>) {
        self.write().extended_context = enabled;
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, OverrideState> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, OverrideState> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "tier_tests.rs"]
mod tests;
