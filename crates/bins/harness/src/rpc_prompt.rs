//! The `/context` view for the RPC host: the discovered instruction sections of a session's
//! working directory, honoring the launch flags' source switches (split out of `rpc.rs`).

use super::Shared;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{
    discover_project_context, discover_skills, filter_skills, format_agents_catalog,
    format_autoloaded_skills, format_skills_catalog, load_subagent_profiles,
};
use hrn_kernel::HrnError;
use std::path::PathBuf;

/// The prompt sections the host may show: context files, skills (autoloaded bodies then the
/// filtered catalog), rules (always-apply then the rulebook listing) and the subagent catalog.
pub(super) fn system_prompt_sections(shared: &Shared, id: &str) -> Result<Vec<String>, HrnError> {
    let meta = shared
        .store
        .get_session(id)?
        .ok_or_else(|| HrnError::Store(format!("unknown session `{id}`")))?;
    let cwd = PathBuf::from(&meta.cwd);
    let sources = &shared.sources;
    // The runtime's built-in base section is private; the host sees the discovered parts.
    let mut sections = vec![discover_project_context(
        &StdWorkspaceFs,
        &cwd,
        sources.context_files,
    )];
    if sources.skills
        && let Ok(skills) = discover_skills(&StdWorkspaceFs, &cwd)
    {
        let skills = filter_skills(
            skills,
            sources.skill_patterns.as_deref().unwrap_or_default(),
        );
        if let Some(autoload) = format_autoloaded_skills(&StdWorkspaceFs, &skills) {
            sections.push(autoload);
        }
        if let Some(catalog) =
            format_skills_catalog(&skills, hrn_harness::prompt_data::budgets().skills_catalog)
        {
            sections.push(catalog);
        }
    }
    if sources.rules
        && let Ok(book) = hrn_harness::Rulebook::discover(&StdWorkspaceFs, &cwd)
    {
        sections.extend(
            [book.always_apply_prompt(), book.rulebook_prompt()]
                .into_iter()
                .flatten(),
        );
    }
    if let Ok(profiles) = load_subagent_profiles(&StdWorkspaceFs, &cwd) {
        sections.push(format_agents_catalog(
            &profiles,
            hrn_harness::prompt_data::budgets().agents_catalog,
        ));
    }
    Ok(sections.into_iter().filter(|s| !s.is_empty()).collect())
}
