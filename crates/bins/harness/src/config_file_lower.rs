//! The lowering of one `[[providers]]`/`[[mcp]]` file entry into the edge configs: the
//! validators live here so `config.rs` keeps only the typed file and the assembly step.

use super::{
    AdvisorConfig, AuthMode, BTreeMap, DEFAULT_TIMEOUT_SECS, LlmConfig, McpServerConfig,
    McpTransport, OAuthConfig, PermissionMode, PkceLoginConfig, ProviderEntry, ProviderKind,
    Result, ThinkingLevel, UserApprovalPolicy, bad, parse_thinking, paths,
};
use hrn_edge_llm::resolve_alias;
use hrn_kernel::ToolApprovalPolicy;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// `[advisor]` block for the passive post-turn reviewer.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdvisorFile {
    #[serde(default)]
    pub(super) enabled: bool,
    pub(super) model: Option<String>,
    pub(super) thinking: Option<String>,
    pub(super) max_tokens: Option<u32>,
}

/// `[tools]` block: how the front-end serves built-in tools.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ToolsFile {
    /// Seconds an `ask` form waits for the user before it submits its current values
    /// (default `0`: wait forever).
    #[serde(default)]
    pub(super) ask_timeout_secs: u64,
    /// Keep the schemas of rarely-used (premium) tools out of every request; they load through
    /// `tool_search` (default `true`).
    #[serde(default = "default_premium_defer")]
    pub(super) premium_defer: bool,
    /// Default tool approval mode (`yolo`, `write`, `always-ask`, `always-ask-cached`, `plan`).
    #[serde(default = "default_approval_mode")]
    pub(super) approval_mode: String,
    /// Folders where matching tool calls always require plain interactive Approve/Deny confirmation.
    #[serde(default)]
    pub(super) restricted_folders: Vec<String>,
    /// Per-tool approval overrides (`[tools.approval] <tool> = "allow" | "deny" | "prompt"`).
    #[serde(default)]
    pub(super) approval: BTreeMap<String, ToolApprovalPolicy>,
}

impl Default for ToolsFile {
    fn default() -> Self {
        Self {
            ask_timeout_secs: 0,
            premium_defer: default_premium_defer(),
            approval_mode: default_approval_mode(),
            restricted_folders: Vec::new(),
            approval: BTreeMap::new(),
        }
    }
}

fn default_premium_defer() -> bool {
    true
}

fn default_approval_mode() -> String {
    "yolo".to_owned()
}

/// `[output]` block: assistant prose output channel settings (`out://`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OutputFile {
    /// Enable the write-only `out://` internal URL scheme for mid-turn assistant prose.
    #[serde(default = "default_out_enabled")]
    pub(super) out_enabled: bool,
}

fn default_out_enabled() -> bool {
    true
}

impl Default for OutputFile {
    fn default() -> Self {
        Self {
            out_enabled: default_out_enabled(),
        }
    }
}

/// Backend used by `/share` and `hrn share`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ShareStoreKind {
    /// Post sealed ciphertext to `[share] server` (`POST <server>/s`).
    #[default]
    Blob,
    /// Create a secret GitHub gist via `gh gist create`.
    Gist,
}

/// Validated `[share]` configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShareConfig {
    pub(crate) store: ShareStoreKind,
    pub(crate) server: String,
    pub(crate) redact: bool,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self {
            store: ShareStoreKind::Blob,
            server: String::new(),
            redact: true,
        }
    }
}

/// Validated `[collab]` configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CollabConfig {
    pub(crate) relay: String,
}

/// `[share]` block in `config.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ShareFile {
    #[serde(default = "default_share_store")]
    pub(super) store: String,
    #[serde(default)]
    pub(super) server: String,
    #[serde(default = "default_share_redact")]
    pub(super) redact: bool,
}

impl Default for ShareFile {
    fn default() -> Self {
        Self {
            store: default_share_store(),
            server: String::new(),
            redact: default_share_redact(),
        }
    }
}

fn default_share_store() -> String {
    "blob".to_owned()
}

fn default_share_redact() -> bool {
    true
}

/// `[collab]` block in `config.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CollabFile {
    #[serde(default)]
    pub(super) relay: String,
}

pub(super) fn lower_share(raw: &ShareFile) -> Result<ShareConfig> {
    let store = match raw.store.trim().to_ascii_lowercase().as_str() {
        "" | "blob" => ShareStoreKind::Blob,
        "gist" => ShareStoreKind::Gist,
        other => {
            return Err(bad(format!(
                "[share] store `{other}` is not one of blob, gist"
            )));
        }
    };
    let server = raw.server.trim().trim_end_matches('/').to_owned();
    if !server.is_empty() {
        hrn_edge_http::guard::check_outbound_url(&server)?;
    }
    Ok(ShareConfig {
        store,
        server,
        redact: raw.redact,
    })
}

pub(super) fn lower_collab(raw: &CollabFile) -> Result<CollabConfig> {
    let relay = raw.relay.trim().trim_end_matches('/').to_owned();
    if !relay.is_empty() {
        let check_url = relay
            .strip_prefix("wss://")
            .map(|r| format!("https://{r}"))
            .or_else(|| relay.strip_prefix("ws://").map(|r| format!("http://{r}")))
            .unwrap_or_else(|| relay.clone());
        hrn_edge_http::guard::check_outbound_url(&check_url)?;
    }
    Ok(CollabConfig { relay })
}
pub(super) fn lower_advisor(llm: &LlmConfig, raw: &AdvisorFile) -> Result<AdvisorConfig> {
    let model = match raw
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(alias) => Some(resolve_alias(llm, Some(alias))?),
        None => None,
    };
    let thinking = match raw.thinking.as_deref() {
        None => ThinkingLevel::Low,
        Some(level) => parse_thinking(level, "advisor thinking")?,
    };
    Ok(AdvisorConfig {
        enabled: raw.enabled,
        model,
        thinking,
        max_tokens: raw.max_tokens.unwrap_or(384).max(64),
    })
}

pub(super) fn lower_tools_approval(
    raw_mode: &str,
    restricted_folders: Vec<String>,
    tool_policies: BTreeMap<String, ToolApprovalPolicy>,
) -> Result<(PermissionMode, UserApprovalPolicy)> {
    let mode = PermissionMode::parse(raw_mode).ok_or_else(|| {
        bad(format!(
            "[tools] approval_mode `{raw_mode}` is not one of always-ask, always-ask-cached, \
             write, yolo, plan, default, auto-edit"
        ))
    })?;
    Ok((
        mode,
        UserApprovalPolicy {
            restricted_folders,
            tool_policies,
        },
    ))
}

/// `[llm]` block: model, role storage, account-pool, service-tier, and extended-context settings.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LlmFile {
    /// Where Roles Hub writes `[roles]` changes: `"global"` (default) or `"project"`.
    pub(super) role_storage: Option<hrn_edge_llm::RoleStorage>,
    /// Promote to a preset's larger `context_promotion_target` before compacting (default `false`).
    #[serde(default)]
    pub(super) context_promotion: bool,
    /// Prefer each preset's `max_context_window` when available (default `false`).
    #[serde(default)]
    pub(super) extended_context: bool,
    /// Allow sending API keys over plaintext HTTP to non-loopback hosts during login probes
    /// (default `false`; set `true` only for trusted LAN gateways).
    #[serde(default)]
    pub(super) allow_plaintext_http: bool,
    /// `[llm.retry]` pool reserve settings.
    #[serde(default)]
    pub(super) retry: hrn_edge_llm::LlmRetryConfig,
    /// `[[llm.accounts]]` per-account routing policies.
    #[serde(default)]
    pub(super) accounts: Vec<hrn_edge_oauth::AccountPolicy>,
    /// `[llm.tier]` per-provider-family default service tiers.
    #[serde(default)]
    pub(super) tier: hrn_edge_llm::LlmTierConfig,
    /// `[llm.anthropic]` subscription transport toggles.
    #[serde(default)]
    pub(super) anthropic: hrn_edge_llm::LlmAnthropicConfig,
    /// `[llm.kimi]` Kimi Code subscription wire format (`auto` | `openai` | `anthropic`).
    #[serde(default)]
    pub(super) kimi: hrn_edge_llm::LlmKimiConfig,
}

/// `[providers.kimi]` shorthand table when `config.toml` sets Kimi provider options without
/// declaring a `[[providers]]` array.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NamedProvidersFile {
    #[serde(default)]
    pub(super) kimi: Option<hrn_edge_llm::LlmKimiConfig>,
}

/// Top-level `providers` key: either `[[providers]]` array or `[providers.<name>]` table.
#[derive(Debug)]
pub(super) enum ProvidersSection {
    List(Vec<ProviderFile>),
    Named(NamedProvidersFile),
}

impl Default for ProvidersSection {
    fn default() -> Self {
        Self::List(Vec::new())
    }
}

impl<'de> Deserialize<'de> for ProvidersSection {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct ProvidersVisitor;
        impl<'de> serde::de::Visitor<'de> for ProvidersVisitor {
            type Value = ProvidersSection;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a `[[providers]]` array or `[providers.<name>]` table")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let de = serde::de::value::SeqAccessDeserializer::new(seq);
                Vec::<ProviderFile>::deserialize(de).map(ProvidersSection::List)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let de = serde::de::value::MapAccessDeserializer::new(map);
                NamedProvidersFile::deserialize(de).map(ProvidersSection::Named)
            }
        }
        deserializer.deserialize_any(ProvidersVisitor)
    }
}

/// `[providers.oauth]` block for `OAuth2` PKCE login + automatic token refresh.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderOAuthFile {
    authorize_url: Option<String>,
    token_url: String,
    client_id: String,
    scope: Option<String>,
    token_file: Option<String>,
    #[serde(default)]
    callback_port: u16,
    callback_path: Option<String>,
    redirect_uri: Option<String>,
    refresh_skew_secs: Option<u64>,
    #[serde(default)]
    json_body: bool,
    #[serde(default)]
    extra_params: BTreeMap<String, String>,
}

/// One `[[providers]]` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderFile {
    name: String,
    kind: ProviderKind,
    base_url: Option<String>,
    auth_mode: Option<AuthMode>,
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    oauth: Option<ProviderOAuthFile>,
    #[serde(default)]
    header_envs: BTreeMap<String, String>,
    prompt_cache: Option<bool>,
    #[serde(default)]
    strip_temperature: bool,
    #[serde(default)]
    slow_mode: bool,
    #[serde(default)]
    api_format: Option<hrn_edge_llm::KimiApiFormat>,
    timeout_secs: Option<u64>,
    // Antigravity CLI (`kind = "agy_acp"`) specific options:
    binary: Option<String>,
    tool_mode: Option<hrn_edge_llm::AgyToolMode>,
    handoff_threshold_tokens: Option<u64>,
    handoff_model: Option<String>,
    turn_timeout_ms: Option<u64>,
    stall_timeout_ms: Option<u64>,
    tool_stall_timeout_ms: Option<u64>,
    #[serde(default)]
    add_dirs: Vec<String>,
}

/// One `[[mcp]]` server entry in `config.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpServerFile {
    name: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    header_envs: BTreeMap<String, String>,
    enabled: Option<bool>,
    #[serde(default)]
    allow_tools: Vec<String>,
    #[serde(default)]
    deny_tools: Vec<String>,
    timeout_secs: Option<u64>,
    auto_approve: Option<bool>,
    #[serde(default)]
    read_only: bool,
}

/// Lower one `[[mcp]]` entry.
pub(super) fn lower_mcp(m: McpServerFile) -> Result<McpServerConfig> {
    let name = m.name.trim().to_owned();
    if name.is_empty() {
        return Err(bad("an [[mcp]] entry has an empty `name`"));
    }
    let transport = match (
        m.command.filter(|c| !c.trim().is_empty()),
        m.url.filter(|u| !u.trim().is_empty()),
    ) {
        (Some(command), None) => McpTransport::Stdio {
            command,
            args: m.args,
            cwd: m.cwd.filter(|c| !c.trim().is_empty()).map(PathBuf::from),
            env: m.env,
        },
        (None, Some(url)) => McpTransport::Http {
            url,
            headers: m.headers,
            header_envs: m.header_envs,
        },
        (Some(_), Some(_)) => {
            return Err(bad(format!(
                "MCP server `{name}`: set only one of `command` or `url`"
            )));
        }
        (None, None) => {
            return Err(bad(format!(
                "MCP server `{name}`: set either `command` (stdio) or `url` (http)"
            )));
        }
    };
    Ok(McpServerConfig {
        name,
        transport,
        enabled: m.enabled.unwrap_or(true),
        allow_tools: m.allow_tools,
        deny_tools: m.deny_tools,
        timeout_secs: m.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1),
        auto_approve: m.auto_approve.unwrap_or(true),
        read_only: m.read_only,
        source: "config.toml".to_owned(),
    })
}

/// Lower one `[[providers]]` entry.
pub(super) fn lower_provider(
    llm: &mut LlmConfig,
    oauth_logins: &mut BTreeMap<String, PkceLoginConfig>,
    p: ProviderFile,
    home: Option<&Path>,
) -> Result {
    let name = p.name.trim().to_owned();
    if name.is_empty() {
        return Err(bad("a provider has an empty `name`"));
    }
    let has_agy_fields = p.binary.is_some()
        || p.tool_mode.is_some()
        || p.handoff_threshold_tokens.is_some()
        || p.handoff_model.is_some()
        || p.turn_timeout_ms.is_some()
        || p.stall_timeout_ms.is_some()
        || p.tool_stall_timeout_ms.is_some()
        || !p.add_dirs.is_empty();
    let key_sources = usize::from(p.api_key_env.is_some())
        + usize::from(p.api_key_file.is_some())
        + usize::from(p.oauth.is_some());

    if p.kind == ProviderKind::AgyAcp {
        return lower_agy_provider(llm, &name, p, key_sources);
    }

    if has_agy_fields {
        return Err(bad(format!(
            "provider `{name}`: agy_acp options (`binary`, `tool_mode`, `handoff_*`, \
             `*_timeout_ms`, `add_dirs`) are only valid for kind = \"agy_acp\""
        )));
    }
    if p.auth_mode == Some(AuthMode::Agy) {
        return Err(bad(format!(
            "provider `{name}`: auth_mode = \"agy\" requires kind = \"agy_acp\""
        )));
    }
    if key_sources > 1 {
        return Err(bad(format!(
            "provider `{name}`: set only one of api_key_env, api_key_file, or oauth"
        )));
    }
    let auth_mode = p.auth_mode.unwrap_or(if p.oauth.is_some() {
        AuthMode::OauthJson
    } else {
        AuthMode::Default
    });
    let subscription = auth_mode.subscription();
    if let Some(kind) = subscription
        && key_sources > 0
    {
        return Err(bad(format!(
            "provider `{name}`: auth_mode = \"{id}\" uses `hrn login {id}`; remove \
             api_key_env, api_key_file and oauth",
            id = kind.id()
        )));
    }
    if p.kind != ProviderKind::OpenaiChat && key_sources == 0 && subscription.is_none() {
        return Err(bad(format!(
            "provider `{name}`: api_key_env, api_key_file, or oauth is required for this kind"
        )));
    }
    let token_file = match subscription {
        Some(kind) => Some(
            paths::auth_dir(home)?
                .join(kind.file_name())
                .to_string_lossy()
                .into_owned(),
        ),
        None => p.api_key_file,
    };
    let oauth = match p.oauth {
        Some(raw) => Some(lower_oauth(&name, raw, oauth_logins, home)?),
        None => None,
    };
    let prompt_cache = p.prompt_cache.unwrap_or(p.kind == ProviderKind::Anthropic);
    let entry = ProviderEntry {
        kind: p.kind,
        base_url: p.base_url,
        auth_mode,
        token_env: p.api_key_env,
        env_file: None,
        token_file,
        oauth,
        header_envs: p.header_envs,
        prompt_cache,
        strip_temperature: p.strip_temperature,
        slow_mode: p.slow_mode,
        api_format: p.api_format.unwrap_or_default(),
        timeout_secs: p.timeout_secs,
        ..ProviderEntry::default()
    };
    if llm.providers.insert(name.clone(), entry).is_some() {
        return Err(bad(format!("duplicate provider `{name}`")));
    }
    Ok(())
}

/// Lower one `kind = "agy_acp"` provider entry.
fn lower_agy_provider(
    llm: &mut LlmConfig,
    name: &str,
    p: ProviderFile,
    key_sources: usize,
) -> Result {
    if let Some(mode) = p.auth_mode
        && !matches!(mode, AuthMode::Agy | AuthMode::Default)
    {
        return Err(bad(format!(
            "provider `{name}`: kind = \"agy_acp\" only accepts auth_mode = \"agy\""
        )));
    }
    if key_sources > 0
        || p.base_url.as_deref().is_some_and(|u| !u.trim().is_empty())
        || !p.header_envs.is_empty()
    {
        return Err(bad(format!(
            "provider `{name}`: kind = \"agy_acp\" delegates auth and transport to the `agy` \
             CLI; remove base_url, api_key_env, api_key_file, header_envs, and oauth"
        )));
    }
    let defaults = hrn_edge_llm::AgyProviderConfig::default();
    let agy = hrn_edge_llm::AgyProviderConfig {
        binary: p.binary,
        tool_mode: p.tool_mode.unwrap_or(defaults.tool_mode),
        handoff_threshold_tokens: p
            .handoff_threshold_tokens
            .unwrap_or(defaults.handoff_threshold_tokens),
        handoff_model: p.handoff_model.unwrap_or(defaults.handoff_model),
        turn_timeout_ms: p.turn_timeout_ms.unwrap_or(defaults.turn_timeout_ms),
        stall_timeout_ms: p.stall_timeout_ms.unwrap_or(defaults.stall_timeout_ms),
        tool_stall_timeout_ms: p
            .tool_stall_timeout_ms
            .unwrap_or(defaults.tool_stall_timeout_ms),
        add_dirs: p.add_dirs,
    };
    agy.validate(name)?;
    let entry = ProviderEntry {
        kind: ProviderKind::AgyAcp,
        auth_mode: AuthMode::Agy,
        prompt_cache: p.prompt_cache.unwrap_or(true),
        agy,
        ..ProviderEntry::default()
    };
    if llm.providers.insert(name.to_owned(), entry).is_some() {
        return Err(bad(format!("duplicate provider `{name}`")));
    }
    Ok(())
}

/// Lower the `[providers.oauth]` block: the credential refresh config and, when an authorize
/// URL is set, the interactive PKCE login (`hrn login <provider>`).
pub(super) fn lower_oauth(
    provider_name: &str,
    raw: ProviderOAuthFile,
    oauth_logins: &mut BTreeMap<String, PkceLoginConfig>,
    home: Option<&Path>,
) -> Result<OAuthConfig> {
    let token_file = match raw.token_file {
        Some(path) if !path.trim().is_empty() => path,
        _ => paths::default_token_file(provider_name, home)?
            .to_string_lossy()
            .into_owned(),
    };
    if let Some(authorize_url) = raw.authorize_url.filter(|u| !u.trim().is_empty()) {
        oauth_logins.insert(
            provider_name.to_owned(),
            PkceLoginConfig {
                authorize_url,
                token_url: raw.token_url.clone(),
                client_id: raw.client_id.clone(),
                scope: raw.scope.clone().unwrap_or_default(),
                callback_port: raw.callback_port,
                callback_path: raw.callback_path.unwrap_or_else(|| "/callback".to_owned()),
                redirect_uri_override: raw.redirect_uri,
                extra_params: raw.extra_params.into_iter().collect(),
                json_body: raw.json_body,
                token_file: token_file.clone(),
            },
        );
    }
    Ok(OAuthConfig {
        token_url: raw.token_url,
        client_id: raw.client_id,
        token_file,
        scope: raw.scope,
        refresh_skew_secs: raw.refresh_skew_secs.unwrap_or(300),
        json_body: raw.json_body,
    })
}

impl super::Config {
    /// Load and validate all 7 layers (defaults -> omp -> global -> project -> overlays -> env ->
    /// runtime), absorbing explicit `catalogs` underneath before validating roles and presets.
    pub(crate) fn load_layered_with_catalogs(
        input: &crate::config_layers::LayerInput,
        catalogs: Vec<hrn_edge_llm::Catalog>,
    ) -> Result<(Self, crate::config_layers::MergedConfig)> {
        let merged = crate::config_layers::load_layers(input)?;
        let file: super::ConfigFile = toml::Value::Table(merged.root.clone())
            .try_into()
            .map_err(|e| bad(e.to_string()))?;
        let cfg = Self::lower_file(file, input.home.as_deref(), catalogs)?;
        Ok((cfg, merged))
    }
}
