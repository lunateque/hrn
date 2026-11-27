//! `[commit]` and `[update]` configuration blocks lowered by `config.rs`.

use super::{Result, bad};
use serde::Deserialize;

/// `[commit]` block: attributed commit workflow settings.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitConfig {
    /// Enable `/commit` and `hrn commit` (default `true`).
    #[serde(default = "default_commit_enabled")]
    pub(crate) enabled: bool,
    /// Append a bullet under `## [Unreleased]` in `CHANGELOG.md` when committing (default `false`).
    #[serde(default)]
    pub(crate) changelog: bool,
}

const fn default_commit_enabled() -> bool {
    true
}

impl Default for CommitConfig {
    fn default() -> Self {
        Self {
            enabled: default_commit_enabled(),
            changelog: false,
        }
    }
}

/// `[update]` block: self-update repository and release channel (operator-only).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateConfig {
    /// GitHub `owner/repo` slug queried for releases (default `"lunateque/harness"`).
    #[serde(default = "default_update_repo")]
    pub(crate) repo: String,
    /// Release channel: `"stable"` (default) or `"canary"`.
    #[serde(default = "default_update_channel")]
    pub(crate) channel: String,
}

fn default_update_repo() -> String {
    "lunateque/harness".to_owned()
}

fn default_update_channel() -> String {
    "stable".to_owned()
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            repo: default_update_repo(),
            channel: default_update_channel(),
        }
    }
}

impl UpdateConfig {
    /// Validate `[update]` fields when lowering `config.toml`.
    pub(crate) fn validate(&self) -> Result {
        if !matches!(self.channel.as_str(), "stable" | "canary") {
            return Err(bad(format!(
                "[update] channel `{}` is not one of stable, canary",
                self.channel
            )));
        }
        Ok(())
    }
}
