//! Slash commands: declarative registry (`data/slash.toml`), parsing and completions.

use crate::port::CustomCommandInfo;
use hrn_kernel::{PermissionMode, ThinkingLevel};
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::OnceLock;

const REGISTRY_TOML: &str = include_str!("../data/slash.toml");

/// Declarative metadata for one built-in slash command, loaded from `data/slash.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Spec {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) aliases: Vec<String>,
    #[serde(default)]
    pub(crate) args: String,
    #[serde(default)]
    pub(crate) subcommands: Vec<String>,
    #[serde(default)]
    pub(crate) hint: String,
    pub(crate) help: String,
}

impl Spec {
    fn matches(&self, token: &str) -> bool {
        self.name == token || self.aliases.iter().any(|alias| alias == token)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    command: Vec<Spec>,
}

fn parse_registry(raw: &str) -> Result<Vec<Spec>, toml::de::Error> {
    toml::from_str::<RegistryFile>(raw).map(|file| file.command)
}

static SPECS: OnceLock<Vec<Spec>> = OnceLock::new();

/// Built-in slash command specifications loaded once from `data/slash.toml`.
pub(crate) fn specs() -> &'static [Spec] {
    SPECS
        .get_or_init(|| parse_registry(REGISTRY_TOML).unwrap_or_default())
        .as_slice()
}

fn find_spec(token: &str) -> Option<&'static Spec> {
    specs().iter().find(|spec| spec.matches(token))
}

const THINKING: [ThinkingLevel; 8] = [
    ThinkingLevel::Auto,
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Xhigh,
    ThinkingLevel::Max,
];

/// Modes `ctrl+y` cycles through; `/plan` or `/mode plan` enters read-only planning.
pub(crate) const MODES: [PermissionMode; 3] = [
    PermissionMode::Default,
    PermissionMode::AutoEdit,
    PermissionMode::Yolo,
];

/// All permission modes accepted by `/mode <mode>`.
pub(crate) const ALL_MODES: [PermissionMode; 5] = [
    PermissionMode::Default,
    PermissionMode::AskCached,
    PermissionMode::AutoEdit,
    PermissionMode::Yolo,
    PermissionMode::Plan,
];

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Login(Option<String>),
    Logout(Option<String>),
    Setup,
    Auth,
    Model(Option<String>),
    Switch(Option<String>),
    Think(Option<String>),
    Fast(Option<String>),
    Slow(Option<String>),
    ExtendedContext(Option<String>),
    Mode(Option<String>),
    ForgetApprovals,
    Plan(Option<String>),
    Advisor(Option<String>),
    Attach(Option<String>),
    Image(Option<String>),
    Tree,
    Branch,
    Fork(Option<String>),
    New,
    Sessions,
    Compact(Option<String>),
    Shake,
    Handoff(Option<String>),
    Btw(Option<String>),
    Export(Option<String>),
    Share(Option<String>),
    Collab(Option<String>),
    Join(Option<String>),
    Leave,
    Stats,
    Usage,
    Settings,
    Context,
    Tools,
    Mcp(Option<String>),
    Jobs,
    Agents,
    Todo(Option<String>),
    Pin(Option<String>),
    Unpin(Option<String>),
    Delete(Option<String>),
    Rename(Option<String>),
    Retry,
    Copy,
    Hotkeys,
    Goal(Option<String>),
    Modelpreset(Option<String>),
    Theme(Option<String>),
    Queue(Option<String>),
    Loop(Option<String>),
    Wt(Option<String>),
    Memory(Option<String>),
    Init,
    Trace(Option<String>),
    Dump(Option<String>),
    Commit(Option<String>),
    Git(Option<String>),
    Remote(Option<String>),
    Review(Option<String>),
    Vibe(Option<String>),
    Agy(Option<String>),
    Help,
    Quit,
}

impl Command {
    /// Canonical slash name without the leading `/`.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Login(_) => "login",
            Self::Logout(_) => "logout",
            Self::Setup => "setup",
            Self::Auth => "auth",
            Self::Model(_) => "model",
            Self::Switch(_) => "switch",
            Self::Think(_) => "think",
            Self::Fast(_) => "fast",
            Self::Slow(_) => "slow",
            Self::ExtendedContext(_) => "extended-context",
            Self::Mode(_) => "mode",
            Self::ForgetApprovals => "forget-approvals",
            Self::Plan(_) => "plan",
            Self::Advisor(_) => "advisor",
            Self::Attach(_) => "attach",
            Self::Image(_) => "image",
            Self::Tree => "tree",
            Self::Branch => "branch",
            Self::Fork(_) => "fork",
            Self::New => "new",
            Self::Sessions => "sessions",
            Self::Compact(_) => "compact",
            Self::Shake => "shake",
            Self::Handoff(_) => "handoff",
            Self::Btw(_) => "btw",
            Self::Export(_) => "export",
            Self::Share(_) => "share",
            Self::Collab(_) => "collab",
            Self::Join(_) => "join",
            Self::Leave => "leave",
            Self::Stats => "stats",
            Self::Usage => "usage",
            Self::Settings => "settings",
            Self::Context => "context",
            Self::Tools => "tools",
            Self::Mcp(_) => "mcp",
            Self::Jobs => "jobs",
            Self::Agents => "agents",
            Self::Todo(_) => "todo",
            Self::Pin(_) => "pin",
            Self::Unpin(_) => "unpin",
            Self::Delete(_) => "delete",
            Self::Rename(_) => "rename",
            Self::Retry => "retry",
            Self::Copy => "copy",
            Self::Hotkeys => "hotkeys",
            Self::Goal(_) => "goal",
            Self::Modelpreset(_) => "modelpreset",
            Self::Theme(_) => "theme",
            Self::Queue(_) => "queue",
            Self::Loop(_) => "loop",
            Self::Wt(_) => "wt",
            Self::Memory(_) => "memory",
            Self::Init => "init",
            Self::Trace(_) => "trace",
            Self::Dump(_) => "dump",
            Self::Commit(_) => "commit",
            Self::Git(_) => "git",
            Self::Remote(_) => "remote",
            Self::Review(_) => "review",
            Self::Vibe(_) => "vibe",
            Self::Agy(_) => "agy",
            Self::Help => "help",
            Self::Quit => "quit",
        }
    }
}

/// Split `/name [args]` into `(name, trimmed_args)`, or `None` for non-commands / `//escaped`.
pub(crate) fn split_command(input: &str) -> Option<(&str, &str)> {
    let rest = input.trim().strip_prefix('/')?;
    if rest.starts_with('/') {
        return None;
    }
    Some(
        rest.split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, arg)| (name, arg.trim())),
    )
}

/// Parse a `/command [arg]` line; `None` when the input is not a command. `//text` escapes a
/// literal leading slash and is not a command either.
pub(crate) fn parse(input: &str) -> Option<Result<Command, String>> {
    let (raw_name, raw_arg) = split_command(input)?;
    let canonical = find_spec(raw_name).map_or(raw_name, |spec| spec.name.as_str());
    let arg = (!raw_arg.is_empty()).then(|| raw_arg.to_owned());
    Some(match canonical {
        "login" => Ok(Command::Login(arg)),
        "logout" => Ok(Command::Logout(arg)),
        "setup" => Ok(Command::Setup),
        "auth" => Ok(Command::Auth),
        "model" => Ok(Command::Model(arg)),
        "switch" => Ok(Command::Switch(arg)),
        "think" => Ok(Command::Think(arg)),
        "fast" => Ok(Command::Fast(arg)),
        "slow" => Ok(Command::Slow(arg)),
        "extended-context" => Ok(Command::ExtendedContext(arg)),
        "mode" => Ok(Command::Mode(arg)),
        "forget-approvals" => Ok(Command::ForgetApprovals),
        "plan" => Ok(Command::Plan(arg)),
        "advisor" => Ok(Command::Advisor(arg)),
        "attach" => Ok(Command::Attach(arg)),
        "export" => Ok(Command::Export(arg)),
        "share" => Ok(Command::Share(arg)),
        "collab" => Ok(Command::Collab(arg)),
        "join" => Ok(Command::Join(arg)),
        "leave" => Ok(Command::Leave),
        "image" => Ok(Command::Image(arg)),
        "tree" => Ok(Command::Tree),
        "branch" => Ok(Command::Branch),
        "fork" => Ok(Command::Fork(arg)),
        "new" => Ok(Command::New),
        "sessions" => Ok(Command::Sessions),
        "compact" => Ok(Command::Compact(arg)),
        "shake" => Ok(Command::Shake),
        "handoff" => Ok(Command::Handoff(arg)),
        "btw" => Ok(Command::Btw(arg)),
        "stats" => Ok(Command::Stats),
        "usage" => Ok(Command::Usage),
        "settings" => Ok(Command::Settings),
        "context" => Ok(Command::Context),
        "tools" => Ok(Command::Tools),
        "mcp" => Ok(Command::Mcp(arg)),
        "jobs" => Ok(Command::Jobs),
        "agents" => Ok(Command::Agents),
        "todo" => Ok(Command::Todo(arg)),
        "pin" => Ok(Command::Pin(arg)),
        "unpin" => Ok(Command::Unpin(arg)),
        "delete" => Ok(Command::Delete(arg)),
        "rename" => Ok(Command::Rename(arg)),
        "retry" => Ok(Command::Retry),
        "copy" => Ok(Command::Copy),
        "hotkeys" => Ok(Command::Hotkeys),
        "goal" => Ok(Command::Goal(arg)),
        "modelpreset" => Ok(Command::Modelpreset(arg)),
        "theme" => Ok(Command::Theme(arg)),
        "queue" => Ok(Command::Queue(arg)),
        "loop" => Ok(Command::Loop(arg)),
        "wt" => Ok(Command::Wt(arg)),
        "memory" => Ok(Command::Memory(arg)),
        "init" => Ok(Command::Init),
        "trace" => Ok(Command::Trace(arg)),
        "dump" => Ok(Command::Dump(arg)),
        "commit" => Ok(Command::Commit(arg)),
        "git" => Ok(Command::Git(arg)),
        "remote" => Ok(Command::Remote(arg)),
        "review" => Ok(Command::Review(arg)),
        "vibe" => Ok(Command::Vibe(arg)),
        "agy" => Ok(Command::Agy(arg)),
        "help" => Ok(Command::Help),
        "quit" => Ok(Command::Quit),
        other => Err(format!("unknown command /{other} · /help lists them")),
    })
}

#[derive(Clone)]
pub(crate) struct Choice {
    /// The input after accepting this choice.
    pub(crate) text: String,
    pub(crate) label: String,
    pub(crate) help: Cow<'static, str>,
    /// Accepting it leaves a complete command, so Enter runs it right away.
    pub(crate) complete: bool,
}

fn command_choice(token: &str, spec: &'static Spec, loop_help: Option<&str>) -> Choice {
    let help = if spec.name == "loop"
        && let Some(dynamic) = loop_help
    {
        Cow::Owned(dynamic.to_owned())
    } else {
        Cow::Borrowed(spec.help.as_str())
    };
    Choice {
        text: if spec.args.is_empty() {
            format!("/{token}")
        } else {
            format!("/{token} ")
        },
        label: format!("/{token} {}", spec.args).trim_end().to_owned(),
        help,
        complete: spec.args.is_empty(),
    }
}

fn complete_names(
    rest: &str,
    custom: &[CustomCommandInfo],
    loop_help: Option<&str>,
) -> Vec<Choice> {
    let all = specs();
    let mut out: Vec<Choice> = all
        .iter()
        .filter(|spec| spec.name.starts_with(rest))
        .map(|spec| command_choice(&spec.name, spec, loop_help))
        .collect();
    if !rest.is_empty() {
        for spec in all {
            for alias in &spec.aliases {
                if alias.starts_with(rest) {
                    out.push(command_choice(alias, spec, loop_help));
                }
            }
        }
    }
    let needle = rest.to_lowercase();
    for cmd in custom {
        if cmd.name.starts_with(&needle) && !all.iter().any(|spec| spec.matches(&cmd.name)) {
            out.push(Choice {
                text: format!("/{}", cmd.name),
                label: format!("/{}", cmd.name),
                help: Cow::Owned(cmd.description.clone()),
                complete: true,
            });
        }
    }
    out
}

fn filter_models<'a>(models: &'a [String], query: &str) -> Vec<&'a str> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    if tokens.is_empty() {
        return models.iter().map(String::as_str).collect();
    }
    let mut scored: Vec<(u32, usize, &'a str)> = models
        .iter()
        .enumerate()
        .filter_map(|(idx, model)| {
            let lower = model.to_ascii_lowercase();
            let short = lower.rsplit('/').next().unwrap_or(&lower);
            let mut score = 0_u32;
            for tok in &tokens {
                if short == tok || lower == *tok {
                    score += 120;
                } else if short.starts_with(tok) || lower.starts_with(tok) {
                    score += 90;
                } else if lower
                    .split(['/', '-', '_', ':', '.'])
                    .any(|seg| seg.starts_with(tok))
                {
                    score += 75;
                } else if short.contains(tok) {
                    score += 55;
                } else if lower.contains(tok) {
                    score += 35;
                } else {
                    return None;
                }
            }
            Some((score, idx, model.as_str()))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, m)| m).collect()
}

fn complete_args(typed_name: &str, arg: &str, models: &[String]) -> Vec<Choice> {
    let Some(spec) = find_spec(typed_name) else {
        return Vec::new();
    };
    let canonical = spec.name.as_str();
    let arg = arg.trim_start();
    if canonical == "advisor"
        && let Some(model_prefix) = arg.strip_prefix("model ")
    {
        let needle = model_prefix.trim_start();
        return filter_models(models, needle)
            .into_iter()
            .map(|m| Choice {
                text: format!("/{typed_name} model {m}"),
                label: m.to_owned(),
                help: Cow::Borrowed("advisor model"),
                complete: true,
            })
            .collect();
    }
    if matches!(canonical, "model" | "switch") {
        return filter_models(models, arg)
            .into_iter()
            .map(|m| Choice {
                text: format!("/{typed_name} {m}"),
                label: m.to_owned(),
                help: Cow::Borrowed(spec.hint.as_str()),
                complete: true,
            })
            .collect();
    }
    if canonical == "theme" {
        if let Some((theme_name, rest)) = arg.split_once(char::is_whitespace) {
            let needle = rest.trim_start();
            return if "save".starts_with(needle) {
                vec![Choice {
                    text: format!("/{typed_name} {theme_name} save"),
                    label: "save".to_owned(),
                    help: Cow::Borrowed("persist theme to config"),
                    complete: true,
                }]
            } else {
                Vec::new()
            };
        }
        return crate::theme::available_theme_names()
            .into_iter()
            .filter(|name| name.starts_with(arg))
            .map(|name| Choice {
                text: format!("/{typed_name} {name}"),
                label: name,
                help: Cow::Borrowed(spec.hint.as_str()),
                complete: true,
            })
            .collect();
    }
    let options: Vec<&str> = match canonical {
        "think" => THINKING.into_iter().map(ThinkingLevel::as_str).collect(),
        "mode" => ALL_MODES.into_iter().map(PermissionMode::as_str).collect(),
        _ => spec.subcommands.iter().map(String::as_str).collect(),
    };
    options
        .into_iter()
        .filter(|option| option.starts_with(arg))
        .map(|option| {
            let needs_arg = (canonical == "advisor" && option == "model")
                || (canonical == "modelpreset" && matches!(option, "save" | "switch" | "delete"))
                || (canonical == "memory" && option == "search")
                || (canonical == "trace" && option == "export");
            Choice {
                text: if needs_arg {
                    format!("/{typed_name} {option} ")
                } else {
                    format!("/{typed_name} {option}")
                },
                label: option.to_owned(),
                help: Cow::Borrowed(spec.hint.as_str()),
                complete: !needs_arg,
            }
        })
        .collect()
}

/// Completions for a partly typed command (built-in or custom) or its argument, optionally
/// substituting a live description for `/loop` (`Loop: off | paused | on (...)`).
pub(crate) fn complete_with_loop(
    input: &str,
    models: &[String],
    custom: &[CustomCommandInfo],
    loop_help: Option<&str>,
) -> Vec<Choice> {
    let Some(rest) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if input.contains('\n') || rest.starts_with('/') {
        return Vec::new();
    }
    match rest.split_once(' ') {
        None => complete_names(rest, custom, loop_help),
        Some((name, arg)) => complete_args(name, arg, models),
    }
}

/// Build the structured P0-P3 code-review prompt for `/review [staged|base|<focus>]`.
pub(crate) fn review_prompt(arg: Option<&str>) -> String {
    let target = arg.map(str::trim).filter(|s| !s.is_empty());
    let scope = match target {
        Some(t) if t.eq_ignore_ascii_case("staged") => {
            "Inspect staged changes (`git diff --cached` and `git status --short`).".to_owned()
        }
        Some(t) if t.eq_ignore_ascii_case("base") => {
            "Inspect all commits since the merge-base with the default branch (`git diff main...HEAD` \
             or `master...HEAD`) plus any uncommitted working-tree changes."
                .to_owned()
        }
        Some(focus) => format!(
            "Inspect current changes (`git status --short` and `git diff`, or `git show HEAD` if \
             the tree is clean) with explicit focus on: {focus}."
        ),
        None => "Inspect current changes (`git status --short`, `git diff`, and `git diff --cached`; \
                 if the working tree is clean, inspect `git show HEAD`)."
            .to_owned(),
    };
    format!(
        "Perform a rigorous engineering code review. {scope}\n\
         Read surrounding code before judging; do not guess.\n\
         Report ONLY real, actionable issues grouped by severity:\n\
         - **P0 (Blocker):** data loss, security vulnerability, broken auth/isolation, crash/UB.\n\
         - **P1 (High):** correctness bug, race condition, unhandled failure path, regression.\n\
         - **P2 (Medium):** edge case, performance hazard, resource leak, fragile API contract.\n\
         - **P3 (Low):** minor clarity or test-coverage gap worth fixing.\n\
         For every finding include `file:line`, why it breaks, and a concrete fix. \
         Skip formatting/style nitpicks. End with a one-line verdict (`SHIP` or `FIX BEFORE MERGE`)."
    )
}
