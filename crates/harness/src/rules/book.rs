//! Rules (omp's rulebook): Markdown instruction files with `globs` / `alwaysApply` front matter,
//! discovered from the `.harness`, `.omp`, `.claude` and `.cursor` directories of the user and the
//! project.
//!
//! - `alwaysApply: true` (or a sticky `RULES.md`): the body joins the system prompt from the
//!   first step on.
//! - `globs: [...]`: the body is appended to the tool result the first time the session touches a
//!   matching file — the rule reaches the model exactly when it becomes relevant.
//! - everything else: listed in the system prompt by name and read on demand through
//!   `rule://<name>`.

use crate::discovery::{FrontMatter, parse_front_matter};
use crate::scope::{hashline_paths, path_matches_pattern};
use hrn_kernel::{PathKind, Result, WorkspaceFs};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Longest rule body kept.
const MAX_RULE_BYTES: usize = 16 * 1024;
/// Longest one-line description in the rulebook listing.
const MAX_DESC_CHARS: usize = 180;
/// Whole always-apply block ceiling.
const MAX_ALWAYS_BYTES: usize = 32 * 1024;

/// Tools whose arguments name files the session touches.
const PATH_ARG_KEYS: [&str; 6] = [
    "path",
    "file_path",
    "target_path",
    "old_path",
    "new_path",
    "notebook_path",
];

/// One discovered rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Rule identity (file stem; same-named rules of lower layers are overridden).
    pub name: String,
    /// Optional one-line description (rulebook listing).
    pub description: Option<String>,
    /// Path globs from `globs:` / `paths:` front matter.
    pub globs: Vec<String>,
    /// `alwaysApply: true` or a sticky `RULES.md`: the body is always in the system prompt.
    pub always_apply: bool,
    /// Full path to the rule file.
    pub path: PathBuf,
    /// Body without front matter.
    pub body: String,
}

fn clip_desc(raw: &str) -> String {
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_DESC_CHARS {
        return one_line;
    }
    let prefix: String = one_line.chars().take(MAX_DESC_CHARS - 1).collect();
    format!("{prefix}…")
}

fn rule_meta(fm: &FrontMatter) -> (Vec<String>, bool, Option<String>) {
    let globs = fm.get_list(&["globs", "paths"]);
    let always = matches!(
        fm.get_any(&["alwaysApply", "always_apply"]).map(str::trim),
        Some("true" | "yes")
    );
    let description = fm
        .get("description")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| clip_desc(&s.replace('\n', " ")));
    (globs, always, description)
}

fn load_rule(fs: &dyn WorkspaceFs, path: &Path, force_always: bool) -> Result<Option<Rule>> {
    let Some(raw) = fs.read_to_string(path)? else {
        return Ok(None);
    };
    let (fm, body) = parse_front_matter(&raw);
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return Ok(None);
    };
    if stem.is_empty() {
        return Ok(None);
    }
    let (globs, always, description) = rule_meta(&fm);
    let body = if trimmed.len() > MAX_RULE_BYTES {
        let cut = trimmed.floor_char_boundary(MAX_RULE_BYTES);
        format!("{}\n[truncated]", trimmed.get(..cut).unwrap_or_default())
    } else {
        trimmed.to_owned()
    };
    Ok(Some(Rule {
        name: stem.to_owned(),
        description,
        globs,
        always_apply: force_always || always,
        path: path.to_path_buf(),
        body,
    }))
}

/// Load one layer: `<base>/rules/*.md|*.mdc`, then the sticky `<base>/RULES.md` (forced
/// always-apply, kept under `sticky_name` so the user's and the project's coexist).
fn load_layer(
    fs: &dyn WorkspaceFs,
    base: &Path,
    sticky_name: Option<&str>,
    rules: &mut BTreeMap<String, Rule>,
) -> Result {
    for path in fs.list_dir(&base.join("rules"))? {
        if !matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("md" | "mdc")
        ) {
            continue;
        }
        if let Some(rule) = load_rule(fs, &path, false)? {
            rules.insert(rule.name.to_ascii_lowercase(), rule);
        }
    }
    if let Some(sticky) = sticky_name {
        let path = base.join("RULES.md");
        if fs.kind(&path) == PathKind::File
            && let Some(mut rule) = load_rule(fs, &path, true)?
        {
            rule.name = sticky.to_string();
            rules.insert(sticky.to_ascii_lowercase(), rule);
        }
    }
    Ok(())
}

/// The rules of one working directory.
#[derive(Clone, Debug, Default)]
pub struct Rulebook {
    rules: BTreeMap<String, Rule>,
}

impl Rulebook {
    /// Discover the rules of `cwd`, lowest precedence first so later layers override same-named
    /// rules: user `~/.cursor`, `~/.claude`, `~/.omp/agent`, `~/.harness`, then project `.claude`,
    /// `.cursor`, `.omp`, `.harness`. Sticky `RULES.md` files (forced always-apply) follow omp's
    /// naming: `RULES` for the user one, `RULES@project` for the project one.
    ///
    /// # Errors
    /// Propagates unreadable directories or rule files.
    pub fn discover(fs: &dyn WorkspaceFs, cwd: &Path) -> Result<Self> {
        let mut rules = BTreeMap::new();
        if let Some(home) = fs.home_dir() {
            for (base, sticky) in [
                (".cursor", None),
                (".claude", None),
                (".omp/agent", Some("RULES")),
                (".harness", Some("RULES")),
            ] {
                let mut dir = home.clone();
                for seg in base.split('/') {
                    dir.push(seg);
                }
                load_layer(fs, &dir, sticky, &mut rules)?;
            }
        }
        for (base, sticky) in [
            (".claude", None),
            (".cursor", None),
            (".omp", Some("RULES@project")),
            (".harness", Some("RULES@project")),
        ] {
            load_layer(fs, &cwd.join(base), sticky, &mut rules)?;
        }
        Ok(Self { rules })
    }

    /// Every rule by lower-cased name.
    #[must_use]
    pub fn rules(&self) -> &BTreeMap<String, Rule> {
        &self.rules
    }

    /// One rule by name, case-insensitively.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Rule> {
        self.rules.get(&name.to_ascii_lowercase())
    }

    /// The always-apply bodies for the system prompt (`None` when there are none).
    #[must_use]
    pub fn always_apply_prompt(&self) -> Option<String> {
        let mut out = String::from(
            "# Always-Applied Rules\nThese rules hold for every task in this workspace.",
        );
        let mut any = false;
        for rule in self.rules.values().filter(|r| r.always_apply) {
            let section = format!("\n\n## {}\n{}", rule.name, rule.body);
            if out.len() + section.len() > MAX_ALWAYS_BYTES {
                break;
            }
            out.push_str(&section);
            any = true;
        }
        any.then_some(out)
    }

    /// The rulebook listing: glob-bearing and described rules the model pulls with
    /// `rule://<name>` when it works on a matching file (`None` when there are none).
    #[must_use]
    pub fn rulebook_prompt(&self) -> Option<String> {
        let listed: Vec<&Rule> = self
            .rules
            .values()
            .filter(|r| !r.always_apply && (r.description.is_some() || !r.globs.is_empty()))
            .collect();
        if listed.is_empty() {
            return None;
        }
        let mut out = String::from(
            "# Rulebook\nRead `rule://<name>` before working on a file that matches a rule:",
        );
        for rule in listed {
            let globs = if rule.globs.is_empty() {
                String::new()
            } else {
                format!(" ({}): ", rule.globs.join(", "))
            };
            let _ = writeln!(
                out,
                "- {}{}{}",
                rule.name,
                globs,
                rule.description.as_deref().unwrap_or_default()
            );
        }
        Some(out.trim_end().to_owned())
    }

    /// The glob rules matching the touched path `rel` (always-apply rules are in the prompt
    /// already).
    #[must_use]
    pub fn matching(&self, rel: &str) -> Vec<&Rule> {
        self.rules
            .values()
            .filter(|r| !r.always_apply && rule_matches_path(r, rel))
            .collect()
    }
}

/// Whether `rule` fires for the touched workspace-relative path `rel`.
pub fn rule_matches_path(rule: &Rule, rel: &str) -> bool {
    rule.globs
        .iter()
        .any(|g| path_matches_pattern(rel, g) || path_matches_pattern(rel, &format!("**/{g}")))
}

/// The file paths a tool call touches, resolved against `cwd`.
///
/// A `bash` command's paths cannot be known without running it, so only path-carrying tools are
/// inspected: `edit` names its files in `[PATH#TAG]` headers, `apply_patch` in its `*** …File:`
/// lines, everything else in a `path`-like argument. Paths inside `cwd` come back
/// workspace-relative (what rule globs match); the rest keep their absolute spelling.
#[must_use]
pub fn touched_paths(cwd: &Path, tool_name: &str, args: &Value) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    let mut push = |value: &str| {
        let cleaned = value.trim().replace('\\', "/");
        if !cleaned.is_empty() && !raw.contains(&cleaned) {
            raw.push(cleaned);
        }
    };
    if tool_name == "edit"
        && let Some(input) = args.get("input").and_then(Value::as_str)
    {
        for path in hashline_paths(input) {
            push(&path);
        }
    }
    if tool_name == "apply_patch"
        && let Some(patch) = args.get("input").and_then(Value::as_str)
    {
        for line in patch.lines() {
            if let Some(rest) = line
                .strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Update File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
                .or_else(|| line.strip_prefix("*** Move to: "))
            {
                push(rest);
            }
        }
    }
    for key in PATH_ARG_KEYS {
        if let Some(value) = args.get(key).and_then(Value::as_str) {
            push(value);
        }
    }
    raw.into_iter()
        .map(|p| normalize_against_cwd(cwd, &p))
        .collect()
}

/// Lexical resolution of `rel_or_abs` (slash-normalized) against `cwd`: the workspace-relative
/// form when the path is inside `cwd`, the joined absolute form otherwise.
fn normalize_against_cwd(cwd: &Path, rel_or_abs: &str) -> String {
    let candidate = Path::new(rel_or_abs);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    let joined_text = joined.to_string_lossy().replace('\\', "/");
    let cwd_text = cwd.to_string_lossy().replace('\\', "/");
    let prefix = cwd_text.trim_end_matches('/');
    if !prefix.is_empty()
        && let Some(rest) = joined_text.strip_prefix(prefix)
    {
        let rest = rest.trim_start_matches('/');
        if !rest.is_empty() {
            return rest.to_owned();
        }
    }
    joined_text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MemFs, hook_ctx};
    use hrn_kernel::{HarnessHook, ToolOutput};
    use serde_json::json;

    fn fs() -> MemFs {
        MemFs::default()
            .with_home("/home/u")
            .with(
                "/home/u/.harness/rules/git.md",
                "---\ndescription: Git rules\nglobs: [\"*.rs\"]\n---\nUser git rule",
            )
            .with(
                "/ws/.cursor/rules/tests.mdc",
                "---\ndescription: Tests\nglobs: \"**/*_test.rs\"\n---\nCursor test rule",
            )
            .with(
                "/ws/.harness/rules/style.md",
                "---\nglobs: src/*.rs\nalwaysApply: true\n---\nAlways style rule",
            )
            .with("/ws/.omp/RULES.md", "Sticky project rule")
    }

    #[test]
    fn rules_discover_layers_and_kinds() {
        let book = Rulebook::discover(&fs(), Path::new("/ws")).expect("discover");
        assert!(book.get("git").is_some(), "user layer");
        assert!(book.get("tests").is_some(), "cursor .mdc layer");
        assert!(
            book.rules()["rules@project"].always_apply,
            "sticky RULES.md"
        );
        assert!(book.rules()["style"].always_apply, "alwaysApply");
        assert!(!book.rules()["git"].always_apply);

        let always = book.always_apply_prompt().expect("always block");
        assert!(always.contains("Always style rule") && always.contains("Sticky project rule"));
        assert!(!always.contains("User git rule"));
        let listing = book.rulebook_prompt().expect("rulebook");
        assert!(listing.contains("- git (*.rs): Git rules"), "{listing}");
        assert!(listing.contains("- tests (**/*_test.rs): Tests"));
    }

    #[test]
    fn touched_file_injects_its_rule_once_per_session() {
        let book = Rulebook::discover(&fs(), Path::new("/ws")).expect("discover");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let hook = crate::rules::RulesHook::new(book, std::sync::Arc::clone(&seen));
        let ctx = hook_ctx();

        let out = crate::testutil::block_on(hook.after_tool(
            &ctx,
            "write",
            &json!({"path": "/ws/notes/git.md", "content": "x"}),
            ToolOutput::ok("wrote"),
        ))
        .expect("after_tool");
        assert!(
            !out.content.contains("User git rule"),
            "globs `*.rs` must not match .md"
        );

        let out = crate::testutil::block_on(hook.after_tool(
            &ctx,
            "write",
            &json!({"path": "/ws/crates/lib.rs"}),
            ToolOutput::ok("wrote lib.rs"),
        ))
        .expect("after_tool");
        assert!(out.content.contains("User git rule"), "{}", out.content);
        assert!(out.content.contains("<rule name=\"git\""));

        // The second touch of the same rule injects nothing again.
        let out = crate::testutil::block_on(hook.after_tool(
            &ctx,
            "edit",
            &json!({"input": "[/ws/other.rs#A1]\n@@ -1 +1 @@\n+x"}),
            ToolOutput::ok("edited"),
        ))
        .expect("after_tool");
        assert!(!out.content.contains("User git rule"));

        // Another session of the same runtime still gets its own injection.
        let mut other = hook_ctx();
        other.session_id = "second".into();
        let out = crate::testutil::block_on(hook.after_tool(
            &other,
            "write",
            &json!({"path": "rel/lib.rs"}),
            ToolOutput::ok("wrote"),
        ))
        .expect("after_tool");
        assert!(out.content.contains("User git rule"));
    }

    #[test]
    fn touched_paths_cover_edit_and_apply_patch() {
        let cwd = Path::new("/ws");
        let paths = touched_paths(
            cwd,
            "edit",
            &json!({"input": "[src/a.rs#A1]\n@@\n-x\n+y\n[b/tests/x_test.rs#B2]\n@@\n-1\n+2"}),
        );
        assert_eq!(paths, vec!["src/a.rs", "b/tests/x_test.rs"]);
        let paths = touched_paths(
            cwd,
            "apply_patch",
            &json!({"input": "*** Begin Patch\n*** Update File: crates/x.rs\n*** Move to: crates/y.rs\n*** End Patch"}),
        );
        assert_eq!(paths, vec!["crates/x.rs", "crates/y.rs"]);
        let paths = touched_paths(cwd, "bash", &json!({"command": "rm -rf /tmp/x"}));
        assert!(paths.is_empty(), "bash paths are unknowable: {paths:?}");
        let paths = touched_paths(cwd, "write", &json!({"path": "C:/else/deep/file.rs"}));
        assert_eq!(paths, vec!["C:/else/deep/file.rs"]);
    }

    #[test]
    fn rule_matching_spans_directories() {
        let rule = Rule {
            name: "t".into(),
            description: None,
            globs: vec!["**/*_test.rs".into(), "Makefile".into()],
            always_apply: false,
            path: PathBuf::from("/ws/.harness/rules/t.md"),
            body: String::new(),
        };
        assert!(rule_matches_path(&rule, "crates/x/tests/y_test.rs"));
        assert!(rule_matches_path(&rule, "Makefile"));
        assert!(!rule_matches_path(&rule, "src/main.rs"));
    }
}
