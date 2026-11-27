//! Typed `[task]` configuration: subagent concurrency, recursion depth, request/runtime budgets,
//! idle TTL, tool shape toggles, isolation settings, and disabled profiles.
//!
//! Defaults live as data in `crates/harness/data/task.defaults.json`.

use hrn_kernel::{HrnError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Embedded baseline defaults for `[task]` (data file, never duplicated in code).
const DEFAULTS_JSON: &str = include_str!("../data/task.defaults.json");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTaskIsolationConfig {
    enabled: bool,
    merge: String,
    apply: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors flat [task] TOML/JSON schema toggles"
)]
struct RawTaskConfig {
    max_concurrency: usize,
    max_recursion_depth: usize,
    soft_request_budget: usize,
    max_runtime_ms: u64,
    agent_idle_ttl_ms: u64,
    #[serde(rename = "async")]
    async_mode: bool,
    batch: bool,
    steps: bool,
    enable_effort: bool,
    per_call_model: String,
    per_call_model_allow: Vec<String>,
    agent_models: BTreeMap<String, String>,
    structured: bool,
    isolation: RawTaskIsolationConfig,
    disabled_agents: Vec<String>,
}

/// Isolation settings under `[task.isolation]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskIsolationConfig {
    /// Enable git-worktree isolation when a task item requests `isolated: true`.
    pub enabled: bool,
    /// How isolated worktree changes are collected (`"patch"` or `"branch"`).
    pub merge: String,
    /// Automatically apply captured patches back into the parent working tree.
    pub apply: bool,
}

impl Default for TaskIsolationConfig {
    fn default() -> Self {
        default_task_config().isolation.clone()
    }
}

/// Configuration for the `task` subagent subsystem (`[task]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors flat [task] TOML/JSON schema toggles"
)]
pub struct TaskConfig {
    /// Maximum concurrently running subagents per root session tree.
    pub max_concurrency: usize,
    /// Maximum subagent nesting depth (`0` disables spawning, `2` allows parent -> child -> grandchild).
    pub max_recursion_depth: usize,
    /// Soft LLM request budget per subagent run (1.5x triggers a hard stop; `0` disables).
    pub soft_request_budget: usize,
    /// Maximum wall-clock runtime per subagent run in milliseconds (`0` = unlimited).
    pub max_runtime_ms: u64,
    /// Idle duration in milliseconds before a completed agent transitions from `Idle` to `Parked`.
    pub agent_idle_ttl_ms: u64,
    /// Run spawned tasks asynchronously in the background (`async` in config).
    #[serde(rename = "async")]
    pub async_mode: bool,
    /// Accept batched `tasks[]` arrays in one `task` call.
    pub batch: bool,
    /// Expose multi-step `steps[]` chains on `task` items.
    pub steps: bool,
    /// Expose per-item `effort` overrides on `task` items.
    pub enable_effort: bool,
    /// Gate per-call model overrides (`"off"`, `"eval"`, `"on"`).
    pub per_call_model: String,
    /// Optional allowlist of `provider/id` selectors allowed for per-call `model` overrides.
    pub per_call_model_allow: Vec<String>,
    /// Per-agent model selector overrides (`[task.agent_models] <agent> = "<selector>"`).
    pub agent_models: BTreeMap<String, String>,
    /// Expose structured `outputSchema` / `schemaMode` fields on `task` items.
    pub structured: bool,
    /// Worktree isolation settings (`[task.isolation]`).
    pub isolation: TaskIsolationConfig,
    /// Subagent profile names disabled from spawning.
    pub disabled_agents: Vec<String>,
}

fn load_defaults() -> TaskConfig {
    match serde_json::from_str::<RawTaskConfig>(DEFAULTS_JSON) {
        Ok(raw) => TaskConfig {
            max_concurrency: raw.max_concurrency,
            max_recursion_depth: raw.max_recursion_depth,
            soft_request_budget: raw.soft_request_budget,
            max_runtime_ms: raw.max_runtime_ms,
            agent_idle_ttl_ms: raw.agent_idle_ttl_ms,
            async_mode: raw.async_mode,
            batch: raw.batch,
            steps: raw.steps,
            enable_effort: raw.enable_effort,
            per_call_model: raw.per_call_model,
            per_call_model_allow: raw.per_call_model_allow,
            agent_models: raw.agent_models,
            structured: raw.structured,
            isolation: TaskIsolationConfig {
                enabled: raw.isolation.enabled,
                merge: raw.isolation.merge,
                apply: raw.isolation.apply,
            },
            disabled_agents: raw.disabled_agents,
        },
        // `task.defaults.json` is unit-tested; this fallback prevents any panic if edited badly.
        Err(_) => TaskConfig {
            max_concurrency: 32,
            max_recursion_depth: 2,
            soft_request_budget: 200,
            max_runtime_ms: 0,
            agent_idle_ttl_ms: 420_000,
            async_mode: true,
            batch: true,
            steps: false,
            enable_effort: false,
            per_call_model: "off".to_owned(),
            per_call_model_allow: Vec::new(),
            agent_models: BTreeMap::new(),
            structured: false,
            isolation: TaskIsolationConfig {
                enabled: false,
                merge: "patch".to_owned(),
                apply: false,
            },
            disabled_agents: Vec::new(),
        },
    }
}

fn default_task_config() -> &'static TaskConfig {
    static DEFAULTS: OnceLock<TaskConfig> = OnceLock::new();
    DEFAULTS.get_or_init(load_defaults)
}

impl Default for TaskConfig {
    fn default() -> Self {
        default_task_config().clone()
    }
}

impl TaskConfig {
    /// Hard LLM request stop limit: 1.5x [`Self::soft_request_budget`] (`0` when budget is disabled).
    pub const fn hard_request_budget(&self) -> usize {
        if self.soft_request_budget == 0 {
            0
        } else {
            self.soft_request_budget.saturating_mul(3).div_ceil(2)
        }
    }

    /// Whether `profile_name` is disabled by [`Self::disabled_agents`] (case-insensitive).
    pub fn is_agent_disabled(&self, profile_name: &str) -> bool {
        self.disabled_agents
            .iter()
            .any(|name| name.eq_ignore_ascii_case(profile_name))
    }

    /// Look up a configured `[task.agent_models]` override for `profile_name` or any of its
    /// `aliases` (case-insensitive).
    #[must_use]
    pub fn model_override_for(&self, profile_name: &str, aliases: &[String]) -> Option<&str> {
        self.agent_models
            .iter()
            .find(|(k, _)| {
                k.eq_ignore_ascii_case(profile_name)
                    || aliases.iter().any(|a| k.eq_ignore_ascii_case(a))
            })
            .map(|(_, v)| v.as_str())
    }

    /// Validate enum-like string fields and bounds.
    pub fn validate(&self) -> Result {
        if self.max_concurrency == 0 {
            return Err(HrnError::Config(
                "task.max_concurrency must be >= 1".to_owned(),
            ));
        }
        if !matches!(self.per_call_model.as_str(), "off" | "eval" | "on") {
            return Err(HrnError::Config(format!(
                "task.per_call_model `{}` must be one of: off, eval, on",
                self.per_call_model
            )));
        }
        if !matches!(self.isolation.merge.as_str(), "patch" | "branch") {
            return Err(HrnError::Config(format!(
                "task.isolation.merge `{}` must be one of: patch, branch",
                self.isolation.merge
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_json_parses_strictly_with_expected_values() {
        let raw: RawTaskConfig =
            serde_json::from_str(DEFAULTS_JSON).expect("task.defaults.json must parse strictly");
        assert_eq!(raw.max_concurrency, 32);
        assert_eq!(raw.max_recursion_depth, 2);
        assert_eq!(raw.soft_request_budget, 200);
        assert_eq!(raw.max_runtime_ms, 0);
        assert_eq!(raw.agent_idle_ttl_ms, 420_000);
        assert!(raw.async_mode);
        assert!(raw.batch);
        assert!(!raw.steps);
        assert!(!raw.enable_effort);
        assert_eq!(raw.per_call_model, "off");
        assert!(raw.per_call_model_allow.is_empty());
        assert!(raw.agent_models.is_empty());
        assert!(!raw.structured);
        assert!(!raw.isolation.enabled);
        assert_eq!(raw.isolation.merge, "patch");
        assert!(!raw.isolation.apply);
        assert!(raw.disabled_agents.is_empty());

        let cfg = TaskConfig::default();
        assert_eq!(cfg.hard_request_budget(), 300);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn partial_override_and_validation() {
        let cfg: TaskConfig = serde_json::from_str(
            r#"{"max_concurrency": 4, "async": false, "isolation": {"enabled": true}}"#,
        )
        .expect("partial task config");
        assert_eq!(cfg.max_concurrency, 4);
        assert!(!cfg.async_mode);
        assert!(cfg.isolation.enabled);
        assert_eq!(cfg.isolation.merge, "patch");
        assert_eq!(cfg.agent_idle_ttl_ms, 420_000);
        assert!(cfg.validate().is_ok());

        let bad_model = TaskConfig {
            per_call_model: "bogus".into(),
            ..TaskConfig::default()
        };
        assert!(bad_model.validate().is_err());
        let bad_merge = TaskConfig {
            isolation: TaskIsolationConfig {
                merge: "squash".into(),
                ..TaskIsolationConfig::default()
            },
            ..TaskConfig::default()
        };
        assert!(bad_merge.validate().is_err());
        let zero_conc = TaskConfig {
            max_concurrency: 0,
            ..TaskConfig::default()
        };
        assert!(zero_conc.validate().is_err());
    }
}
