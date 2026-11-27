//! Subcommand definitions split out of `cli.rs` to keep every CLI file concise.

use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

/// `hrn usage <command>`.
#[derive(Debug, Subcommand)]
pub(crate) enum UsageCommand {
    /// Print `<provider>/<account_id>` for every stored subscription account (offline).
    Accounts,
}

/// Arguments of `hrn dry-balance`.
#[derive(Debug, Args)]
pub(crate) struct DryBalanceArgs {
    /// Optional subscription provider to simulate (`claude` or `chatgpt`); omit for all stored.
    pub(crate) provider: Option<String>,
    /// Number of synthetic sessions to distribute through the account pool (default 100).
    #[arg(long, default_value_t = 100, value_name = "N")]
    pub(crate) sessions: usize,
}

/// `hrn worktree <command>`.
#[derive(Debug, Subcommand)]
pub(crate) enum WorktreeCommand {
    /// Create a new git worktree at `<repo>/.harness/wt/<slug>` on branch `agent/<slug>`.
    Add {
        /// Short ASCII slug for the worktree and branch.
        slug: String,
        /// Optional base commit/branch (default: current HEAD).
        #[arg(long)]
        base: Option<String>,
    },
    /// List git worktrees in the current repository.
    List,
    /// Remove a worktree by slug or path.
    Rm {
        /// Worktree slug or path.
        target: String,
        /// Pass `--force` to `git worktree remove`.
        #[arg(long)]
        force: bool,
    },
}

/// `hrn remote <command>`.
#[derive(Debug, Subcommand)]
pub(crate) enum RemoteCommand {
    /// Run the loopback daemon ready for SSH-tunneled clients.
    Serve {
        /// Loopback listen address (default `127.0.0.1:0`).
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: String,
    },
    /// Print this machine's persistent daemon host-key fingerprint (`SHA256:...`).
    HostKey,
    /// Print the active daemon `run.json` to stdout (used internally over SSH stdout).
    #[command(hide = true)]
    PrintRunFile,
    /// Pin a remote daemon's `SHA256:...` host-key fingerprint.
    Trust {
        /// Remote host (`host` or `host:ssh_port`).
        host: String,
        /// Daemon fingerprint (`SHA256:<64 hex chars>`).
        fingerprint: String,
    },
    /// List all pinned remote daemon hosts.
    Hosts,
    /// Query a remote host's daemon fingerprint over SSH and pin it locally.
    Setup {
        /// Remote SSH destination (`[user@]host[:ssh_port]`).
        host: String,
    },
    /// Diagnose SSH connectivity, remote `hrn`, trust pin, and daemon status.
    Doctor {
        /// Remote SSH destination (`[user@]host[:ssh_port]`).
        host: String,
    },
    /// Remove a host from the local trust store.
    Forget {
        /// Remote host (`host` or `host:ssh_port`).
        host: String,
    },
    /// Serve the mobile PWA and print a QR code for instant phone pairing.
    #[command(visible_alias = "web")]
    Pair {
        /// Pair with full interactive control (default unless `--view` is set).
        #[arg(long, conflicts_with = "view")]
        control: bool,
        /// Pair in read-only monitoring mode (cannot send prompts, approve tools, or interrupt).
        #[arg(long, visible_alias = "read-only")]
        view: bool,
        /// Socket address to bind (`0.0.0.0:0` by default for LAN/Tailscale reachability).
        #[arg(long, default_value = "0.0.0.0:0")]
        bind: String,
        /// Optional fixed TCP port (overrides the port in `--bind`).
        #[arg(long)]
        port: Option<u16>,
        /// Public base URL (e.g. Tailscale Serve or Cloudflare Tunnel URL) embedded in the QR code.
        #[arg(long, value_name = "URL")]
        public_url: Option<String>,
        /// Pairing QR code lifetime in minutes (default 10).
        #[arg(long, default_value_t = 10)]
        ttl_mins: u64,
    },
}

/// `hrn mcp <command>`.
#[derive(Debug, Subcommand)]
pub(crate) enum McpCommand {
    /// List all configured and auto-discovered MCP servers.
    List,
    /// Connect to one or all enabled MCP servers, measure latency, and list tools.
    Ping {
        /// Optional server name to test (default: all enabled servers).
        server: Option<String>,
    },
    /// Run `hrn` as a strict stdio MCP JSON-RPC 2.0 server.
    Serve {
        /// Server profile (`default` or `agy`).
        #[arg(long, default_value = "default")]
        profile: String,
    },
}

/// Arguments of `hrn share`.
#[derive(Debug, Args)]
pub(crate) struct ShareArgs {
    /// Session id or unique id prefix (omit to share the latest session in cwd).
    pub(crate) session: Option<String>,
    /// Skip automatic secret redaction before sealing and uploading.
    #[arg(long)]
    pub(crate) no_redact: bool,
}

/// Arguments of `hrn join`.
#[derive(Debug, Args)]
pub(crate) struct JoinArgs {
    /// Collab room link (`<relay>/r/<room>#<key>[.<write_token>]` or `<room>.<key>[.<write_token>]`).
    pub(crate) link: String,
    /// Optional display name shown in `[guest:<name>]` badges on the host.
    #[arg(long)]
    pub(crate) name: Option<String>,
}

/// Arguments of `hrn gc [--apply]`.
#[derive(Debug, Default, Args)]
pub(crate) struct GcArgs {
    /// Apply deletions, orphan artifact pruning, stale lock removal, and VACUUM (default is dry-run).
    #[arg(long)]
    pub(crate) apply: bool,
}

/// External session transcript source for `hrn sessions import`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ImportSourceArg {
    /// Claude Code (`~/.claude`).
    Claude,
    /// Codex CLI (`~/.codex`).
    Codex,
    /// OMP (`~/.omp/agent`).
    Omp,
}

/// Arguments of `hrn sessions import <claude|codex|omp>`.
#[derive(Debug, Args)]
pub(crate) struct ImportArgs {
    /// Source harness (`claude`, `codex`, or `omp`).
    #[arg(value_enum)]
    pub(crate) source: ImportSourceArg,
    /// Import the most recently updated session from the source.
    #[arg(long, conflicts_with = "id")]
    pub(crate) latest: bool,
    /// Source session id or unique prefix to import.
    #[arg(long, value_name = "SOURCE_ID", conflicts_with = "latest")]
    pub(crate) id: Option<String>,
    /// Override source root directory (defaults to `~/.claude`, `~/.codex`, or `~/.omp/agent`).
    #[arg(long, value_name = "PATH")]
    pub(crate) dir: Option<PathBuf>,
}

/// `hrn providers <command>`.
#[derive(Debug, Subcommand)]
pub(crate) enum ProvidersCommand {
    /// Import providers, model roles, and compatible settings from an external harness.
    Import(ProvidersImportArgs),
}

/// External provider config source for `hrn providers import`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ProvidersImportSource {
    /// OMP (`~/.omp/agent/models.yml`, `config.yml`, `.env`, `auth-gateway-tokens/`).
    Omp,
}

/// Arguments of `hrn providers import omp [--dir <path>] [--dry-run] [--force]`.
#[derive(Debug, Args)]
pub(crate) struct ProvidersImportArgs {
    /// Source harness (`omp`).
    #[arg(value_enum)]
    pub(crate) source: ProvidersImportSource,
    /// Override source OMP agent directory (defaults to `$OMP_CODING_AGENT_DIR` or `~/.omp/agent`).
    #[arg(long, value_name = "PATH")]
    pub(crate) dir: Option<PathBuf>,
    /// Preview imported providers, roles, and settings without writing any files.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Overwrite existing providers, tokens, env keys, and config keys in `~/.harness/`.
    #[arg(long)]
    pub(crate) force: bool,
}

/// Arguments of `hrn sessions export`.
#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    /// Session id or unique id prefix (see `hrn sessions`).
    pub(crate) session: String,
    /// File to write, `-` for stdout (default: `hrn-session-<id>.<ext>` in the current directory).
    #[arg(short, long, value_name = "PATH")]
    pub(crate) output: Option<PathBuf>,
    /// Format (default: by the file extension, else markdown).
    #[arg(long, value_enum)]
    pub(crate) format: Option<ExportFormat>,
}

/// Session export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ExportFormat {
    /// Markdown.
    Md,
    /// One self-contained html file.
    Html,
}

/// Arguments of `hrn commit [--yes] [--dry-run] [--changelog] [--session <id>]`.
#[derive(Debug, Default, Args)]
pub(crate) struct CommitArgs {
    /// Stage attributed hunks and commit without prompting for confirmation.
    #[arg(long, short = 'y')]
    pub(crate) yes: bool,
    /// Preview attributed files/hunks and generated commit message without staging or committing.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Also add a changelog bullet under `[Unreleased]` in `CHANGELOG.md` if present.
    #[arg(long)]
    pub(crate) changelog: bool,
    /// Session id or unique prefix whose edits to attribute (default: latest session in cwd).
    #[arg(long, value_name = "SESSION")]
    pub(crate) session: Option<String>,
}

/// Arguments of `hrn git [revision]`.
#[derive(Debug, Default, Args)]
pub(crate) struct GitArgs {
    /// Optional git revision to inspect in read-only mode (e.g. `HEAD`, `HEAD~1`).
    pub(crate) revision: Option<String>,
}

/// Arguments of `hrn update [--check] [--canary|--stable]`.
#[derive(Debug, Default, Args)]
pub(crate) struct UpdateArgs {
    /// Check for a newer release without downloading or replacing binaries.
    #[arg(long)]
    pub(crate) check: bool,
    /// Query the prerelease/canary release channel.
    #[arg(long, conflicts_with = "stable")]
    pub(crate) canary: bool,
    /// Query the stable release channel.
    #[arg(long, conflicts_with = "canary")]
    pub(crate) stable: bool,
}

/// Target shell for `hrn completions <shell>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CompletionShell {
    /// Bash shell.
    Bash,
    /// Z shell.
    Zsh,
    /// Fish shell.
    Fish,
    /// PowerShell (`pwsh` / Windows PowerShell).
    Powershell,
}
