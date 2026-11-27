//! Live config watcher (250 ms debounce, zero external watcher crate) and [`SettingsPort`]
//! adapter backing `cfg://` inside running sessions.

use crate::config::Config;
use crate::config_layers::{LayerInput, MergedConfig, insert_dotted, load_layers};
use crate::settings_schema::{parse_cli_value, schema, toml_to_json};
use crate::toml_write;
use hrn_harness::{SettingView, SettingsPort};
use hrn_kernel::{ApprovalDecision, ApprovalGate, ApprovalRequest, CancelFlag, HrnError, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::runtime::Handle;

/// Poll and debounce interval for config file live reload.
const WATCH_INTERVAL: Duration = Duration::from_millis(250);

/// Fingerprint of one watched file (follows symlinks via `std::fs::metadata`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    exists: bool,
    len: u64,
    modified: Option<SystemTime>,
}

fn stamp_of(path: &Path) -> FileStamp {
    match std::fs::metadata(path) {
        Ok(meta) => FileStamp {
            exists: true,
            len: meta.len(),
            modified: meta.modified().ok(),
        },
        Err(_) => FileStamp {
            exists: false,
            len: 0,
            modified: None,
        },
    }
}

fn snapshot_stamps(paths: &[PathBuf]) -> Vec<FileStamp> {
    paths.iter().map(|p| stamp_of(p)).collect()
}

/// Watched config file paths derived from `input`.
pub(crate) fn watched_paths(input: &LayerInput) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = &input.home {
        out.push(home.join(".omp").join("agent").join("config.yml"));
    }
    if let Some(p) = &input.global_path {
        out.push(p.clone());
    }
    if let Some(p) = &input.project_path {
        out.push(p.clone());
    }
    out.extend(input.overlays.iter().cloned());
    out
}

/// Spawn a debounced background poller that re-evaluates `input` when any watched file changes.
/// On parse/validation error the callback receives `Err(msg)` and the previous config stays live.
pub(crate) fn spawn_watcher<F>(handle: &Handle, input: LayerInput, mut on_change: F)
where
    F: FnMut(std::result::Result<(Config, MergedConfig), String>) + Send + 'static,
{
    let paths = watched_paths(&input);
    if paths.is_empty() {
        return;
    }
    handle.spawn(async move {
        let mut last = snapshot_stamps(&paths);
        let mut pending: Option<Vec<FileStamp>> = None;
        loop {
            tokio::time::sleep(WATCH_INTERVAL).await;
            let now = snapshot_stamps(&paths);
            if let Some(prev_pending) = &pending {
                if *prev_pending == now {
                    // Debounced: file stayed stable for 250 ms after change.
                    last = now;
                    pending = None;
                    let reloaded = load_layers(&input)
                        .and_then(|merged| {
                            let cfg = Config::from_merged(&merged, input.home.as_deref())?;
                            Ok((cfg, merged))
                        })
                        .map_err(|e| e.to_string());
                    on_change(reloaded);
                } else {
                    pending = Some(now);
                }
            } else if now != last {
                pending = Some(now);
            }
        }
    });
}

/// Live settings bridge handed to `hrn_harness::CfgScheme` so `read cfg://` and `write cfg://`
/// inspect and mutate the active session's layered settings.
pub(crate) struct LiveSettingsPort {
    input: Mutex<LayerInput>,
    merged: Mutex<MergedConfig>,
    global_path: PathBuf,
}

impl LiveSettingsPort {
    /// Wrap the loaded layers and target global config file.
    pub(crate) fn new(input: LayerInput, merged: MergedConfig, global_path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            input: Mutex::new(input),
            merged: Mutex::new(merged),
            global_path,
        })
    }

    /// Refresh the cached merged snapshot after a live reload.
    pub(crate) fn replace_merged(&self, next: MergedConfig) {
        *self
            .merged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
    }

    /// Snapshot of the current merged configuration and layer sources.
    pub(crate) fn merged_snapshot(&self) -> MergedConfig {
        self.merged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Path to the global config file edited by `/settings`.
    pub(crate) fn global_path(&self) -> &Path {
        &self.global_path
    }

    /// Re-evaluate all config layers from disk and update the cached merged snapshot.
    pub(crate) fn reload_from_disk(&self) -> Result<(Config, MergedConfig)> {
        let input = self
            .input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let merged = self.reload_locked(&input)?;
        let mut cfg = Config::from_merged(&merged, input.home.as_deref())?;
        cfg.apply_output_default(&merged, true);
        Ok((cfg, merged))
    }

    fn reload_locked(&self, input: &LayerInput) -> Result<MergedConfig> {
        let next = load_layers(input)?;
        self.replace_merged(next.clone());
        Ok(next)
    }
}

impl SettingsPort for LiveSettingsPort {
    fn list(&self) -> Vec<SettingView> {
        let merged = self
            .merged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        schema()
            .all()
            .iter()
            .map(|spec| build_view(spec, &merged))
            .collect()
    }

    fn get(&self, key: &str) -> Option<SettingView> {
        let spec = schema().get(key)?;
        let merged = self
            .merged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Some(build_view(spec, &merged))
    }

    fn similar(&self, key: &str) -> Vec<String> {
        schema().similar_keys(key)
    }

    fn set_session(&self, key: &str, raw_value: &str) -> Result<String> {
        let spec = schema()
            .get(key)
            .ok_or_else(|| HrnError::Tool(format!("unknown setting `{key}`")))?;
        let parsed = parse_cli_value(spec, raw_value)?;
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        insert_dotted(&mut input.runtime, key, parsed.clone());
        let _ = self.reload_locked(&input)?;
        Ok(format!("updated `{key}` for this session"))
    }

    fn save_global(&self, key: &str, raw_value: &str) -> Result<String> {
        let spec = schema()
            .get(key)
            .ok_or_else(|| HrnError::Tool(format!("unknown setting `{key}`")))?;
        let parsed = parse_cli_value(spec, raw_value)?;
        toml_write::set_key(&self.global_path, key, &parsed)?;
        let input = self
            .input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let merged = self.reload_locked(&input)?;
        if let Some(winner) = merged.overridden_above_global(key) {
            return Ok(format!(
                "saved `{key}` to {} (note: currently shadowed by {})",
                self.global_path.display(),
                winner.label()
            ));
        }
        Ok(format!("saved `{key}` to {}", self.global_path.display()))
    }

    fn confirm(
        &self,
        gate: &Arc<dyn ApprovalGate>,
        request: ApprovalRequest,
        cancel: CancelFlag,
    ) -> Result<ApprovalDecision> {
        let handle = Handle::try_current().map_err(|_| {
            HrnError::Tool("cannot prompt for `cfg://` write outside an async runtime".to_owned())
        })?;
        handle.block_on(gate.await_decision(request, cancel))
    }
}

fn build_view(spec: &crate::settings_schema::SettingSpec, merged: &MergedConfig) -> SettingView {
    let val = merged.get(&spec.key).or(spec.default.as_ref());
    SettingView {
        key: spec.key.clone(),
        value: val.map(toml_to_json),
        default: spec.default.as_ref().map(toml_to_json),
        kind: spec.kind.as_str().to_owned(),
        source: merged.source_of(&spec.key).label(),
        values: spec.values.clone(),
        description: spec.description.clone(),
        secret: spec.secret,
    }
}

/// Mutable slots updated in place when `hrn rpc` / `hrn acp` reloads config.
pub(crate) struct RpcWatchTargets {
    pub(crate) port: Arc<LiveSettingsPort>,
    pub(crate) runtime_slot: Arc<Mutex<hrn_harness::AgentRuntime>>,
    pub(crate) llm_slot: Arc<Mutex<hrn_edge_llm::LlmConfig>>,
    pub(crate) sqlite: Arc<hrn_edge_store::SqliteStore>,
    pub(crate) cwd: PathBuf,
    pub(crate) mcp_tools: hrn_edge_mcp::McpToolSet,
    pub(crate) background: crate::wiring::Background,
}

/// Start live config reload for `hrn rpc` / `hrn acp`, rebuilding the runtime in place on valid
/// edits and logging a warning on parse/validation errors while keeping last-good values.
pub(crate) fn watch_rpc(handle: &Handle, input: LayerInput, targets: RpcWatchTargets) {
    spawn_watcher(handle, input, move |res| match res {
        Ok((mut next_cfg, next_merged)) => {
            next_cfg.apply_output_default(&next_merged, false);
            targets.background.reapply_discovered(&mut next_cfg);
            targets.port.replace_merged(next_merged);
            targets.background.pool.clear_all_invalidated();
            match crate::wiring::build_runtime_with_tools(
                &next_cfg,
                Arc::clone(&targets.sqlite),
                &targets.cwd,
                &targets.mcp_tools,
                &targets.background,
            ) {
                Ok(next_rt) => {
                    *targets
                        .runtime_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = next_rt;
                    *targets
                        .llm_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = next_cfg.llm;
                }
                Err(err) => {
                    crate::term::err_line(&format!("config reload failed (kept previous): {err}"));
                }
            }
        }
        Err(err) => {
            crate::term::err_line(&format!("config reload failed (kept previous): {err}"));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn watcher_reloads_valid_edit_and_keeps_last_good_on_invalid_toml() {
        let dir = std::env::temp_dir().join(format!("hrn-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let global = dir.join("config.toml");
        let base = "\
[[providers]]
name = \"local\"
kind = \"openai_chat\"
base_url = \"http://127.0.0.1:1/v1\"

[models.m]
provider = \"local\"
id = \"demo\"
context_window = 128000

[roles]
default = \"m\"
";
        std::fs::write(&global, format!("{base}\n[tui]\ntheme = \"titanium\"\n")).expect("seed");

        let input = LayerInput {
            global_path: Some(global.clone()),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            ..LayerInput::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_watcher(&Handle::current(), input, move |outcome| {
            let _ = tx.send(outcome.map(|(cfg, _)| cfg.appearance.theme.dark));
        });

        // Give the watcher one tick to capture baseline, then write a valid update.
        tokio::time::sleep(Duration::from_millis(80)).await;
        std::fs::write(&global, format!("{base}\n[tui]\ntheme = \"cobalt\"\n")).expect("update");
        let first = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("first reload timeout")
            .expect("first event");
        assert_eq!(first.expect("valid reload"), "cobalt");

        // Write broken TOML -> watcher reports Err so callers keep previous config.
        std::fs::write(&global, "[tui\nbroken = [").expect("corrupt");
        let second = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("second reload timeout")
            .expect("second event");
        assert!(second.is_err(), "expected parse error, got {second:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
