//! The `[hooks]` config table: the event → command map and the loader knobs, with the defaults
//! shipped as data (`defaults.json`), not Rust tables.

use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{HrnError, Registry, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The defaults, as data.
const DEFAULTS: &str = include_str!("../defaults.json");

/// The lifecycle events a user hook may subscribe to; a `[hooks]` key that is neither a knob nor
/// one of these is a typo and fails the launch.
pub(crate) const EVENTS: [&str; 5] = [
    "pre_tool",
    "post_tool",
    "turn_end",
    "session_start",
    "user_prompt",
];

/// The resolved `[hooks]` settings.
#[derive(Debug)]
pub(crate) struct Config {
    pub(crate) enabled: bool,
    pub(crate) timeout_secs: u64,
    pub(crate) max_output_bytes: usize,
    pub(crate) discover_tools: bool,
    pub(crate) discover_claude_settings: bool,
    /// Event name → commands, from `[hooks] pre_tool = [...]`.
    pub(crate) events: BTreeMap<String, Vec<String>>,
    /// Hook executables added by `--hook <path>` (injected into the table by the binary).
    pub(crate) extra: Vec<PathBuf>,
}

/// Read and validate the `[hooks]` table of `registry`; the defaults come from `defaults.json`.
pub(crate) fn load(registry: &mut Registry) -> Result<Config> {
    let table: BTreeMap<String, Value> = registry.config()?;
    let defaults: BTreeMap<String, Value> = serde_json::from_str(DEFAULTS)
        .map_err(|e| HrnError::Config(format!("hooks defaults.json: {e}")))?;
    let scalar = |key: &str| -> Result<Value> {
        table.get(key).cloned().map_or_else(
            || {
                defaults
                    .get(key)
                    .cloned()
                    .ok_or_else(|| HrnError::Config(format!("hooks defaults.json has no `{key}`")))
            },
            Ok,
        )
    };
    let bool_of = |key: &str| -> Result<bool> {
        scalar(key)?
            .as_bool()
            .ok_or_else(|| HrnError::Config(format!("[hooks] `{key}` must be a boolean")))
    };
    let number_of = |key: &str| -> Result<u64> {
        scalar(key)?
            .as_u64()
            .ok_or_else(|| HrnError::Config(format!("[hooks] `{key}` must be a positive integer")))
    };
    let knobs = [
        "enabled",
        "timeout_secs",
        "max_output_bytes",
        "discover_tools",
        "discover_claude_settings",
        "extra",
    ];
    let mut events = BTreeMap::new();
    for (key, value) in &table {
        if knobs.contains(&key.as_str()) {
            continue;
        }
        if !EVENTS.contains(&key.as_str()) {
            return Err(HrnError::Config(format!(
                "[hooks] unknown key `{key}` (expected one of: {}, or an event name)",
                EVENTS.join(", ")
            )));
        }
        let commands = value.as_array().ok_or_else(|| {
            HrnError::Config(format!("[hooks] `{key}` must be a list of commands"))
        })?;
        let mut list = Vec::with_capacity(commands.len());
        for command in commands {
            let text = command.as_str().ok_or_else(|| {
                HrnError::Config(format!("[hooks] `{key}` entries must be command strings"))
            })?;
            list.push(text.to_owned());
        }
        events.insert(key.clone(), list);
    }
    let extra = match table.get("extra") {
        None => Vec::new(),
        Some(value) => {
            let items = value.as_array().ok_or_else(|| {
                HrnError::Config("[hooks] `extra` must be a list of paths".to_owned())
            })?;
            items
                .iter()
                .map(|item| {
                    item.as_str().map(PathBuf::from).ok_or_else(|| {
                        HrnError::Config("[hooks] `extra` entries must be paths".to_owned())
                    })
                })
                .collect::<Result<Vec<_>>>()?
        }
    };
    Ok(Config {
        enabled: bool_of("enabled")?,
        timeout_secs: number_of("timeout_secs")?,
        max_output_bytes: usize::try_from(number_of("max_output_bytes")?)
            .map_err(|_| HrnError::Config("[hooks] `max_output_bytes` is too large".to_owned()))?,
        discover_tools: bool_of("discover_tools")?,
        discover_claude_settings: bool_of("discover_claude_settings")?,
        events,
        extra,
    })
}
