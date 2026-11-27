//! Command-line surface of `hrn`.

use clap::{Args, Parser, Subcommand, ValueEnum};
use hrn_kernel::{PermissionMode, ThinkingLevel, Tier};
use std::path::PathBuf;

#[path = "cli_sub.rs"]
mod cli_sub;
pub(crate) use cli_sub::{
    CommitArgs, CompletionShell, DryBalanceArgs, ExportArgs, ExportFormat, GcArgs, GitArgs,
    ImportArgs, ImportSourceArg, JoinArgs, McpCommand, ProvidersCommand, ProvidersImportArgs,
    ProvidersImportSource, RemoteCommand, ShareArgs, UpdateArgs, UsageCommand, WorktreeCommand,
};

/// `hrn`: a multi-agent coding harness.
#[derive(Debug, Parser)]
#[command(name = "hrn", version, about)]
pub(crate) struct Cli {
    /// Overlay config file(s); repeatable, later wins (missing file is an error).
    #[arg(long = "config", global = true, value_name = "PATH")]
    pub(crate) config: Vec<PathBuf>,
    /// Named config profile (`~/.harness/profiles/<name>/`; overrides `HRN_PROFILE`).
    #[arg(long, global = true, value_name = "NAME")]
    pub(crate) profile: Option<String>,
    /// Tool approval mode (`always-ask`, `always-ask-cached`, `write`, `yolo`, `plan`).
    #[arg(long, global = true, value_name = "MODE", value_parser = parse_approval_mode)]
    pub(crate) approval_mode: Option<PermissionMode>,
    /// Auto-approve tools (alias for `--approval-mode yolo`).
    #[arg(long = "auto-approve", visible_alias = "yolo", global = true)]
    pub(crate) auto_approve: bool,
    /// Serve a stdio protocol instead of the interactive chat (`text`/`json` output is `hrn exec`).
    #[arg(long, value_name = "MODE", value_enum)]
    pub(crate) mode: Option<ServeMode>,
    #[command(flatten)]
    pub(crate) serve: ServeArgs,
    /// Resume the most recent session of this directory (`hrn chat -c` does the same).
    #[arg(short = 'c', long = "continue")]
    pub(crate) continue_session: bool,
    /// Run without touching the session store: nothing is saved, nothing is resumed (chat, exec).
    #[arg(long, global = true)]
    pub(crate) no_session: bool,

    // Without a subcommand `hrn` opens the interactive chat.
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Open the interactive chat (the default).
    Chat(ChatArgs),
    /// Run one headless agent turn in a directory.
    Exec(ExecArgs),
    /// Run a session tool as a shell command (`hrn tool <name> [args]`; the agent's shell defines
    /// a function per tool). Hidden: it is what the tool commands execute, not for humans.
    #[command(hide = true, disable_help_flag = true)]
    Tool {
        /// The tool name and its command-line arguments, passed through untouched.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        argv: Vec<String>,
    },
    /// List stored sessions (pinned first, then most recently updated); `export`, `delete`,
    /// `rename`, `pin`, `unpin`, and `import` act on one.
    Sessions {
        #[command(subcommand)]
        command: Option<SessionsCommand>,
    },
    /// Garbage-collect old unpinned sessions, orphan artifacts, and stale locks (dry-run by default).
    Gc(GcArgs),
    /// Sessions, tokens and cost by model and day.
    Stats,
    /// List the models hrn can use, named `<provider>/<id>` as omp names them.
    Models,
    /// Manage or import external LLM providers (`hrn providers import omp`).
    Providers {
        #[command(subcommand)]
        command: ProvidersCommand,
    },
    /// Show provider usage limits for every logged-in subscription (`accounts` lists stored account ids).
    Usage {
        #[command(subcommand)]
        command: Option<UsageCommand>,
    },
    /// Offline simulation of multi-account pool load balancing (zero network calls).
    DryBalance(DryBalanceArgs),
    /// List discovered subagent profiles (built-in + global + project).
    Agents,
    /// List discovered `SKILL.md` skills (global + project); a session loads one with
    /// `/skill:<name> [args]`.
    Skills,
    /// List discovered custom slash commands (global + project).
    Commands,
    /// Log in: provider API keys, an `OAuth2` PKCE provider, or a subscription
    /// (`claude` = Claude Pro/Max, `chatgpt` = `ChatGPT` Plus/Pro, `kimi-code` = Kimi Code, `agy` = Antigravity CLI).
    Login(LoginArgs),
    /// Log out of a provider or subscription: delete its stored credentials.
    Logout {
        /// Provider or subscription id (`claude`, `chatgpt`, `kimi-code`, or configured provider). Omit for interactive picker.
        provider: Option<String>,
    },
    /// First-run onboarding wizard (login, default model role, theme).
    Setup,
    /// Inspect provider authentication status.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Check configuration, provider credentials, and Antigravity CLI (`agy`) health.
    Doctor,
    /// Configuration utilities (`list`, `get`, `set`, `reset`, `path`, `check`).
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Inspect, ping, or serve MCP (Model Context Protocol) servers.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Print the per-section system-prompt token table for a workspace and fail over budget
    /// (the gate behind `cargo xtask promptstats`). Hidden: CI tooling, not for humans.
    #[command(name = "promptstats", hide = true)]
    PromptStats {
        /// Workspace to assemble the prompt for (default: the current directory).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// Use the shipped fixture config and workspace instead of the user's.
        #[arg(long)]
        fixture: bool,
    },
    /// Run the local multi-agent loopback HTTP+SSE daemon.
    Daemon {
        /// Loopback listen address (default `127.0.0.1:0` ephemeral port).
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: String,
    },
    /// Attach to a local daemon session or `ssh://[user@]host[/session]` remote daemon.
    Attach {
        /// Local session ID/prefix or `ssh://[user@]host[:port][/session_id]`.
        target: String,
    },
    /// Seal and share a session snapshot as an encrypted link.
    Share(ShareArgs),
    /// Join a live encrypted collaboration room as a guest.
    Join(JoinArgs),
    /// Manage isolated git worktrees (`<repo>/.harness/wt/<slug>`).
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Manage SSH-tunneled remote `hrn` daemons and pinned host keys.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Stage and commit ONLY edits attributed to the current/specified hrn session.
    Commit(CommitArgs),
    /// Interactive fullscreen git status, diff, stage/unstage, and commit workbench.
    Git(GitArgs),
    /// Check for and install SHA-256 verified `hrn` release updates.
    Update(UpdateArgs),
    /// Generate shell completion scripts (`bash`, `zsh`, `fish`, `powershell`).
    Completions {
        /// Target shell (`bash`, `zsh`, `fish`, `powershell`).
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Serve omp's NDJSON RPC protocol on stdin/stdout.
    Rpc(ServeArgs),
    /// Serve omp's RPC protocol with the `ask` tool's form dialogs enabled (`rpc-ui`).
    RpcUi(ServeArgs),
    /// Serve Zed's Agent Client Protocol (ACP) on stdin/stdout.
    Acp(ServeArgs),
    /// A command an installed module provides (`hrn <module command> ...`).
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Which stdio protocol `--mode` serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ServeMode {
    /// omp's NDJSON RPC protocol.
    Rpc,
    /// omp's RPC protocol with `ask` form dialogs (omp's `rpc-ui`).
    RpcUi,
    /// Zed's Agent Client Protocol.
    Acp,
}

/// `hrn rpc|rpc-ui|acp`: one protocol server on stdio.
#[derive(Debug, Default, Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent serve flags (--yes, --no-ui, --no-lsp, --no-title)"
)]
pub(crate) struct ServeArgs {
    /// Model alias or role (default: the `default` role).
    #[arg(long, value_name = "ALIAS")]
    pub(crate) model: Option<String>,
    /// Thinking effort: off, low, medium, high or xhigh (default: from the config).
    #[arg(long, value_name = "LEVEL", value_parser = parse_think)]
    pub(crate) think: Option<ThinkingLevel>,
    /// Working directory of agent sessions (default: the current directory).
    #[arg(long, value_name = "DIR")]
    pub(crate) cwd: Option<PathBuf>,
    /// Approve every mutating tool call without asking.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Do not send UI requests at all: approvals are refused and `ask` reports it has nobody
    /// to ask (`hrn --no-ui`).
    #[arg(long)]
    pub(crate) no_ui: bool,
    /// Replace the base system prompt with this text, or the contents of this file.
    #[arg(long, value_name = "TEXT|PATH")]
    pub(crate) system_prompt: Option<String>,
    /// Append this text (or the contents of this file) after every `APPEND_SYSTEM.md` layer.
    #[arg(long, value_name = "TEXT|PATH")]
    pub(crate) append_system_prompt: Option<String>,
    /// Non-persisted session service tier (`none|auto|default|flex|scale|priority|ultrafast`).
    #[arg(long = "service-tier", value_name = "TIER", value_parser = parse_service_tier)]
    pub(crate) service_tier: Option<Tier>,
    /// Disable the LSP tool and post-write/edit hooks for this session (`lsp.enabled = false`).
    #[arg(long)]
    pub(crate) no_lsp: bool,
    /// Suppress automatic model-generated session titles for this run.
    #[arg(long)]
    pub(crate) no_title: bool,
}

/// `hrn auth <command>`.
#[derive(Debug, Subcommand)]
pub(crate) enum AuthCommand {
    /// Show auth status for all configured providers without printing secrets.
    Status,
}

/// Arguments of `hrn login`.
#[derive(Debug, Args)]
pub(crate) struct LoginArgs {
    /// Optional provider name (`claude`, `chatgpt`, `kimi-code`, `agy`, `anthropic`, `openai`, `gemini`, `openrouter`, `ollama`, `groq`, `moonshot`, `xai`, `deepseek`, `custom`, or an OAuth provider). Omit for an interactive menu.
    pub(crate) provider: Option<String>,
    /// Headless/SSH mode: print the auth URL and paste the redirected URL or code manually.
    #[arg(long)]
    pub(crate) manual: bool,
    /// Print the authorization URL without trying to open a browser automatically.
    #[arg(long)]
    pub(crate) no_browser: bool,
    /// Authentication method for `hrn login agy` (`oauth-personal` or `oauth-business`).
    #[arg(long, value_name = "METHOD")]
    pub(crate) method: Option<String>,
}

/// Scope filter for `hrn config list --scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ConfigScope {
    /// Only values set in the global config file.
    Global,
    /// Only values set in `<cwd>/.harness/config.toml`.
    Project,
}

/// `hrn config <command>`.
#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// List settings grouped by category with their effective values and sources.
    List {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Show only raw keys written in a specific layer (`global` or `project`).
        #[arg(long, value_enum)]
        scope: Option<ConfigScope>,
    },
    /// Show the effective value of one setting key.
    Get {
        /// Exact dotted setting key (e.g. `tools.premium_defer`).
        key: String,
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Set a key in the global config file (preserving comments).
    Set {
        /// Exact dotted setting key.
        key: String,
        /// New value (parsed according to the key's schema type).
        value: String,
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete a key from the global config file so lower layers or defaults apply.
    Reset {
        /// Exact dotted setting key.
        key: String,
    },
    /// Print the active config directory and global config file path.
    Path,
    /// Validate the config file and print the resolved roles.
    Check,
}

/// The instruction-source switches shared by `chat` and `exec`.
#[derive(Debug, Default, Clone, Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one bool per CLI disable switch (--no-skills/--no-rules/--no-context-files/--no-lsp)"
)]
pub(crate) struct SourceArgs {
    /// Only load skills whose name matches these comma-separated globs
    /// (`/skill:<name> [args]` loads a discovered skill into the conversation).
    #[arg(long, value_name = "GLOBS")]
    pub(crate) skills: Option<String>,
    /// Do not discover or inject skills at all.
    #[arg(long)]
    pub(crate) no_skills: bool,
    /// Do not discover or inject rules (`.harness/rules`, `.omp`, `.cursor`, `.claude`).
    #[arg(long)]
    pub(crate) no_rules: bool,
    /// Do not inject `AGENTS.md` / `CLAUDE.md` context files.
    #[arg(long)]
    pub(crate) no_context_files: bool,
    /// Disable the LSP tool and post-write/edit hooks for this session (`lsp.enabled = false`).
    #[arg(long)]
    pub(crate) no_lsp: bool,
    /// Replace the base system prompt with this text, or the contents of this file
    /// (`--system-prompt`, omp's flag of the same name).
    #[arg(long, value_name = "TEXT|PATH")]
    pub(crate) system_prompt: Option<String>,
    /// Append this text (or the contents of this file) after every `APPEND_SYSTEM.md` layer.
    #[arg(long, value_name = "TEXT|PATH")]
    pub(crate) append_system_prompt: Option<String>,
    /// Extra hook executable for this session (repeatable); gets every event.
    #[arg(long, value_name = "PATH")]
    pub(crate) hook: Vec<PathBuf>,
}

/// The omp role-flag surface shared by `hrn chat` and `hrn exec` (`--smol` / `--slow` /
/// `--plan` set model roles, `--preset` applies a `[modelPresets.<name>]` bundle).
#[derive(Debug, Default, Args)]
pub(crate) struct RoleFlags {
    /// Smol/fast model for lightweight tasks (sets the `fast` role; omp's `--smol`).
    #[arg(long, value_name = "ALIAS")]
    pub(crate) smol: Option<String>,
    /// Slow/reasoning model for thorough analysis (sets the `slow` role; omp's `--slow`).
    #[arg(long, value_name = "ALIAS")]
    pub(crate) slow: Option<String>,
    /// Plan model for architectural planning (sets the `plan` role; omp's `--plan`).
    #[arg(long, value_name = "ALIAS")]
    pub(crate) plan: Option<String>,
    /// Apply a configured `[modelPresets.<name>]` role bundle before starting.
    #[arg(long, value_name = "NAME")]
    pub(crate) preset: Option<String>,
}

/// Arguments of `hrn chat`.
#[derive(Debug, Default, Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one bool per permission/visibility switch"
)]
pub(crate) struct ChatArgs {
    /// Instruction-source switches (skills, rules, context files, hooks).
    #[command(flatten)]
    pub(crate) sources: SourceArgs,
    /// Model alias or role (default: the `default` role).
    #[arg(long, value_name = "ALIAS")]
    pub(crate) model: Option<String>,
    /// Resume a stored session by id or unique id prefix (see `hrn sessions`).
    #[arg(long, value_name = "SESSION")]
    pub(crate) resume: Option<String>,
    /// Resume an interrupted session and continue its unfinished turn (latest in cwd when omitted).
    #[arg(
        long,
        value_name = "SESSION",
        num_args = 0..=1,
        default_missing_value = "",
        conflicts_with_all = ["goal", "no_session", "continue_session", "resume", "from_claude", "from_codex", "from_omp"]
    )]
    pub(crate) restore: Option<String>,
    /// Import a Claude Code session read-only and open it in chat (`latest` when omitted).
    #[arg(
        long = "from-claude",
        value_name = "SESSION",
        num_args = 0..=1,
        default_missing_value = "latest",
        conflicts_with_all = ["resume", "continue_session", "restore", "no_session", "from_codex", "from_omp"]
    )]
    pub(crate) from_claude: Option<String>,
    /// Import a Codex CLI session read-only and open it in chat (`latest` when omitted).
    #[arg(
        long = "from-codex",
        value_name = "SESSION",
        num_args = 0..=1,
        default_missing_value = "latest",
        conflicts_with_all = ["resume", "continue_session", "restore", "no_session", "from_claude", "from_omp"]
    )]
    pub(crate) from_codex: Option<String>,
    /// Import an OMP session read-only and open it in chat (`latest` when omitted).
    #[arg(
        long = "from-omp",
        value_name = "SESSION",
        num_args = 0..=1,
        default_missing_value = "latest",
        conflicts_with_all = ["resume", "continue_session", "restore", "no_session", "from_claude", "from_codex"]
    )]
    pub(crate) from_omp: Option<String>,
    /// Suppress automatic model-generated session titles for this chat session.
    #[arg(long)]
    pub(crate) no_title: bool,
    /// Enable the passive post-turn advisor reviewer.
    #[arg(long)]
    pub(crate) advisor: bool,
    /// Model alias or role for the passive advisor (implies `--advisor`).
    #[arg(long, value_name = "ALIAS")]
    pub(crate) advisor_model: Option<String>,
    /// Resume the most recent session of this directory.
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    pub(crate) continue_session: bool,
    /// Start in goal mode (omp's `--goal`): work autonomously on the objective until the agent
    /// completes it or the budget runs out.
    #[arg(long, value_name = "OBJECTIVE")]
    pub(crate) goal: Option<String>,
    /// Goal token budget for `--goal` (default: unbounded).
    #[arg(long, value_name = "TOKENS", requires = "goal")]
    pub(crate) goal_budget: Option<u64>,
    /// Hide thinking blocks in the transcript (display only; the model still thinks).
    #[arg(long)]
    pub(crate) hide_thinking: bool,
    /// omp's role flags (`--smol` / `--slow` / `--plan` / `--preset`).
    #[command(flatten)]
    pub(crate) roles: RoleFlags,
    /// Thinking effort for this session (also accepted as a `model:level` suffix on `--model`).
    #[arg(long, value_name = "LEVEL", value_parser = parse_think)]
    pub(crate) thinking: Option<ThinkingLevel>,
    /// Non-persisted session service tier (`none|auto|default|flex|scale|priority|ultrafast`).
    #[arg(long = "service-tier", value_name = "TIER", value_parser = parse_service_tier)]
    pub(crate) service_tier: Option<Tier>,
    /// Run without touching the session store (set from the top-level `--no-session`).
    #[arg(skip)]
    pub(crate) no_session: bool,
}

/// Arguments of `hrn exec`.
#[derive(Debug, Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent exec CLI flags (--yes, --advisor, --no-session, --no-title, --no-mcp, --read-only, --no-tools)"
)]
pub(crate) struct ExecArgs {
    /// Instruction-source switches (skills, rules, context files, hooks).
    #[command(flatten)]
    pub(crate) sources: SourceArgs,
    /// Model alias or role (default: the `default` role).
    #[arg(long, value_name = "ALIAS")]
    pub(crate) model: Option<String>,
    /// Thinking effort: off, minimal, low, medium, high, xhigh, max or auto (default: config).
    #[arg(long, value_name = "LEVEL", value_parser = parse_think)]
    pub(crate) think: Option<ThinkingLevel>,
    /// Non-persisted session service tier (`none|auto|default|flex|scale|priority|ultrafast`).
    #[arg(long = "service-tier", value_name = "TIER", value_parser = parse_service_tier)]
    pub(crate) service_tier: Option<Tier>,
    /// Start in goal mode (omp's `--goal`): keep turning until the agent completes the
    /// objective or the budget runs out.
    #[arg(long, value_name = "OBJECTIVE")]
    pub(crate) goal: Option<String>,
    /// Goal token budget for `--goal` (default: unbounded).
    #[arg(long, value_name = "TOKENS", requires = "goal")]
    pub(crate) goal_budget: Option<u64>,
    /// Goal time budget in seconds for `--goal`.
    #[arg(long, value_name = "SECONDS", requires = "goal")]
    pub(crate) goal_time_budget: Option<u64>,
    /// omp's role flags (`--smol` / `--slow` / `--plan` / `--preset`).
    #[command(flatten)]
    pub(crate) roles: RoleFlags,
    /// Working directory of the agent (default: the current directory).
    #[arg(long, value_name = "DIR")]
    pub(crate) cwd: Option<PathBuf>,
    /// Resume an existing session by exact id or unique prefix.
    #[arg(long, value_name = "SESSION")]
    pub(crate) resume: Option<String>,
    /// Override the SQLite session database path (for per-run or per-caller isolation).
    #[arg(long, value_name = "PATH")]
    pub(crate) db: Option<PathBuf>,
    /// Approve every mutating tool call (file edits, shell). Without it they are refused.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Enable the passive post-turn advisor reviewer.
    #[arg(long)]
    pub(crate) advisor: bool,
    /// Model alias or role for the passive advisor (implies `--advisor`).
    #[arg(long, value_name = "ALIAS")]
    pub(crate) advisor_model: Option<String>,
    /// Output: `text` (final answer on stdout) or `json` (one event per line on stdout).
    #[arg(long, value_enum, default_value_t = OutputMode::Text)]
    pub(crate) mode: OutputMode,
    /// Suppress automatic model-generated session titles for this run.
    #[arg(long)]
    pub(crate) no_title: bool,
    /// Skip MCP server discovery/connection and startup model catalog HTTP probes.
    #[arg(long)]
    pub(crate) no_mcp: bool,
    /// Strip all mutating tools from the agent's tool registry.
    #[arg(long)]
    pub(crate) read_only: bool,
    /// Restrict the agent to this comma-separated tool allowlist.
    #[arg(long, value_name = "TOOLS", conflicts_with = "no_tools")]
    pub(crate) tools: Option<String>,
    /// Disable all tools (pure text reasoning without tool schemas).
    #[arg(long)]
    pub(crate) no_tools: bool,
    /// Run without touching the session store (set from the top-level `--no-session`).
    #[arg(skip)]
    pub(crate) no_session: bool,
    /// The task for the agent (pass `-` or omit with piped stdin; optional when `--goal` is set).
    pub(crate) prompt: Option<String>,
}

/// Output of `hrn exec`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputMode {
    /// The final answer on stdout, progress on stderr.
    #[default]
    Text,
    /// One JSON object per line on stdout: a session header, then every agent event.
    Json,
}

/// `hrn sessions <command>`; without one the sessions are listed.
#[derive(Debug, Subcommand)]
pub(crate) enum SessionsCommand {
    /// Write a session as markdown or html.
    Export(ExportArgs),
    /// Delete a session, its messages, its usage and its subagent sessions.
    Delete {
        /// Session id or unique id prefix (see `hrn sessions`).
        session: String,
        /// Delete without the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Set a session's title (what `hrn sessions` and the picker show).
    Rename {
        /// Session id or unique id prefix (see `hrn sessions`).
        session: String,
        /// The new title.
        title: String,
    },
    /// Keep a session at the top of the list.
    Pin {
        /// Session id or unique id prefix (see `hrn sessions`).
        session: String,
    },
    /// Let a session fall back into the list.
    Unpin {
        /// Session id or unique id prefix (see `hrn sessions`).
        session: String,
    },
    /// List or import Claude Code / Codex CLI / OMP sessions read-only into hrn.
    Import(ImportArgs),
}

impl Cli {
    /// Explicit CLI approval mode override (`--approval-mode`, `--auto-approve`, or `--yolo`).
    pub(crate) fn approval_mode_override(&self) -> Option<PermissionMode> {
        self.approval_mode
            .or_else(|| self.auto_approve.then_some(PermissionMode::Yolo))
    }
}

fn parse_approval_mode(raw: &str) -> Result<PermissionMode, String> {
    PermissionMode::parse(raw).ok_or_else(|| {
        format!(
            "`{raw}` is not one of always-ask, always-ask-cached, write, yolo, plan, default, auto-edit"
        )
    })
}

fn parse_think(raw: &str) -> Result<ThinkingLevel, String> {
    ThinkingLevel::parse(raw).ok_or_else(|| {
        format!("`{raw}` is not one of auto, off, minimal, low, medium, high, xhigh, max")
    })
}

fn parse_service_tier(raw: &str) -> Result<Tier, String> {
    Tier::parse(raw).ok_or_else(|| {
        format!("`{raw}` is not one of none, auto, default, flex, scale, priority, ultrafast")
    })
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
