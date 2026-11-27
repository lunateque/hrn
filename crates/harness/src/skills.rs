//! On-demand `SKILL.md` discovery and compact system-prompt catalog formatting.

use crate::discovery::{DiscoverySource, discovery_dirs, first_summary_line, parse_front_matter};
use hrn_kernel::{PathKind, Result, WorkspaceFs};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The most skills one catalog line list may carry; the rest get the `… (N more)` marker.
const MAX_CATALOG_SKILLS: usize = 32;
const MAX_DESC_CHARS: usize = 180;
/// Longest single autoloaded skill body injected into the system prompt.
const MAX_AUTOLOAD_BYTES: usize = 8 * 1024;
/// Longest autoloaded-skills block, whatever the number of skills.
const MAX_AUTOLOAD_TOTAL_BYTES: usize = 24 * 1024;

/// Metadata for one discovered `SKILL.md` entry (body is read on demand via `read`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillEntry {
    /// Skill identifier (from front matter `name`, else directory name).
    pub name: String,
    /// Concise one-line description shown in the system-prompt catalog.
    pub description: String,
    /// Full path to the `SKILL.md` file for on-demand reading.
    pub path: PathBuf,
    /// Layer where the winning definition was found.
    pub source: DiscoverySource,
    /// Front matter `autoload: true`: the body is injected into the system prompt instead of
    /// only being listed in the catalog.
    pub autoload: bool,
}

/// Keep the skills whose name matches at least one of `patterns` (empty list keeps all;
/// `*` does not cross `/`, `**` does).
#[must_use]
pub fn filter_skills(
    skills: BTreeMap<String, SkillEntry>,
    patterns: &[String],
) -> BTreeMap<String, SkillEntry> {
    if patterns.is_empty() {
        return skills;
    }
    skills
        .into_iter()
        .filter(|(_, skill)| {
            patterns
                .iter()
                .any(|p| crate::scope::path_matches_pattern(&skill.name, p))
        })
        .collect()
}

/// Expand one `--skills` flag value (`git-*,docker`) into patterns.
#[must_use]
pub fn parse_skill_patterns(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The `autoload: true` skills' bodies, injected into the system prompt so the model has them
/// without reading each `SKILL.md`. A file that cannot be read is reported inline instead of
/// silently dropped.
pub fn format_autoloaded_skills(
    fs: &dyn WorkspaceFs,
    skills: &BTreeMap<String, SkillEntry>,
) -> Option<String> {
    let mut out = String::from(
        "# Autoloaded Skills\n\
         These skills apply to your work now; follow them where relevant.\n",
    );
    let mut count = 0usize;
    for skill in skills.values().filter(|s| s.autoload) {
        let rendered = match fs.read_to_string(&skill.path) {
            Ok(Some(raw)) => {
                let (_fm, body) = parse_front_matter(&raw);
                let trimmed = body.trim();
                if trimmed.len() > MAX_AUTOLOAD_BYTES {
                    let cut = trimmed.floor_char_boundary(MAX_AUTOLOAD_BYTES);
                    format!("{}\n[truncated]", trimmed.get(..cut).unwrap_or_default())
                } else {
                    trimmed.to_owned()
                }
            }
            Ok(None) => String::new(),
            Err(err) => format!("(could not be read: {err})"),
        };
        if rendered.is_empty() {
            continue;
        }
        let section = format!(
            "\n## {name}\n{rendered}\n[Skill directory: {dir}]\n",
            name = skill.name,
            dir = skill
                .path
                .parent()
                .map_or_else(String::new, |p| p.display().to_string()),
        );
        if out.len() + section.len() > MAX_AUTOLOAD_TOTAL_BYTES {
            break;
        }
        out.push_str(&section);
        count += 1;
    }
    (count > 0).then_some(out.trim_end().to_owned())
}

fn clip_desc(raw: &str) -> String {
    let single_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= MAX_DESC_CHARS {
        return single_line;
    }
    let prefix: String = single_line.chars().take(MAX_DESC_CHARS - 1).collect();
    format!("{prefix}…")
}

fn find_skill_file(fs: &dyn WorkspaceFs, entry: &Path) -> Option<PathBuf> {
    if fs.kind(entry) != PathKind::Dir {
        return None;
    }
    for candidate in ["SKILL.md", "skill.md"] {
        let path = entry.join(candidate);
        if fs.kind(&path) == PathKind::File {
            return Some(path);
        }
    }
    None
}

/// Discover `*/SKILL.md` definitions across global (`~/.agents`, `~/.agent`, `~/.codex`,
/// `~/.claude`, `~/.omp/agent`, `~/.harness`) and project (`.claude`, `.agent`, `.omp`,
/// `.harness`) skill directories (later layers win, so `~/.harness` and project `.harness`
/// override omp/claude/codex/agents definitions of the same lower-cased name).
///
/// # Errors
/// Propagates unreadable directories or skill files.
pub fn discover_skills(fs: &dyn WorkspaceFs, cwd: &Path) -> Result<BTreeMap<String, SkillEntry>> {
    let mut skills = BTreeMap::new();
    for (source, dir) in discovery_dirs(fs, cwd, "skills") {
        for entry in fs.list_dir(&dir)? {
            let Some(skill_file) = find_skill_file(fs, &entry) else {
                continue;
            };
            let Some(dir_name) = entry.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(raw) = fs.read_to_string(&skill_file)? else {
                continue;
            };
            let (fm, body) = parse_front_matter(&raw);
            let name = fm
                .get("name")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(dir_name)
                .to_owned();
            let description = fm
                .get("description")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map_or_else(|| first_summary_line(body, "custom skill"), str::to_owned);
            let key = name.to_ascii_lowercase();
            skills.insert(
                key,
                SkillEntry {
                    name,
                    description: clip_desc(&description),
                    path: skill_file,
                    source: source.clone(),
                    autoload: matches!(fm.get("autoload").map(str::trim), Some("true" | "yes")),
                },
            );
        }
    }
    Ok(skills)
}

/// Render a compact system-prompt index of the read-on-demand skills (`None` when empty).
/// Autoloaded skills are injected in full elsewhere, so they are not listed here. Lines carry
/// name + the skill's own trigger text (no absolute path — `skill://<name>` addresses it) and
/// are cut at `budget` tokens, oldest last lines first.
pub fn format_skills_catalog(
    skills: &BTreeMap<String, SkillEntry>,
    budget: usize,
) -> Option<String> {
    let listed: Vec<&SkillEntry> = skills.values().filter(|s| !s.autoload).collect();
    if listed.is_empty() {
        return None;
    }
    let mut out = String::from(
        "# Available Skills\n\
         When a task matches a skill below, `read skill://<name>` and follow it before acting:\n",
    );
    // The cut marker must fit inside the budget too.
    let ceiling = budget
        .saturating_mul(crate::prompt_data::BYTES_PER_TOKEN)
        .saturating_sub(64);
    for (shown, skill) in listed.iter().enumerate() {
        let remaining = listed.len().saturating_sub(shown);
        if shown >= MAX_CATALOG_SKILLS {
            let _ = writeln!(
                out,
                "- … ({remaining} more skills; run `hrn skills` to list all)"
            );
            break;
        }
        let line = format!("- `{}`: {}\n", skill.name, skill.description);
        if out.len() + line.len() > ceiling {
            let _ = writeln!(
                out,
                "- … ({remaining} more skills; run `hrn skills` to list all)"
            );
            break;
        }
        out.push_str(&line);
    }
    Some(out.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MemFs;

    #[test]
    fn skills_discover_across_layers_and_higher_precedence_wins() {
        let fs = MemFs::default()
            .with_home("/home/u")
            .with(
                "/home/u/.claude/skills/rust-debug/SKILL.md",
                "---\nname: rust-debug\ndescription: Global Claude skill\n---\n# Body",
            )
            .with(
                "/home/u/.omp/agent/skills/omp-only/SKILL.md",
                "---\ndescription: From omp\n---\nBody",
            )
            .with(
                "/ws/.harness/skills/rust-debug/SKILL.md",
                "---\nname: rust-debug\ndescription: Project override\n---\n# Override",
            );

        let skills = discover_skills(&fs, Path::new("/ws")).expect("discover");
        assert_eq!(skills.len(), 2);

        let rd = skills.get("rust-debug").expect("rust-debug");
        assert_eq!(rd.description, "Project override");
        assert_eq!(rd.source, DiscoverySource::Project(".harness".into()));

        let omp = skills.get("omp-only").expect("omp-only");
        assert_eq!(omp.description, "From omp");
        assert_eq!(omp.source, DiscoverySource::Global("~/.omp/agent".into()));

        let catalog = format_skills_catalog(&skills, 1200).expect("catalog");
        assert!(catalog.contains("`rust-debug`: Project override"));
        assert!(catalog.contains("`omp-only`: From omp"));
    }

    #[test]
    fn autoload_bodies_inject_and_catalog_and_filter_skip_them() {
        let fs = MemFs::default()
            .with_home("/home/u")
            .with(
                "/home/u/.harness/skills/always-do/SKILL.md",
                "---\ndescription: Injected\ntimeout: 5\nautoload: true\n---\n# Do it always",
            )
            .with(
                "/home/u/.harness/skills/on-demand/SKILL.md",
                "---\ndescription: Listed\n---\nBody",
            );
        let skills = discover_skills(&fs, Path::new("/ws")).expect("discover");
        assert!(skills.get("always-do").expect("always-do").autoload);
        assert!(!skills.get("on-demand").expect("on-demand").autoload);

        let autoload = format_autoloaded_skills(&fs, &skills).expect("autoload block");
        assert!(autoload.contains("# Do it always"), "{autoload}");
        assert!(!autoload.contains("# Body"));
        // The catalog lists only the non-autoloaded skill.
        let catalog = format_skills_catalog(&skills, 1200).expect("catalog");
        assert!(catalog.contains("`on-demand`"));
        assert!(!catalog.contains("`always-do`"));

        let filtered = filter_skills(skills.clone(), &["on-*".to_owned()]);
        assert_eq!(filtered.keys().collect::<Vec<_>>(), vec!["on-demand"]);
        assert_eq!(filter_skills(skills.clone(), &[]).len(), 2);
    }
}
