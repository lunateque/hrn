//! Built-in `omp`-style subagent profiles (`scout` / alias `explore`, `task`,
//! `sonic` / alias `quick_task`, `plan`, `reviewer`, `security-reviewer`) and auto-discovery
//! of custom `agents/*.md` profiles across global and project directories.

use crate::discovery::{
    DiscoverySource, FrontMatter, discovery_dirs, first_summary_line, parse_front_matter,
};
use hrn_kernel::{HrnError, Result, ThinkingLevel, WorkspaceFs};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// Named profile configuring a subagent's role, tools, model, thinking effort, spawning policy,
/// structured output schema, and step budget.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors omp agent frontmatter boolean switches (read_only, blocking, structured, prewalk)"
)]
pub struct SubagentProfile {
    /// Canonical profile name (`scout`, `task`, `sonic`, `plan`, `reviewer`, `security-reviewer`,
    /// or custom `<name>.md`).
    pub name: String,
    /// Legacy or secondary names that resolve to this profile (e.g. `explore` -> `scout`).
    pub aliases: Vec<String>,
    /// Concise summary shown to the primary agent and in `hrn agents`.
    pub description: String,
    /// System prompt instructions prepended to the subagent's task.
    pub instructions: String,
    /// Restrict the child agent to `ReadOnly` tools only.
    pub read_only: bool,
    /// Optional model alias or role override (e.g. `"fast"`, `"plan"`, `"reviewer"`).
    pub model: Option<String>,
    /// Optional thinking effort override (`off | low | medium | high | xhigh`).
    pub thinking: Option<ThinkingLevel>,
    /// Optional tool allowlist; when non-empty, only these tools are exposed to the child.
    pub tools: Vec<String>,
    /// Tools always withheld from this subagent.
    pub deny_tools: Vec<String>,
    /// Optional per-turn step budget override from profile front matter; when unset the limit
    /// comes from `[task.agent_max_steps]` / `[task.max_steps]` (see [`crate::TaskConfig::max_steps_for`]).
    pub max_steps: Option<usize>,
    /// Optional child-agent allowlist (`None` = unrestricted beyond depth/read-only,
    /// `Some([])` = forbid all nested spawns, `Some(["*"])` = allow all non-self children).
    pub spawns: Option<Vec<String>>,
    /// Optional JSON Schema enforced on the child's hidden `yield` result.
    pub output: Option<Value>,
    /// When `true`, `task` always runs this agent synchronously and returns its settled summary.
    pub blocking: bool,
    /// When `true`, exposes structured `outputSchema` / `schemaMode` for this agent even when
    /// `[task] structured = false`.
    pub structured: bool,
    /// Optional override enabling/disabling the passive advisor for this subagent.
    pub advisor: Option<bool>,
    /// Metadata flag noting that `prewalk` frontmatter was parsed and ignored (documented divergence).
    pub prewalk_ignored: bool,
    /// Discovery layer where this profile came from.
    pub source: DiscoverySource,
}

struct BuiltinSpec {
    name: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
    instructions: &'static str,
    read_only: bool,
    model: Option<&'static str>,
    thinking: Option<ThinkingLevel>,
}

/// Built-in subagent profiles. They carry no step budget of their own:
/// `[task.agent_max_steps]` (falling back to `[task.max_steps]`) is the single place that sizes
/// them, so the limits stay user-configurable.
const BUILTIN_PROFILES: &[BuiltinSpec] = &[
    BuiltinSpec {
        name: "scout",
        aliases: &["explore"],
        description: "Fast read-only explorer locating files, symbols, and exact line ranges.",
        instructions: "You are a fast read-only codebase explorer. Search efficiently with glob/grep/read, map relevant files, symbols, and exact line ranges, and report concisely with zero speculation.",
        read_only: true,
        model: Some("fast"),
        thinking: Some(ThinkingLevel::Low),
    },
    BuiltinSpec {
        name: "task",
        aliases: &[],
        description: "Full implementation worker: edits files, runs builds/tests, and verifies changes.",
        instructions: "You are a focused implementation worker. Execute the assigned task cleanly with minimal targeted edits, verify with builds/tests/linters, and summarize all changed files.",
        read_only: false,
        model: None,
        thinking: None,
    },
    BuiltinSpec {
        name: "sonic",
        aliases: &["quick_task"],
        description: "Fast lightweight worker for small, isolated single-purpose edits.",
        instructions: "You are a fast, lightweight worker for small, well-scoped edits. Make the exact requested change quickly, verify it compiles/passes, and reply briefly.",
        read_only: false,
        model: Some("fast"),
        thinking: Some(ThinkingLevel::Low),
    },
    BuiltinSpec {
        name: "plan",
        aliases: &[],
        description: "Read-only architect inspecting code to produce a concrete step-by-step plan.",
        instructions: "You are a thorough read-only software architect. Inspect the relevant code and constraints, identify edge cases and trade-offs, and return a concrete, file-referenced implementation plan.",
        read_only: true,
        model: Some("plan"),
        thinking: Some(ThinkingLevel::High),
    },
    BuiltinSpec {
        name: "reviewer",
        aliases: &[],
        description: "Strict read-only code & diff reviewer catching bugs, regressions, and edge cases.",
        instructions: "You are a strict read-only code reviewer. Audit the target code or diff for correctness bugs, regressions, security issues, and untested edge cases. Cite exact file:line references.",
        read_only: true,
        model: Some("reviewer"),
        thinking: Some(ThinkingLevel::High),
    },
    BuiltinSpec {
        name: "security-reviewer",
        aliases: &[],
        description: "Read-only security auditor checking auth boundaries, injection, and secrets.",
        instructions: "You are a strict read-only security reviewer. Audit attack surfaces, access controls, path/command injection, untrusted input validation, and secret handling. Report verified findings with exact file:line references.",
        read_only: true,
        model: Some("reviewer"),
        thinking: Some(ThinkingLevel::High),
    },
];

/// Built-in default subagent profiles matching `omp`'s bundled agent set (`scout` with alias
/// `explore`, `task`, `sonic` with alias `quick_task`, `plan`, `reviewer`, `security-reviewer`).
pub fn default_subagent_profiles() -> BTreeMap<String, SubagentProfile> {
    BUILTIN_PROFILES
        .iter()
        .map(|spec| {
            (
                spec.name.to_owned(),
                SubagentProfile {
                    name: spec.name.to_owned(),
                    aliases: spec.aliases.iter().map(|a| (*a).to_owned()).collect(),
                    description: spec.description.to_owned(),
                    instructions: spec.instructions.to_owned(),
                    read_only: spec.read_only,
                    model: spec.model.map(str::to_owned),
                    thinking: spec.thinking,
                    tools: Vec::new(),
                    deny_tools: Vec::new(),
                    max_steps: None,
                    spawns: None,
                    output: None,
                    blocking: false,
                    structured: false,
                    advisor: None,
                    prewalk_ignored: false,
                    source: DiscoverySource::BuiltIn,
                },
            )
        })
        .collect()
}

/// Resolve a profile by canonical name or alias (case-insensitive; exact key always wins).
#[must_use]
pub fn lookup_profile<'a>(
    profiles: &'a BTreeMap<String, SubagentProfile>,
    name: &str,
) -> Option<&'a SubagentProfile> {
    let key = name.trim().to_ascii_lowercase();
    if let Some(exact) = profiles.get(&key) {
        return Some(exact);
    }
    profiles
        .values()
        .find(|p| p.aliases.iter().any(|a| a.eq_ignore_ascii_case(&key)))
}

/// Human-readable names (with aliases) of every discovered profile, used in unknown-agent errors.
#[must_use]
pub fn available_profile_names(profiles: &BTreeMap<String, SubagentProfile>) -> Vec<String> {
    profiles
        .values()
        .map(|p| {
            if p.aliases.is_empty() {
                p.name.clone()
            } else {
                format!("{} (alias: {})", p.name, p.aliases.join(", "))
            }
        })
        .collect()
}

/// Normalize `omp` tool names to `hrn` ones: `hrn` names its tools after omp's, so only `find`
/// (omp's file finder) differs.
fn normalize_tool_name(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "find" => "glob".to_owned(),
        other => other.to_owned(),
    }
}

/// Normalize `omp` model shorthands (`pi/smol` -> `fast`, `pi/slow` -> `plan`, `pi/default` -> none).
fn normalize_model_ref(raw: &str) -> Option<String> {
    let first = raw.split(',').next().map(str::trim).unwrap_or_default();
    if first.is_empty() {
        return None;
    }
    match first.to_ascii_lowercase().as_str() {
        "pi/smol" => Some("fast".to_owned()),
        "pi/slow" => Some("plan".to_owned()),
        "pi/default" | "default" => None,
        _ => Some(first.to_owned()),
    }
}

fn parse_bool_field(path: &Path, key: &str, raw: Option<&str>) -> Result<bool> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(other) => Err(HrnError::Config(format!(
            "{}: `{key}` must be true or false, got `{other}`",
            path.display()
        ))),
    }
}

fn parse_optional_bool_field(path: &Path, key: &str, raw: Option<&str>) -> Result<Option<bool>> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(other) => Err(HrnError::Config(format!(
            "{}: `{key}` must be true or false, got `{other}`",
            path.display()
        ))),
    }
}

fn parse_thinking_field(path: &Path, raw: Option<&str>) -> Result<Option<ThinkingLevel>> {
    let Some(val) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let normalized = if val.eq_ignore_ascii_case("none") {
        "off"
    } else {
        val
    };
    ThinkingLevel::parse(normalized).map(Some).ok_or_else(|| {
        HrnError::Config(format!(
            "{}: invalid thinking level `{val}` (expected auto, off, minimal, low, medium, high, xhigh, max)",
            path.display()
        ))
    })
}

fn parse_max_steps_field(path: &Path, raw: Option<&str>) -> Result<Option<usize>> {
    let Some(val) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let parsed: usize = val.parse().map_err(|_| {
        HrnError::Config(format!(
            "{}: `max_steps` must be a positive integer, got `{val}`",
            path.display()
        ))
    })?;
    if parsed == 0 {
        return Err(HrnError::Config(format!(
            "{}: `max_steps` must be > 0",
            path.display()
        )));
    }
    Ok(Some(parsed))
}

fn parse_spawns_field(fm: &FrontMatter) -> Option<Vec<String>> {
    if !fm.has_any(&["spawns"]) {
        return None;
    }
    if let Some(raw) = fm.get("spawns") {
        let low = raw.trim().to_ascii_lowercase();
        if matches!(low.as_str(), "none" | "false" | "[]") {
            return Some(Vec::new());
        }
    }
    let items: Vec<String> = fm
        .get_list(&["spawns"])
        .into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    Some(items)
}

fn parse_output_field(path: &Path, fm: &FrontMatter) -> Result<Option<Value>> {
    if !fm.has_any(&["output"]) {
        return Ok(None);
    }
    let raw = fm
        .get("output")
        .or_else(|| fm.get_block("output"))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return Ok(None);
    };
    let parsed: Value = serde_json::from_str(raw).map_err(|e| {
        HrnError::Config(format!(
            "{}: `output` must be a valid JSON Schema object: {e}",
            path.display()
        ))
    })?;
    Ok(Some(parsed))
}

/// Load built-in profiles merged with auto-discovered `*.md` profiles across global
/// (`~/.agents`, `~/.agent`, `~/.codex`, `~/.claude`, `~/.omp/agent`, `~/.harness`) and project
/// (`.claude`, `.agent`, `.omp`, `.harness`) `agents/` directories (later wins).
///
/// # Errors
/// Propagates unreadable directories/files or invalid typed front-matter values.
pub fn load_subagent_profiles(
    fs: &dyn WorkspaceFs,
    cwd: &Path,
) -> Result<BTreeMap<String, SubagentProfile>> {
    let mut profiles = default_subagent_profiles();

    for (source, dir) in discovery_dirs(fs, cwd, "agents") {
        for path in fs.list_dir(&dir)? {
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(raw) = fs.read_to_string(&path)? else {
                continue;
            };
            let profile = parse_profile_file(&path, stem, &raw, source.clone())?;
            profiles.insert(profile.name.to_ascii_lowercase(), profile);
        }
    }
    Ok(profiles)
}

fn parse_profile_file(
    path: &Path,
    stem: &str,
    raw: &str,
    source: DiscoverySource,
) -> Result<SubagentProfile> {
    let (fm, body) = parse_front_matter(raw);
    let name = fm
        .get("name")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(stem)
        .to_owned();
    let read_only = parse_bool_field(path, "read_only", fm.get("read_only"))?;
    let thinking = parse_thinking_field(path, fm.get_any(&["thinking", "thinking_level"]))?;
    let max_steps = parse_max_steps_field(path, fm.get("max_steps"))?;
    let model = fm.get("model").and_then(normalize_model_ref);
    let tools: Vec<String> = fm
        .get_list(&["tools"])
        .into_iter()
        .map(|t| normalize_tool_name(&t))
        .collect();
    let deny_tools: Vec<String> = fm
        .get_list(&["deny_tools", "disallowed_tools"])
        .into_iter()
        .map(|t| normalize_tool_name(&t))
        .collect();
    let spawns = parse_spawns_field(&fm);
    let output = parse_output_field(path, &fm)?;
    let blocking = parse_bool_field(path, "blocking", fm.get("blocking"))?;
    let structured = parse_bool_field(path, "structured", fm.get("structured"))?;
    let advisor = parse_optional_bool_field(path, "advisor", fm.get("advisor"))?;
    let prewalk_ignored = fm.has_any(&["prewalk"]);
    let aliases = fm
        .get_list(&["aliases"])
        .into_iter()
        .map(|a| a.trim().to_ascii_lowercase())
        .filter(|a| !a.is_empty())
        .collect();
    let description = fm
        .get("description")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(
            || first_summary_line(body, "custom subagent"),
            str::to_owned,
        );
    Ok(SubagentProfile {
        name,
        aliases,
        description,
        instructions: body.trim().to_owned(),
        read_only,
        model,
        thinking,
        tools,
        deny_tools,
        max_steps,
        spawns,
        output,
        blocking,
        structured,
        advisor,
        prewalk_ignored,
        source,
    })
}

/// Format a compact catalog of discovered subagent profiles for the primary agent's system
/// prompt. Built-ins always list (one line each); custom agents are capped at sixteen entries
/// and the whole block at `budget` tokens — the rest stay
/// reachable through `hrn agents` and `task`'s unknown-profile error.
pub fn format_agents_catalog(
    profiles: &BTreeMap<String, SubagentProfile>,
    budget: usize,
) -> String {
    let mut out = String::from(
        "# Available Subagents (`task`)\n\
         Delegate scoped work with `task(context, tasks=[{agent, task, solutionSpace}])`:\n",
    );
    let ceiling = budget
        .saturating_mul(crate::prompt_data::BYTES_PER_TOKEN)
        .saturating_sub(64);
    let is_builtin = |p: &SubagentProfile| p.source == DiscoverySource::BuiltIn;
    let custom: Vec<&SubagentProfile> = profiles.values().filter(|p| !is_builtin(p)).collect();
    let mut listed = 0usize;
    for profile in profiles.values().filter(|p| is_builtin(p)) {
        let line = profile_line(profile);
        if out.len() + line.len() <= ceiling {
            out.push_str(&line);
            listed += 1;
        }
    }
    for profile in custom.iter().take(MAX_CATALOG_CUSTOM) {
        let line = profile_line(profile);
        if out.len() + line.len() > ceiling {
            break;
        }
        out.push_str(&line);
        listed += 1;
    }
    if profiles.len() > listed {
        let _ = writeln!(
            out,
            "- … ({} more; run `hrn agents` to list all)",
            profiles.len() - listed
        );
    }
    out.trim_end().to_owned()
}

/// One catalog line: name (+ aliases), capability tags, description.
fn profile_line(profile: &SubagentProfile) -> String {
    let mut tags = Vec::new();
    if profile.read_only {
        tags.push("ro".to_owned());
    }
    if profile.blocking {
        tags.push("sync".to_owned());
    }
    if let Some(m) = &profile.model {
        tags.push(format!("model={m}"));
    }
    if let Some(t) = profile.thinking {
        tags.push(format!("think={}", t.as_str()));
    }
    let alias_part = if profile.aliases.is_empty() {
        String::new()
    } else {
        format!(" (alias `{}`)", profile.aliases.join("`, `"))
    };
    let suffix = if tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", tags.join(", "))
    };
    format!(
        "- `{}`{alias_part}{suffix}: {}\n",
        profile.name, profile.description
    )
}

/// Longest custom-agent listing kept inline; more are routing data behind `hrn agents`.
const MAX_CATALOG_CUSTOM: usize = 16;

#[cfg(test)]
#[path = "profiles_omp_tests.rs"]
mod omp_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MemFs;
    use serde_json::json;

    #[test]
    fn built_in_profiles_match_omp_roles() {
        let profiles = default_subagent_profiles();
        for name in [
            "scout",
            "task",
            "sonic",
            "plan",
            "reviewer",
            "security-reviewer",
        ] {
            assert!(profiles.contains_key(name), "missing built-in `{name}`");
        }
        assert!(profiles.get("scout").is_some_and(|p| p.read_only));
        assert!(profiles.get("plan").is_some_and(|p| p.read_only));
        assert!(profiles.get("task").is_some_and(|p| !p.read_only));
        assert!(profiles.get("sonic").is_some_and(|p| !p.read_only));
        assert!(profiles.get("reviewer").is_some_and(|p| p.read_only));
        assert!(
            profiles
                .get("security-reviewer")
                .is_some_and(|p| p.read_only)
        );
        assert_eq!(
            lookup_profile(&profiles, "explore").map(|p| p.name.as_str()),
            Some("scout")
        );
        assert_eq!(
            lookup_profile(&profiles, "quick_task").map(|p| p.name.as_str()),
            Some("sonic")
        );
        let catalog = format_agents_catalog(&profiles, 400);
        assert!(catalog.contains("`scout`") && catalog.contains("`explore`"));
        assert!(catalog.contains("`sonic`") && catalog.contains("`quick_task`"));
        assert!(catalog.contains("`security-reviewer`"));
    }

    #[test]
    fn custom_profiles_honour_rich_front_matter_and_layer_precedence() {
        let fs = MemFs::default()
            .with_home("/home/u")
            .with(
                "/home/u/.omp/agent/agents/auditor.md",
                "---\ndescription: Global auditor\nmodel: pi/slow\nthinking-level: high\ntools: read, grep\n---\nGlobal body.",
            )
            .with(
                "/ws/.agent/agents/auditor.md",
                "---\ndescription: Project auditor\nread_only: true\nmodel: strong\nthinking: xhigh\nmax_steps: 12\ntools:\n  - read\n  - find\ndeny-tools: [bash]\nspawns: [scout, reviewer]\noutput: {\"type\":\"object\",\"required\":[\"ok\"]}\nblocking: true\nstructured: true\nadvisor: false\nprewalk:\n  into: @smol\n---\nAudit the change.",
            )
            .with(
                "/ws/.agent/agents/plain.md",
                "Just do it. read_only: true is mentioned in prose.",
            );
        let profiles = load_subagent_profiles(&fs, Path::new("/ws")).expect("profiles");
        let auditor = profiles.get("auditor").expect("auditor");
        assert!(auditor.read_only);
        assert_eq!(auditor.description, "Project auditor");
        assert_eq!(auditor.model.as_deref(), Some("strong"));
        assert_eq!(auditor.thinking, Some(ThinkingLevel::Xhigh));
        assert_eq!(auditor.max_steps, Some(12));
        assert_eq!(auditor.tools, vec!["read", "glob"]);
        assert_eq!(auditor.deny_tools, vec!["bash"]);
        assert_eq!(
            auditor.spawns,
            Some(vec!["scout".to_owned(), "reviewer".to_owned()])
        );
        assert_eq!(
            auditor.output,
            Some(json!({"type": "object", "required": ["ok"]}))
        );
        assert!(auditor.blocking);
        assert!(auditor.structured);
        assert_eq!(auditor.advisor, Some(false));
        assert!(auditor.prewalk_ignored);
        assert_eq!(auditor.instructions, "Audit the change.");
        assert_eq!(auditor.source, DiscoverySource::Project(".agent".into()));
        assert!(!profiles.get("plain").expect("plain").read_only);

        let bad =
            MemFs::default().with("/ws/.agent/agents/x.md", "---\nread_only: maybe\n---\nbody");
        assert!(load_subagent_profiles(&bad, Path::new("/ws")).is_err());
    }

    #[test]
    fn omp_model_aliases_map_cleanly() {
        assert_eq!(normalize_model_ref("pi/smol").as_deref(), Some("fast"));
        assert_eq!(normalize_model_ref("pi/slow").as_deref(), Some("plan"));
        assert_eq!(normalize_model_ref("pi/default"), None);
        assert_eq!(normalize_model_ref("opus, sonnet").as_deref(), Some("opus"));
    }
}
