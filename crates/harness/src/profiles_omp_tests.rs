//! End-to-end `.omp/agent/` compatibility tests (10+ custom `*-exec`/`opus-max` profiles,
//! `thinking-level: max` & `minimal`, `@role` model refs, `skills/route/SKILL.md`, and
//! `APPEND_SYSTEM.md`).

use crate::profiles::{format_agents_catalog, load_subagent_profiles};
use crate::registry::ToolRegistry;
use crate::runtime::{AgentRuntime, HarnessConfig};
use crate::skills::discover_skills;
use crate::testutil::{MemFs, MemStore, ScriptedLlm};
use hrn_kernel::ThinkingLevel;
use std::path::Path;
use std::sync::Arc;

#[test]
fn omp_agent_setup_loads_all_exec_agents_max_thinking_skills_and_append_system() {
    let mut fs = MemFs::default()
        .with_home("/home/u")
        .with(
            "/home/u/.omp/agent/APPEND_SYSTEM.md",
            "Always route multi-file work via route skill.",
        )
        .with(
            "/home/u/.omp/agent/skills/route/SKILL.md",
            "---\nname: route\ndescription: Route tasks across specialist *-exec agents.\n---\nUse `task` with the right specialist.",
        )
        .with(
            "/home/u/.omp/agent/agents/opus-max.md",
            "---\nname: opus-max\ndescription: Maximum reasoning Opus worker\nmodel: anthropic/claude-opus-5-5\nthinking-level: max\n---\nThink deeply.",
        )
        .with(
            "/home/u/.omp/agent/agents/glm-exec.md",
            "---\nname: glm-exec\ndescription: GLM executor\nmodel: '@glm'\nthinking-level: minimal\n---\nExecute fast.",
        );

    for name in [
        "deepseek-exec",
        "glm53-exec",
        "gpt-exec",
        "haiku-exec",
        "qwen-exec",
        "qwen27b-exec",
        "ripple-exec",
        "sonnet-exec",
    ] {
        fs = fs.with(
            &format!("/home/u/.omp/agent/agents/{name}.md"),
            &format!(
                "---\nname: {name}\ndescription: {name} specialist\nmodel: '@{name}'\n---\nRun {name}."
            ),
        );
    }

    let cwd = Path::new("/ws");
    let profiles = load_subagent_profiles(&fs, cwd).expect("load profiles");
    let opus = profiles.get("opus-max").expect("opus-max");
    assert_eq!(opus.thinking, Some(ThinkingLevel::Max));
    assert_eq!(opus.model.as_deref(), Some("anthropic/claude-opus-5-5"));

    let glm = profiles.get("glm-exec").expect("glm-exec");
    assert_eq!(glm.thinking, Some(ThinkingLevel::Minimal));
    assert_eq!(glm.model.as_deref(), Some("@glm"));

    let catalog = format_agents_catalog(&profiles, 2_000);
    for expected in [
        "opus-max",
        "glm-exec",
        "deepseek-exec",
        "glm53-exec",
        "gpt-exec",
        "haiku-exec",
        "qwen-exec",
        "qwen27b-exec",
        "ripple-exec",
        "sonnet-exec",
    ] {
        assert!(
            catalog.contains(&format!("`{expected}`")),
            "catalog must list all 10 custom agents (missing `{expected}`):\n{catalog}"
        );
    }

    let skills = discover_skills(&fs, cwd).expect("skills");
    assert!(skills.contains_key("route"));

    let rt = AgentRuntime::new(
        Arc::new(ScriptedLlm::default()),
        Arc::new(MemStore::default()),
        ToolRegistry::new(),
        Arc::new(fs),
        HarnessConfig::default(),
    );
    let prompt = rt.build_system_prompt(cwd, None, None);
    assert!(prompt.contains("Always route multi-file work via route skill."));
}
