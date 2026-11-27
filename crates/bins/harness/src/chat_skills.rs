//! `/skill:<name>` over the chat backend: every discovered skill doubles as a slash command
//! whose expansion is the skill's own `SKILL.md` body (the launch flags' `--skills` filter
//! applies here exactly as it does to the system-prompt catalog).

use super::ChatBackend;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{discover_skills, filter_skills};
use hrn_kernel::WorkspaceFs;
use hrn_tui::CustomCommandInfo;

impl ChatBackend {
    /// The `/skill:<name>` pseudo commands: one per discovered skill (the launch flags filter the
    /// same list the system prompt shows).
    pub(super) fn skill_commands(&self) -> Vec<CustomCommandInfo> {
        if !self.cfg.sources.skills {
            return Vec::new();
        }
        let Ok(skills) = discover_skills(&StdWorkspaceFs, &self.cwd) else {
            return Vec::new();
        };
        let skills = filter_skills(
            skills,
            self.cfg
                .sources
                .skill_patterns
                .as_deref()
                .unwrap_or_default(),
        );
        skills
            .values()
            .map(|skill| CustomCommandInfo {
                name: format!("skill:{}", skill.name),
                description: skill.description.clone(),
            })
            .collect()
    }

    /// Expand `/skill:<name> [args]`: the skill's `SKILL.md` body plus the arguments, as a
    /// prompt the model is told to follow.
    pub(super) fn expand_skill(&self, name: &str, args: &str) -> Option<String> {
        let skills = discover_skills(&StdWorkspaceFs, &self.cwd).ok()?;
        let skill = skills.get(&name.to_ascii_lowercase())?;
        let raw = StdWorkspaceFs.read_to_string(&skill.path).ok()??;
        let (_fm, body) = hrn_harness::discovery::parse_front_matter(&raw);
        let mut prompt = format!(
            "Follow the `{}` skill ({}) for this task:\n\n{}",
            skill.name,
            skill.path.display(),
            body.trim()
        );
        if !args.trim().is_empty() {
            prompt.push_str("\n\nTask:\n");
            prompt.push_str(args.trim());
        }
        Some(prompt)
    }
}
