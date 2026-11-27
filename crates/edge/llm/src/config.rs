//! Typed LLM configuration (`providers`, `presets`, `roles`, `fallback_chains`, `cycle_order`).

use crate::config_pool::{
    KimiApiFormat, LlmAnthropicConfig, LlmKimiConfig, LlmRetryConfig, LlmTierConfig,
    default_extended_context, validate_pool_config,
};
use hrn_edge_http::guard;
use hrn_edge_oauth::{AccountPolicy, OAuthConfig, Subscription};
use hrn_kernel::{HrnError, ModelPrice, Result, ThinkingLevel};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Wire protocol spoken by a configured provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible `/chat/completions` (`LiteLLM`, `OpenRouter`, GLM, Qwen, `DeepSeek`, Kimi).
    #[default]
    OpenaiChat,
    /// `OpenAI` `/responses` API (GPT / Codex with `prompt_cache_key` and `reasoning.effort`).
    OpenaiResponses,
    /// Anthropic Messages API (`cache_control: ephemeral`, `thinking` budgets).
    Anthropic,
    /// Official Gemini Developer API (`generativelanguage.googleapis.com`).
    Gemini,
    /// Official Antigravity CLI over ACP v1 (`agy acp --stdio`) + loopback MCP bridge.
    AgyAcp,
}

/// Authentication mechanism for a provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Use protocol default (`x-api-key` for Anthropic, `x-goog-api-key` for Gemini, `Bearer` for `OpenAI`).
    #[default]
    Default,
    /// Force protocol API-key header.
    ApiKey,
    /// Force `Authorization: Bearer <token>`.
    Bearer,
    /// Manage an `OAuth2` JSON token file with automatic refresh.
    OauthJson,
    /// Claude Pro/Max subscription login (`hrn login claude`); Anthropic kind only.
    Claude,
    /// `ChatGPT` Plus/Pro subscription login (`hrn login chatgpt`); `openai_responses` only.
    Chatgpt,
    /// Kimi Code subscription login (`hrn login kimi-code`); `/coding/v1` transport.
    #[serde(alias = "kimi-code")]
    KimiCode,
    /// Antigravity CLI managed subscription authentication (`hrn login agy`); `agy_acp` only.
    Agy,
}

impl AuthMode {
    /// The built-in subscription login this mode authenticates with, if any.
    pub const fn subscription(self) -> Option<Subscription> {
        match self {
            Self::Claude => Some(Subscription::Claude),
            Self::Chatgpt => Some(Subscription::Chatgpt),
            Self::KimiCode => Some(Subscription::KimiCode),
            Self::Default | Self::ApiKey | Self::Bearer | Self::OauthJson | Self::Agy => None,
        }
    }
}

/// Configuration for one named LLM backend under `[llm.providers.<name>]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProviderEntry {
    /// Wire protocol kind.
    pub kind: ProviderKind,
    /// Base URL for this provider.
    pub base_url: Option<String>,
    /// Authentication mode.
    pub auth_mode: AuthMode,
    /// Environment variable carrying the API key/token.
    pub token_env: Option<String>,
    /// Optional fallback API-key environment variables consulted in order after `token_env`
    /// (e.g. `MOONSHOT_API_KEY` -> `KIMI_API_KEY`).
    #[serde(default)]
    pub token_envs: Vec<String>,
    /// Dotenv file consulted for `token_env` when the process environment does not set it
    /// (omp's `~/.omp/agent/.env`).
    pub env_file: Option<String>,
    /// File path carrying the API key/token (never a consumer CLI store); for a subscription
    /// mode, the credential file written by `hrn login`.
    pub token_file: Option<String>,
    /// `OAuth2` refresh configuration (used when `auth_mode = "oauth_json"`).
    pub oauth: Option<OAuthConfig>,
    /// Extra HTTP headers loaded from environment variables (`Header-Name = "ENV_VAR"`).
    pub header_envs: BTreeMap<String, String>,
    /// Enable explicit prompt caching markers (Anthropic `cache_control: ephemeral`).
    pub prompt_cache: bool,
    /// Omit `temperature` from payloads (auto-enabled for Codex/`o1`/`o3`/`o4` on `/responses`).
    pub strip_temperature: bool,
    /// Enable subscription slow-mode lane for this provider (`[llm.anthropic] slow_mode` or per-provider).
    #[serde(default)]
    pub slow_mode: bool,
    /// Kimi Code wire format override (`auto | openai | anthropic`).
    #[serde(default)]
    pub api_format: KimiApiFormat,
    /// Request timeout in seconds (default 180).
    pub timeout_secs: Option<u64>,
    /// Antigravity CLI (`agy_acp`) provider settings.
    #[serde(default)]
    pub agy: crate::agy_acp::AgyProviderConfig,
}

impl ProviderEntry {
    /// Resolve default base URL for the provider kind.
    pub fn effective_base_url(&self) -> &str {
        if let Some(url) = self.base_url.as_deref()
            && !url.trim().is_empty()
        {
            return url.trim_end_matches('/');
        }
        if self.auth_mode == AuthMode::KimiCode {
            return "https://api.kimi.com/coding/v1";
        }
        match self.kind {
            ProviderKind::OpenaiResponses if self.auth_mode == AuthMode::Chatgpt => {
                "https://chatgpt.com/backend-api"
            }
            ProviderKind::OpenaiChat | ProviderKind::OpenaiResponses => "https://api.openai.com/v1",
            ProviderKind::Anthropic => "https://api.anthropic.com",
            ProviderKind::Gemini => "https://generativelanguage.googleapis.com",
            ProviderKind::AgyAcp => "acp://stdio",
        }
    }

    /// Resolve static token from `token_env` / `token_envs` (process environment, then `env_file`)
    /// or `token_file`.
    pub fn resolve_static_token(&self) -> Result<Option<String>> {
        for env_name in self
            .token_env
            .iter()
            .chain(self.token_envs.iter())
            .map(String::as_str)
        {
            let from_env = std::env::var(env_name).ok();
            let value = from_env
                .filter(|v| !v.trim().is_empty())
                .or_else(|| self.env_file_value(env_name));
            if let Some(val) = value {
                return Ok(Some(val.trim().to_owned()));
            }
        }
        if let Some(path) = self.token_file.as_deref() {
            guard::check_token_path(path)?;
            let raw = std::fs::read_to_string(path)
                .map_err(|e| HrnError::Auth(format!("read token_file `{path}`: {e}")))?;
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_owned()));
            }
        }
        Ok(None)
    }

    /// `name` from the dotenv `env_file`, if it is set there and not empty.
    pub(crate) fn env_file_value(&self, name: &str) -> Option<String> {
        let path = self.env_file.as_deref()?;
        guard::check_token_path(path).ok()?;
        let text = std::fs::read_to_string(path).ok()?;
        crate::dotenv::lookup(&text, name).filter(|v| !v.trim().is_empty())
    }

    /// Resolve extra HTTP headers from `header_envs`.
    pub fn resolve_env_headers(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (hdr, env_key) in &self.header_envs {
            if let Ok(val) = std::env::var(env_key) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    out.push((hdr.clone(), trimmed.to_owned()));
                }
            }
        }
        out
    }
}

/// Named model preset under `[llm.presets.<alias>]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelPreset {
    /// Key inside `[llm.providers]`.
    pub provider: String,
    /// Upstream model identifier.
    pub id: String,
    /// Optional default thinking effort for this preset.
    pub thinking: Option<ThinkingLevel>,
    /// omp's `thinking: { efforts, defaultLevel, effortBudgets }`: the levels this model
    /// accepts, what `auto` resolves to, and the per-level reasoning token budgets.
    #[serde(default)]
    pub thinking_budgets: Option<hrn_kernel::ThinkingBudgets>,
    /// Context window size in tokens (default `200_000`).
    pub context_window: Option<u64>,
    /// Optional extended context ceiling (omp `maxContextWindow`, consumed by 10d/10e).
    #[serde(default)]
    pub max_context_window: Option<u64>,
    /// Optional target selector for context-overflow promotion (omp `contextPromotionTarget`).
    #[serde(default)]
    pub context_promotion_target: Option<String>,
    /// Optional maximum output tokens advertised by the model (omp `maxTokens`).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Display name (omp's `name:`), when it differs from the id.
    pub name: Option<String>,
    /// Whether the model reasons; `Some(false)` sends no reasoning effort.
    pub reasoning: Option<bool>,
    /// Whether the model accepts inline image inputs (`None` infers from model id/family).
    #[serde(default)]
    pub vision: Option<bool>,
    /// Base-prompt variant for this model (`claude` | `gpt5`); unset picks the family table.
    #[serde(default)]
    pub prompt_variant: Option<String>,
    /// Per-million-token prices; absent means no cost is computed for this model.
    pub price: Option<ModelPrice>,
    /// Replay `reasoning_content` on assistant tool-call messages (required by Moonshot/Kimi).
    #[serde(default)]
    pub replay_reasoning_content: bool,
    /// Per-level wire effort override (e.g. xAI clamps `minimal -> low`, `xhigh/max -> high`).
    #[serde(default)]
    pub reasoning_effort_map: BTreeMap<ThinkingLevel, String>,
    /// Provider-specific thinking body shape (`"zai"`, `"openrouter"`, `"openai"`).
    #[serde(default)]
    pub thinking_format: Option<String>,
    /// Session cache header name sent on requests when `session_id` is set (e.g. `x-grok-conv-id`).
    #[serde(default)]
    pub prompt_cache_session_header: Option<String>,
    /// Optional per-model stream idle timeout in milliseconds (omp `compat.streamIdleTimeoutMs`).
    #[serde(default)]
    pub stream_idle_timeout_ms: Option<u64>,
}

/// Where interactive role changes (`/model`, `/modelpreset switch`) are persisted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleStorage {
    /// Persist to the user's global `~/.harness/config.toml` (default).
    #[default]
    Global,
    /// Persist only to the project-local `<cwd>/.harness/config.toml`.
    Project,
}

/// Display badge attached to a model or role (omp's `modelTags`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelTag {
    /// Short badge label shown next to the model or role.
    pub name: String,
    /// Optional color name or hex code for the badge.
    #[serde(default)]
    pub color: Option<String>,
}

/// Top-level `[llm]` configuration section.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LlmConfig {
    /// Named provider backends.
    pub providers: BTreeMap<String, ProviderEntry>,
    /// Named model presets (`opus`, `sonnet`, `codex`, `glm`, `qwen`, etc.).
    pub presets: BTreeMap<String, ModelPreset>,
    /// Semantic roles (`default`, `plan`, `smol`, `slow`, `reviewer`, ...) -> model/role selector.
    pub roles: BTreeMap<String, String>,
    /// Ordered fallback chains keyed by preset alias, role name, or `<provider>/*` wildcard.
    pub fallback_chains: BTreeMap<String, Vec<String>>,
    /// Display badges keyed by preset alias or role name (omp `modelTags`).
    #[serde(default)]
    pub model_tags: BTreeMap<String, ModelTag>,
    /// Ordered cycle list (role names or preset aliases) for quick model switching in TUI.
    pub cycle_order: Vec<String>,
    /// Named role bundles (omp's `modelPresets`): a snapshot of `[roles]` plus a default
    /// thinking level, switched as one unit with `/modelpreset` or `--preset`.
    #[serde(default)]
    pub role_presets: BTreeMap<String, RolePreset>,
    /// Optional allowlist of enabled models (path-scoped arrays resolved before lowering).
    #[serde(default)]
    pub enabled_models: Vec<String>,
    /// Optional list of disabled provider names (path-scoped arrays resolved before lowering).
    #[serde(default)]
    pub disabled_providers: Vec<String>,
    /// Where role reassignments persist (`global` or `project`).
    #[serde(default)]
    pub role_storage: RoleStorage,
    /// Pool retry & usage-reserve settings (`[llm.retry]`).
    #[serde(default)]
    pub retry: LlmRetryConfig,
    /// Per-account priority & reserve overrides (`[[llm.accounts]]`).
    #[serde(default)]
    pub accounts: Vec<AccountPolicy>,
    /// Per-provider-family default service tiers (`[llm.tier]`).
    #[serde(default)]
    pub tier: LlmTierConfig,
    /// Prefer each preset's `max_context_window` when available (`/extended-context`).
    #[serde(default = "default_extended_context")]
    pub extended_context: bool,
    /// Anthropic subscription transport toggles (`[llm.anthropic]`).
    #[serde(default)]
    pub anthropic: LlmAnthropicConfig,
    /// Kimi Code subscription transport toggles (`[llm.kimi]` / `[providers.kimi]`).
    #[serde(default)]
    pub kimi: LlmKimiConfig,
}

/// One named model-preset bundle: omp's `modelPresets.<name>` = `{ modelRoles,
/// defaultThinkingLevel }`, applied atomically by `/modelpreset switch`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct RolePreset {
    /// omp's `modelRoles`: the role assignments the bundle applies (role -> model selector,
    /// a `:level` suffix on the default role sets the level too).
    #[serde(default)]
    pub model_roles: BTreeMap<String, String>,
    /// omp's `defaultThinkingLevel`: the thinking level the bundle switches the session to.
    #[serde(default)]
    pub default_thinking_level: Option<ThinkingLevel>,
}

/// Validate the entire `[llm]` section at startup against `ToS` / Antigravity guardrails.
pub fn validate_config(cfg: &LlmConfig) -> Result {
    for (name, prov) in &cfg.providers {
        validate_provider(name, prov)?;
    }
    for (alias, preset) in &cfg.presets {
        let Some(prov) = cfg.providers.get(&preset.provider) else {
            return Err(HrnError::Config(format!(
                "preset `{alias}` references unknown provider `{}`",
                preset.provider
            )));
        };
        validate_preset(alias, preset, prov)?;
    }
    let known = |alias: &str, what: &str| -> Result {
        if cfg.presets.contains_key(alias) {
            Ok(())
        } else {
            Err(HrnError::Config(format!(
                "{what} references unknown preset `{alias}`"
            )))
        }
    };
    crate::candidates::validate_roles(cfg)?;
    crate::candidates::validate_fallback_chains(cfg)?;
    for entry in &cfg.cycle_order {
        let is_role =
            cfg.roles.contains_key(entry) || crate::candidates::role_fallbacks(entry).is_some();
        if !is_role {
            known(entry, "cycle_order")?;
        }
    }
    validate_pool_config(cfg)?;
    Ok(())
}

/// One provider against the guardrails.
pub(crate) fn validate_provider(name: &str, prov: &ProviderEntry) -> Result {
    if prov.kind == ProviderKind::AgyAcp {
        if !matches!(prov.auth_mode, AuthMode::Agy | AuthMode::Default) {
            return Err(HrnError::Config(format!(
                "provider `{name}`: kind = \"agy_acp\" only accepts auth_mode = \"agy\""
            )));
        }
        if prov
            .base_url
            .as_deref()
            .is_some_and(|u| !u.trim().is_empty())
            || prov.token_env.is_some()
            || !prov.token_envs.is_empty()
            || prov.env_file.is_some()
            || prov.token_file.is_some()
            || prov.oauth.is_some()
            || !prov.header_envs.is_empty()
        {
            return Err(HrnError::Config(format!(
                "provider `{name}`: kind = \"agy_acp\" delegates authentication and transport to \
                 the `agy` CLI; remove `base_url`, API keys, headers, and `oauth`"
            )));
        }
        return prov.agy.validate(name);
    }
    if prov.auth_mode == AuthMode::Agy {
        return Err(HrnError::Config(format!(
            "provider `{name}`: auth_mode = \"agy\" requires kind = \"agy_acp\""
        )));
    }
    let base = prov.effective_base_url();
    guard::check_outbound_url(base)?;
    if prov.kind == ProviderKind::Gemini {
        guard::check_gemini_base_url(base)?;
        if prov.auth_mode == AuthMode::OauthJson || prov.oauth.is_some() {
            return Err(HrnError::Config(format!(
                "provider `{name}`: OAuth is forbidden for Gemini provider"
            )));
        }
    }
    if let Some(path) = prov.token_file.as_deref() {
        guard::check_token_path(path)?;
    }
    if let Some(oauth) = &prov.oauth {
        guard::check_oauth_client_id(&oauth.client_id)?;
        guard::check_token_path(&oauth.token_file)?;
        guard::check_outbound_url(&oauth.token_url)?;
        if !oauth.token_url.to_ascii_lowercase().starts_with("https://") {
            return Err(HrnError::Config(format!(
                "provider `{name}`: oauth token_url must use https"
            )));
        }
    }
    if prov.auth_mode == AuthMode::OauthJson && prov.oauth.is_none() {
        return Err(HrnError::Config(format!(
            "provider `{name}`: auth_mode = \"oauth_json\" requires an `oauth` table"
        )));
    }
    validate_subscription(name, prov)
}

/// One model against the guardrails and its provider's billing.
pub(crate) fn validate_preset(alias: &str, preset: &ModelPreset, prov: &ProviderEntry) -> Result {
    guard::check_model_id(&preset.id)?;
    if let (Some(base), Some(max)) = (preset.context_window, preset.max_context_window)
        && max < base
    {
        return Err(HrnError::Config(format!(
            "model `{alias}`: max_context_window ({max}) cannot be smaller than context_window ({base})"
        )));
    }
    if let Some(price) = &preset.price {
        if !price.is_valid() {
            return Err(HrnError::Config(format!(
                "model `{alias}`: price entries must be finite, non-negative numbers"
            )));
        }
        if prov.auth_mode.subscription().is_some()
            || prov.kind == ProviderKind::AgyAcp
            || prov.auth_mode == AuthMode::Agy
        {
            return Err(HrnError::Config(format!(
                "model `{alias}`: a subscription login has no per-token billing; remove `price`"
            )));
        }
    }
    Ok(())
}

/// A subscription mode needs its own wire kind, the `hrn login` credential file and nothing else.
fn validate_subscription(name: &str, prov: &ProviderEntry) -> Result {
    let Some(kind) = prov.auth_mode.subscription() else {
        return Ok(());
    };
    let kind_ok = match kind {
        Subscription::Claude => prov.kind == ProviderKind::Anthropic,
        Subscription::Chatgpt => prov.kind == ProviderKind::OpenaiResponses,
        Subscription::KimiCode => {
            matches!(
                prov.kind,
                ProviderKind::OpenaiChat | ProviderKind::Anthropic
            )
        }
    };
    if !kind_ok {
        let wanted_name = match kind {
            Subscription::Claude => "anthropic",
            Subscription::Chatgpt => "openai_responses",
            Subscription::KimiCode => "openai_chat\" or \"anthropic",
        };
        return Err(HrnError::Config(format!(
            "provider `{name}`: auth_mode = \"{}\" requires kind = \"{wanted_name}\"",
            kind.id()
        )));
    }
    if prov.oauth.is_some() || prov.token_env.is_some() {
        return Err(HrnError::Config(format!(
            "provider `{name}`: auth_mode = \"{}\" uses the `hrn login {}` credentials; remove \
             api keys and the `oauth` table",
            kind.id(),
            kind.id()
        )));
    }
    if prov.token_file.is_none() {
        return Err(HrnError::Config(format!(
            "provider `{name}`: auth_mode = \"{}\" has no credential file",
            kind.id()
        )));
    }
    validate_subscription_base_url(name, kind, prov.effective_base_url())
}

/// Subscription bearer tokens may only be transmitted to the provider's official HTTPS origin
/// (or loopback for local mock tests), never to an arbitrary external host.
fn validate_subscription_base_url(name: &str, kind: Subscription, base_url: &str) -> Result {
    let host = guard::extract_host(base_url).ok_or_else(|| {
        HrnError::Config(format!(
            "provider `{name}`: invalid subscription base_url `{base_url}`"
        ))
    })?;
    let host_lc = host.to_ascii_lowercase();
    let is_loopback = matches!(host_lc.as_str(), "127.0.0.1" | "localhost" | "[::1]");
    if is_loopback {
        return Ok(());
    }
    let is_https = base_url
        .trim()
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"));
    let host_allowed = match kind {
        Subscription::Claude => host_lc == "api.anthropic.com",
        Subscription::Chatgpt => matches!(host_lc.as_str(), "chatgpt.com" | "api.openai.com"),
        Subscription::KimiCode => host_lc == "api.kimi.com",
    };
    if !is_https || !host_allowed {
        return Err(HrnError::Config(format!(
            "provider `{name}`: auth_mode = \"{}\" forbids base_url `{base_url}` (only official HTTPS host allowed)",
            kind.id()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub_provider(
        kind: ProviderKind,
        auth_mode: AuthMode,
        base_url: Option<&str>,
    ) -> ProviderEntry {
        ProviderEntry {
            kind,
            auth_mode,
            base_url: base_url.map(str::to_owned),
            token_file: Some("/tmp/hrn-auth/sub.json".into()),
            ..ProviderEntry::default()
        }
    }

    #[test]
    fn validate_subscription_rejects_non_official_external_base_urls() {
        assert!(
            validate_provider(
                "claude",
                &sub_provider(ProviderKind::Anthropic, AuthMode::Claude, None)
            )
            .is_ok()
        );
        assert!(
            validate_provider(
                "claude",
                &sub_provider(
                    ProviderKind::Anthropic,
                    AuthMode::Claude,
                    Some("https://api.anthropic.com"),
                )
            )
            .is_ok()
        );
        assert!(
            validate_provider(
                "claude",
                &sub_provider(
                    ProviderKind::Anthropic,
                    AuthMode::Claude,
                    Some("http://127.0.0.1:8080"),
                )
            )
            .is_ok()
        );
        assert!(
            validate_provider(
                "chatgpt",
                &sub_provider(
                    ProviderKind::OpenaiResponses,
                    AuthMode::Chatgpt,
                    Some("https://chatgpt.com/backend-api"),
                )
            )
            .is_ok()
        );
        assert!(
            validate_provider(
                "chatgpt",
                &sub_provider(
                    ProviderKind::OpenaiResponses,
                    AuthMode::Chatgpt,
                    Some("https://api.openai.com/v1"),
                )
            )
            .is_ok()
        );

        for bad in [
            "https://attacker.example.com",
            "https://api.anthropic.com.attacker.example.com",
            "http://api.anthropic.com",
        ] {
            assert!(
                validate_provider(
                    "claude",
                    &sub_provider(ProviderKind::Anthropic, AuthMode::Claude, Some(bad))
                )
                .is_err(),
                "claude must reject {bad}"
            );
        }
        for bad in [
            "https://attacker.example.com",
            "https://chatgpt.com.evil.example",
            "http://chatgpt.com/backend-api",
        ] {
            assert!(
                validate_provider(
                    "chatgpt",
                    &sub_provider(ProviderKind::OpenaiResponses, AuthMode::Chatgpt, Some(bad))
                )
                .is_err(),
                "chatgpt must reject {bad}"
            );
        }
    }

    #[test]
    fn validate_agy_acp_provider_and_preset_invariants() {
        let valid = ProviderEntry {
            kind: ProviderKind::AgyAcp,
            auth_mode: AuthMode::Agy,
            ..ProviderEntry::default()
        };
        assert!(validate_provider("agy", &valid).is_ok());

        let with_key = ProviderEntry {
            token_env: Some("AGY_KEY".into()),
            ..valid.clone()
        };
        assert!(validate_provider("agy", &with_key).is_err());

        let with_url = ProviderEntry {
            base_url: Some("https://example.com".into()),
            ..valid.clone()
        };
        assert!(validate_provider("agy", &with_url).is_err());

        let wrong_auth_on_openai = ProviderEntry {
            kind: ProviderKind::OpenaiChat,
            auth_mode: AuthMode::Agy,
            ..ProviderEntry::default()
        };
        assert!(validate_provider("openai", &wrong_auth_on_openai).is_err());

        let priced = ModelPreset {
            provider: "agy".into(),
            id: "claude-sonnet-4-6".into(),
            price: Some(ModelPrice {
                input: 3.0,
                output: 15.0,
                cache_read: None,
                cache_write: None,
            }),
            ..ModelPreset::default()
        };
        assert!(validate_preset("agy-sonnet", &priced, &valid).is_err());

        let free_preset = ModelPreset {
            provider: "agy".into(),
            id: "claude-sonnet-4-6".into(),
            ..ModelPreset::default()
        };
        assert!(validate_preset("agy-sonnet", &free_preset, &valid).is_ok());
    }
}
