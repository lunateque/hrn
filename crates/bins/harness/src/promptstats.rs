//! `hrn promptstats`: the per-section system-prompt token table for a workspace, checked
//! against the embedded §4.3 budgets. `cargo xtask promptstats` runs it on the fixture
//! workspace and fails the gate over budget.

use crate::config::Config;
use crate::term::out;
use crate::wiring::{Background, build_runtime_with_tools};
use hrn_edge_mcp::McpToolSet;
use hrn_edge_store::SqliteStore;
use hrn_harness::prompt_data::{budgets, tokens};
use hrn_kernel::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Print the table for `cwd`; `Err` names the first over-budget section.
pub(crate) fn run(cfg: &Config, cwd: &Path) -> Result {
    let store = SqliteStore::open_in_memory()?;
    let runtime = build_runtime_with_tools(
        cfg,
        Arc::new(store),
        cwd,
        &McpToolSet::default(),
        &Background::new(),
    )?;
    let budgets = budgets();
    let mut rows = runtime.prompt_breakdown(cwd);
    let schema_tokens: usize = runtime
        .request_specs(&[])
        .iter()
        .map(|s| tokens(&s.name) + tokens(&s.description) + tokens(&s.parameters.to_string()))
        .sum();
    rows.push(("tool schemas", schema_tokens));
    let table: [(&str, usize); 9] = [
        ("base variant", budgets.base),
        ("APPEND_SYSTEM.md", budgets.append_system),
        ("context files", budgets.context_files),
        ("skills catalog", budgets.skills_catalog),
        ("agents catalog", budgets.agents_catalog),
        ("deferred groups", budgets.deferred_groups),
        ("memories", budgets.memories),
        ("tool schemas", budgets.tool_schemas),
        (
            "system total",
            budgets.base
                + budgets.static_rest
                + budgets.append_system
                + budgets.context_files
                + budgets.skills_catalog
                + budgets.agents_catalog
                + budgets.deferred_groups,
        ),
    ];
    out(&format!(
        "{:<18}{:>8}{:>10}\n",
        "promptstats section", "tokens", "budget"
    ));
    let mut problems: Vec<String> = Vec::new();
    for (name, got) in &rows {
        let budget = table
            .iter()
            .find(|(n, _)| n == name)
            .map_or(usize::MAX, |(_, b)| *b);
        let flag = if *got > budget {
            problems.push(format!("`{name}` costs {got} tokens (budget {budget})"));
            " OVER"
        } else {
            ""
        };
        out(&format!(
            "{:<18}{:>8}{:>10}{flag}\n",
            name,
            got,
            if budget == usize::MAX {
                "-".to_owned()
            } else {
                budget.to_string()
            }
        ));
    }
    let system_total = rows
        .iter()
        .find(|(n, _)| *n == "system total")
        .map_or(0, |(_, t)| *t);
    let fixed = system_total + schema_tokens;
    out(&format!(
        "{:<18}{:>8}{:>10}{}\n",
        "cold-turn fixed",
        fixed,
        budgets.cold_total,
        if fixed > budgets.cold_total {
            " OVER"
        } else {
            ""
        }
    ));
    if let Some(problem) = problems.first() {
        return Err(hrn_kernel::HrnError::Config(problem.clone()));
    }
    Ok(())
}

/// The `hrn promptstats` command body: the fixture config/workspace or the user's own,
/// resolved here so `dispatch` stays small.
pub(crate) fn command(
    input: &crate::config_layers::LayerInput,
    cwd: Option<PathBuf>,
    fixture: bool,
) -> Result {
    if fixture {
        return run(&fixture_config()?, &fixture_workspace());
    }
    let (cfg, _) = Config::load_layered(input)?;
    let dir = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    run(&cfg, &dir)
}

/// The fixture workspace config shipped in the tree (a keyless built-in catalog row).
pub(crate) fn fixture_config() -> Result<Config> {
    Config::parse_with_home(include_str!("../fixtures/promptstats.toml"), None)
}

/// The fixture workspace directory shipped in the tree.
pub(crate) fn fixture_workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/promptstats")
}
