//! The one place where edge crates and the harness are plugged together.

use crate::config::Config;
use crate::mcp;
use crate::mcp_urls::McpScheme;
use crate::modules;
use crate::paths;
use crate::url_adapters::{DiskAttachmentAdapter, RemoteSshTransport, out_event_emitter};
use hrn_edge_http::HttpClient;
use hrn_edge_llm::{Catalog, LlmConfig, LlmRouter, RouterOverrides};
use hrn_edge_mcp::{McpManager, McpResources, McpToolSet};
use hrn_edge_oauth::{CredentialPool, OAuthManager};
use hrn_edge_store::{HistoryAgentEntry, HistoryScheme, SqliteStore};
use hrn_edge_tools::{DiskAttachmentStore, PngSnapRenderer, SkillScheme, StdWorkspaceFs};
use hrn_harness::{
    AgentRegistry, AgentRuntime, AgentScheme, ArtifactStore, AttachmentScheme, CfgScheme,
    HarnessConfig, OutScheme, ProcScheme, RuleScheme, SettingsPort, Spawn, SshScheme, SubagentTool,
    ToolRegistry, WaitTool, discover_skills,
};
use hrn_kernel::{
    BoxFuture, CancelFlag, HelpScheme, Host, HrnError, LlmProvider, ProcSource, ProcTable,
    Registry, Result, SessionStore, Shared, UrlRouter, UrlScheme, WorkspaceFs as _,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Open (creating if needed) the session database at `path`; with `no_session` an empty
/// in-memory database that dies with the process (nothing is read, nothing is kept).
pub(crate) fn open_store(path: &Path, no_session: bool) -> Result<Arc<SqliteStore>> {
    if no_session {
        return SqliteStore::open_in_memory().map(Arc::new);
    }
    SqliteStore::open(path).map(Arc::new)
}

/// Runs background agents on the tokio runtime the calling turn is already on.
struct TokioSpawner;

impl Spawn for TokioSpawner {
    fn spawn(&self, task: BoxFuture<()>) -> Result {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            HrnError::Config(format!("no async runtime for background agents: {e}"))
        })?;
        drop(handle.spawn(task));
        Ok(())
    }

    fn cancel_after(&self, flag: CancelFlag, delay: std::time::Duration) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        drop(handle.spawn(async move {
            tokio::time::sleep(delay).await;
            flag.cancel();
        }));
    }
}

/// Everything that lives as long as the process: background agents and the values modules keep
/// across rebuilds (`bash` jobs and services, the HTTP client). A hot-reload (`/login`) hands the
/// old one to the rebuilt runtime, so running agents, jobs and services survive it.
#[derive(Clone)]
pub(crate) struct Background {
    pub(crate) agents: Arc<AgentRegistry>,
    /// Shared subscription account pool (survives live config reloads and shares `/usage` cache).
    pub(crate) pool: Arc<CredentialPool>,
    /// Session-scoped runtime overrides (`/fast`, `/slow`, `/extended-context`, `--service-tier`).
    pub(crate) overrides: RouterOverrides,
    shared: Arc<Shared>,
    settings: Arc<std::sync::Mutex<Option<Arc<dyn SettingsPort>>>>,
    mcp_resources: Arc<std::sync::Mutex<McpResources>>,
    discovered: Arc<std::sync::Mutex<Option<Vec<Catalog>>>>,
}

impl Background {
    pub(crate) fn new() -> Self {
        Self {
            agents: Arc::new(AgentRegistry::new(Arc::new(TokioSpawner))),
            pool: Arc::new(CredentialPool::default()),
            overrides: RouterOverrides::default(),
            shared: Arc::new(Shared::new()),
            settings: Arc::new(std::sync::Mutex::new(None)),
            mcp_resources: Arc::new(std::sync::Mutex::new(McpResources::empty())),
            discovered: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Seed startup-discovered provider catalogs so hot-reloads preserve them without re-probing.
    pub(crate) fn set_discovered(&self, catalogs: Vec<Catalog>) {
        *self
            .discovered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(catalogs);
    }

    /// Re-absorb cached startup-discovered catalogs into `cfg`, respecting `disabled_providers`.
    pub(crate) fn reapply_discovered(&self, cfg: &mut Config) {
        let cached = self
            .discovered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(catalogs) = cached {
            absorb_filtered(&mut cfg.llm, &catalogs);
        }
    }

    /// Run startup discovery once (or reuse already-seeded catalogs) and absorb into `cfg`.
    pub(crate) async fn discover_and_absorb(&self, cfg: &mut Config) -> Result {
        if let Some(cached) = self
            .discovered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            absorb_filtered(&mut cfg.llm, &cached);
            return Ok(());
        }
        let http = self.http()?;
        let cache_dir = crate::app_data_dir().ok().map(|d| d.join("catalog-cache"));
        let discovered =
            hrn_edge_llm::discover_startup(&http, &cfg.llm, cache_dir.as_deref()).await;
        self.set_discovered(discovered.clone());
        absorb_filtered(&mut cfg.llm, &discovered);
        Ok(())
    }

    /// Attach a [`SettingsPort`] so the assembled runtime exposes `cfg://`.
    pub(crate) fn with_settings(self, port: Arc<dyn SettingsPort>) -> Self {
        *self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(port);
        self
    }

    fn settings(&self) -> Option<Arc<dyn SettingsPort>> {
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Store the live MCP resource registry so `mcp://` and `/mcp resources` survive reloads.
    pub(crate) fn set_mcp_resources(&self, resources: McpResources) {
        *self
            .mcp_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = resources;
    }

    /// Current shared MCP resource registry.
    pub(crate) fn mcp_resources(&self) -> McpResources {
        self.mcp_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Shared disk-backed session attachment store (`attachment://`).
    pub(crate) fn attachments(&self, data_dir: &Path) -> Result<Arc<DiskAttachmentStore>> {
        self.shared
            .get_or_init(|| Ok(DiskAttachmentStore::new(data_dir.to_path_buf())))
    }

    /// The process-wide HTTP client (one connection pool for the LLM router, MCP and the `web`
    /// module).
    fn http(&self) -> Result<HttpClient> {
        Ok(self.shared.get_or_init(HttpClient::new)?.as_ref().clone())
    }
}

/// Build a runtime from the registered modules plus `task`, persisting into `store`.
pub(crate) fn build_runtime(cfg: &Config, store: Arc<SqliteStore>) -> Result<AgentRuntime> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    assemble_runtime(cfg, store, &cwd, &McpToolSet::default(), &Background::new())
}

/// Build a runtime from the registered modules, `task`, and an already connected MCP tool set
/// (used by `/login` hot-reload so MCP servers stay connected without restarting).
pub(crate) fn build_runtime_with_tools(
    cfg: &Config,
    store: Arc<SqliteStore>,
    cwd: &Path,
    mcp_tools: &McpToolSet,
    background: &Background,
) -> Result<AgentRuntime> {
    assemble_runtime(cfg, store, cwd, mcp_tools, background)
}

/// Absorb startup-discovered `catalogs` into `llm`, filtering out any provider currently listed in
/// `llm.disabled_providers`.
pub(crate) fn absorb_filtered(llm: &mut LlmConfig, catalogs: &[Catalog]) {
    let disabled: Vec<String> = llm.disabled_providers.clone();
    let is_disabled = |name: &str| disabled.iter().any(|p| p.eq_ignore_ascii_case(name));
    for cat in catalogs {
        let mut filtered = cat.clone();
        filtered.providers.retain(|name, _| !is_disabled(name));
        filtered
            .presets
            .retain(|_, preset| !is_disabled(&preset.provider));
        filtered
            .roles
            .retain(|_, target| filtered.presets.contains_key(target));
        if !filtered.providers.is_empty() || !filtered.presets.is_empty() {
            llm.absorb(filtered);
        }
    }
}

/// Load layered config and absorb startup-discovered providers (`ollama`, `openrouter`, and
/// auto-detected `agy_acp`), bootstrapping zero-config machines where an auto-discovered provider
/// supplies the only default model.
pub(crate) async fn load_config_with_discovery(
    overlays: &[PathBuf],
) -> Result<(
    Config,
    crate::config_layers::LayerInput,
    crate::config_layers::MergedConfig,
    PathBuf,
    Vec<Catalog>,
)> {
    let cwd = std::env::current_dir()?;
    let input = crate::layer_input(overlays, &cwd)?;
    let global = crate::global_config_path()?;
    let no_files =
        !global.is_file() && overlays.is_empty() && !paths::project_config_path(&cwd).is_file();
    let cache_dir = crate::app_data_dir().ok().map(|d| d.join("catalog-cache"));

    match Config::load_layered(&input) {
        Ok((mut cfg, merged)) => {
            let http = HttpClient::new()?;
            let discovered =
                hrn_edge_llm::discover_startup(&http, &cfg.llm, cache_dir.as_deref()).await;
            absorb_filtered(&mut cfg.llm, &discovered);
            Ok((cfg, input, merged, global, discovered))
        }
        Err(first_err) => {
            if let Ok(http) = HttpClient::new() {
                let mut probe_llm = LlmConfig::default();
                if let Ok(merged) = crate::config_layers::load_layers(&input)
                    && let Some(arr) = merged
                        .root
                        .get("disabled_providers")
                        .and_then(toml::Value::as_array)
                {
                    probe_llm.disabled_providers = arr
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned)
                        .collect();
                }
                let discovered =
                    hrn_edge_llm::discover_startup(&http, &probe_llm, cache_dir.as_deref()).await;
                if !discovered.is_empty() {
                    let mut catalogs = crate::config::model_catalogs(input.home.as_deref());
                    catalogs.extend(discovered.clone());
                    if let Ok((cfg, merged)) = Config::load_layered_with_catalogs(&input, catalogs)
                    {
                        return Ok((cfg, input, merged, global, discovered));
                    }
                }
            }
            if no_files {
                Err(HrnError::Config(format!(
                    "no config at `{}` · run `hrn login` to configure a provider ({first_err})",
                    global.display()
                )))
            } else {
                Err(first_err)
            }
        }
    }
}

/// Build a runtime from the registered modules, `task`, and all discovered/configured MCP tools.
///
/// Returns `(runtime, mcp_manager, discovery_warnings)`. Offline MCP servers degrade gracefully
/// and are reported through `mcp_manager.warnings()`.
pub(crate) async fn build_runtime_with_mcp(
    cfg: &mut Config,
    store: Arc<SqliteStore>,
    cwd: &Path,
    background: &Background,
) -> Result<(AgentRuntime, McpManager, Vec<String>)> {
    background.discover_and_absorb(cfg).await?;
    let http = background.http()?;
    let (servers, mut warnings) = mcp::resolve_servers(cfg, cwd);
    let cache = mcp::tool_cache();
    let manager = McpManager::open(servers, http, cache.as_ref()).await;
    background.set_mcp_resources(manager.resources());
    warnings.extend(manager.warnings());
    let runtime = assemble_runtime(
        cfg,
        store,
        cwd,
        &manager.tool_set(cfg.mcp_tool_budget),
        background,
    )?;
    Ok((runtime, manager, warnings))
}

fn assemble_runtime(
    cfg: &Config,
    store: Arc<SqliteStore>,
    cwd: &Path,
    mcp_tools: &McpToolSet,
    background: &Background,
) -> Result<AgentRuntime> {
    let router = Arc::new(
        LlmRouter::try_new(cfg.llm.clone(), background.http()?, OAuthManager::default())?
            .with_pool(Arc::clone(&background.pool))
            .with_overrides(background.overrides.clone()),
    );
    let data_dir = crate::app_data_dir()?;
    let artifacts = Arc::new(ArtifactStore::new(data_dir.clone()));
    background.agents.set_artifacts(Arc::clone(&artifacts));
    background
        .agents
        .set_idle_ttl_ms(cfg.task.agent_idle_ttl_ms);
    let host = Host {
        cwd: cwd.to_path_buf(),
        data_dir: data_dir.clone(),
    };
    let eval_host = Arc::new(crate::eval_host::RuntimeEvalHost::new());
    let registry = Registry::new(
        host,
        cfg.module_tables.clone(),
        Arc::clone(&background.shared),
    )
    .with_llm(Arc::clone(&router) as Arc<dyn LlmProvider>)
    .with_eval_host(Arc::clone(&eval_host) as Arc<dyn hrn_kernel::EvalHost>);
    let registered = modules::registry(registry)?.finish()?;
    let mut tools = assemble_tools(
        cfg.premium_defer,
        registered.tools,
        registered.deferrable,
        registered.deferred,
        mcp_tools,
    );

    let eval_hints = background
        .shared
        .get_or_init(|| Ok(hrn_edge_tools::EvalRuntimeHints::default()))?;
    eval_hints.set_max_concurrency(cfg.task.max_concurrency);
    eval_hints.set_desktop_tool_registered(tools.get("desktop").is_some());
    let edit_hint = background
        .shared
        .get_or_init(|| Ok(hrn_edge_tools::EditModeHint::new()))?;

    let mut procs: Vec<Arc<dyn ProcSource>> = vec![Arc::clone(&background.agents) as _];
    procs.extend(registered.procs);
    let procs = Arc::new(ProcTable::new(procs));
    let grant_store: Arc<dyn SessionStore> = Arc::clone(&store) as _;
    let grants = background.shared.get_or_init(|| {
        Ok(hrn_harness::GrantCache::with_store(
            grant_store,
            Arc::new(hrn_kernel::SystemClock),
        ))
    })?;
    let urls = assemble_urls(
        Arc::clone(&store) as _,
        data_dir,
        background,
        &procs,
        registered.help_pages,
        registered.schemes,
        &grants,
        cfg.out_enabled,
    )?;
    let mut base = AgentRuntime::new(
        router,
        store,
        tools.clone(),
        Arc::new(StdWorkspaceFs),
        harness_config(
            cfg,
            registered.prompt_sections,
            edit_hint.recover_inline_edits(),
        ),
    )
    .with_grants(grants)
    .with_urls(Arc::new(urls))
    .with_agents(Arc::clone(&background.agents))
    .with_isolator(Arc::new(crate::task_isolation::GitWorktreeIsolator::new(
        Some(artifacts),
    )))
    .with_snap_renderer(Arc::new(PngSnapRenderer))
    .with_procs(procs)
    .with_todos(
        background
            .shared
            .get_or_init(|| Ok(hrn_kernel::todo::TodoBoards::new()))?,
    );
    for hook in registered.hooks {
        base = base.with_hook(hook);
    }
    let dyn_tools_slot = background
        .shared
        .get_or_init(|| Ok(hrn_edge_tools::EvalDynamicToolsSlot::default()))?;
    if dyn_tools_slot.is_enabled() {
        base = base.with_dynamic_tools(Arc::new(crate::eval_host::SharedDynamicToolResolver::new(
            dyn_tools_slot,
        )));
    }
    // `task` needs a runtime to spawn children from, so it joins the registry afterwards.
    tools.register(Arc::new(WaitTool::new(Arc::clone(base.procs()))));
    tools.register(Arc::new(SubagentTool::new(base.clone())));
    eval_hints.set_registered_tool_names(tools.names());
    let runtime = base.with_tools(tools);
    eval_host.bind(&runtime);
    Ok(runtime)
}

fn harness_config(
    cfg: &Config,
    module_sections: Vec<String>,
    recover_inline_edits: bool,
) -> HarnessConfig {
    HarnessConfig {
        system_prompt_override: cfg.system_prompt_override.clone(),
        append_system_override: cfg.append_system_override.clone(),
        module_sections,
        advisor: cfg.advisor.clone(),
        context: cfg.sources.clone(),
        task: cfg.task.clone(),
        compaction: hrn_harness::CompactionPolicy::from_config(cfg.compaction.clone()),
        context_promotion: cfg.context_promotion,
        subagent_tier: cfg.llm.tier.subagent.override_tier(),
        approval_policy: cfg.approval_policy.clone(),
        recover_inline_edits,
        auto_title: cfg.auto_title,
        title_refresh_on_replan: cfg.title_refresh_on_replan,
        ..HarnessConfig::default()
    }
}

fn assemble_tools(
    premium_defer: bool,
    module_tools: Vec<Arc<dyn hrn_kernel::Tool>>,
    deferrable: Vec<(String, String, Arc<dyn hrn_kernel::Tool>)>,
    deferred: Vec<hrn_kernel::DeferredTool>,
    mcp_tools: &McpToolSet,
) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    // The hidden tools (omp's HIDDEN_TOOLS): the runtime activates their schemas per session
    // state; they are callable the moment they appear.
    tools.register(Arc::new(hrn_harness::goal::GoalTool));
    tools.register(Arc::new(hrn_harness::ThinkTool));
    tools.register(Arc::new(hrn_harness::YieldTool));
    let mut premium = Vec::new();
    for (group, name, tool) in deferrable {
        if premium_defer {
            premium.push(hrn_kernel::DeferredTool {
                group,
                short_name: name,
                tool,
            });
        } else {
            tools.register(tool);
        }
    }
    tools.extend_tools(module_tools);
    tools.defer(deferred);
    tools.defer(premium);
    tools.extend_tools(mcp_tools.loaded.iter().cloned());
    tools.defer(mcp_tools.deferred.iter().cloned());
    tools
}

#[expect(
    clippy::too_many_arguments,
    reason = "single assembly seam connecting every built-in and module URL scheme"
)]
fn assemble_urls(
    history: Arc<dyn SessionStore>,
    data_dir: std::path::PathBuf,
    background: &Background,
    procs: &Arc<ProcTable>,
    help_pages: hrn_kernel::HelpPages,
    module_schemes: Vec<Arc<dyn UrlScheme>>,
    grants: &hrn_harness::GrantCache,
    out_enabled: bool,
) -> Result<UrlRouter> {
    let attachments = background.attachments(&data_dir)?;
    let hosts_path = paths::remote_hosts_path(StdWorkspaceFs.home_dir().as_deref())
        .unwrap_or_else(|_| data_dir.join("remote-hosts.json"));
    let rt_handle = tokio::runtime::Handle::try_current().ok();
    let mut urls = [
        Arc::new(SkillScheme::new(Arc::new(|cwd| {
            Ok(discover_skills(&StdWorkspaceFs, cwd)?
                .into_values()
                .map(|skill| (skill.name, skill.path))
                .collect())
        }))) as Arc<dyn UrlScheme>,
        Arc::new(RuleScheme::new(Arc::new(|cwd| {
            Ok(hrn_harness::Rulebook::discover(&StdWorkspaceFs, cwd)?
                .rules()
                .values()
                .map(|rule| (rule.name.clone(), rule.body.clone()))
                .collect())
        }))),
        Arc::new(history_scheme(
            history,
            data_dir,
            Arc::clone(&background.agents),
        )),
        Arc::new(AgentScheme::new(Arc::clone(&background.agents))),
        Arc::new(ProcScheme::new(Arc::clone(procs))),
        Arc::new(HelpScheme::new(help_pages)),
        Arc::new(SshScheme::new(Arc::new(RemoteSshTransport::new(
            hosts_path,
            rt_handle.clone(),
        )))),
        Arc::new(McpScheme::new(background.mcp_resources(), rt_handle)),
        Arc::new(AttachmentScheme::new(Arc::new(DiskAttachmentAdapter::new(
            attachments,
        )))),
    ]
    .into_iter()
    .fold(UrlRouter::new(), UrlRouter::with_builtin);
    if out_enabled {
        urls = urls.with_builtin(Arc::new(OutScheme::with_emitter(out_event_emitter())));
    }
    if let Some(port) = background.settings() {
        let cfg_scheme = Arc::new(CfgScheme::new(port));
        cfg_scheme.bind_grant_cache(grants);
        urls = urls.with_builtin(cfg_scheme);
    }
    for scheme in module_schemes {
        if scheme.scheme().eq_ignore_ascii_case("out") {
            return Err(HrnError::Config(format!(
                "module URL scheme `{}://` cannot shadow built-in scheme",
                scheme.scheme()
            )));
        }
        urls = urls.try_with_module(scheme)?;
    }
    Ok(urls)
}

fn history_scheme(
    store: Arc<dyn SessionStore>,
    data_dir: std::path::PathBuf,
    agents: Arc<AgentRegistry>,
) -> HistoryScheme {
    HistoryScheme::new(store)
        .with_data_dir(data_dir)
        .with_live_agents(Arc::new(move |root| {
            agents
                .in_tree(root)
                .into_iter()
                .map(|a| {
                    let s = a.snapshot();
                    HistoryAgentEntry {
                        id: a.id.clone(),
                        status: s.status.as_str().to_owned(),
                        summary: format!("[{}] {}", a.profile, a.task),
                        session_id: s.session_id,
                    }
                })
                .collect()
        }))
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
