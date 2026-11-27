//! `hrn --mode rpc|acp` and `hrn rpc|acp`: the stdio protocol fronts over the real runtime.
//!
//! The `Shared` type implements the front crate's `Backend` seam over `AgentRuntime`: sessions share the
//! store, every protocol session gets its own live `SessionControl` and cancellation, and turns
//! run the same settled loop `hrn chat` drives by hand.

use crate::cli::ServeArgs;
use crate::config::Config;
use crate::config_layers::{LayerInput, MergedConfig};
use crate::config_watch::{LiveSettingsPort, RpcWatchTargets, watch_rpc};
use crate::term::err_line;
use crate::wiring::{Background, build_runtime_with_mcp, open_store};
use hrn_edge_llm::{LlmConfig, RouterOverrides, resolve_alias};
use hrn_edge_mcp::McpManager;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{AgentRuntime, CustomCommand, SessionControl, discover_custom_commands};
use hrn_kernel::{
    BoxFuture, CancelFlag, DagNode, DagPayload, EventSink, HrnError, PermissionMode, Result,
    SessionMeta, SessionStore, ThinkingLevel, Usage,
};
use hrn_rpc::{
    Backend, CommandInfo, CompactionReport, ModelEntry, NewSession, SessionPatch, TurnRequest,
    TurnSummary, model_row_json,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

#[path = "rpc_prompt.rs"]
mod rpc_prompt;
#[path = "rpc_tiers.rs"]
mod rpc_tiers;
use rpc_tiers::{Recorder, Recording, text_of};

/// Fallback context window when a preset does not state one.
const DEFAULT_WINDOW: u64 = 200_000;
/// The runtime behind both protocol fronts, plus everything `hrn chat` holds.
struct Shared {
    runtime: Arc<Mutex<AgentRuntime>>,
    store: Arc<dyn SessionStore>,
    llm: Arc<Mutex<LlmConfig>>,
    default_model: String,
    default_mode: PermissionMode,
    thinking: ThinkingLevel,
    cwd: PathBuf,
    mcp: Mutex<Option<McpManager>>,
    sessions: Mutex<BTreeMap<String, SessionControl>>,
    custom_commands: Mutex<BTreeMap<String, CustomCommand>>,
    sources: hrn_harness::ContextSources,
    overrides: RouterOverrides,
    idle: crate::idle_compact::RpcIdleTracker,
    background: Background,
}

impl Shared {
    fn runtime(&self) -> AgentRuntime {
        lock(&self.runtime).clone()
    }

    fn control(&self, id: &str) -> SessionControl {
        let mut map = lock(&self.sessions);
        if let Some(control) = map.get(id) {
            return control.clone();
        }
        let control = SessionControl::default();
        if let Ok(Some(meta)) = self.store.get_session(id) {
            control.set_permission_mode(meta.permission_mode);
        }
        map.insert(id.to_owned(), control.clone());
        control
    }

    /// The catalog row of a preset alias.
    fn entry(&self, alias: &str) -> Option<ModelEntry> {
        let llm = lock(&self.llm);
        let preset = llm.presets.get(alias)?;
        Some(ModelEntry {
            alias: alias.to_owned(),
            provider: preset.provider.clone(),
            id: preset.id.clone(),
            name: preset.name.clone().unwrap_or_else(|| preset.id.clone()),
            context_window: preset.context_window.unwrap_or(DEFAULT_WINDOW),
            reasoning: preset.reasoning.unwrap_or(false),
            input_price: preset.price.as_ref().map(|p| p.input),
            output_price: preset.price.as_ref().map(|p| p.output),
        })
    }

    /// Enabled preset aliases in deterministic order.
    fn aliases(&self) -> Vec<String> {
        let llm = lock(&self.llm);
        hrn_edge_llm::enabled_presets(&llm)
            .into_iter()
            .map(|(alias, _)| alias.to_owned())
            .collect()
    }

    /// The commands of `cwd`, rediscovered when the directory changes.
    fn commands_at(&self, cwd: &Path) -> BTreeMap<String, CustomCommand> {
        let remembered = lock(&self.custom_commands);
        if !remembered.is_empty() && self.cwd == cwd {
            return remembered.clone();
        }
        drop(remembered);
        let discovered = discover_custom_commands(&StdWorkspaceFs, cwd).unwrap_or_default();
        *lock(&self.custom_commands) = discovered.clone();
        discovered
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Backend for Shared {
    fn models(&self) -> Vec<ModelEntry> {
        self.aliases()
            .iter()
            .filter_map(|alias| self.entry(alias))
            .collect()
    }

    fn find_model(&self, selector: &str) -> Result<ModelEntry> {
        let alias = {
            let llm = lock(&self.llm);
            resolve_alias(&llm, Some(selector))
                .map_err(|_| HrnError::Config(format!("Model not found: {selector}")))?
        };
        self.entry(&alias)
            .ok_or_else(|| HrnError::Config(format!("Model not found: {selector}")))
    }

    fn default_model(&self) -> Result<ModelEntry> {
        self.entry(&self.default_model.clone()).ok_or_else(|| {
            HrnError::Config(format!("unknown default model `{}`", self.default_model))
        })
    }

    fn default_thinking(&self) -> ThinkingLevel {
        self.thinking
    }

    fn default_mode(&self) -> PermissionMode {
        self.default_mode
    }

    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }

    fn create_session(&self, spec: NewSession) -> Result<SessionMeta> {
        let meta = self.runtime().create_session(
            spec.title,
            &spec.cwd,
            Some(spec.model),
            spec.thinking,
            spec.mode,
        )?;
        self.control(&meta.id);
        Ok(meta)
    }

    fn find_session(&self, wanted: &str) -> Result<SessionMeta> {
        let id = crate::resume::find_session(self.store.as_ref(), wanted)?;
        let _ = self.runtime().repair_interrupted(&id)?;
        self.store
            .get_session(&id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{id}`")))
    }

    fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        self.store.list_sessions(200)
    }

    fn patch_session(&self, id: &str, patch: SessionPatch) -> Result<SessionMeta> {
        let mut meta = self
            .store
            .get_session(id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{id}`")))?;
        if let Some(model) = patch.model {
            meta.model = Some(model);
        }
        if let Some(thinking) = patch.thinking {
            meta.thinking = thinking;
        }
        if let Some(mode) = patch.mode {
            meta.permission_mode = mode;
            self.control(id).set_permission_mode(mode);
        }
        if let Some(title) = patch.title {
            meta.title = title;
        }
        self.store.update_session(&meta)?;
        Ok(meta)
    }

    fn history(&self, id: &str) -> Result<Vec<DagNode>> {
        let meta = self
            .store
            .get_session(id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{id}`")))?;
        match &meta.head_node_id {
            Some(head) => self.store.load_branch(id, head),
            None => Ok(Vec::new()),
        }
    }

    fn fork(&self, id: &str, node: Option<&str>) -> Result<SessionMeta> {
        let forked = self.runtime().fork_session(id, node)?;
        self.control(&forked.id);
        Ok(forked)
    }

    fn model_by_alias(&self, alias: &str) -> Option<ModelEntry> {
        self.entry(alias)
    }

    fn tools(&self) -> Vec<Value> {
        self.runtime()
            .tools()
            .specs()
            .into_iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "description": spec.description,
                    "parameters": spec.parameters,
                })
            })
            .collect()
    }

    fn model_rows(&self, current: &str) -> Vec<Value> {
        self.models()
            .iter()
            .map(|entry| model_row_json(entry, current))
            .collect()
    }

    fn model_json(&self, alias: &str) -> Value {
        match self.entry(alias) {
            Some(entry) => model_row_json(&entry, alias),
            None => json!({ "alias": alias, "provider": "", "id": alias, "name": alias }),
        }
    }

    fn context_window(&self, alias: &str) -> Option<u64> {
        let known = lock(&self.llm).presets.contains_key(alias);
        known.then(|| self.runtime().capabilities(Some(alias)).context_window)
    }

    fn pending_async(&self, id: &str) -> bool {
        let rt = self.runtime();
        let root = rt.agents().root_of(id);
        rt.agents().inbox().pending(&root) != 0
            || !rt.procs().running_ids(&root).is_empty()
            || rt.agents().in_tree(&root).iter().any(|a| a.is_running())
    }

    fn usage(&self, id: &str) -> Result<Usage> {
        self.store.session_usage(id)
    }

    fn run_turn(&self, request: TurnRequest) -> BoxFuture<Result<TurnSummary>> {
        let mut runtime = self.runtime();
        if !request.hooks.is_empty() {
            for hook in request.hooks {
                runtime = runtime.with_hook(hook);
            }
        }
        let store = Arc::clone(&self.store);
        let control = self.control(&request.session_id);
        let idle = self.idle.clone();
        idle.touch(&request.session_id, control.clone());
        let io = hrn_harness::TurnIo {
            cancel: request.cancel.clone(),
            events: Arc::clone(&request.events),
            approval: request.approval.clone(),
            ask: request.ask.clone(),
        };
        let mut prompt = crate::url_adapters::materialize_turn_images(
            &self.background,
            &runtime,
            &request.session_id,
            &request.prompt,
            &request.images,
        );
        let mut images = request.images;
        let session_id = request.session_id.clone();
        let cancel = request.cancel;
        Box::pin(async move {
            let mut total = Usage::default();
            loop {
                let outcome = runtime
                    .run_turn_with_images(
                        &session_id,
                        &prompt,
                        std::mem::take(&mut images),
                        &control,
                        io.clone(),
                    )
                    .await;
                match outcome {
                    Ok(result) => {
                        idle.touch(&session_id, control.clone());
                        if let Some(usage) = result.usage {
                            total.accumulate(usage);
                        }
                        let late = control.drain_steers();
                        if late.is_empty() || cancel.is_cancelled() {
                            return Ok(TurnSummary {
                                text: result.final_text,
                                usage: total,
                                aborted: false,
                                failed: false,
                            });
                        }
                        prompt = late.join("\n\n");
                    }
                    Err(HrnError::Cancelled) => {
                        return Ok(TurnSummary {
                            text: text_of(&store, &session_id),
                            usage: total,
                            aborted: true,
                            failed: false,
                        });
                    }
                    Err(err) => return Err(err),
                }
            }
        })
    }

    fn steer(&self, id: &str, text: String) {
        self.control(id).push_steer(text);
    }

    fn interrupt(&self, id: &str) {
        self.control(id).interrupt();
    }

    fn take_steers(&self, id: &str) -> Vec<String> {
        self.control(id).drain_steers()
    }

    fn compact(
        &self,
        id: &str,
        cancel: CancelFlag,
        events: Arc<dyn EventSink>,
    ) -> BoxFuture<Result<Option<CompactionReport>>> {
        let runtime = self.runtime();
        let store = Arc::clone(&self.store);
        let id = id.to_owned();
        let recorder = Arc::new(Recorder::default());
        let events: Arc<dyn EventSink> = Arc::new(Recording {
            inner: events,
            recorder: Arc::clone(&recorder),
        });
        Box::pin(async move {
            let covered = runtime.compact_now(&id, None, &cancel, &events).await?;
            let Some(node_id) = covered else {
                return Ok(None);
            };
            let meta = store
                .get_session(&id)?
                .ok_or_else(|| HrnError::Store(format!("unknown session `{id}`")))?;
            let head = meta
                .head_node_id
                .clone()
                .ok_or_else(|| HrnError::Store(format!("session `{id}` has no head")))?;
            let branch = store.load_branch(&id, &head)?;
            let summary = branch
                .iter()
                .find_map(|node| match &node.payload {
                    DagPayload::Summary {
                        covers_node_id,
                        text,
                        ..
                    } if *covers_node_id == node_id => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let (saved_chars, tokens_before) = match branch.iter().find(|node| node.id == node_id) {
                Some(_) => (*lock(&recorder.0), 0),
                None => (0, 0),
            };
            Ok(Some(CompactionReport {
                first_kept: node_id,
                summary,
                saved_chars,
                tokens_before,
            }))
        })
    }

    fn export_html(&self, id: &str, path: Option<&Path>) -> Result<PathBuf> {
        crate::export::write_file(
            self.store.as_ref(),
            id,
            path,
            Some(crate::cli::ExportFormat::Html),
            &self.cwd,
        )
    }

    fn commands(&self, cwd: &Path) -> Vec<CommandInfo> {
        self.commands_at(cwd)
            .values()
            .map(|command| CommandInfo {
                name: command.name.clone(),
                description: command.description.clone(),
            })
            .collect()
    }

    fn expand_command(&self, cwd: &Path, name: &str, args: &str) -> Option<String> {
        self.commands_at(cwd)
            .get(&name.to_lowercase())
            .map(|command| command.expand(args))
    }

    fn system_prompt(&self, id: &str) -> Result<Vec<String>> {
        rpc_prompt::system_prompt_sections(self, id)
    }

    fn forget_approvals(&self) -> Result {
        self.runtime().forget_approvals()
    }

    fn supports_service_tier(&self, alias: &str) -> bool {
        self.supports_service_tier_live(alias)
    }

    fn supports_slow_mode(&self, alias: &str) -> bool {
        self.supports_slow_mode_live(alias)
    }

    fn set_fast_mode(&self, enabled: bool) {
        self.set_fast_mode_live(enabled);
    }

    fn set_slow_mode(&self, enabled: bool) {
        self.set_slow_mode_live(enabled);
    }

    fn fast_mode_enabled(&self) -> bool {
        self.fast_mode_enabled_live()
    }

    fn slow_mode_enabled(&self) -> bool {
        self.slow_mode_enabled_live()
    }

    fn shutdown(&self) -> BoxFuture<()> {
        let manager = lock(&self.mcp).take();
        Box::pin(async move {
            if let Some(manager) = manager {
                manager.shutdown().await;
            }
        })
    }
}

/// Open the runtime a protocol server needs and wire 250 ms live config reload.
async fn shared(
    cfg: &Config,
    layer_input: LayerInput,
    merged: MergedConfig,
    global_path: PathBuf,
    args: &ServeArgs,
    discovered: Vec<hrn_edge_llm::Catalog>,
) -> Result<Shared> {
    let mut cfg = cfg.clone();
    let cwd = crate::exec::absolute_cwd(args.cwd.clone())?;
    let sqlite = open_store(&crate::database_location()?, false)?;
    let port = LiveSettingsPort::new(layer_input.clone(), merged, global_path);
    let background = Background::new().with_settings(Arc::clone(&port) as _);
    background.set_discovered(discovered);
    background.reapply_discovered(&mut cfg);
    let default_model = resolve_alias(&cfg.llm, args.model.as_deref())?;
    if let Some(tier) = args.service_tier {
        background.overrides.set_service_tier(Some(tier));
    }
    let overrides = background.overrides.clone();
    let (runtime, mcp, warnings) =
        build_runtime_with_mcp(&mut cfg, Arc::clone(&sqlite), &cwd, &background).await?;
    for warning in warnings {
        err_line(&format!("mcp warning: {warning}"));
    }
    let mcp_tools = mcp.tool_set(cfg.mcp_tool_budget);
    let runtime_slot = Arc::new(Mutex::new(runtime));
    let llm_slot = Arc::new(Mutex::new(cfg.llm.clone()));
    watch_rpc(
        &tokio::runtime::Handle::current(),
        layer_input,
        RpcWatchTargets {
            port,
            runtime_slot: Arc::clone(&runtime_slot),
            llm_slot: Arc::clone(&llm_slot),
            sqlite: Arc::clone(&sqlite),
            cwd: cwd.clone(),
            mcp_tools,
            background: background.clone(),
        },
    );
    let idle = crate::idle_compact::RpcIdleTracker::default();
    crate::idle_compact::watch_rpc(
        &tokio::runtime::Handle::current(),
        Arc::clone(&runtime_slot),
        idle.clone(),
    );
    let store: Arc<dyn SessionStore> = Arc::clone(&sqlite) as Arc<dyn SessionStore>;
    Ok(Shared {
        runtime: runtime_slot,
        store,
        llm: llm_slot,
        default_model,
        default_mode: cfg.approval_mode,
        thinking: args.think.unwrap_or(cfg.thinking),
        cwd,
        mcp: Mutex::new(Some(mcp)),
        sessions: Mutex::new(BTreeMap::new()),
        custom_commands: Mutex::new(BTreeMap::new()),
        sources: cfg.sources.clone(),
        overrides,
        idle,
        background,
    })
}

/// `hrn rpc` / `hrn --mode rpc[​-ui]`: omp's NDJSON RPC on stdio.
pub(crate) async fn serve(
    cfg: &Config,
    layer_input: LayerInput,
    merged: MergedConfig,
    global_path: PathBuf,
    args: ServeArgs,
    tool_ui: bool,
    discovered: Vec<hrn_edge_llm::Catalog>,
) -> Result<ExitCode> {
    let inner = shared(cfg, layer_input, merged, global_path, &args, discovered).await?;
    let opts = hrn_rpc::RpcOptions {
        cwd: inner.cwd.clone(),
        tool_ui,
        approvals: !args.no_ui,
        auto_approve: args.yes,
    };
    match hrn_rpc::serve_rpc(
        Arc::new(inner) as Arc<dyn Backend>,
        opts,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
    {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(err) => Err(HrnError::Internal(format!("rpc server: {err}"))),
    }
}

/// `hrn acp` / `hrn --mode acp`: Zed's Agent Client Protocol on stdio.
pub(crate) async fn serve_acp(
    cfg: &Config,
    layer_input: LayerInput,
    merged: MergedConfig,
    global_path: PathBuf,
    args: ServeArgs,
    discovered: Vec<hrn_edge_llm::Catalog>,
) -> Result<ExitCode> {
    let inner = shared(cfg, layer_input, merged, global_path, &args, discovered).await?;
    let opts = hrn_rpc::AcpOptions {
        cwd: inner.cwd.clone(),
        approvals: !args.no_ui,
        auto_approve: args.yes,
    };
    match hrn_rpc::serve_acp(
        Arc::new(inner) as Arc<dyn Backend>,
        opts,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
    {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(err) => Err(HrnError::Internal(format!("acp server: {err}"))),
    }
}
