//! Multi-provider LLM wire adapters (Anthropic Messages, `OpenAI` `/responses`,
//! `OpenAI` `/chat/completions`, Gemini Developer API) + usage-aware fallback router, plus the
//! Claude Pro/Max and `ChatGPT` Plus/Pro subscription transports.

pub mod agy_acp;
mod anthropic;
mod anthropic_wire;
mod candidates;
mod catalog;
mod claude_code;
mod claude_sub;
mod codex;
mod common;
pub mod config;
mod config_pool;
mod discover;
mod dotenv;
mod gemini;
mod kimi_sub;
mod models_yml;
mod openai_chat;
mod openai_responses;
mod overflow;
mod prompt_variant;
mod provider_env;
mod remote_compact;
mod router;
mod router_agy;
mod router_keys;
mod router_pool;
mod sub_auth;
mod tier;
mod usage;
mod usage_claude;
mod usage_codex;
mod usage_kimi;
mod vision;
mod xxhash;
pub mod yaml;

pub use agy_acp::{
    AgyAcpRuntime, AgyAuthMethod, AgyDoctorReport, AgyProviderConfig, AgyToolMode,
    discover_agy_models, doctor_agy, login_agy,
};

pub use candidates::{
    CycleEntry, ResolvedSelector, effective_cycle_entries, enabled_presets, expand_selector,
    is_preset_enabled, resolve_alias, standard_role_names,
};
pub use catalog::{Catalog, EnvLookup, standard_catalogs, standard_catalogs_with_env};
pub use config::{
    AuthMode, LlmConfig, ModelPreset, ModelTag, ProviderEntry, ProviderKind, RolePreset,
    RoleStorage, validate_config,
};
pub use config_pool::{
    KimiApiFormat, LlmAnthropicConfig, LlmKimiConfig, LlmRetryConfig, LlmTierConfig, SubagentTier,
    default_extended_context,
};
pub use discover::{
    AGY_CACHE_TTL, DEFAULT_OLLAMA_TIMEOUT, build_agy_catalog, default_agy_provider,
    discover_ollama, discover_openrouter, discover_provider, discover_startup, load_cached_catalog,
    openai_models_list, save_cached_catalog,
};
pub use overflow::is_context_overflow;
pub use provider_env::{
    DEFAULT_OLLAMA_ROOT, ProviderEnvRule, normalize_ollama_root, ollama_root_url,
    provider_env_rules, rule_for,
};
pub use router::{LlmRouter, ThinkingLoopDetector};
pub use tier::{RouterOverrides, supports_service_tier, supports_slow_mode};
pub use usage::{
    AccountUsage, ProviderUsage, UsageReport, UsageWindow, fetch_usage, fetch_usage_with_pool,
    render_usage,
};
