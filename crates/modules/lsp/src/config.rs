//! `[lsp]` module settings (`data/defaults.json`) and built-in language server descriptors.

use hrn_kernel::serde::Deserialize;
use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::time::Duration;

/// Embedded `[lsp]` defaults (kept inside `hrn-mod-lsp` for strict module removability).
const DEFAULTS_JSON: &str = include_str!("../data/defaults.json");
const DEFAULT_TOOL_DIAG_WAIT_MS: u64 = 800;
const DEFAULT_HOOK_DIAG_WAIT_MS: u64 = 450;

#[derive(Debug, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "deserializes [lsp] boolean defaults from data/defaults.json"
)]
struct RawDefaults {
    enabled: bool,
    lazy: bool,
    diagnostics_on_write: bool,
    diagnostics_on_edit: bool,
    format_on_write: bool,
    diagnostics_deduplicate: bool,
    request_timeout_secs: u64,
    idle_timeout_secs: u64,
}

/// Typed `[lsp]` config table deserialized from `Registry::config()`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields, default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors omp [lsp] boolean configuration switches"
)]
pub struct LspTableConfig {
    /// Master enable switch (`--no-lsp` forces `false`, unregistering tool and hook).
    pub enabled: bool,
    /// Start servers lazily on first query/edit rather than eagerly at startup.
    pub lazy: bool,
    /// Append `[LSP diagnostics]` in post-tool hook after `write` calls.
    pub diagnostics_on_write: bool,
    /// Append `[LSP diagnostics]` in post-tool hook after `edit` / `apply_patch` calls.
    pub diagnostics_on_edit: bool,
    /// Run `textDocument/formatting` in post-tool hook after `write` calls.
    pub format_on_write: bool,
    /// Deduplicate identical diagnostics returned for the same file position/code/message.
    pub diagnostics_deduplicate: bool,
    /// Per-request LSP RPC timeout in seconds (must be >= 1).
    pub request_timeout_secs: u64,
    /// Idle duration in seconds before warm LSP clients are evicted (`0` disables eviction).
    pub idle_timeout_secs: u64,
}

impl Default for LspTableConfig {
    fn default() -> Self {
        let d: RawDefaults = serde_json::from_str(DEFAULTS_JSON).unwrap_or(RawDefaults {
            enabled: true,
            lazy: true,
            diagnostics_on_write: true,
            diagnostics_on_edit: false,
            format_on_write: false,
            diagnostics_deduplicate: true,
            request_timeout_secs: 10,
            idle_timeout_secs: 300,
        });
        Self {
            enabled: d.enabled,
            lazy: d.lazy,
            diagnostics_on_write: d.diagnostics_on_write,
            diagnostics_on_edit: d.diagnostics_on_edit,
            format_on_write: d.format_on_write,
            diagnostics_deduplicate: d.diagnostics_deduplicate,
            request_timeout_secs: d.request_timeout_secs,
            idle_timeout_secs: d.idle_timeout_secs,
        }
    }
}

impl LspTableConfig {
    /// Validate numeric bounds.
    ///
    /// # Errors
    /// Returns `HrnError::Config` if `request_timeout_secs == 0`.
    pub fn validate(&self) -> Result {
        if self.request_timeout_secs == 0 {
            return Err(HrnError::Config(
                "[lsp] request_timeout_secs must be at least 1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Configuration for one language server binary.
#[derive(Clone, Debug)]
pub struct LspServerSpec {
    /// Human name (e.g. `rust-analyzer`).
    pub name: String,
    /// LSP `languageId`s handled by this server (e.g. `["rust"]`).
    pub languages: Vec<String>,
    /// File extensions or filenames matched by this server (e.g. `[".rs"]`).
    pub file_types: Vec<String>,
    /// Candidate executable names or absolute paths (tried in order via `PATH`).
    pub commands: Vec<String>,
    /// Command-line arguments passed to the server.
    pub args: Vec<String>,
    /// Filenames/globs marking a project root (checked inside `cwd` only).
    pub root_markers: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Optional `initializationOptions` sent in the `initialize` request.
    pub init_options: Option<Value>,
    /// Optional server settings returned on `workspace/configuration` and `didChangeConfiguration`.
    pub settings: Option<Value>,
    /// Disable this server completely (excluded from detection, queries, and hooks).
    pub disabled: bool,
    /// Optional initialize/warmup timeout override in milliseconds.
    pub warmup_timeout_ms: Option<u64>,
    /// Whether this server acts primarily as a linter/diagnostics provider.
    pub is_linter: bool,
}

/// Effective runtime configuration for [`crate::LspManager`].
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "holds the resolved [lsp] switches from LspTableConfig"
)]
pub struct LspConfig {
    /// Server specs checked in order (built-ins merged with user/project `lsp.toml`).
    pub servers: Vec<LspServerSpec>,
    /// Auto-warm a detected server in the background after edits when not yet running.
    pub warm_on_edit: bool,
    /// Start servers lazily on first use.
    pub lazy: bool,
    /// Append diagnostics after `write` calls.
    pub diagnostics_on_write: bool,
    /// Append diagnostics after `edit` / `apply_patch` calls.
    pub diagnostics_on_edit: bool,
    /// Run `textDocument/formatting` after `write` calls.
    pub format_on_write: bool,
    /// Deduplicate identical diagnostics before rendering.
    pub diagnostics_deduplicate: bool,
    /// Timeout for normal LSP RPC requests.
    pub request_timeout: Duration,
    /// Idle eviction timeout (`Duration::ZERO` disables eviction).
    pub idle_timeout: Duration,
    /// Wait window for `publishDiagnostics` during explicit tool calls.
    pub tool_diag_wait: Duration,
    /// Wait window for `publishDiagnostics` during post-edit hooks.
    pub hook_diag_wait: Duration,
    /// Non-fatal warnings recorded while loading `lsp.toml` files.
    pub warnings: Vec<String>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self::from_table(
            &LspTableConfig::default(),
            builtin_server_specs(),
            Vec::new(),
        )
    }
}

impl LspConfig {
    /// Build an [`LspConfig`] from a validated [`LspTableConfig`] and resolved server list.
    #[must_use]
    pub fn from_table(
        table: &LspTableConfig,
        servers: Vec<LspServerSpec>,
        warnings: Vec<String>,
    ) -> Self {
        Self {
            servers,
            warm_on_edit: true,
            lazy: table.lazy,
            diagnostics_on_write: table.diagnostics_on_write,
            diagnostics_on_edit: table.diagnostics_on_edit,
            format_on_write: table.format_on_write,
            diagnostics_deduplicate: table.diagnostics_deduplicate,
            request_timeout: Duration::from_secs(table.request_timeout_secs.max(1)),
            idle_timeout: Duration::from_secs(table.idle_timeout_secs),
            tool_diag_wait: Duration::from_millis(DEFAULT_TOOL_DIAG_WAIT_MS),
            hook_diag_wait: Duration::from_millis(DEFAULT_HOOK_DIAG_WAIT_MS),
            warnings,
        }
    }
}

/// Built-in language server specs covering Rust, TypeScript/JS, Python, Go, and C/C++.
pub fn builtin_server_specs() -> Vec<LspServerSpec> {
    vec![
        builtin(
            "rust-analyzer",
            &["rust"],
            &[".rs"],
            &["rust-analyzer"],
            &[],
            &["Cargo.toml", ".git"],
        ),
        builtin(
            "typescript-language-server",
            &[
                "typescript",
                "typescriptreact",
                "javascript",
                "javascriptreact",
            ],
            &[".ts", ".mts", ".cts", ".tsx", ".js", ".mjs", ".cjs", ".jsx"],
            &["typescript-language-server"],
            &["--stdio"],
            &["tsconfig.json", "jsconfig.json", "package.json", ".git"],
        ),
        builtin(
            "pyright",
            &["python"],
            &[".py", ".pyi"],
            &["basedpyright-langserver", "pyright-langserver", "pylsp"],
            &["--stdio"],
            &["pyproject.toml", "pyrightconfig.json", "setup.py", ".git"],
        ),
        builtin(
            "gopls",
            &["go"],
            &[".go"],
            &["gopls"],
            &[],
            &["go.work", "go.mod", ".git"],
        ),
        builtin(
            "clangd",
            &["c", "cpp"],
            &[".c", ".h", ".cc", ".cpp", ".cxx", ".hpp", ".hxx", ".hh"],
            &["clangd"],
            &[],
            &["compile_commands.json", "CMakeLists.txt", ".git"],
        ),
    ]
}

fn builtin(
    name: &str,
    languages: &[&str],
    file_types: &[&str],
    commands: &[&str],
    args: &[&str],
    root_markers: &[&str],
) -> LspServerSpec {
    LspServerSpec {
        name: name.to_owned(),
        languages: languages.iter().map(|s| (*s).to_owned()).collect(),
        file_types: file_types.iter().map(|s| (*s).to_owned()).collect(),
        commands: commands.iter().map(|s| (*s).to_owned()).collect(),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        root_markers: root_markers.iter().map(|s| (*s).to_owned()).collect(),
        env: BTreeMap::new(),
        init_options: None,
        settings: None,
        disabled: false,
        warmup_timeout_ms: None,
        is_linter: false,
    }
}
