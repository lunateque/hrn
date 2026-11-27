//! Data the information commands render: `/settings` notifications, `/mcp` servers,
//! `/context` usage, `/tools` rows and `/jobs` rows. The `Backend` trait exposes them;
//! `crate::app::panels` formats them.

use std::time::Duration;

/// Desktop notification delivery (`[tui] notify`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notify {
    /// Notifications are off when `false`.
    pub enabled: bool,
    /// A still-running turn this old earns a notification even while the window is focused.
    pub after: Duration,
    /// The title in the toast and the terminal window title.
    pub title: String,
}

impl Default for Notify {
    fn default() -> Self {
        Self {
            enabled: true,
            after: Duration::from_secs(30),
            title: "hrn".to_owned(),
        }
    }
}

/// One MCP server as `/mcp` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpStatus {
    /// Configured server name.
    pub name: String,
    /// `stdio` or `http`, with the endpoint or command.
    pub transport: String,
    /// Where the entry came from (`config.toml`, `.mcp.json`, …).
    pub source: String,
    /// Whether the server was declared enabled.
    pub enabled: bool,
    /// Whether the session holds a live connection to it.
    pub connected: bool,
    /// Tools the session can call from it.
    pub tools: usize,
    /// Deferred tools `tool_search` can activate.
    pub deferred: usize,
    /// Connection or discovery problem, when there is one.
    pub note: Option<String>,
}

/// One advertised MCP resource as `/mcp resources` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpResourceInfo {
    /// Configured server name advertising this resource.
    pub server: String,
    /// Resource URI addressable via `read mcp://<uri>`.
    pub uri: String,
    /// Human-readable resource name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Optional MIME type.
    pub mime_type: Option<String>,
}

/// The live context of the current session as `/context` breaks it down.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextBreakdown {
    /// Tokens the system prompt costs (estimated).
    pub system_tokens: u64,
    /// Tokens the tool schemas cost (estimated).
    pub schema_tokens: u64,
    /// Tokens the conversation messages cost (estimated).
    pub message_tokens: u64,
    /// Model context window.
    pub window: u64,
    /// Tokens of the latest real prompt, when a call has run (`Some` wins over the estimate).
    pub last_prompt_tokens: Option<u64>,
    /// How many DAG nodes the branch holds.
    pub nodes: usize,
}

/// One row of `/tools`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolInfo {
    /// Registered tool name.
    pub name: String,
    /// `read` or `mutating`.
    pub effect: String,
    /// Whether the schema rides in every request or waits behind `tool_search`.
    pub deferred: bool,
}

/// One row of `/jobs`: background work addressable under `proc://`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobInfo {
    /// The `proc://<id>` handle.
    pub id: String,
    /// Whether it still runs.
    pub running: bool,
    /// Whether `wait` blocks on it (services never settle on their own).
    pub waitable: bool,
    /// When it started (ms since epoch).
    pub started_ms: i64,
    /// When it settled (ms since epoch); `0` while running.
    pub finished_ms: i64,
    /// What it is (command, agent task…).
    pub line: String,
}

/// Action taken on double-`Esc` when the editor is empty (`[interaction] double_escape_action`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DoubleEscapeAction {
    /// Open the full `/tree` session DAG navigator.
    #[default]
    Tree,
    /// Open `/branch` (`/rewind`) in user-turns-first mode.
    Rewind,
    /// Do nothing on double-`Esc` when the editor is empty.
    None,
}

impl DoubleEscapeAction {
    /// Parse a config string (`"tree" | "rewind" | "none"`).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tree" => Some(Self::Tree),
            "rewind" => Some(Self::Rewind),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// One user-defined slash command discovered from project or user `commands/*.md` directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomCommandInfo {
    /// Command name without the leading `/` (e.g. `"review"`).
    pub name: String,
    /// Short description shown in completion popups.
    pub description: String,
}

/// One provider option presented by `/login`.
#[expect(
    clippy::struct_excessive_bools,
    reason = "four orthogonal UI descriptor flags for provider login forms"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginProviderChoice {
    /// Stable provider identifier (`anthropic`, `openai`, `gemini`, `openrouter`, `deepseek`, `custom`, or OAuth name).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Short description (endpoint + default presets).
    pub hint: String,
    /// Whether valid credentials are currently resolved for this provider.
    pub configured: bool,
    /// True for custom OpenAI-compatible gateways / local Ollama where URL & model are entered.
    pub needs_custom_fields: bool,
    /// Default base URL suggested when editing custom fields.
    pub default_base_url: String,
    /// Default model ID suggested when editing custom fields.
    pub default_model: String,
    /// True when an API key is optional (e.g. local Ollama).
    pub key_optional: bool,
    /// True when this provider uses interactive browser OAuth PKCE (`hrn login <name>`).
    pub is_oauth: bool,
}

/// Credentials submitted from the in-TUI `/login` view. `Debug` redacts `api_key`.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginSubmission {
    /// Selected provider ID from [`LoginProviderChoice::id`].
    pub provider_id: String,
    /// Optional custom provider name (for `custom`).
    pub custom_name: Option<String>,
    /// API key (empty allowed when `key_optional` is true).
    pub api_key: String,
    /// Optional custom base URL (for `custom`).
    pub base_url: Option<String>,
    /// Optional model ID override.
    pub model_id: Option<String>,
}

impl std::fmt::Debug for LoginSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginSubmission")
            .field("provider_id", &self.provider_id)
            .field("custom_name", &self.custom_name)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "(empty)"
                } else {
                    "[redacted]"
                },
            )
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .finish()
    }
}

/// Result returned by [`crate::Backend::save_login`] after persisting credentials and reloading the router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginOutcome {
    /// Provider name written to config.
    pub provider: String,
    /// Model aliases created or updated for this provider.
    pub aliases: Vec<String>,
    /// Human-readable confirmation summary.
    pub summary: String,
}

/// One external Claude Code or Codex CLI session shown in the `/sessions` import sub-view (`I`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalSessionRow {
    /// External source label (`"claude"` or `"codex"`).
    pub source: String,
    /// Source-native session/thread identifier.
    pub id: String,
    /// Session title or prompt snippet.
    pub title: String,
    /// Working directory of the external session.
    pub cwd: String,
    /// Last updated timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Whether this session came from an archived directory.
    pub archived: bool,
}

/// Result returned by [`crate::Backend::relocate_worktree`] after creating a git worktree,
/// carrying uncommitted changes, and relocating the live session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeOutcome {
    /// Absolute path of the newly created worktree directory.
    pub new_cwd: std::path::PathBuf,
    /// Checked-out branch name in the new worktree.
    pub branch: String,
    /// Count of tracked modified files carried via patch apply.
    pub carried_tracked: usize,
    /// Count of untracked non-ignored files copied into the new worktree.
    pub carried_untracked: usize,
    /// Human-readable summary of the previous checkout's state.
    pub previous_checkout: String,
    /// Non-fatal warnings encountered while carrying uncommitted changes.
    pub warnings: Vec<String>,
}

pub(crate) fn no_store<T>(what: &str) -> hrn_kernel::Result<T> {
    Err(hrn_kernel::HrnError::Store(format!(
        "{what} is not supported by this backend"
    )))
}

pub(crate) fn no_cfg<T>(what: &str) -> hrn_kernel::Result<T> {
    Err(hrn_kernel::HrnError::Config(format!(
        "{what} is not supported by this backend"
    )))
}

pub(crate) fn no_tool<T>(what: &str) -> hrn_kernel::Result<T> {
    Err(hrn_kernel::HrnError::Tool(format!(
        "{what} is not supported by this backend"
    )))
}
