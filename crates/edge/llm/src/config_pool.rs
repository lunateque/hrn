//! Account-pool retry policy, service-tier defaults, and Anthropic slow-mode config, loaded
//! from the embedded `data/accounts.toml` defaults file.

use crate::config::LlmConfig;
use hrn_kernel::{HrnError, Result, Tier};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::LazyLock;

const ACCOUNTS_DEFAULTS_TOML: &str = include_str!("../data/accounts.toml");

/// Raw deserialization shape for `data/accounts.toml` (no `#[serde(default)]`, so initializing
/// `DEFAULTS` never recurses into `Default::default()`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccountDefaults {
    extended_context: bool,
    retry: RawRetry,
    tier: RawTier,
    anthropic: RawAnthropic,
    kimi: RawKimi,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRetry {
    usage_reserve_pct: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTier {
    openai: Tier,
    anthropic: Tier,
    google: Tier,
    subagent: SubagentTier,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnthropic {
    slow_mode: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKimi {
    api_format: KimiApiFormat,
}

#[derive(Clone, Debug)]
struct AccountDefaults {
    extended_context: bool,
    retry: LlmRetryConfig,
    tier: LlmTierConfig,
    anthropic: LlmAnthropicConfig,
    kimi: LlmKimiConfig,
}

static DEFAULTS: LazyLock<AccountDefaults> = LazyLock::new(|| {
    toml::from_str::<RawAccountDefaults>(ACCOUNTS_DEFAULTS_TOML).map_or_else(
        |_| AccountDefaults {
            extended_context: false,
            retry: LlmRetryConfig {
                usage_reserve_pct: hrn_edge_oauth::DEFAULT_RESERVE_PCT,
            },
            tier: LlmTierConfig {
                openai: Tier::None,
                anthropic: Tier::None,
                google: Tier::None,
                subagent: SubagentTier::Inherit,
            },
            anthropic: LlmAnthropicConfig { slow_mode: false },
            kimi: LlmKimiConfig {
                api_format: KimiApiFormat::Auto,
            },
        },
        |raw| AccountDefaults {
            extended_context: raw.extended_context,
            retry: LlmRetryConfig {
                usage_reserve_pct: raw.retry.usage_reserve_pct,
            },
            tier: LlmTierConfig {
                openai: raw.tier.openai,
                anthropic: raw.tier.anthropic,
                google: raw.tier.google,
                subagent: raw.tier.subagent,
            },
            anthropic: LlmAnthropicConfig {
                slow_mode: raw.anthropic.slow_mode,
            },
            kimi: LlmKimiConfig {
                api_format: raw.kimi.api_format,
            },
        },
    )
});

/// Default value of `[llm] extended_context` from `data/accounts.toml`.
#[must_use]
pub fn default_extended_context() -> bool {
    DEFAULTS.extended_context
}

/// `[llm.retry]` pool reserve settings.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LlmRetryConfig {
    /// Protected remaining-quota threshold (0..=100) for account selection when no per-account
    /// policy overrides it (default `10`).
    pub usage_reserve_pct: u8,
}

impl Default for LlmRetryConfig {
    fn default() -> Self {
        DEFAULTS.retry.clone()
    }
}

/// Service-tier setting for subagents: either inherit the parent session's tier or force one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubagentTier {
    /// Inherit the parent session's active tier.
    #[default]
    Inherit,
    /// Override subagent requests to a specific [`Tier`].
    Explicit(Tier),
}

impl SubagentTier {
    /// Canonical lowercase wire/config string (`"inherit"` or [`Tier::as_str`]).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Explicit(t) => t.as_str(),
        }
    }

    /// Parse `"inherit"` or any valid [`Tier`] string.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("inherit") {
            Some(Self::Inherit)
        } else {
            Tier::parse(trimmed).map(Self::Explicit)
        }
    }

    /// Explicit tier override for subagent requests, or `None` when inheriting.
    #[must_use]
    pub const fn override_tier(self) -> Option<Tier> {
        match self {
            Self::Inherit => None,
            Self::Explicit(t) => Some(t),
        }
    }
}

impl Serialize for SubagentTier {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SubagentTier {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown subagent tier `{s}` (expected inherit|none|auto|default|flex|scale|priority|ultrafast)"
            ))
        })
    }
}

/// `[llm.tier]` per-provider-family default service tiers.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LlmTierConfig {
    /// Default service tier for `OpenAI` / Codex / `OpenAI`-family `OpenRouter` models.
    pub openai: Tier,
    /// Default service tier for direct Anthropic / Claude models.
    pub anthropic: Tier,
    /// Default service tier for Gemini Developer API models.
    pub google: Tier,
    /// Service tier for subagent turns (`"inherit"` or an explicit [`Tier`]).
    pub subagent: SubagentTier,
}

impl Default for LlmTierConfig {
    fn default() -> Self {
        DEFAULTS.tier.clone()
    }
}

/// `[llm.anthropic]` subscription transport options.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LlmAnthropicConfig {
    /// Enable Claude subscription low-priority recovery lane (`/slow`).
    pub slow_mode: bool,
}

impl Default for LlmAnthropicConfig {
    fn default() -> Self {
        DEFAULTS.anthropic.clone()
    }
}

/// Wire protocol format for Kimi Code (`/coding/v1`): OpenAI `/chat/completions` or Anthropic
/// `/messages` (`auto` follows the provider kind, defaulting to OpenAI-compatible).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KimiApiFormat {
    /// Choose based on provider kind (`anthropic` -> Anthropic Messages, else OpenAI chat).
    #[default]
    Auto,
    /// Use OpenAI-compatible `/coding/v1/chat/completions`.
    Openai,
    /// Use Anthropic-compatible `/coding/v1/messages`.
    Anthropic,
}

impl KimiApiFormat {
    /// Canonical lowercase configuration string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

/// `[llm.kimi]` / `[providers.kimi]` Kimi Code transport options.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LlmKimiConfig {
    /// Wire format for Kimi Code (`/coding/v1`): `"auto" | "openai" | "anthropic"`.
    pub api_format: KimiApiFormat,
}

impl Default for LlmKimiConfig {
    fn default() -> Self {
        DEFAULTS.kimi.clone()
    }
}

/// Validate `[llm.retry]` and `[[llm.accounts]]` settings.
pub(crate) fn validate_pool_config(cfg: &LlmConfig) -> Result {
    if cfg.retry.usage_reserve_pct > 100 {
        return Err(HrnError::Config(format!(
            "llm.retry.usage_reserve_pct ({}) must be in 0..=100",
            cfg.retry.usage_reserve_pct
        )));
    }
    for (idx, policy) in cfg.accounts.iter().enumerate() {
        if policy.provider.trim().is_empty() {
            return Err(HrnError::Config(format!(
                "llm.accounts[{idx}].provider cannot be empty"
            )));
        }
        if policy.account.trim().is_empty() {
            return Err(HrnError::Config(format!(
                "llm.accounts[{idx}].account cannot be empty"
            )));
        }
        if let Some(pct) = policy.reserve_pct
            && pct > 100
        {
            return Err(HrnError::Config(format!(
                "llm.accounts[{idx}].reserve_pct ({pct}) must be in 0..=100"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_accounts_defaults_parse_cleanly() {
        let parsed: RawAccountDefaults =
            toml::from_str(ACCOUNTS_DEFAULTS_TOML).expect("accounts.toml parses");
        assert!(!parsed.extended_context);
        assert_eq!(parsed.retry.usage_reserve_pct, 10);
        assert_eq!(parsed.tier.openai, Tier::None);
        assert_eq!(parsed.tier.anthropic, Tier::None);
        assert_eq!(parsed.tier.google, Tier::None);
        assert_eq!(parsed.tier.subagent, SubagentTier::Inherit);
        assert!(!parsed.anthropic.slow_mode);
        // Verify Default impls read DEFAULTS without recursion.
        assert_eq!(LlmRetryConfig::default().usage_reserve_pct, 10);
        assert_eq!(LlmTierConfig::default().subagent, SubagentTier::Inherit);
        assert!(!LlmAnthropicConfig::default().slow_mode);
    }

    #[test]
    fn subagent_tier_round_trips_inherit_and_explicit() {
        assert_eq!(SubagentTier::parse("inherit"), Some(SubagentTier::Inherit));
        assert_eq!(
            SubagentTier::parse("priority"),
            Some(SubagentTier::Explicit(Tier::Priority))
        );
        assert_eq!(SubagentTier::Inherit.override_tier(), None);
        assert_eq!(
            SubagentTier::Explicit(Tier::Flex).override_tier(),
            Some(Tier::Flex)
        );
    }
}
