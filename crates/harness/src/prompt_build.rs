//! System-prompt assembly (§4.1 order, cache-prefix first): the static block (base variant,
//! module one-liners, tool commands, internal URLs), then everything a turn or a workspace can
//! change wrapped in `<project-context>` — user instructions, project context files, skills,
//! rules, agents — and last the deferred-tool groups. Advisor notes never enter this string;
//! the turn delivers them as `[system]` user messages at the next turn boundary.

use crate::context::discover_project_context;
use crate::prompt_data::{
    BASE_CLAUDE, BASE_DEFAULT, BASE_GPT5, PROJECT_CLOSE, PROJECT_OPEN, SUBAGENT, TOOL_COMMANDS,
    budgets, clip_at, clip_bytes, tokens,
};
use crate::rules::Rulebook;
use crate::runtime::AgentRuntime;
use crate::skills::{
    discover_skills, filter_skills, format_autoloaded_skills, format_skills_catalog,
};
use crate::subagent::{format_agents_catalog, load_subagent_profiles};
use hrn_kernel::WorkspaceFs;
use std::path::Path;

/// One named prompt section (the `promptstats` table keys on these names).
pub(crate) type Section = (&'static str, String);

/// The base prompt for `variant` (`None`/unknown → the prescriptive default).
#[must_use]
pub(crate) fn base_for(variant: Option<&str>) -> &'static str {
    match variant {
        Some("claude") => BASE_CLAUDE,
        Some("gpt5") => BASE_GPT5,
        _ => BASE_DEFAULT,
    }
}

impl AgentRuntime {
    /// The turn's system string from `cwd`'s instruction sources, `rules` and the session
    /// model's prompt variant.
    pub(crate) fn build_system_prompt(
        &self,
        cwd: &Path,
        rules: Option<&Rulebook>,
        model: Option<&str>,
    ) -> String {
        let parts = self.assembled(cwd, rules, model);
        let mut out = parts.static_block;
        if let Some(rest) = parts.dynamic_tail {
            out.push('\n');
            out.push_str(PROJECT_OPEN);
            out.push('\n');
            out.push_str(&rest);
            out.push('\n');
            out.push_str(PROJECT_CLOSE);
        }
        out
    }

    /// The per-section token table of the prompt for `cwd` (budget names from §4.3), schemas
    /// excluded; the last row is the whole static+dynamic system string.
    #[must_use]
    pub fn prompt_breakdown(&self, cwd: &Path) -> Vec<(&'static str, usize)> {
        let parts = self.assembled(cwd, None, None);
        let mut rows = vec![("base variant", tokens(&parts.base))];
        for (name, text) in parts.static_rest {
            rows.push((name, tokens(&text)));
        }
        for (name, text) in parts.dynamic {
            rows.push((name, tokens(&text)));
        }
        let full = self.build_system_prompt(cwd, None, None);
        rows.push(("system total", tokens(&full)));
        rows
    }

    fn assembled(&self, cwd: &Path, rules: Option<&Rulebook>, model: Option<&str>) -> Assembled {
        let budgets = budgets();
        let base = self.base_prompt(cwd, model);
        let mut static_rest: Vec<Section> = Vec::new();
        static_rest.push(("module sections", self.config.module_sections.join("\n\n")));
        if self.tools.get("bash").is_some() {
            static_rest.push(("tool commands", TOOL_COMMANDS.trim_end().to_owned()));
        }
        if let Some(reference) = self.urls.reference() {
            static_rest.push(("internal urls", reference));
        }
        static_rest.retain(|(_, text)| !text.is_empty());

        let replacement = self.config.system_prompt_override.is_some()
            || replace_text(self.fs.as_ref(), cwd, "SYSTEM.md").is_some();
        // A wholesale replacement carries its own length; embedded variants share §4.3's budget.
        let head_budget = if replacement {
            budgets.static_rest.max(tokens(&base))
        } else {
            budgets.base + budgets.static_rest
        };
        let mut static_block = base.clone();
        for (_, text) in &static_rest {
            static_block.push_str("\n\n");
            static_block.push_str(text);
        }
        static_block = clip_at(&static_block, head_budget);

        let mut dynamic: Vec<Section> = Vec::new();
        if self.config.subagent {
            dynamic.push(("hand-off", SUBAGENT.trim_end().to_owned()));
        }
        if let Some(append) = self.append_text(cwd) {
            dynamic.push(("APPEND_SYSTEM.md", clip_at(&append, budgets.append_system)));
        }
        let context =
            discover_project_context(self.fs.as_ref(), cwd, self.config.context.context_files);
        if !context.trim().is_empty() {
            dynamic.push(("context files", clip_at(&context, budgets.context_files)));
        }
        dynamic.extend(self.skills_sections(cwd));
        if let Some(book) = rules
            && let Some(always) = book.always_apply_prompt()
        {
            dynamic.push(("rules", always));
        }
        if self.tools.get("task").is_some()
            && let Ok(profiles) = load_subagent_profiles(self.fs.as_ref(), cwd)
        {
            dynamic.push((
                "agents catalog",
                format_agents_catalog(&profiles, budgets.agents_catalog),
            ));
        }
        if let Some(deferred) = self.tools.deferred_prompt() {
            dynamic.push((
                "deferred groups",
                clip_at(&deferred, budgets.deferred_groups),
            ));
        }
        Assembled {
            base,
            static_rest,
            static_block,
            dynamic_tail: (!dynamic.is_empty()).then(|| {
                dynamic
                    .iter()
                    .map(|(_, t)| t.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }),
            dynamic,
        }
    }

    fn base_prompt(&self, cwd: &Path, model: Option<&str>) -> String {
        if let Some(override_) = &self.config.system_prompt_override {
            return override_.clone();
        }
        // A `SYSTEM.md` replaces the base wholesale (omp's `loadSystemPromptFiles`).
        if let Some(system) = replace_text(self.fs.as_ref(), cwd, "SYSTEM.md") {
            return system;
        }
        let variant = self
            .config
            .prompt_variant
            .clone()
            .or_else(|| self.llm.prompt_variant(model));
        base_for(variant.as_deref()).trim().to_owned()
    }

    /// The `APPEND_SYSTEM.md` layers plus the `--append-system-prompt` flag text.
    fn append_text(&self, cwd: &Path) -> Option<String> {
        let mut parts = append_layers(self.fs.as_ref(), cwd, "APPEND_SYSTEM.md");
        if let Some(cli) = &self.config.append_system_override {
            parts.push(cli.clone());
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    fn skills_sections(&self, cwd: &Path) -> Vec<Section> {
        if !self.config.context.skills {
            return Vec::new();
        }
        let patterns = self
            .config
            .context
            .skill_patterns
            .as_deref()
            .unwrap_or_default();
        let Ok(skills) = discover_skills(self.fs.as_ref(), cwd) else {
            return Vec::new();
        };
        let skills = filter_skills(skills, patterns);
        let budgets = budgets();
        let mut out = Vec::new();
        if let Some(autoload) = format_autoloaded_skills(self.fs.as_ref(), &skills) {
            out.push(("autoloaded skills", autoload));
        }
        if let Some(catalog) = format_skills_catalog(&skills, budgets.skills_catalog) {
            out.push(("skills catalog", catalog));
        }
        out
    }
}

/// The assembled prompt: the base, the named static blocks after it, the joined static string,
/// and the named dynamic blocks (whose joined form is the wrapped tail).
struct Assembled {
    base: String,
    static_rest: Vec<Section>,
    static_block: String,
    dynamic: Vec<Section>,
    dynamic_tail: Option<String>,
}

/// Read one instruction file (§4.4): the user layer is `~/.harness/<name>`, else — as a
/// read-compatible fallback — `~/.omp/agent/<name>`; the project `.harness/<name>` concatenates
/// after it (the later block wins). CRLF is normalized; each file is capped at 16 KiB.
pub(crate) fn append_layers(fs: &dyn WorkspaceFs, cwd: &Path, name: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(home) = fs.home_dir() {
        for dir in [home.join(".harness"), home.join(".omp").join("agent")] {
            if let Some(text) = read_instruction(fs, &dir.join(name)) {
                parts.push(text);
                break;
            }
        }
    }
    if let Some(text) = read_instruction(fs, &cwd.join(".harness").join(name)) {
        parts.push(text);
    }
    parts
}

/// One instruction file, CRLF-normalized and capped; `None` when absent or blank.
fn read_instruction(fs: &dyn WorkspaceFs, path: &Path) -> Option<String> {
    let raw = fs.read_to_string(path).ok()??;
    let kept = clip_bytes(&raw.replace("\r\n", "\n"), 16 * 1024);
    (!kept.trim().is_empty()).then(|| kept.trim().to_owned())
}

/// The highest-precedence `name` file (project over user), or `None`; such a file replaces its
/// target wholesale instead of appending.
pub(crate) fn replace_text(fs: &dyn WorkspaceFs, cwd: &Path, name: &str) -> Option<String> {
    append_layers(fs, cwd, name).pop()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolRegistry;
    use crate::runtime::{AgentRuntime, HarnessConfig};
    use crate::testutil::{MemFs, MemStore, ScriptedLlm};
    use std::sync::Arc;

    fn runtime(fs: MemFs) -> AgentRuntime {
        AgentRuntime::new(
            Arc::new(ScriptedLlm::default()),
            Arc::new(MemStore::default()),
            ToolRegistry::new(),
            Arc::new(fs),
            HarnessConfig::default(),
        )
    }

    #[test]
    fn advisor_notes_and_skill_changes_leave_the_static_prefix_byte_identical() {
        let fs = MemFs::default().with("/ws/.harness/skills/alpha/SKILL.md", "# Alpha\nDo alpha.");
        let rt = runtime(fs);
        let control = crate::control::SessionControl::default();
        control.push_advisor_note("note one".into());

        let first = rt.build_system_prompt(Path::new("/ws"), None, None);
        // A skill appears and an advisor note is pending (the note travels as a `[system]`
        // message, not prompt text): the static head never moves.
        let fs2 = MemFs::default()
            .with("/ws/.harness/skills/alpha/SKILL.md", "# Alpha\nDo alpha.")
            .with("/ws/.harness/skills/beta/SKILL.md", "# Beta\nDo beta.");
        let rt2 = AgentRuntime {
            fs: Arc::new(fs2),
            ..rt.clone()
        };
        let second = rt2.build_system_prompt(Path::new("/ws"), None, None);
        let prefix_one = first.split("<project-context>").next().expect("marker");
        let prefix_two = second.split("<project-context>").next().expect("marker");
        assert_eq!(prefix_one, prefix_two, "the static block never moves");
        assert!(
            second.contains("beta"),
            "the new skill is in the dynamic tail"
        );
        assert!(!first.contains("note one") && !second.contains("note one"));
    }

    #[test]
    fn system_md_replaces_and_append_layers_append() {
        let fs = MemFs::default()
            .with_home("/home/u")
            .with(
                "/home/u/.harness/APPEND_SYSTEM.md",
                "user rules\r\nline two",
            )
            .with("/ws/.harness/APPEND_SYSTEM.md", "project rules")
            .with("/ws/.harness/SYSTEM.md", "You are a pirate.");
        let rt = runtime(fs);
        let prompt = rt.build_system_prompt(Path::new("/ws"), None, None);
        assert!(prompt.starts_with("You are a pirate."), "{prompt}");
        assert!(prompt.contains("user rules\nline two"), "{prompt}");
        assert!(prompt.contains("project rules"), "{prompt}");
        assert!(!prompt.contains("You are `hrn`"), "base.md was replaced");
    }

    #[test]
    fn cli_overrides_win_over_files() {
        let rt = AgentRuntime::new(
            Arc::new(ScriptedLlm::default()),
            Arc::new(MemStore::default()),
            ToolRegistry::new(),
            Arc::new(MemFs::default()),
            HarnessConfig {
                system_prompt_override: Some("Custom base.".into()),
                append_system_override: Some("Tail rules.".into()),
                ..HarnessConfig::default()
            },
        );
        let prompt = rt.build_system_prompt(Path::new("/ws"), None, None);
        assert!(prompt.starts_with("Custom base."), "{prompt}");
        assert!(prompt.contains("Tail rules."), "{prompt}");
    }

    #[test]
    fn the_subagent_handoff_block_joins_child_sessions_only() {
        let parent = runtime(MemFs::default());
        let child = parent.clone().as_subagent();
        assert!(
            !parent
                .build_system_prompt(Path::new("/ws"), None, None)
                .contains("Hand-off")
        );
        assert!(
            child
                .build_system_prompt(Path::new("/ws"), None, None)
                .contains("Hand-off")
        );
    }

    #[test]
    fn the_variant_selection_picks_the_per_family_base() {
        let rt = AgentRuntime::new(
            Arc::new(ScriptedLlm::default()),
            Arc::new(MemStore::default()),
            ToolRegistry::new(),
            Arc::new(MemFs::default()),
            HarnessConfig {
                prompt_variant: Some("gpt5".into()),
                ..HarnessConfig::default()
            },
        );
        let prompt = rt.build_system_prompt(Path::new("/ws"), None, None);
        assert!(prompt.contains("senior engineer"), "{prompt}");
        assert!(
            !prompt.contains("# Task framing"),
            "gpt5 drops the framing block"
        );
        let default = runtime(MemFs::default()).build_system_prompt(Path::new("/ws"), None, None);
        let lean = AgentRuntime {
            config: HarnessConfig {
                prompt_variant: Some("claude".into()),
                ..HarnessConfig::default()
            },
            ..rt.clone()
        }
        .build_system_prompt(Path::new("/ws"), None, None);
        assert!(lean.contains("You judge well"), "{lean}");
        assert!(
            lean.len() * 2 < default.len(),
            "the claude variant is leaner than half the prescriptive one"
        );
    }
}
