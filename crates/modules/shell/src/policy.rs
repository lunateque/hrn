//! Shell approval policy, critical-command rules (`data/bash-critical.toml`), `[shell] git_guard`,
//! and `[shell.interceptor]` post-approval tool redirection.

use hrn_edge_tools::command_policy::{
    CommandPolicy, CommandVerdict, SegmentSep, evaluate, extract_subshell_commands,
    normalize_command_word, normalize_segment_words, peel_env_prefixes, shell_words,
    skip_command_wrappers, split_segments, unwrap_segment_command,
};
use hrn_edge_tools::notify::OutputRegex;
use hrn_kernel::serde::Deserialize;
use hrn_kernel::{ToolApproval, ToolApprovalPolicy, ToolTier};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Embedded critical-command rules table (parsed without external TOML crate to preserve
/// `Layer::Module`'s zero-external-dep invariant).
const CRITICAL_TOML: &str = include_str!("../data/bash-critical.toml");

/// One parsed rule from `data/bash-critical.toml`.
pub(crate) struct CriticalRule {
    pub id: String,
    pub reason: String,
    regex: OutputRegex,
}

static CRITICAL_RULES: OnceLock<Vec<CriticalRule>> = OnceLock::new();

fn critical_rules() -> &'static [CriticalRule] {
    CRITICAL_RULES
        .get_or_init(|| parse_critical_rules(CRITICAL_TOML))
        .as_slice()
}

/// Zero-dependency parser for `[[rule]]` rows with `id`, `pattern`, and `reason`.
fn parse_critical_rules(raw: &str) -> Vec<CriticalRule> {
    let mut out = Vec::new();
    let (mut id, mut pattern, mut reason) = (None, None, None);
    let flush = |out: &mut Vec<CriticalRule>,
                 id: &mut Option<String>,
                 pat: &mut Option<String>,
                 rsn: &mut Option<String>| {
        if let (Some(id), Some(pat), Some(rsn)) = (id.take(), pat.take(), rsn.take())
            && let Ok(regex) = OutputRegex::per_line(&pat, "bash-critical")
        {
            out.push(CriticalRule {
                id,
                reason: rsn,
                regex,
            });
        }
    };
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "[[rule]]" {
            flush(&mut out, &mut id, &mut pattern, &mut reason);
            continue;
        }
        let Some((key, val)) = trimmed.split_once('=') else {
            continue;
        };
        let Some(parsed) = parse_toml_string(val.trim()) else {
            continue;
        };
        match key.trim() {
            "id" => id = Some(parsed),
            "pattern" => pattern = Some(parsed),
            "reason" => reason = Some(parsed),
            _ => {}
        }
    }
    flush(&mut out, &mut id, &mut pattern, &mut reason);
    out
}

fn parse_toml_string(raw: &str) -> Option<String> {
    if let Some(inner) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Some(inner.to_owned());
    }
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next()? {
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            } else {
                out.push(c);
            }
        }
        return Some(out);
    }
    None
}

/// First critical rule matching `command` (checking raw text, line-continuation-collapsed text,
/// fully unquoted/wrapper-unwrapped segments, and recursively extracted `( ... )` / `{ ... ; }` /
/// `$(...)` / backtick subshells).
#[must_use]
pub(crate) fn match_critical(command: &str) -> Option<&'static CriticalRule> {
    let mut candidates = Vec::new();
    collect_critical_candidates(command, 0, &mut candidates);
    critical_rules().iter().find(|rule| {
        candidates
            .iter()
            .any(|cand| rule.regex.is_match(cand).unwrap_or(false))
    })
}

const MAX_CRITICAL_DEPTH: usize = 8;

fn collect_critical_candidates(command: &str, depth: usize, out: &mut Vec<String>) {
    push_candidate(out, command);
    let raw_single = command.replace("\\\r\n", "").replace("\\\n", "");
    push_candidate(out, &raw_single);
    if depth < MAX_CRITICAL_DEPTH {
        for sub in extract_subshell_commands(&raw_single) {
            if sub != raw_single.trim() {
                collect_critical_candidates(&sub, depth + 1, out);
            }
        }
    }

    let segments = split_segments(command);
    let mut joined_norm = String::new();
    let mut joined_unwrapped = String::new();
    for (idx, seg) in segments.iter().enumerate() {
        let (_, bare) = peel_env_prefixes(&seg.text);
        let word_norm = collapse_slashes(&normalize_command_word(&bare));
        let full_norm = collapse_slashes(&normalize_segment_words(&bare));
        let unwrapped = unwrap_segment_command(&bare).map(|u| collapse_slashes(&u));
        if idx > 0 {
            let sep = match seg.sep_before {
                Some(SegmentSep::Pipe) => " | ",
                Some(SegmentSep::And) => " && ",
                Some(SegmentSep::Or) => " || ",
                Some(SegmentSep::Bg) => " & ",
                Some(SegmentSep::Semi | SegmentSep::Newline) | None => " ; ",
            };
            joined_norm.push_str(sep);
            joined_unwrapped.push_str(sep);
        }
        joined_norm.push_str(&full_norm);
        joined_unwrapped.push_str(unwrapped.as_deref().unwrap_or(&full_norm));

        push_candidate(out, &word_norm);
        push_candidate(out, &full_norm);
        if let Some(u) = &unwrapped {
            push_candidate(out, u);
        }
        if depth < MAX_CRITICAL_DEPTH {
            for sub in extract_subshell_commands(&seg.text) {
                if sub != seg.text.trim() {
                    collect_critical_candidates(&sub, depth + 1, out);
                }
            }
        }
    }
    push_candidate(out, &joined_norm);
    push_candidate(out, &joined_unwrapped);
}

fn push_candidate(out: &mut Vec<String>, cand: &str) {
    let trimmed = cand.trim();
    if !trimmed.is_empty() && !out.iter().any(|s| s == trimmed) {
        out.push(trimmed.to_owned());
    }
}

fn collapse_slashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while let Some(&ch) = chars.get(i) {
        if ch == '/'
            && chars.get(i + 1) == Some(&'/')
            && i.checked_sub(1).and_then(|j| chars.get(j)) != Some(&':')
        {
            out.push('/');
            while chars.get(i) == Some(&'/') {
                i += 1;
            }
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Compute the [`ToolApproval`] descriptor for one `bash` invocation by combining `[shell] patterns`
/// with `data/bash-critical.toml`.
#[must_use]
pub(crate) fn bash_approval(policy: &CommandPolicy, command: &str) -> ToolApproval {
    let verdict = evaluate(policy, command);
    if let CommandVerdict::Deny { reason } = verdict {
        return ToolApproval {
            tier: ToolTier::Exec,
            policy: Some(ToolApprovalPolicy::Deny),
            reason: Some(reason),
            force_prompt: false,
        };
    }
    if let Some(rule) = match_critical(command) {
        return ToolApproval {
            tier: ToolTier::Exec,
            policy: None,
            reason: Some(format!("critical command ({}): {}", rule.id, rule.reason)),
            force_prompt: true,
        };
    }
    match verdict {
        CommandVerdict::Allow => ToolApproval {
            tier: ToolTier::Exec,
            policy: Some(ToolApprovalPolicy::Allow),
            reason: None,
            force_prompt: false,
        },
        CommandVerdict::Prompt { reason } => ToolApproval {
            tier: ToolTier::Exec,
            policy: Some(ToolApprovalPolicy::Prompt),
            reason: Some(reason),
            force_prompt: false,
        },
        CommandVerdict::Default => ToolApproval::exec(),
        CommandVerdict::Deny { .. } => unreachable!("handled above"),
    }
}

/// One `[shell.interceptor] patterns` rule.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
pub(crate) struct InterceptorRule {
    /// Rust regex tested against the trimmed bash command line.
    pub pattern: String,
    /// Target tool name that must be registered for this rule to be active.
    pub tool: String,
    /// Guidance returned to the model when the command is intercepted.
    pub message: String,
}

/// Effective `[shell.interceptor]` settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct InterceptorConfig {
    pub enabled: bool,
    pub patterns: Vec<InterceptorRule>,
}

/// Run the post-approval shell interceptor against `command`. Returns `Some(refusal_message)` when
/// a rule matches and its target tool is registered in `registered_tools`.
#[must_use]
pub(crate) fn check_interceptor(
    cfg: &InterceptorConfig,
    registered_tools: &[String],
    command: &str,
) -> Option<String> {
    if !cfg.enabled {
        return None;
    }
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }
    for rule in &cfg.patterns {
        if !registered_tools.iter().any(|t| t == &rule.tool) {
            continue;
        }
        let Ok(re) = OutputRegex::per_line(&rule.pattern, "shell.interceptor") else {
            continue;
        };
        if re.is_match(trimmed).unwrap_or(false) {
            return Some(format!(
                "blocked by shell interceptor (use `{}` tool): {}",
                rule.tool, rule.message
            ));
        }
    }
    None
}

/// Enforce `[shell] git_guard`: in the main git checkout (where `.git` is a directory, not a
/// linked-worktree `.git` file), refuse `git stash` mutations, `git reset --hard`, and branch
/// switch/checkout/restore (except conflict resolution `--ours`/`--theirs`).
#[must_use]
pub(crate) fn check_git_guard(enabled: bool, cwd: &Path, command: &str) -> Option<String> {
    if !enabled {
        return None;
    }
    for seg in split_segments(command) {
        let stripped = strip_subshell_wrappers(&seg.text);
        if stripped != seg.text
            && let Some(reason) = check_git_guard(true, cwd, stripped)
        {
            return Some(reason);
        }
        let (_, bare) = peel_env_prefixes(stripped);
        let words = shell_words(&bare);
        if let Some(reason) = check_git_segment(cwd, &words) {
            return Some(reason);
        }
    }
    None
}

fn strip_subshell_wrappers(text: &str) -> &str {
    let mut s = text.trim();
    loop {
        if let Some(inner) = s.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
            s = inner.trim();
            continue;
        }
        if let Some(inner) = s.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
            s = inner.trim().trim_end_matches(';').trim();
            continue;
        }
        break;
    }
    s
}

fn check_git_segment(cwd: &Path, words: &[String]) -> Option<String> {
    let mut idx = skip_command_wrappers(words);
    let bin = words.get(idx)?;
    let exe = Path::new(bin)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(bin);
    if !exe.eq_ignore_ascii_case("git") && !exe.eq_ignore_ascii_case("git.exe") {
        return None;
    }
    idx += 1;

    let mut target_dir = cwd.to_path_buf();
    let mut explicit_git_dir: Option<PathBuf> = None;
    let mut explicit_work_tree: Option<PathBuf> = None;
    while let Some(flag) = words.get(idx).map(String::as_str) {
        if flag == "-C" {
            if let Some(dir) = words.get(idx + 1) {
                target_dir = join_dir(&target_dir, dir);
                idx += 2;
                continue;
            }
            break;
        }
        if let Some(dir) = flag.strip_prefix("-C").filter(|d| !d.is_empty()) {
            target_dir = join_dir(&target_dir, dir);
            idx += 1;
            continue;
        }
        if flag == "--git-dir" {
            if let Some(dir) = words.get(idx + 1) {
                explicit_git_dir = Some(join_dir(&target_dir, dir));
                idx += 2;
                continue;
            }
            break;
        }
        if let Some(dir) = flag.strip_prefix("--git-dir=").filter(|d| !d.is_empty()) {
            explicit_git_dir = Some(join_dir(&target_dir, dir));
            idx += 1;
            continue;
        }
        if flag == "--work-tree" {
            if let Some(dir) = words.get(idx + 1) {
                explicit_work_tree = Some(join_dir(&target_dir, dir));
                idx += 2;
                continue;
            }
            break;
        }
        if let Some(dir) = flag.strip_prefix("--work-tree=").filter(|d| !d.is_empty()) {
            explicit_work_tree = Some(join_dir(&target_dir, dir));
            idx += 1;
            continue;
        }
        if matches!(flag, "-c" | "--namespace" | "--exec-path") {
            idx += 2;
            continue;
        }
        if flag.starts_with('-') {
            idx += 1;
            continue;
        }
        break;
    }
    let in_main = is_main_git_checkout(&target_dir)
        || explicit_work_tree
            .as_deref()
            .is_some_and(is_main_git_checkout)
        || explicit_git_dir.as_deref().is_some_and(is_main_git_dir);
    if !in_main {
        return None;
    }
    let sub = words.get(idx).map(String::as_str)?;
    let rest = words.get(idx + 1..).unwrap_or_default();
    check_git_subcommand(sub, rest)
}

fn join_dir(base: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn is_main_git_dir(git_dir: &Path) -> bool {
    if !git_dir.is_dir() {
        return false;
    }
    let norm = git_dir.to_string_lossy().replace('\\', "/");
    !norm.contains("/worktrees/")
}

fn check_git_subcommand(sub: &str, rest: &[String]) -> Option<String> {
    match sub {
        "stash" => {
            // Allow ONLY read-only `git stash list`, `git stash show`, and `--help`/`-h`.
            let first = rest.first().map(String::as_str);
            if matches!(first, Some("list" | "show" | "--help" | "-h")) {
                None
            } else {
                Some(
                    "blocked by [shell] git_guard: `git stash` mutations are refused in the \
                     main checkout (use a linked worktree)"
                        .to_owned(),
                )
            }
        }
        "reset" => {
            if rest.iter().any(|a| a == "--hard") {
                Some(
                    "blocked by [shell] git_guard: `git reset --hard` is refused in the main \
                     checkout (use a linked worktree)"
                        .to_owned(),
                )
            } else {
                None
            }
        }
        "switch" => Some(
            "blocked by [shell] git_guard: `git switch` is refused in the main checkout \
             (use a linked worktree)"
                .to_owned(),
        ),
        "checkout" | "restore" => {
            let is_conflict = rest.iter().any(|a| a == "--ours" || a == "--theirs");
            if is_conflict {
                None
            } else {
                Some(format!(
                    "blocked by [shell] git_guard: `git {sub}` is refused in the main checkout \
                     (only `--ours`/`--theirs` conflict resolution is allowed)"
                ))
            }
        }
        _ => None,
    }
}

/// `true` iff `dir` is inside a main git checkout (`.git` directory), `false` for a linked
/// worktree (`.git` file) or outside any git repository.
pub(crate) fn is_main_git_checkout(dir: &Path) -> bool {
    let mut cur: Option<&Path> = Some(dir);
    while let Some(d) = cur {
        let dot_git = d.join(".git");
        if dot_git.is_dir() {
            return true;
        }
        if dot_git.is_file() {
            return false;
        }
        cur = d.parent();
    }
    false
}
