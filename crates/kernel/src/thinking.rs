//! The thinking ladder: effort levels and the per-model budget table that resolves a requested
//! level into one a concrete model can serve.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Reasoning / thinking effort level requested by the user or preset. The named levels are
/// omp's effort ladder (`packages/catalog/src/effort.ts`); `Auto` is the per-prompt classifier
/// omp's `:auto` selector and `--thinking auto` ask for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Pick the level per prompt: the magic keywords first, then a cheap model classifier.
    Auto,
    /// Disable extended thinking where supported.
    Off,
    /// Smallest reasoning budget (omp's `minimal`).
    Minimal,
    /// Low reasoning budget.
    Low,
    /// Balanced reasoning budget (default).
    #[default]
    Medium,
    /// High reasoning budget.
    High,
    /// Extra-high reasoning budget.
    Xhigh,
    /// The model's maximum reasoning budget (omp's `max`).
    Max,
}

impl ThinkingLevel {
    /// Cycle to the next effort level (`Shift+Tab` in TUI).
    pub const fn next(self) -> Self {
        match self {
            Self::Auto => Self::Off,
            Self::Off => Self::Minimal,
            Self::Minimal => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Xhigh,
            Self::Xhigh => Self::Max,
            Self::Max => Self::Auto,
        }
    }

    /// Short display label, also the serialized/config spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parse a user/config thinking level string.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "off" | "none" => Some(Self::Off),
            "minimal" | "min" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "extended" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

/// The `:level` suffix omp allows on a model selector (`off|minimal|low|medium|high|xhigh|max|
/// auto|inherit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelSuffix {
    /// No suffix (or one that is not a level, e.g. an ollama tag like `llama3.2:3b`).
    None,
    /// A concrete level the session should switch to.
    Level(ThinkingLevel),
    /// omp's `inherit`: switch the model, leave the thinking level as it is.
    Inherit,
}

/// Split omp's `:level` suffix off a model selector (`anthropic/opus:high` ->
/// `("anthropic/opus", Level(High))`). A selector without a level suffix passes through whole.
#[must_use]
pub fn split_level_suffix(selector: &str) -> (&str, LevelSuffix) {
    let Some((model, level)) = selector.rsplit_once(':') else {
        return (selector, LevelSuffix::None);
    };
    if model.is_empty() {
        return (selector, LevelSuffix::None);
    }
    if level.eq_ignore_ascii_case("inherit") {
        return (model, LevelSuffix::Inherit);
    }
    match ThinkingLevel::parse(level) {
        Some(parsed) => (model, LevelSuffix::Level(parsed)),
        None => (selector, LevelSuffix::None),
    }
}

/// Per-model thinking metadata from the config: omp's `thinking: { efforts, defaultLevel,
/// effortBudgets }` (`packages/catalog`, the shape a `models.yml` `thinking:` block declares).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ThinkingBudgets {
    /// omp's `thinking.efforts`: the levels this model accepts; the requested level is clamped
    /// into this ladder before it reaches the wire. Absent means every level is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<ThinkingLevel>>,
    /// omp's `thinking.defaultLevel`: the level `auto` resolves to for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_level: Option<ThinkingLevel>,
    /// omp's `thinking.effortBudgets`: reasoning token budgets per level, used by budget-style
    /// providers (Anthropic `thinking.budget_tokens`, Gemini `thinkingBudget`) instead of the
    /// built-in ladder.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effort_budgets: BTreeMap<ThinkingLevel, u32>,
}

impl ThinkingBudgets {
    /// The concrete level `requested` resolves to for this model: the requested one clamped
    /// into the supported ladder (nearest supported level), and `auto` replaced by the model's
    /// `default_level` (else `medium`).
    #[must_use]
    pub fn resolve(&self, requested: ThinkingLevel) -> ThinkingLevel {
        let level = match requested {
            ThinkingLevel::Auto => self.default_level.unwrap_or(ThinkingLevel::Medium),
            other => other,
        };
        let Some(efforts) = self.efforts.as_ref().filter(|e| !e.is_empty()) else {
            return level;
        };
        if efforts.contains(&level) {
            return level;
        }
        // Nearest supported level up the ladder, else the highest supported one below it.
        efforts
            .iter()
            .filter(|supported| **supported >= level)
            .min()
            .or_else(|| efforts.iter().max())
            .copied()
            .unwrap_or(level)
    }

    /// The reasoning token budget `level` should get on a budget-style provider: the configured
    /// `effortBudgets` table first, then `fallback` (the built-in ladder default).
    #[must_use]
    pub fn budget_for(
        &self,
        level: ThinkingLevel,
        fallback: impl FnOnce() -> Option<u32>,
    ) -> Option<u32> {
        self.effort_budgets.get(&level).copied().or_else(fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::ThinkingLevel as L;
    use super::*;

    #[test]
    fn every_level_round_trips_through_its_spelling() {
        for level in [
            L::Auto,
            L::Off,
            L::Minimal,
            L::Low,
            L::Medium,
            L::High,
            L::Xhigh,
            L::Max,
        ] {
            assert_eq!(L::parse(level.as_str()), Some(level));
        }
        assert_eq!(L::parse("legacy-max"), None);
    }

    #[test]
    fn auto_and_requests_clamp_into_the_supported_ladder() {
        let table = ThinkingBudgets {
            efforts: Some(vec![L::Low, L::Medium, L::High]),
            default_level: Some(L::Low),
            effort_budgets: BTreeMap::new(),
        };
        assert_eq!(table.resolve(L::Auto), L::Low);
        assert_eq!(
            table.resolve(L::Max),
            L::High,
            "above the ladder clamps down"
        );
        assert_eq!(table.resolve(L::Off), L::Low, "below the ladder clamps up");
        let unbounded = ThinkingBudgets::default();
        assert_eq!(unbounded.resolve(L::Auto), L::Medium);
        assert_eq!(unbounded.resolve(L::Max), L::Max);
    }

    #[test]
    fn configured_budgets_outrank_the_builtin_ladder() {
        let mut budgets = BTreeMap::new();
        budgets.insert(L::High, 1234_u32);
        let table = ThinkingBudgets {
            effort_budgets: budgets,
            ..ThinkingBudgets::default()
        };
        assert_eq!(table.budget_for(L::High, || Some(16_384)), Some(1234));
        assert_eq!(table.budget_for(L::Low, || Some(2048)), Some(2048));
        assert_eq!(table.budget_for(L::Low, || None), None);
    }
}
