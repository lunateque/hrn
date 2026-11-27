//! `shell`: the `bash` tool with its background jobs and supervised services.
//!
//! The [`JobManager`] lives in the process-wide shared store, so jobs
//! and services outlive a runtime rebuild (`/login` hot-reload); it is also registered as a
//! `proc://` source.
//!
//! ```toml
//! [shell]
//! auto_background_secs = 60  # a longer running foreground command becomes a background job (0 = never)
//! pty_default = false        # run foreground commands on a pseudo-terminal unless the call says otherwise
//! pty_idle_ms = 2000         # a `pty` call returns after this quiet time and lets the job keep running
//! input_wait_ms = 3000       # `write proc://<id>` waits this long for the program's answer to settle
//! ```

mod bash;
mod bash_bg;
#[cfg(test)]
mod bash_tests;
mod bridge;
mod fg;
mod policy;
mod pty;
mod tool_cli;

use hrn_edge_proc::JobManager;
use hrn_edge_tools::command_policy::{CommandPattern, CommandPolicy};
use hrn_kernel::serde::Deserialize;
use hrn_kernel::serde_json;
use hrn_kernel::{Registry, Result};
use policy::{InterceptorConfig, InterceptorRule};
use std::sync::Arc;
use std::time::Duration;

/// Embedded `[shell]` defaults (kept inside the module so `cargo xtask removability` succeeds when
/// `shell` is removed).
const DEFAULTS_JSON: &str = include_str!("../data/defaults.json");

#[derive(Debug, Default, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct InterceptorTable {
    enabled: Option<bool>,
    #[serde(default)]
    patterns: Vec<InterceptorRule>,
}

/// The `[shell]` config table.
#[derive(Debug, Default, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct ShellConfig {
    /// Explicit POSIX shell executable path override (overridden by `HRN_SHELL`).
    #[serde(default)]
    shell_path: Option<String>,
    /// Seconds after which a still-running foreground command becomes a background job whose id
    /// the call returns; `0` never backgrounds (default 60).
    auto_background_secs: Option<u64>,
    /// Run foreground commands on a pseudo-terminal unless the call says otherwise.
    pty_default: Option<bool>,
    /// Milliseconds a `pty` call waits for output to go quiet before returning with the job id
    /// (default 2000).
    pty_idle_ms: Option<u64>,
    /// Milliseconds `write proc://<id>` waits for the program's answer to go quiet (default 3000).
    input_wait_ms: Option<u64>,
    /// Allow multi-segment `&&` chains when every segment is explicitly allowed by `patterns`.
    allow_compound_commands: Option<bool>,
    /// Leading `KEY=VAL` env var names permitted on auto-approved commands.
    allowed_env_vars: Option<Vec<String>>,
    /// Ordered command approval pattern rules (`{ match, approval, env }`).
    patterns: Option<Vec<CommandPattern>>,
    /// Refuse destructive git stash/reset/switch/checkout/restore in the main git checkout.
    git_guard: Option<bool>,
    /// Post-approval shell command interceptor (`enabled`, `patterns`).
    interceptor: Option<InterceptorTable>,
}

#[derive(Debug, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct DefaultShellConfig {
    #[serde(default)]
    shell_path: Option<String>,
    auto_background_secs: u64,
    pty_default: bool,
    pty_idle_ms: u64,
    input_wait_ms: u64,
    allow_compound_commands: bool,
    allowed_env_vars: Vec<String>,
    patterns: Vec<CommandPattern>,
    git_guard: bool,
    interceptor: DefaultInterceptorConfig,
}

#[derive(Debug, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct DefaultInterceptorConfig {
    enabled: bool,
    patterns: Vec<InterceptorRule>,
}

/// What the `bash` tool does with the settings and paths of this process.
pub(crate) struct Settings {
    /// Foreground running time after which the command is handed to the job table; `None` never.
    pub auto_background: Option<Duration>,
    /// Whether a call without `pty` still gets a terminal.
    pub pty_default: bool,
    /// Quiet time after which a `pty` call returns and lets its job keep running.
    pub pty_idle: Duration,
    /// Generated bash rc file defining the tool-command functions.
    pub rc: std::path::PathBuf,
    /// `[shell]` command approval patterns, compound flag, and allowed env vars.
    pub command_policy: CommandPolicy,
    /// Whether `[shell] git_guard` is active.
    pub git_guard: bool,
    /// Post-approval `[shell.interceptor]` configuration.
    pub interceptor: InterceptorConfig,
}

const DEFAULT_AUTO_BACKGROUND_SECS: u64 = 60;
const DEFAULT_PTY_IDLE_MS: u64 = 2000;
const DEFAULT_INPUT_WAIT_MS: u64 = 3000;

fn default_shell_config() -> DefaultShellConfig {
    serde_json::from_str(DEFAULTS_JSON).unwrap_or(DefaultShellConfig {
        shell_path: None,
        auto_background_secs: DEFAULT_AUTO_BACKGROUND_SECS,
        pty_default: false,
        pty_idle_ms: DEFAULT_PTY_IDLE_MS,
        input_wait_ms: DEFAULT_INPUT_WAIT_MS,
        allow_compound_commands: false,
        allowed_env_vars: Vec::new(),
        patterns: Vec::new(),
        git_guard: false,
        interceptor: DefaultInterceptorConfig {
            enabled: false,
            patterns: Vec::new(),
        },
    })
}

/// Register `bash` and the job table behind `proc://`.
///
/// # Errors
///
/// When the `bash` tool name is already taken.
pub fn register(registry: &mut Registry) -> Result {
    let defaults = default_shell_config();
    let config: ShellConfig = registry.config()?;
    let configured_shell = config
        .shell_path
        .or(defaults.shell_path)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    hrn_edge_proc::set_shell_path(configured_shell);
    let auto_bg_secs = config
        .auto_background_secs
        .unwrap_or(defaults.auto_background_secs);
    let mut interceptor_patterns = config
        .interceptor
        .as_ref()
        .map(|i| i.patterns.clone())
        .unwrap_or_default();
    // Configured (project + global) interceptor rules prepend ahead of module defaults.
    interceptor_patterns.extend(defaults.interceptor.patterns);
    let interceptor = InterceptorConfig {
        enabled: config
            .interceptor
            .and_then(|i| i.enabled)
            .unwrap_or(defaults.interceptor.enabled),
        patterns: interceptor_patterns,
    };
    let settings = Settings {
        auto_background: (auto_bg_secs > 0).then(|| Duration::from_secs(auto_bg_secs)),
        pty_default: config.pty_default.unwrap_or(defaults.pty_default),
        pty_idle: Duration::from_millis(config.pty_idle_ms.unwrap_or(defaults.pty_idle_ms).max(1)),
        rc: bridge::rc_path(&registry.host().data_dir),
        command_policy: CommandPolicy {
            patterns: config.patterns.unwrap_or(defaults.patterns),
            allow_compound_commands: config
                .allow_compound_commands
                .unwrap_or(defaults.allow_compound_commands),
            allowed_env_vars: config.allowed_env_vars.unwrap_or(defaults.allowed_env_vars),
        },
        git_guard: config.git_guard.unwrap_or(defaults.git_guard),
        interceptor,
    };
    bridge::set_rc_path(settings.rc.clone());
    // The file must exist before the first shell sources `BASH_ENV`; the first call rewrites it
    // with the real tool list.
    bridge::write_rc(&settings.rc, &[])?;
    hrn_edge_proc::set_shell_env(bridge::shell_env());
    let jobs: Arc<JobManager> = registry.shared().get_or_init(|| Ok(JobManager::new()))?;
    jobs.set_input_wait(Duration::from_millis(
        config
            .input_wait_ms
            .unwrap_or(defaults.input_wait_ms)
            .max(1),
    ));
    registry.proc_source(Arc::clone(&jobs) as _);
    registry.help_page("bash/tool-commands", crate::bash::HELP_TOOL_COMMANDS);
    registry.tool(Arc::new(bash::BashTool::new(jobs, Arc::new(settings))))?;
    Ok(())
}

impl Settings {
    /// Settings for tests: no auto-background, no pty default, a 2 s idle window, no rc file.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn test_default() -> Self {
        let defaults = default_shell_config();
        Self {
            auto_background: None,
            pty_default: false,
            pty_idle: Duration::from_millis(DEFAULT_PTY_IDLE_MS),
            rc: std::path::PathBuf::new(),
            command_policy: CommandPolicy::default(),
            git_guard: false,
            interceptor: InterceptorConfig {
                enabled: false,
                patterns: defaults.interceptor.patterns,
            },
        }
    }
}
