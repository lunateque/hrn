//! `checkpoint`: working-tree snapshots with `checkpoint` / `rewind` tools and
//! `hrn checkpoints list|restore`. The engine (a shadow git repository per working directory)
//! lives in `hrn-edge-tools`; this module is the tool, hook and command surface.
//!
//! - `checkpoint` snapshots the tree and marks the conversation; `rewind` puts the files back to
//!   the snapshot and returns the conversation to the mark, keeping only the agent's report.
//! - Before a turn's first call of an editing tool the tree is snapshotted automatically, so
//!   `hrn checkpoints restore` (or `rewind` with that snapshot's id) can undo the whole turn.
//!
//! ```toml
//! [checkpoint]
//! auto = true                 # snapshot before a turn that edits files
//! timeout_secs = 60           # bound on each git call
//! tools = ["edit", "write"]   # which tool calls count as editing
//! exclude = ["target/"]       # gitignore patterns that are never snapshotted
//! ```
//!
//! Removable with `cargo xtask module remove checkpoint`.

mod cli;
mod hook;
mod tools;

use hrn_edge_tools::Checkpoints;
use hrn_kernel::serde::Deserialize;
use hrn_kernel::serde_json;
use hrn_kernel::{HrnError, Registry, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The defaults, as data.
const DEFAULTS: &str = include_str!("../defaults.json");

/// The resolved `[checkpoint]` settings.
#[derive(Debug, Deserialize)]
#[serde(crate = "hrn_kernel::serde")]
struct Config {
    auto: bool,
    timeout_secs: u64,
    tools: Vec<String>,
    exclude: Vec<String>,
}

/// What the user's `[checkpoint]` table may override.
#[derive(Debug, Default, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct Overrides {
    auto: Option<bool>,
    timeout_secs: Option<u64>,
    tools: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

impl Config {
    fn resolve(overrides: Overrides) -> Result<Self> {
        let mut config: Self = serde_json::from_str(DEFAULTS)
            .map_err(|e| HrnError::Internal(format!("checkpoint defaults.json: {e}")))?;
        config.auto = overrides.auto.unwrap_or(config.auto);
        config.timeout_secs = overrides.timeout_secs.unwrap_or(config.timeout_secs);
        config.tools = overrides.tools.unwrap_or(config.tools);
        config.exclude = overrides.exclude.unwrap_or(config.exclude);
        Ok(config)
    }
}

/// What every part of the module shares.
struct Shared {
    config: Config,
    /// Where the shadow repositories live.
    root: PathBuf,
    /// Serialises snapshots of this process; other processes meet git's own lock.
    busy: Mutex<()>,
}

impl Shared {
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.timeout_secs)
    }

    /// The snapshots of `cwd`, creating the shadow repository if there is none yet.
    fn writer(&self, cwd: &Path) -> Result<Checkpoints> {
        Checkpoints::open(&self.root, cwd, Some(&self.config.exclude), self.timeout())
    }

    /// The snapshots of `cwd` as they are (nothing is created).
    fn reader(&self, cwd: &Path) -> Result<Checkpoints> {
        Checkpoints::open(&self.root, cwd, None, self.timeout())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    fn for_test() -> Self {
        Self {
            config: Config::resolve(Overrides::default()).expect("default config"),
            root: PathBuf::new(),
            busy: Mutex::new(()),
        }
    }
}

const PROMPT: &str = "\
Before a risky change or a long investigation, call `checkpoint` with your goal; when you are \
done, call `rewind` with your findings: files return to the snapshot and the conversation to \
that point, keeping only your report. Every turn that edits files is also snapshotted \
automatically (`hrn checkpoints list`).";

/// Register `checkpoint`, `rewind`, the auto-snapshot hook and `hrn checkpoints`.
///
/// # Errors
///
/// When `[checkpoint]` is malformed or a name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let config = Config::resolve(registry.config()?)?;
    let shared = Arc::new(Shared {
        config,
        root: registry.host().data_dir.join("checkpoints"),
        busy: Mutex::new(()),
    });
    registry.tool(Arc::new(tools::CheckpointTool(Arc::clone(&shared))))?;
    registry.tool(Arc::new(tools::RewindTool(Arc::clone(&shared))))?;
    registry.hook(Arc::new(hook::AutoSnapshot::new(Arc::clone(&shared))));
    registry.command(cli::command(shared, registry.host().cwd.clone()))?;
    registry.prompt_section(PROMPT);
    Ok(())
}

#[cfg(test)]
mod tests;
