//! `~/.harness/config.toml`: the typed user-facing file, validated and lowered into the
//! `edge/llm` [`LlmConfig`] (providers, presets, roles, fallback chains, cycle order) plus
//! optional `OAuth2` PKCE login definitions for `hrn login <provider>`.

use crate::paths;
use hrn_edge_llm::{
    AuthMode, Catalog, LlmConfig, ModelPreset, ProviderEntry, ProviderKind, standard_catalogs,
    validate_config,
};
use hrn_edge_mcp::{DEFAULT_TIMEOUT_SECS, McpServerConfig, McpTransport};
use hrn_edge_oauth::{OAuthConfig, PkceLoginConfig};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{AdvisorConfig, UserApprovalPolicy};
use hrn_kernel::{HrnError, ModelPrice, PermissionMode, Result, ThinkingLevel, WorkspaceFs};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// One `[models.<alias>]` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelFile {
    provider: String,
    id: String,
    context_window: u64,
    /// Optional default thinking effort for this model preset (a `:level` suffix on a selector
    /// and `[modelPresets.*].defaultThinkingLevel` override it per session).
    thinking: Option<String>,
    /// Display name (omp's `name:`).
    name: Option<String>,
    /// Whether the model reasons (omp's `reasoning:`); `false` sends no reasoning effort.
    reasoning: Option<bool>,
    /// Whether the model accepts inline image inputs (`None` infers from model id/family).
    vision: Option<bool>,
    /// Base-prompt variant (`"claude"`, `"gpt5"`); unset resolves by model family.
    prompt_variant: Option<String>,
    /// Optional prices in USD per million tokens
    /// (`price = { input = 3, output = 15, cache_read = 0.3, cache_write = 3.75 }`); without
    /// them no cost is shown for this model. Cache prices default to `input`.
    price: Option<ModelPrice>,
    /// Optional extended context ceiling (omp `maxContextWindow`).
    max_context_window: Option<u64>,
    /// Optional larger-context model selector promoted to before compacting on overflow.
    context_promotion_target: Option<String>,
}

/// Schema tokens MCP tools may add to every request before whole servers are deferred.
/// Loaded MCP schemas ride on every step, so the default is small; `tool_search` costs one
/// cheap step for the rest (spec §4.2).
const DEFAULT_MCP_TOOL_BUDGET: usize = 4_000;

/// `[mcp_tools]` block: how many schema tokens MCP tools may cost per request.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpToolsFile {
    /// MCP servers are loaded smallest first while their schemas fit this many tokens
    /// (default 4000); the rest are listed by name and loaded with `tool_search`. `0` defers all.
    budget_tokens: Option<usize>,
}

/// `[session]` block: session startup and lifecycle preferences.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent [session] preference flags"
)]
struct SessionFile {
    /// Automatically resume the newest session in `cwd` on bare `hrn chat` (default `false`).
    #[serde(default)]
    auto_resume: bool,
    /// Offer abandoned-branch summarization by default on branch switch (default `false`).
    #[serde(default)]
    branch_summary: bool,
    /// Automatically generate a concise session title after the first substantive turn (default `true`).
    #[serde(default = "default_true")]
    auto_title: bool,
    /// Refresh an auto-generated session title when a todo replan occurs (default `true`).
    #[serde(default = "default_true")]
    title_refresh_on_replan: bool,
}

impl Default for SessionFile {
    fn default() -> Self {
        Self {
            auto_resume: false,
            branch_summary: false,
            auto_title: true,
            title_refresh_on_replan: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// `[gc]` retention and vacuum thresholds for `hrn gc [--apply]`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct GcConfig {
    /// Minimum idle age in days before an unpinned session is eligible for deletion (default 90).
    pub(crate) retain_days: u64,
    /// Always keep at least this many newest sessions per working directory (default 20).
    pub(crate) retain_newest_per_cwd: usize,
    /// Always keep at least this many newest sessions globally (default 20).
    pub(crate) retain_newest_global: usize,
    /// Run WAL checkpoint + VACUUM when reclaimed bytes reach this threshold (default 1 MiB).
    pub(crate) vacuum_reclaim_bytes: u64,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            retain_days: 90,
            retain_newest_per_cwd: 20,
            retain_newest_global: 20,
            vacuum_reclaim_bytes: 1_048_576,
        }
    }
}

/// `[interaction]` block: TUI interaction preferences.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InteractionFile {
    /// Action on double-Esc when the editor is empty: `"tree"`, `"rewind"`, or `"none"`.
    double_escape_action: Option<String>,
    /// Enable `<inject>...</inject>` rail and `Alt+I` send-once in `/btw` (default `false`).
    #[serde(default)]
    btw_inject: bool,
}

/// The raw file contents. Every top-level key that is not a core setting is a module's `[<id>]`
/// section: it is kept raw in `modules`, and the module that owns it parses it (an unclaimed one
/// is refused when the registry is finished).
#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    providers: ProvidersSection,
    #[serde(default)]
    models: BTreeMap<String, ModelFile>,
    #[serde(default)]
    roles: BTreeMap<String, String>,
    /// Per-model fallback chains: `[fallback]` `opus = ["sonnet", "gpt"]`.
    #[serde(default)]
    fallback: BTreeMap<String, Vec<String>>,
    /// Ordered model alias cycle list for quick TUI switching.
    #[serde(default)]
    cycle: Vec<String>,
    /// omp's `modelPresets`: named role bundles applied by `/modelpreset` or `--preset`.
    #[serde(default, rename = "modelPresets")]
    model_presets: BTreeMap<String, hrn_edge_llm::RolePreset>,
    /// Default thinking effort: auto | off | minimal | low | medium | high | xhigh | max.
    thinking: Option<String>,
    /// LLM storage preferences (`[llm]`).
    #[serde(default)]
    llm: LlmFile,
    /// Session startup and persistence settings (`[session]`).
    #[serde(default)]
    session: SessionFile,
    /// Garbage-collection retention and vacuum settings (`[gc]`).
    #[serde(default)]
    gc: GcConfig,
    /// Attributed commit workflow settings (`[commit]`).
    #[serde(default)]
    commit: CommitConfig,
    /// Self-update repository and channel (`[update]`).
    #[serde(default)]
    update: UpdateConfig,
    /// Interactive UX preferences (`[interaction]`).
    #[serde(default)]
    interaction: InteractionFile,
    /// Passive post-turn advisor configuration (`[advisor]`).
    #[serde(default)]
    advisor: AdvisorFile,
    /// Configured MCP servers (`[[mcp]]`).
    #[serde(default)]
    mcp: Vec<McpServerFile>,
    /// Deferred-tool settings (`[mcp_tools]`).
    #[serde(default)]
    mcp_tools: McpToolsFile,
    /// Front-end tool settings (`[tools]`).
    #[serde(default)]
    tools: ToolsFile,
    /// Terminal UI settings (`[tui]`).
    #[serde(default)]
    tui: TuiFile,
    /// Assistant output channel settings (`[output]`).
    #[serde(default)]
    output: OutputFile,
    /// Subagent task settings (`[task]`).
    #[serde(default)]
    task: hrn_harness::TaskConfig,
    /// Context compaction settings (`[compaction]`).
    #[serde(default)]
    compaction: hrn_harness::CompactionConfig,
    /// Sealed share link settings (`[share]`).
    #[serde(default)]
    share: ShareFile,
    /// Live collaboration settings (`[collab]`).
    #[serde(default)]
    collab: CollabFile,
    /// Optional allowlist of enabled model selectors (path-scoped arrays resolved before lowering).
    #[serde(default)]
    enabled_models: Vec<String>,
    /// Optional list of disabled provider names (path-scoped arrays resolved before lowering).
    #[serde(default)]
    disabled_providers: Vec<String>,
    /// The module sections, by module id.
    #[serde(flatten)]
    modules: BTreeMap<String, serde_json::Value>,
}

/// A validated configuration.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent boolean flags from [tools], [session], and [interaction]"
)]
pub(crate) struct Config {
    /// Providers, presets, roles, fallback chains, and cycle order for the router.
    pub(crate) llm: LlmConfig,
    /// Thinking effort used when `--think` is not given.
    pub(crate) thinking: ThinkingLevel,
    /// Passive post-turn advisor defaults.
    pub(crate) advisor: AdvisorConfig,
    /// Interactive `hrn login <provider>` PKCE parameters for providers that declare `authorize_url`.
    pub(crate) oauth_logins: BTreeMap<String, PkceLoginConfig>,
    /// MCP servers declared in `config.toml` (`[[mcp]]`).
    pub(crate) mcp_servers: Vec<McpServerConfig>,
    /// Schema-token budget for MCP tools; servers beyond it are deferred behind `tool_search`.
    pub(crate) mcp_tool_budget: usize,
    /// The raw `[<id>]` section of every module, handed to the `Registry`.
    pub(crate) module_tables: BTreeMap<String, serde_json::Value>,
    /// How long an `ask` form waits for the user (`[tools] ask_timeout_secs`); `None` forever.
    pub(crate) ask_timeout: Option<Duration>,
    /// Whether rarely-used module tools are deferred behind `tool_search` (`[tools]
    /// premium_defer`, default `true`).
    pub(crate) premium_defer: bool,
    /// Default tool approval mode (`[tools] approval_mode`, default `yolo`).
    pub(crate) approval_mode: PermissionMode,
    /// User-configured restricted folders and `[tools.approval]` per-tool overrides.
    pub(crate) approval_policy: UserApprovalPolicy,
    /// `--system-prompt`: replaces the embedded base prompt wholesale.
    pub(crate) system_prompt_override: Option<String>,
    /// `--append-system-prompt`: appended after every `APPEND_SYSTEM.md` layer.
    pub(crate) append_system_override: Option<String>,
    /// TUI appearance: dark/light theme slots, symbol preset, color-blind mode, and statusline.
    pub(crate) appearance: hrn_tui::AppearanceConfig,
    /// Desktop notification settings (`[tui] notify*`), handed to the TUI.
    pub(crate) notify: hrn_tui::Notify,
    /// Instruction-source switches, set by a launch overlay from the chat/exec flags.
    pub(crate) sources: hrn_harness::ContextSources,
    /// Subagent task limits, shape, isolation, and idle TTL (`[task]`).
    pub(crate) task: hrn_harness::TaskConfig,
    /// Context compaction thresholds, method order, and idle settings (`[compaction]`).
    pub(crate) compaction: hrn_harness::CompactionConfig,
    /// Enable promoting to a larger-context target before compacting (`[llm] context_promotion`).
    pub(crate) context_promotion: bool,
    /// Allow sending API keys over plaintext HTTP to non-loopback hosts during login probes
    /// (`[llm] allow_plaintext_http`, default `false`).
    pub(crate) allow_plaintext_http: bool,
    /// Whether bare `hrn chat` automatically resumes the newest session in `cwd` (`[session] auto_resume`).
    pub(crate) auto_resume: bool,
    /// Offer abandoned-branch summarization by default (`[session] branch_summary`).
    pub(crate) branch_summary: bool,
    /// Automatically title sessions after the first substantive turn (`[session] auto_title`).
    pub(crate) auto_title: bool,
    /// Refresh auto-generated titles after a todo replan (`[session] title_refresh_on_replan`).
    pub(crate) title_refresh_on_replan: bool,
    /// Double-Esc action on empty editor (`[interaction] double_escape_action`).
    pub(crate) double_escape_action: hrn_tui::DoubleEscapeAction,
    /// Enable `/btw` `<inject>` rail and `Alt+I` send-once (`[interaction] btw_inject`).
    pub(crate) btw_inject: bool,
    /// Pinned Subagents jump block display mode (`[tui] pinned_agents`).
    pub(crate) pinned_agents: hrn_tui::PinnedAgentsMode,
    /// Show active tool + elapsed (> 5s) preview line in the pinned jump block (`[tui] subagent_live_preview`).
    pub(crate) subagent_live_preview: bool,
    /// Enable minimal mouse capture for click-to-focus on pinned rows (`[tui] mouse`).
    pub(crate) mouse: bool,
    /// Enable the write-only `out://` URL scheme for mid-turn assistant prose (`[output] out_enabled`).
    pub(crate) out_enabled: bool,
    /// Sealed share settings (`[share]`).
    pub(crate) share: ShareConfig,
    /// Live collaboration relay settings (`[collab]`).
    pub(crate) collab: CollabConfig,
    /// Attributed commit workflow settings (`[commit]`).
    pub(crate) commit: CommitConfig,
    /// Self-update repository and channel (`[update]`, operator-only).
    #[expect(
        dead_code,
        reason = "parsed during Config load; hrn update reloads strictly from operator layers"
    )]
    pub(crate) update: UpdateConfig,
}

fn bad(msg: impl Into<String>) -> HrnError {
    HrnError::Config(msg.into())
}

/// The model sources below `config.toml` for `home` (none without a home directory).
pub(crate) fn model_catalogs(home: Option<&Path>) -> Vec<Catalog> {
    match (home, paths::auth_dir(home)) {
        (Some(home), Ok(auth_dir)) => standard_catalogs(home, &auth_dir),
        _ => Vec::new(),
    }
}

fn parse_thinking(raw: &str, what: &str) -> Result<ThinkingLevel> {
    ThinkingLevel::parse(raw).ok_or_else(|| {
        bad(format!(
            "{what} `{raw}` is not one of auto, off, minimal, low, medium, high, xhigh, max"
        ))
    })
}

impl Config {
    /// Read and validate a single config file layered over embedded defaults, absorbing model
    /// catalogs underneath.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let input = crate::config_layers::LayerInput {
            global_path: Some(path.to_path_buf()),
            project_path: None,
            overlays: Vec::new(),
            cwd,
            home: StdWorkspaceFs.home_dir(),
            env: BTreeMap::new(),
            runtime: toml::Table::new(),
        };
        if !path.is_file() {
            return Err(bad(format!(
                "cannot read config `{}`: file not found",
                path.display()
            )));
        }
        Self::load_layered(&input)
            .map(|(cfg, _)| cfg)
            .map_err(|e| match e {
                HrnError::Config(msg) => bad(format!("{}: {msg}", path.display())),
                other => other,
            })
    }

    /// Load and validate all 6 layers (defaults -> global -> project -> overlays -> env -> runtime),
    /// absorbing model catalogs underneath.
    pub(crate) fn load_layered(
        input: &crate::config_layers::LayerInput,
    ) -> Result<(Self, crate::config_layers::MergedConfig)> {
        let merged = crate::config_layers::load_layers(input)?;
        let cfg = Self::from_merged(&merged, input.home.as_deref())?;
        Ok((cfg, merged))
    }

    /// Lower an already-merged 6-layer TOML document into a validated [`Config`].
    pub(crate) fn from_merged(
        merged: &crate::config_layers::MergedConfig,
        home: Option<&Path>,
    ) -> Result<Self> {
        let file: ConfigFile = toml::Value::Table(merged.root.clone())
            .try_into()
            .map_err(|e| bad(e.to_string()))?;
        Self::lower_file(file, home, model_catalogs(home))
    }

    /// Parse and validate TOML text using the current user's home directory for default paths.
    #[cfg(test)]
    pub(crate) fn parse(text: &str) -> Result<Self> {
        Self::parse_with_home(text, Some(Path::new("/home/test")))
    }

    /// Parse and validate TOML text (layered over embedded core defaults) with an explicit
    /// optional home directory; no model catalogs beyond the text's own are consulted.
    pub(crate) fn parse_with_home(text: &str, home: Option<&Path>) -> Result<Self> {
        Self::parse_with_catalogs(text, home, Vec::new())
    }

    /// Parse and validate TOML text (layered over embedded core defaults), absorbing `catalogs`
    /// underneath before validating roles and presets.
    pub(crate) fn parse_with_catalogs(
        text: &str,
        home: Option<&Path>,
        catalogs: Vec<Catalog>,
    ) -> Result<Self> {
        let defaults: toml::Table = toml::from_str(crate::settings_schema::DEFAULTS_TOML)
            .map_err(|e| bad(e.to_string()))?;
        let user: toml::Table = toml::from_str(text).map_err(|e| bad(e.to_string()))?;
        let merged = crate::config_layers::merge_layer_list(
            &[
                (crate::config_layers::LayerSource::Default, defaults),
                (crate::config_layers::LayerSource::Global, user),
            ],
            Path::new("."),
            home,
        );
        let file: ConfigFile = toml::Value::Table(merged.root)
            .try_into()
            .map_err(|e| bad(e.to_string()))?;
        Self::lower_file(file, home, catalogs)
    }

    fn lower_llm(
        file: &mut ConfigFile,
        home: Option<&Path>,
        catalogs: Vec<Catalog>,
    ) -> Result<(LlmConfig, BTreeMap<String, PkceLoginConfig>)> {
        let mut llm = LlmConfig::default();
        let mut oauth_logins = BTreeMap::new();
        let mut named_kimi: Option<hrn_edge_llm::LlmKimiConfig> = None;
        match std::mem::take(&mut file.providers) {
            ProvidersSection::List(list) => {
                for p in list {
                    lower_provider(&mut llm, &mut oauth_logins, p, home)?;
                }
            }
            ProvidersSection::Named(named) => {
                named_kimi = named.kimi;
            }
        }
        for (alias, m) in std::mem::take(&mut file.models) {
            if m.context_window == 0 {
                return Err(bad(format!("model `{alias}`: context_window must be > 0")));
            }
            let model_thinking = match m.thinking.as_deref() {
                None => None,
                Some(raw) => Some(parse_thinking(raw, &format!("model `{alias}` thinking"))?),
            };
            llm.presets.insert(
                alias,
                ModelPreset {
                    provider: m.provider,
                    id: m.id,
                    thinking: model_thinking,
                    thinking_budgets: None,
                    context_window: Some(m.context_window),
                    max_context_window: m.max_context_window,
                    context_promotion_target: m.context_promotion_target,
                    name: m.name,
                    reasoning: m.reasoning,
                    vision: m.vision,
                    prompt_variant: m.prompt_variant,
                    price: m.price,
                    ..ModelPreset::default()
                },
            );
        }
        llm.roles = std::mem::take(&mut file.roles);
        llm.fallback_chains = std::mem::take(&mut file.fallback);
        llm.cycle_order = std::mem::take(&mut file.cycle);
        llm.role_presets = std::mem::take(&mut file.model_presets);
        llm.enabled_models = std::mem::take(&mut file.enabled_models);
        llm.disabled_providers = std::mem::take(&mut file.disabled_providers);
        llm.role_storage = file.llm.role_storage.unwrap_or_default();
        llm.extended_context = file.llm.extended_context;
        llm.retry = std::mem::take(&mut file.llm.retry);
        llm.accounts = std::mem::take(&mut file.llm.accounts);
        llm.tier = std::mem::take(&mut file.llm.tier);
        llm.anthropic = std::mem::take(&mut file.llm.anthropic);
        llm.kimi = named_kimi.unwrap_or_else(|| std::mem::take(&mut file.llm.kimi));
        for catalog in &catalogs {
            for (agent, sel) in &catalog.agent_models {
                file.task
                    .agent_models
                    .entry(agent.clone())
                    .or_insert_with(|| sel.clone());
            }
        }
        llm.absorb_all(catalogs);
        if !llm.roles.contains_key("default") {
            return Err(bad("missing [roles] table with a `default` model alias"));
        }
        validate_config(&llm)?;
        Ok((llm, oauth_logins))
    }

    /// Lower a parsed [`ConfigFile`], merge `catalogs` (highest priority first) under it and validate.
    fn lower_file(
        mut file: ConfigFile,
        home: Option<&Path>,
        catalogs: Vec<Catalog>,
    ) -> Result<Self> {
        let thinking = match file.thinking.as_deref() {
            None => ThinkingLevel::default(),
            Some(raw) => parse_thinking(raw, "thinking")?,
        };
        let (llm, oauth_logins) = Self::lower_llm(&mut file, home, catalogs)?;
        let (approval_mode, approval_policy) = lower_tools_approval(
            &file.tools.approval_mode,
            file.tools.restricted_folders,
            file.tools.approval,
        )?;
        let advisor = lower_advisor(&llm, &file.advisor)?;
        let mut mcp_servers = Vec::with_capacity(file.mcp.len());
        let mut seen_mcp = std::collections::BTreeSet::new();
        for entry in file.mcp {
            let lowered = lower_mcp(entry)?;
            if !seen_mcp.insert(lowered.name.clone()) {
                return Err(bad(format!("duplicate MCP server `{}`", lowered.name)));
            }
            mcp_servers.push(lowered);
        }
        file.task.validate()?;
        file.compaction.validate()?;
        file.update.validate()?;
        let double_escape_action = match file.interaction.double_escape_action.as_deref() {
            None => hrn_tui::DoubleEscapeAction::Tree,
            Some(raw) => hrn_tui::DoubleEscapeAction::parse(raw).ok_or_else(|| {
                bad(format!(
                    "interaction.double_escape_action `{raw}` is not one of rewind, tree, none"
                ))
            })?,
        };
        let tui = lower_tui(file.tui)?;
        // `[gc]` is validated by `ConfigFile` deserialization; `hrn gc` loads it directly from
        // operator config layers via `gc::load_gc_config`.
        let _ = file.gc;
        let share = lower_share(&file.share)?;
        let collab = lower_collab(&file.collab)?;
        Ok(Self {
            llm,
            thinking,
            advisor,
            oauth_logins,
            mcp_servers,
            mcp_tool_budget: file
                .mcp_tools
                .budget_tokens
                .unwrap_or(DEFAULT_MCP_TOOL_BUDGET),
            module_tables: file.modules,
            ask_timeout: (file.tools.ask_timeout_secs > 0)
                .then(|| Duration::from_secs(file.tools.ask_timeout_secs)),
            premium_defer: file.tools.premium_defer,
            approval_mode,
            approval_policy,
            system_prompt_override: None,
            append_system_override: None,
            appearance: tui.appearance,
            notify: tui.notify,
            sources: hrn_harness::ContextSources::default(),
            task: file.task,
            compaction: file.compaction,
            context_promotion: file.llm.context_promotion,
            allow_plaintext_http: file.llm.allow_plaintext_http,
            auto_resume: file.session.auto_resume,
            branch_summary: file.session.branch_summary,
            auto_title: file.session.auto_title,
            title_refresh_on_replan: file.session.title_refresh_on_replan,
            double_escape_action,
            btw_inject: file.interaction.btw_inject,
            pinned_agents: tui.pinned_agents,
            subagent_live_preview: tui.subagent_live_preview,
            mouse: tui.mouse,
            out_enabled: file.output.out_enabled,
            share,
            collab,
            commit: file.commit,
            update: file.update,
        })
    }

    /// Apply the frontend-specific default for `[output] out_enabled` (`true` in interactive TUI,
    /// `false` in `exec` / `rpc` / `acp` / `daemon`) when no explicit layer configured it.
    pub(crate) fn apply_output_default(
        &mut self,
        merged: &crate::config_layers::MergedConfig,
        interactive_tui: bool,
    ) {
        if merged.source_of("output.out_enabled") == crate::config_layers::LayerSource::Default {
            self.out_enabled = interactive_tui;
        }
    }
}

#[path = "config_file_lower.rs"]
mod config_file_lower;
use config_file_lower::{
    AdvisorFile, CollabFile, LlmFile, McpServerFile, OutputFile, ProvidersSection, ShareFile,
    ToolsFile, lower_advisor, lower_collab, lower_mcp, lower_provider, lower_share,
    lower_tools_approval,
};
pub(crate) use config_file_lower::{CollabConfig, ShareConfig, ShareStoreKind};

#[path = "config_commit_update.rs"]
mod config_commit_update;
pub(crate) use config_commit_update::{CommitConfig, UpdateConfig};

#[path = "config_tui_lower.rs"]
mod config_tui_lower;
use config_tui_lower::{TuiFile, lower_tui};

#[path = "config_sources.rs"]
mod config_sources;
#[cfg(test)]
pub(crate) use config_sources::resolve_prompt_text;
pub(crate) use config_sources::{apply_no_lsp, apply_prompt_flags, apply_source_flags};
#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
