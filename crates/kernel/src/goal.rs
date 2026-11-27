//! Goal mode: the tracked objective and its token/time budget (omp's `Goal`,
//! `packages/tui/src/tools/goal.ts`).

use serde::{Deserialize, Serialize};

/// Lifecycle state of a tracked goal-mode objective (omp's `Goal`,
/// `packages/tui/src/tools/goal.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoalStatus {
    /// Working toward the objective; the runtime auto-continues while enabled.
    Active,
    /// The user paused it; no continuation until resumed.
    Paused,
    /// The token budget ran out; wrap-up guidance replaces continuation.
    BudgetLimited,
    /// The agent verified every deliverable against current evidence.
    Complete,
    /// Discarded without completing.
    Dropped,
}

impl GoalStatus {
    /// Short display label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::BudgetLimited => "budget-limited",
            Self::Complete => "complete",
            Self::Dropped => "dropped",
        }
    }

    /// Parse a persisted status string.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "budget-limited" => Some(Self::BudgetLimited),
            "complete" => Some(Self::Complete),
            "dropped" => Some(Self::Dropped),
            _ => None,
        }
    }

    /// Whether this status still spends budget (omp's accounting predicate).
    pub const fn is_accounting(self) -> bool {
        matches!(self, Self::Active | Self::BudgetLimited)
    }
}

/// A goal-mode objective with its token/time budget (omp's `Goal`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// Session-stable identifier.
    pub id: String,
    /// The objective the user set.
    pub objective: String,
    /// Lifecycle state.
    pub status: GoalStatus,
    /// omp's `tokenBudget`: the goal's total token ceiling (`None`: unbounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// Wall-clock budget in seconds (hrn addition; omp tracks time but budgets only tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_budget_secs: Option<u64>,
    /// omp's `tokensUsed`: prompt (incl. cache writes) + completion tokens spent on the goal.
    pub tokens_used: u64,
    /// omp's `timeUsedSeconds`: wall-clock seconds spent while accounting.
    pub time_used_seconds: u64,
    /// Creation timestamp (Unix ms).
    pub created_at_ms: i64,
    /// Last update timestamp (Unix ms).
    pub updated_at_ms: i64,
}

impl Goal {
    /// Tokens left before the budget is exhausted (`None` when unbounded).
    #[must_use]
    pub const fn remaining_tokens(&self) -> Option<u64> {
        match self.token_budget {
            Some(budget) => Some(budget.saturating_sub(self.tokens_used)),
            None => None,
        }
    }
}
