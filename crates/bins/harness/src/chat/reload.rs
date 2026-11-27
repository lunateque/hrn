//! Live configuration reload slot and `hrn chat` entry point (250 ms debounced watcher).

use super::{ChatBackend, agents};
use crate::cli::ChatArgs;
use crate::config::Config;
use crate::config_layers::{LayerInput, MergedConfig};
use crate::config_watch::{LiveSettingsPort, spawn_watcher};
use crate::term::err_line;
use crate::wiring::{Background, build_runtime_with_mcp, build_runtime_with_tools, open_store};
use hrn_edge_llm::resolve_alias;
use hrn_edge_mcp::McpManager;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{SessionControl, discover_custom_commands};
use hrn_kernel::{AgentEvent, AgentStatus, HrnError, Result, SessionStore};
use hrn_tui::{Settings, UiEvent, UiSender};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, Once};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::runtime::Handle;

type PanicTarget = (Arc<dyn SessionStore>, Arc<Mutex<Option<String>>>);

static PANIC_HOOK_ONCE: Once = Once::new();
static ACTIVE_PANIC_TARGET: std::sync::LazyLock<Mutex<Option<PanicTarget>>> =
    std::sync::LazyLock::new(Mutex::default);

struct PanicTargetGuard;

impl PanicTargetGuard {
    fn install(store: Arc<dyn SessionStore>, last: Arc<Mutex<Option<String>>>) -> Self {
        if let Ok(mut slot) = ACTIVE_PANIC_TARGET.lock() {
            *slot = Some((store, last));
        }
        PANIC_HOOK_ONCE.call_once(|| {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if let Ok(slot) = ACTIVE_PANIC_TARGET.lock()
                    && let Some((store, last)) = slot.as_ref()
                    && let Ok(guard) = last.lock()
                    && let Some(sid) = guard.as_deref()
                {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
                    let _ = hrn_harness::record_fatal_exit(store.as_ref(), sid, now);
                }
                prev(info);
            }));
        });
        Self
    }
}

impl Drop for PanicTargetGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = ACTIVE_PANIC_TARGET.lock() {
            *slot = None;
        }
    }
}

/// Pending validated config reload produced by the background watcher.
#[derive(Clone, Default)]
pub(super) struct ReloadSlot {
    pending: Arc<Mutex<Option<Config>>>,
}

impl ReloadSlot {
    pub(super) fn take(&self) -> Option<Config> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub(super) fn store(&self, cfg: Config) {
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cfg);
    }
}

/// Start watching config layers and the active custom theme file for live reload in `hrn chat`.
fn watch(
    handle: &Handle,
    input: LayerInput,
    slot: ReloadSlot,
    settings_port: Arc<LiveSettingsPort>,
    session: Arc<Mutex<Option<String>>>,
    tx: UiSender,
) {
    let themes_dir = crate::app_data_dir().ok().map(|d| d.join("themes"));
    let profile = crate::paths::active_profile();
    let keymap_paths = crate::keybindings_paths();
    let tx_cfg = tx.clone();
    let tx_keys = tx.clone();
    let themes_dir_watch = themes_dir.clone();
    let keymap_paths_watch = keymap_paths.clone();
    spawn_watcher(handle, input, move |outcome| {
        let sid = session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_default();
        let notice = match outcome {
            Ok((mut cfg, merged)) => {
                cfg.apply_output_default(&merged, true);
                let mut appearance = cfg.appearance.clone();
                appearance.themes_dir.clone_from(&themes_dir);
                appearance.profile.clone_from(&profile);
                appearance.keymap_paths.clone_from(&keymap_paths);
                let _ = tx_cfg.send(UiEvent::Appearance(Box::new(appearance)));
                let _ = tx_cfg.send(UiEvent::KeymapReload);
                settings_port.replace_merged(merged);
                slot.store(cfg);
                "config reloaded (active for the next turn)".to_owned()
            }
            Err(err) => format!("config reload failed (kept previous values): {err}"),
        };
        let _ = tx_cfg.send(UiEvent::Agent(AgentEvent::StatusChanged {
            session_id: sid,
            status: AgentStatus::Working,
            message: Some(notice),
        }));
    });
    watch_active_theme(handle, tx, themes_dir_watch);
    watch_keybindings(handle, tx_keys, keymap_paths_watch);
}

fn file_stamps(list: &[PathBuf]) -> Vec<(bool, u64, Option<SystemTime>)> {
    list.iter()
        .map(|p| match std::fs::metadata(p) {
            Ok(m) => (true, m.len(), m.modified().ok()),
            Err(_) => (false, 0, None),
        })
        .collect()
}

/// Watch default and active-profile `keybindings.toml` files (250 ms debounce) and trigger a
/// live keymap reload whenever either layer changes on disk.
fn watch_keybindings(handle: &Handle, tx: UiSender, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    handle.spawn(async move {
        let mut last = file_stamps(&paths);
        let mut pending: Option<Vec<(bool, u64, Option<SystemTime>)>> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let now = file_stamps(&paths);
            if let Some(prev) = &pending {
                if *prev == now {
                    last = now;
                    pending = None;
                    if !tx.send(UiEvent::KeymapReload) {
                        break;
                    }
                } else {
                    pending = Some(now);
                }
            } else if now != last {
                pending = Some(now);
            }
        }
    });
}

/// Watch the active custom theme `.toml` file (250 ms cadence) using [`hrn_tui::ThemeWatcher`]
/// and live-reload edits, keeping the last-good palette on any parse/read failure.
fn watch_active_theme(handle: &Handle, tx: UiSender, themes_dir: Option<PathBuf>) {
    handle.spawn(async move {
        let mut watcher: Option<hrn_tui::ThemeWatcher> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let Some(active_path) = hrn_tui::active_theme_path() else {
                watcher = None;
                continue;
            };
            let matches_current = watcher
                .as_ref()
                .is_some_and(|w| w.path() == active_path.as_path());
            if !matches_current {
                watcher = Some(hrn_tui::ThemeWatcher::new(active_path, themes_dir.clone()));
                continue;
            }
            if let Some(w) = watcher.as_mut()
                && let Some(outcome) = w.poll()
            {
                let res = outcome.map(|()| hrn_tui::active_theme_name());
                if !tx.send(UiEvent::ThemeReloaded(res)) {
                    break;
                }
            }
        }
    });
}

impl ChatBackend {
    /// A backend over the database at `db_path`, starting new sessions in `cwd`, with MCP tools.
    #[cfg(test)]
    #[expect(
        clippy::too_many_arguments,
        reason = "constructor wires runtime dependencies"
    )]
    pub(crate) async fn open(
        cfg: &Config,
        config_path: PathBuf,
        db_path: &Path,
        cwd: PathBuf,
        args: &ChatArgs,
        tx: UiSender,
        handle: Handle,
        settings_port: Option<Arc<LiveSettingsPort>>,
    ) -> Result<(Self, McpManager, Vec<String>)> {
        Self::open_with_discovered(
            cfg,
            config_path,
            db_path,
            cwd,
            args,
            tx,
            handle,
            settings_port,
            Vec::new(),
        )
        .await
    }

    /// Opens a chat backend while seeding startup-discovered provider catalogs so they are visible
    /// to alias resolution and survive subsequent live config reloads.
    #[expect(
        clippy::too_many_arguments,
        reason = "constructor wires runtime dependencies"
    )]
    pub(crate) async fn open_with_discovered(
        cfg: &Config,
        config_path: PathBuf,
        db_path: &Path,
        cwd: PathBuf,
        args: &ChatArgs,
        tx: UiSender,
        handle: Handle,
        settings_port: Option<Arc<LiveSettingsPort>>,
        discovered: Vec<hrn_edge_llm::Catalog>,
    ) -> Result<(Self, McpManager, Vec<String>)> {
        let mut cfg = cfg.clone();
        let no_title_forced = args.no_title || args.no_session;
        if no_title_forced {
            cfg.auto_title = false;
            cfg.title_refresh_on_replan = false;
        }
        let mut background = Background::new();
        background.set_discovered(discovered);
        background.reapply_discovered(&mut cfg);
        crate::roles::apply(&mut cfg, &args.roles, args.thinking)?;
        let model = resolve_alias(&cfg.llm, args.model.as_deref())?;
        let advisor_model = match args.advisor_model.as_deref() {
            Some(alias) => Some(resolve_alias(&cfg.llm, Some(alias))?),
            None => cfg.advisor.model.clone(),
        };
        let advisor = cfg.advisor.enabled || args.advisor || args.advisor_model.is_some();
        let sqlite = open_store(db_path, args.no_session)?;
        if let Some(tier) = args.service_tier {
            background.overrides.set_service_tier(Some(tier));
        }
        if let Some(port) = settings_port.as_ref() {
            background = background.with_settings(Arc::clone(port) as _);
        }
        let (runtime, mcp_manager, warnings) =
            build_runtime_with_mcp(&mut cfg, Arc::clone(&sqlite), &cwd, &background).await?;
        let mcp_tools = mcp_manager.tool_set(cfg.mcp_tool_budget);
        let custom_commands = discover_custom_commands(&StdWorkspaceFs, &cwd).unwrap_or_default();
        let control = SessionControl::default();
        control.set_permission_mode(cfg.approval_mode);
        control.set_advisor(advisor, advisor_model.clone());
        let store: Arc<dyn SessionStore> = Arc::clone(&sqlite) as Arc<dyn SessionStore>;
        let lock_dir = (!args.no_session)
            .then(|| db_path.parent().map(|d| d.join("session-locks")))
            .flatten();
        let last: Arc<Mutex<Option<String>>> = Arc::default();
        let idle = crate::idle_compact::ChatIdleHandle::new(runtime.clone(), Arc::clone(&last));
        let remote_models: Vec<String> = hrn_edge_llm::enabled_presets(&cfg.llm)
            .into_iter()
            .map(|(alias, _)| alias.to_owned())
            .collect();
        let remote =
            super::remote::ChatRemoteState::new(tx.clone(), Arc::clone(&store), remote_models);
        let backend = Self {
            runtime,
            background,
            sqlite,
            store,
            cfg: cfg.clone(),
            model_names: crate::stats::names(&cfg.llm),
            config_path,
            mcp_tools,
            llm: cfg.llm.clone(),
            handle,
            tx,
            cwd,
            settings: Settings {
                goal: None,
                hide_thinking: args.hide_thinking || cfg.appearance.hide_thinking,
                model,
                thinking: crate::roles::thinking_for(&cfg, args.model.as_deref(), args.thinking),
                mode: cfg.approval_mode,
                advisor,
                advisor_model,
                service_tier: args.service_tier.unwrap_or(hrn_kernel::Tier::None),
            },
            session: None,
            control,
            cancel: None,
            last,
            custom_commands,
            mcp_statuses: mcp_manager.statuses().to_vec(),
            reload: ReloadSlot::default(),
            login_state: super::login::ActiveLogin::default(),
            login_reload: ReloadSlot::default(),
            lock_dir,
            lease: None,
            btw_cancel: None,
            loop_cancel: None,
            btw_active: Arc::default(),
            idle,
            focused_agent: None,
            focused_on_child: Arc::default(),
            saved_main_settings: None,
            no_title_forced,
            collab_host: None,
            auto_relay: None,
            collab_guest: None,
            remote,
            settings_port,
        };
        Ok((backend, mcp_manager, warnings))
    }

    /// Apply any validated config reload queued by the watcher, rebuilding the runtime without
    /// restarting MCP servers or background jobs.
    pub(super) fn apply_pending_reload(&mut self) {
        let Some(mut new_cfg) = self.reload.take() else {
            return;
        };
        if self.no_title_forced {
            new_cfg.auto_title = false;
            new_cfg.title_refresh_on_replan = false;
        }
        self.background.reapply_discovered(&mut new_cfg);
        self.background.pool.clear_all_invalidated();
        match build_runtime_with_tools(
            &new_cfg,
            Arc::clone(&self.sqlite),
            &self.cwd,
            &self.mcp_tools,
            &self.background,
        ) {
            Ok(new_runtime) => {
                self.runtime = new_runtime;
                self.idle.sync_runtime(&self.runtime);
                self.llm = new_cfg.llm.clone();
                self.remote.hub.set_models(
                    hrn_edge_llm::enabled_presets(&new_cfg.llm)
                        .into_iter()
                        .map(|(alias, _)| alias.to_owned())
                        .collect(),
                );
                self.model_names = crate::stats::names(&new_cfg.llm);
                self.cfg = new_cfg;
            }
            Err(err) => {
                let sid = self.session.clone().unwrap_or_default();
                let _ = self.tx.send(UiEvent::Agent(AgentEvent::StatusChanged {
                    session_id: sid,
                    status: AgentStatus::Working,
                    message: Some(format!(
                        "config reload failed (kept previous values): {err}"
                    )),
                }));
            }
        }
    }
}

/// Open the TUI and run until the user quits.
pub(crate) async fn run(
    cfg: &Config,
    layer_input: LayerInput,
    merged: MergedConfig,
    config_path: PathBuf,
    db_path: &Path,
    args: ChatArgs,
    discovered: Vec<hrn_edge_llm::Catalog>,
) -> Result<ExitCode> {
    let (tx, rx) = hrn_tui::channel();
    let cwd = std::env::current_dir()?;
    let port = LiveSettingsPort::new(layer_input.clone(), merged, config_path.clone());
    let (mut backend, mcp_manager, warnings) = ChatBackend::open_with_discovered(
        cfg,
        config_path,
        db_path,
        cwd,
        &args,
        tx,
        Handle::current(),
        Some(Arc::clone(&port)),
        discovered,
    )
    .await?;
    let startup = if let Some(sel) = args.from_claude.as_deref() {
        let id = crate::import_cmd::import_for_chat(
            &backend.sqlite,
            hrn_edge_store::ImportSource::Claude,
            sel,
        )?;
        crate::resume::StartupSession::Resume(id)
    } else if let Some(sel) = args.from_codex.as_deref() {
        let id = crate::import_cmd::import_for_chat(
            &backend.sqlite,
            hrn_edge_store::ImportSource::Codex,
            sel,
        )?;
        crate::resume::StartupSession::Resume(id)
    } else if let Some(sel) = args.from_omp.as_deref() {
        let id = crate::import_cmd::import_for_chat(
            &backend.sqlite,
            hrn_edge_store::ImportSource::Omp,
            sel,
        )?;
        crate::resume::StartupSession::Resume(id)
    } else {
        crate::resume::resolve_startup_session(
            backend.store.as_ref(),
            &backend.cwd,
            &args,
            cfg.auto_resume,
        )?
    };
    let mut options = backend.options_for_startup(&startup)?;
    if let Some(objective) = args.goal.as_deref() {
        // omp's `--goal`: the session starts in goal mode and begins working right away.
        let session_id = backend.session_for(objective)?;
        backend
            .runtime
            .start_goal(&session_id, objective, args.goal_budget, None)?;
        backend.settings.goal = backend.runtime.goal_of(&session_id)?;
        options.initial_prompt = Some(objective.to_owned());
    }
    let last = Arc::clone(&backend.last);
    let _panic_guard = PanicTargetGuard::install(Arc::clone(&backend.store), Arc::clone(&last));
    agents::watch(
        &Handle::current(),
        Arc::clone(backend.runtime.agents().inbox()),
        Arc::clone(backend.runtime.procs()),
        Arc::clone(backend.runtime.agents()),
        Arc::clone(&backend.store),
        backend.llm.clone(),
        Arc::clone(&last),
        backend.tx.clone(),
    );
    watch(
        &Handle::current(),
        layer_input,
        backend.reload.clone(),
        port,
        Arc::clone(&last),
        backend.tx.clone(),
    );
    crate::idle_compact::watch_chat(&Handle::current(), backend.idle.clone(), backend.tx.clone());
    let tui_res =
        tokio::task::spawn_blocking(move || hrn_tui::run(Box::new(backend), &rx, options))
            .await
            .map_err(|err| HrnError::Internal(format!("the TUI thread failed: {err}")));
    mcp_manager.shutdown().await;
    tui_res??;
    for warning in warnings {
        err_line(&format!("mcp warning: {warning}"));
    }
    if !args.no_session
        && let Some(id) = last.lock().ok().and_then(|id| id.clone())
    {
        err_line(&format!(
            "session {id} · resume with: hrn chat --resume {id}"
        ));
    }
    Ok(ExitCode::SUCCESS)
}
