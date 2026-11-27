//! `ast`: structural code search and rewrite (`ast_grep`, `ast_edit`), after omp's ast tools.
//!
//! - `ast_grep` finds nodes by AST pattern; `ast_edit` stages a rewrite as a proposal and only
//!   writes files when `resolve <reason>` confirms it (`reject <reason>` discards it).
//! - Every file is parsed in the language its extension maps to; the language-to-extension table
//!   is embedded data in `hrn-edge-ast` (`languages.json`). The tree-sitter grammars themselves
//!   live in the `hrn-ast` sidecar binary (full language set, found next to `hrn.exe`), so
//!   `hrn` never links a parser and pays nothing for them at startup.
//! - Both tools are discoverable (found through `tool_search`) unless `[ast] discover = false`.
//!
//! ```toml
//! [ast]
//! max_matches = 50     # matches one `ast_grep` call shows
//! max_files = 1000     # files one `ast_edit` pass may touch
//! discover = true      # load the tools through `tool_search` instead of every request
//! ```
//!
//! Removable with `cargo xtask module remove ast`.

mod client;
mod edit;
mod grep;
mod staged;

#[cfg(test)]
mod tests;

use hrn_kernel::serde::Deserialize;
use hrn_kernel::{HrnError, Registry, Result, Tool};
use std::path::Path;
use std::sync::Arc;

/// The tool descriptions, as data.
const DESCRIPTION_GREP: &str = include_str!("../prompts/ast_grep.md");
const DESCRIPTION_EDIT: &str = include_str!("../prompts/ast_edit.md");
pub(crate) const DESCRIPTION_RESOLVE: &str = include_str!("../prompts/resolve.md");
pub(crate) const DESCRIPTION_REJECT: &str = include_str!("../prompts/reject.md");
/// The defaults, as data.
const DEFAULTS: &str = include_str!("../defaults.json");

/// The resolved `[ast]` settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(crate = "hrn_kernel::serde")]
pub(crate) struct Config {
    /// Matches one `ast_grep` call shows.
    pub(crate) max_matches: usize,
    /// Files one `ast_edit` pass may touch.
    pub(crate) max_files: usize,
    /// Whether the two tools are deferred behind `tool_search`.
    pub(crate) discover: bool,
}

impl Config {
    /// The defaults of `defaults.json`, overlaid with what `[ast]` set.
    ///
    /// # Errors
    ///
    /// `HrnError::Internal` when the embedded defaults do not parse, `HrnError::Config`
    /// when a value is out of range.
    fn resolve(overrides: Overrides) -> Result<Self> {
        let mut config: Self = hrn_kernel::serde_json::from_str(DEFAULTS)
            .map_err(|e| HrnError::Internal(format!("ast defaults.json: {e}")))?;
        config.max_matches = overrides.max_matches.unwrap_or(config.max_matches);
        config.max_files = overrides.max_files.unwrap_or(config.max_files);
        config.discover = overrides.discover.unwrap_or(config.discover);
        for (name, value) in [
            ("max_matches", config.max_matches),
            ("max_files", config.max_files),
        ] {
            if value == 0 {
                return Err(HrnError::Config(format!("[ast] {name} must be at least 1")));
            }
        }
        Ok(config)
    }
}

/// The `[ast]` table a user may write.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct Overrides {
    max_matches: Option<usize>,
    max_files: Option<usize>,
    discover: Option<bool>,
}

/// Check a `path`/`paths` scope expression (`;`-separated entries in the shared edge scope
/// grammar) and report how results should be rendered. Existence of the entries is validated
/// by the sidecar's scope parser; here only the shape is checked.
///
/// # Errors
///
/// When `raw` is empty or has a blank entry.
pub(crate) fn check_scope(cwd: &Path, raw: &str, empty_message: &str) -> Result<bool> {
    let entries: Vec<&str> = raw.split(';').map(str::trim).collect();
    if raw.trim().is_empty() || entries.iter().any(|e| e.is_empty()) {
        return Err(HrnError::Tool(format!("ast: {empty_message}")));
    }
    let directory = entries.len() > 1
        || entries.first().is_some_and(|e| {
            e.contains(['*', '?', '[', '!'])
                || matches!(*e, "." | "./")
                || hrn_edge_tools::paths::resolve(cwd, e).is_dir()
        });
    Ok(directory)
}

/// The `[path#TAG]` content tag of a file, exactly as `read` and `edit` print it.
pub(crate) fn content_tag(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some(hrn_edge_tools::tag_of(&text.replace("\r\n", "\n")))
}

/// Tag lookup for the displayed paths of a pass (resolved against `cwd`).
pub(crate) fn tags_of(cwd: &Path) -> impl Fn(&str) -> Option<String> {
    let cwd = cwd.to_path_buf();
    move |shown| content_tag(&hrn_edge_tools::paths::resolve(&cwd, shown))
}

/// Register `resolve` / `reject` eagerly and the two search tools per `[ast] discover`.
///
/// # Errors
///
/// When `[ast]` is malformed or a tool name is taken.
pub fn register(registry: &mut Registry) -> Result {
    let config = Config::resolve(registry.config()?)?;
    let staged = staged::shared(registry)?;
    let grep: Arc<dyn Tool> = Arc::new(grep::AstGrepTool::new(config.clone()));
    let edit: Arc<dyn Tool> = Arc::new(edit::AstEditTool::new(config.clone(), Arc::clone(&staged)));
    if config.discover {
        registry.deferred(
            [grep, edit]
                .into_iter()
                .map(|tool| hrn_kernel::DeferredTool {
                    group: "ast".to_owned(),
                    short_name: tool.spec().name,
                    tool,
                })
                .collect::<Vec<_>>(),
        )?;
    } else {
        registry.tool(grep)?;
        registry.tool(edit)?;
    }
    registry.tool(Arc::new(edit::ResolveTool::new(
        config,
        Arc::clone(&staged),
    )))?;
    registry.tool(Arc::new(edit::RejectTool::new(staged)))?;
    Ok(())
}
