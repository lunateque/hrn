//! `[eval]` module configuration parsed from `data/defaults.json` plus user overrides.

use crate::lang::Lang;
use hrn_kernel::serde::Deserialize;
use hrn_kernel::serde_json;
use hrn_kernel::{HrnError, Result};
use std::time::Duration;

/// Embedded `[eval]` defaults (kept inside `hrn-mod-eval` for strict module removability).
const DEFAULTS_JSON: &str = include_str!("../data/defaults.json");

/// Default per-cell timeout in seconds (`omp` parity).
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Minimum non-zero timeout in seconds.
pub const MIN_TIMEOUT_SECS: u64 = 1;
/// Maximum timeout in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 3600;
const MIN_TIMEOUT_F64: f64 = 1.0;
const MAX_TIMEOUT_F64: f64 = 3600.0;

#[derive(Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct RawDefaults {
    py: bool,
    js: bool,
    tools_enabled: bool,
    default_timeout_secs: u64,
}

/// `[eval]` module configuration table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields, default)]
pub struct EvalConfig {
    /// Enable the persistent Python (`py`) interpreter backend.
    pub py: bool,
    /// Enable the persistent JavaScript (`js`) interpreter backend.
    pub js: bool,
    /// Allow eval cells to register kernel-defined `@tool` functions for `task` subagents.
    pub tools_enabled: bool,
    /// Default per-cell timeout in seconds (`0` disables timeout; positive values clamp to `1..=3600`).
    pub default_timeout_secs: u64,
}

impl Default for EvalConfig {
    fn default() -> Self {
        if let Ok(d) = serde_json::from_str::<RawDefaults>(DEFAULTS_JSON) {
            Self {
                py: d.py,
                js: d.js,
                tools_enabled: d.tools_enabled,
                default_timeout_secs: d.default_timeout_secs,
            }
        } else {
            Self {
                py: true,
                js: true,
                tools_enabled: true,
                default_timeout_secs: DEFAULT_TIMEOUT_SECS,
            }
        }
    }
}

impl EvalConfig {
    /// Whether `lang` is enabled by `[eval]`.
    #[must_use]
    pub const fn is_lang_enabled(&self, lang: Lang) -> bool {
        match lang {
            Lang::Py => self.py,
            Lang::Js => self.js,
        }
    }

    /// Effective default timeout as `Option<Duration>` (`0` disables timeout, else clamped `1..=3600`).
    #[must_use]
    pub fn default_timeout(&self) -> Option<Duration> {
        let secs = self.default_timeout_secs;
        (secs > 0).then(|| Duration::from_secs(secs.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)))
    }

    /// Effective default timeout in seconds (`0` if disabled, else clamped `1..=3600`).
    #[must_use]
    pub fn effective_default_secs(&self) -> u64 {
        let secs = self.default_timeout_secs;
        if secs == 0 {
            0
        } else {
            secs.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)
        }
    }

    /// Resolve a per-call timeout override (in seconds) against this config's default.
    ///
    /// Negative or non-finite values are rejected. `0` disables timeout ONLY when
    /// `default_timeout_secs == 0` (preventing calls from bypassing an operator-configured timeout).
    /// Positive values clamp to `1..=3600`.
    ///
    /// # Errors
    /// Returns an error if `requested_secs` is negative, non-finite, or `0` while a finite default
    /// timeout is enforced.
    pub fn resolve_timeout(&self, requested_secs: Option<f64>) -> Result<Option<Duration>> {
        match requested_secs {
            Some(secs) if !secs.is_finite() || secs < 0.0 => Err(HrnError::Tool(
                "eval: `timeout` must be a non-negative finite number".into(),
            )),
            Some(0.0) => {
                if self.default_timeout_secs == 0 {
                    Ok(None)
                } else {
                    Err(HrnError::Tool(
                        "eval: `timeout: 0` is only permitted when `[eval] default_timeout_secs = 0`"
                            .into(),
                    ))
                }
            }
            Some(secs) => Ok(Some(Duration::from_secs_f64(
                secs.clamp(MIN_TIMEOUT_F64, MAX_TIMEOUT_F64),
            ))),
            None => Ok(self.default_timeout()),
        }
    }
}
