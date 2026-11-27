//! Workspace context discovery (`AGENTS.md`, `CLAUDE.md`, git branch) and slash-command parser.

use crate::discovery::{DiscoverySource, discovery_dirs, first_summary_line, parse_front_matter};
use hrn_kernel::{PathKind, Result, WorkspaceFs};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MAX_CONTEXT_FILE_BYTES: usize = 16 * 1024;

/// Which instruction sources a session reads. The `--skills` / `--no-skills` / `--no-rules` /
/// `--no-context-files` launch flags land here; subagents inherit the parent's settings.
#[derive(Clone, Debug)]
pub struct ContextSources {
    /// `--skills <globs>`: only skills whose name matches one pattern are discovered (`None`
    /// keeps every skill).
    pub skill_patterns: Option<Vec<String>>,
    /// `--no-skills`: skip skill discovery and catalog injection entirely.
    pub skills: bool,
    /// `--no-rules`: skip rules discovery, the rulebook and rule injection.
    pub rules: bool,
    /// `--no-context-files`: skip `AGENTS.md` / `CLAUDE.md` injection (the workspace/branch line
    /// stays).
    pub context_files: bool,
}

impl Default for ContextSources {
    fn default() -> Self {
        Self {
            skill_patterns: None,
            skills: true,
            rules: true,
            context_files: true,
        }
    }
}

/// A custom prompt template discovered from global or project `commands/*.md` directories.
#[derive(Clone, Debug)]
pub struct CustomCommand {
    /// Slash command name without leading `/` (lower-case, as typed).
    pub name: String,
    /// First-line description (from front matter `description` or first markdown line).
    pub description: String,
    /// Markdown prompt body (supports `$ARGUMENTS`).
    pub template: String,
    /// Layer where the winning command file was found.
    pub source: DiscoverySource,
}

impl CustomCommand {
    /// Render the command template with `args`, substituting `$ARGUMENTS` or appending when absent.
    pub fn expand(&self, args: &str) -> String {
        let arg_text = args.trim();
        if self.template.contains("$ARGUMENTS") {
            self.template.replace("$ARGUMENTS", arg_text)
        } else if arg_text.is_empty() {
            self.template.clone()
        } else {
            format!("{}\n\n{arg_text}", self.template)
        }
    }
}

/// Parsed slash command invocation entered by the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedSlash {
    /// Fork the DAG at a node id (or current head if omitted).
    Fork(Option<String>),
    /// Run explicit Tier 3 context compaction.
    Compact,
    /// Switch active model preset or role.
    Model(Option<String>),
    /// Switch thinking level (`off`/`low`/`medium`/`high`/`xhigh`).
    Think(Option<String>),
    /// Switch permission mode (`plan`/`default`/`auto-edit`/`yolo`).
    Mode(Option<String>),
    /// Show recent diff summary.
    Diff,
    /// Inspect configured MCP servers.
    Mcp,
    /// Trigger interactive OAuth login helper.
    Login(Option<String>),
    /// List sessions.
    Sessions,
    /// Clear / start a fresh session.
    New,
    /// Show built-in + custom slash commands.
    Help,
    /// Expanded custom `.agent/commands/<name>.md` prompt.
    CustomPrompt {
        /// Command name.
        name: String,
        /// Rendered prompt sent to the agent.
        prompt: String,
    },
}

/// Discover `AGENTS.md` / `CLAUDE.md` / `GEMINI.md` files from the repository root down to
/// `cwd`, plus the git branch. Outside a git repository only `cwd` itself is consulted
/// (ancestors such as the home directory are unrelated to the project). Unreadable files are
/// reported inline so the user and the model can see why instructions are missing.
/// `include_files` false (`--no-context-files`) keeps only the workspace/branch line. The
/// chain is capped (§4.3): at most `context_files_max` files and `context_files` tokens total,
/// oldest-cut with a visible marker. Nested files *below* `cwd` are not here — they are
/// injected on first touch by the nested-context hook.
pub fn discover_project_context(fs: &dyn WorkspaceFs, cwd: &Path, include_files: bool) -> String {
    let mut sections: Vec<String> = Vec::new();
    match detect_git_branch(fs, cwd) {
        Some(branch) => sections.push(format!(
            "Workspace: {}\nGit branch: {branch}",
            cwd.display()
        )),
        None => sections.push(format!("Workspace: {}", cwd.display())),
    }

    if include_files {
        let budgets = crate::prompt_data::budgets();
        let mut files: Vec<String> = Vec::new();
        let mut used = 0usize;
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        let mut found_root = false;
        let mut cur = Some(cwd);
        while let Some(dir) = cur {
            dirs.push(dir.to_path_buf());
            if fs.kind(&dir.join(".git")) != PathKind::Missing {
                found_root = true;
                break;
            }
            cur = dir.parent();
        }
        if !found_root {
            dirs = vec![cwd.to_path_buf()];
        }
        dirs.reverse();
        let mut dropped = 0usize;
        for dir in dirs {
            for name in ["AGENTS.md", "CLAUDE.md", "GEMINI.md"] {
                let path = dir.join(name);
                match fs.read_to_string(&path) {
                    Ok(Some(raw)) => {
                        let trimmed = raw.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let clipped =
                            crate::prompt_data::clip_bytes(trimmed, MAX_CONTEXT_FILE_BYTES);
                        let section =
                            format!("# Project Instructions ({})\n{clipped}", path.display());
                        let cost = crate::prompt_data::tokens(&section);
                        if files.len() >= budgets.context_files_max
                            || used + cost > budgets.context_files * 4
                        {
                            dropped += 1;
                            continue;
                        }
                        used += cost;
                        files.push(section);
                    }
                    Ok(None) => {}
                    Err(err) => sections.push(format!(
                        "# Project Instructions ({}) could not be read: {err}",
                        path.display()
                    )),
                }
            }
        }
        if !files.is_empty() {
            sections.push(
                "You MUST follow these context files; where one conflicts with the base \
                 prompt, this later block wins, except the base's NEVER clauses."
                    .to_owned(),
            );
            sections.extend(files);
            if dropped > 0 {
                sections.push(format!("… ({dropped} more context files not loaded)"));
            }
        }
    }
    sections.join("\n\n")
}

fn read_head(fs: &dyn WorkspaceFs, git_dir: &Path) -> Option<String> {
    // Branch display is best-effort decoration: an unreadable HEAD just means "no branch shown".
    fs.read_to_string(&git_dir.join("HEAD")).ok().flatten()
}

fn detect_git_branch(fs: &dyn WorkspaceFs, cwd: &Path) -> Option<String> {
    let mut cur = Some(cwd);
    while let Some(dir) = cur {
        let dot_git = dir.join(".git");
        match fs.kind(&dot_git) {
            PathKind::Dir => return parse_git_head(&read_head(fs, &dot_git)?),
            PathKind::File => {
                // Git worktree / submodule pointer file: `gitdir: <path>`
                let pointer = fs.read_to_string(&dot_git).ok().flatten()?;
                let gitdir = pointer.trim().strip_prefix("gitdir:")?.trim();
                let resolved = if Path::new(gitdir).is_absolute() {
                    PathBuf::from(gitdir)
                } else {
                    dir.join(gitdir)
                };
                return parse_git_head(&read_head(fs, &resolved)?);
            }
            PathKind::Missing => cur = dir.parent(),
        }
    }
    None
}

fn parse_git_head(head: &str) -> Option<String> {
    let trimmed = head.trim();
    if let Some(ref_name) = trimmed.strip_prefix("ref: refs/heads/") {
        Some(ref_name.to_owned())
    } else if !trimmed.is_empty() {
        let short = trimmed
            .get(..trimmed.floor_char_boundary(8))
            .unwrap_or(trimmed);
        Some(format!("detached@{short}"))
    } else {
        None
    }
}

/// Discover custom slash commands across global (`~/.agents`, `~/.agent`, `~/.codex`,
/// `~/.claude`, `~/.omp/agent`, `~/.harness`) and project (`.claude`, `.agent`, `.harness`)
/// `commands/` directories (later layers win). Command names are lower-cased because
/// [`parse_slash`] lower-cases what the user types.
///
/// # Errors
/// Propagates the first unreadable directory or file.
pub fn discover_custom_commands(
    fs: &dyn WorkspaceFs,
    cwd: &Path,
) -> Result<BTreeMap<String, CustomCommand>> {
    let mut out = BTreeMap::new();

    for (source, dir) in discovery_dirs(fs, cwd, "commands") {
        for path in fs.list_dir(&dir)? {
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Names containing whitespace could never be typed as a single slash token.
            if stem.is_empty() || stem.contains(char::is_whitespace) {
                continue;
            }
            if let Some(raw) = fs.read_to_string(&path)? {
                let (fm, body) = parse_front_matter(&raw);
                let description = fm
                    .get("description")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map_or_else(|| first_summary_line(body, "custom command"), str::to_owned);
                let name = stem.to_lowercase();
                out.insert(
                    name.clone(),
                    CustomCommand {
                        name,
                        description,
                        template: body.trim().to_owned(),
                        source: source.clone(),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// Parse user input starting with `/` into either a built-in or custom slash command.
pub fn parse_slash(input: &str, custom: &BTreeMap<String, CustomCommand>) -> Option<ParsedSlash> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let cmd = parts.next()?.trim().to_lowercase();
    let arg = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    match cmd.as_str() {
        "fork" => Some(ParsedSlash::Fork(arg)),
        "compact" => Some(ParsedSlash::Compact),
        "model" => Some(ParsedSlash::Model(arg)),
        "think" | "thinking" => Some(ParsedSlash::Think(arg)),
        "mode" => Some(ParsedSlash::Mode(arg)),
        "diff" => Some(ParsedSlash::Diff),
        "mcp" => Some(ParsedSlash::Mcp),
        "login" => Some(ParsedSlash::Login(arg)),
        "sessions" => Some(ParsedSlash::Sessions),
        "new" | "clear" => Some(ParsedSlash::New),
        "help" => Some(ParsedSlash::Help),
        other => custom.get(other).map(|c| ParsedSlash::CustomPrompt {
            name: c.name.clone(),
            prompt: c.expand(arg.as_deref().unwrap_or("")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MemFs;

    #[test]
    fn instructions_are_collected_root_first_and_stop_at_the_repo_root() {
        let fs = MemFs::default()
            .with("/repo/.git/HEAD", "ref: refs/heads/main\n")
            .with("/repo/AGENTS.md", "root rules")
            .with("/repo/pkg/CLAUDE.md", "pkg rules")
            .with("/AGENTS.md", "outside the repo");
        let ctx = discover_project_context(&fs, Path::new("/repo/pkg"), true);
        assert!(ctx.contains("Git branch: main"), "{ctx}");
        let root = ctx.find("root rules").expect("root rules");
        let pkg = ctx.find("pkg rules").expect("pkg rules");
        assert!(root < pkg, "root instructions must precede nested ones");
        assert!(!ctx.contains("outside the repo"));
    }

    #[test]
    fn outside_a_repo_only_cwd_is_consulted() {
        let fs = MemFs::default()
            .with("/home/me/AGENTS.md", "personal notes")
            .with("/home/me/proj/AGENTS.md", "proj");
        let ctx = discover_project_context(&fs, Path::new("/home/me/proj"), true);
        assert!(ctx.contains("proj") && !ctx.contains("personal notes"));
    }

    #[test]
    fn context_files_can_be_disabled_but_the_workspace_line_stays() {
        let fs = MemFs::default()
            .with("/repo/.git/HEAD", "ref: refs/heads/main\n")
            .with("/repo/AGENTS.md", "root rules");
        let ctx = discover_project_context(&fs, Path::new("/repo"), false);
        assert!(ctx.contains("Workspace: /repo"), "{ctx}");
        assert!(ctx.contains("Git branch: main"));
        assert!(
            !ctx.contains("root rules"),
            "--no-context-files drops the instructions"
        );
    }

    #[test]
    fn worktree_pointer_and_detached_head_are_understood() {
        let fs = MemFs::default()
            .with("/wt/.git", "gitdir: /main/.git/worktrees/wt\n")
            .with("/main/.git/worktrees/wt/HEAD", "0123456789abcdef\n");
        let ctx = discover_project_context(&fs, Path::new("/wt"), true);
        assert!(ctx.contains("Git branch: detached@01234567"), "{ctx}");
    }

    #[test]
    fn custom_commands_are_lowercased_so_the_typed_name_matches() {
        let fs = MemFs::default()
            .with(
                "/ws/.agent/commands/Review.md",
                "---\nx: y\n---\n# Review the diff\nLook at $ARGUMENTS",
            )
            .with("/ws/.agent/commands/two words.md", "ignored");
        let cmds = discover_custom_commands(&fs, Path::new("/ws")).expect("discover");
        assert_eq!(cmds.keys().collect::<Vec<_>>(), ["review"]);
        let cmd = cmds.get("review").expect("review");
        assert_eq!(cmd.description, "Review the diff");
        let parsed = parse_slash("/Review src/lib.rs", &cmds);
        assert!(
            matches!(parsed, Some(ParsedSlash::CustomPrompt { prompt, .. }) if prompt.ends_with("Look at src/lib.rs"))
        );
        assert_eq!(parse_slash("/usr/bin/ls is broken", &cmds), None);
    }
}
