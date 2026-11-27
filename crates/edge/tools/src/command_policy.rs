//! Pure command-line approval policy engine backing `[shell] patterns`, compound-chain gating,
//! leading `KEY=VAL` environment prefix validation, and operator-no-loosen merging.

use hrn_kernel::serde::{Deserialize, Serialize};

#[path = "command_policy_parse.rs"]
mod parse;

pub use parse::{
    CommandSegment, SegmentSep, collapse_whitespace, extract_subshell_commands,
    has_unquoted_output_redirection, has_unquoted_subshell, is_excluded_compound_builtin,
    normalize_command_word, normalize_segment_words, pattern_matches, peel_env_prefixes,
    shell_words, skip_command_wrappers, split_segments, strip_bin_prefix, unwrap_segment_command,
};

/// Action taken when a `[shell] patterns` rule matches a shell segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "hrn_kernel::serde", rename_all = "snake_case")]
pub enum CommandApproval {
    /// Permit the segment without prompting (subject to env-var & compound-chain rules).
    Allow,
    /// Require an interactive prompt for this segment.
    Prompt,
    /// Refuse the command immediately, even in `yolo` mode.
    Deny,
}

/// One `[shell] patterns` entry (`{ match, approval, env }`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
pub struct CommandPattern {
    /// Glob pattern (`*`, `?`) matched against whitespace-collapsed shell segments.
    #[serde(rename = "match")]
    pub pattern: String,
    /// Approval decision when this rule is the first match for a segment.
    pub approval: CommandApproval,
    /// Additional leading `KEY=VAL` environment variable names permitted for this rule.
    #[serde(default)]
    pub env: Vec<String>,
}

/// Complete `[shell]` command-policy configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "hrn_kernel::serde")]
pub struct CommandPolicy {
    /// Ordered command approval rules; first match wins per segment.
    #[serde(default)]
    pub patterns: Vec<CommandPattern>,
    /// Allow multi-segment `&&` chains to auto-approve when every segment is allowed and free of
    /// `cd`, `source`, `.`, `eval`, and `exec`.
    #[serde(default)]
    pub allow_compound_commands: bool,
    /// Global allowlist of leading `KEY=VAL` environment variable names.
    #[serde(default)]
    pub allowed_env_vars: Vec<String>,
}

/// Aggregate verdict returned by [`evaluate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandVerdict {
    /// Command is explicitly approved by policy.
    Allow,
    /// Command requires interactive prompting with an explanation.
    Prompt {
        /// Human-readable reason shown in the approval prompt.
        reason: String,
    },
    /// Command is unconditionally denied (beats `yolo`).
    Deny {
        /// Human-readable reason returned to the model.
        reason: String,
    },
    /// No policy rule decided this command; fall through to the normal mode tier.
    Default,
}

/// Merge operator-layer (global/overlay/env/runtime) rules with project-layer rules while:
/// 1. Preserving exact operator rule order (so an operator `allow` exception ahead of an operator
///    `prompt`/`deny` rule keeps its intended precedence).
/// 2. Allowing project `deny` rules to tighten operator `allow`/`prompt` rules.
/// 3. Guaranteeing project rules can NEVER loosen any operator `deny` or `prompt` rule (including
///    via broader wildcard patterns).
#[must_use]
pub fn merge_patterns(
    operator: &[CommandPattern],
    project: &[CommandPattern],
) -> Vec<CommandPattern> {
    let mut out = Vec::with_capacity(operator.len() + project.len());
    // 1. Project Deny rules may tighten operator allow/prompt rules without ever loosening denies.
    for rule in project
        .iter()
        .filter(|r| r.approval == CommandApproval::Deny)
    {
        push_unique(&mut out, rule);
    }
    // 2. All operator rules in exact operator order (preserves `allow "git status"` before
    //    `prompt "git *"` while keeping every operator deny/prompt ahead of project allows).
    for rule in operator {
        push_unique(&mut out, rule);
    }
    // 3. Project Prompt rules (cannot weaken operator denies because operator rules ran first).
    for rule in project
        .iter()
        .filter(|r| r.approval == CommandApproval::Prompt)
    {
        let loosens_deny = operator.iter().any(|op| {
            op.approval == CommandApproval::Deny
                && collapse_whitespace(&op.pattern) == collapse_whitespace(&rule.pattern)
        });
        if !loosens_deny {
            push_unique(&mut out, rule);
        }
    }
    // 4. Project Allow rules last (cannot bypass any operator deny or prompt).
    for rule in project
        .iter()
        .filter(|r| r.approval == CommandApproval::Allow)
    {
        let loosens_same = operator.iter().any(|op| {
            matches!(op.approval, CommandApproval::Deny | CommandApproval::Prompt)
                && collapse_whitespace(&op.pattern) == collapse_whitespace(&rule.pattern)
        });
        if !loosens_same {
            push_unique(&mut out, rule);
        }
    }
    out
}

fn push_unique(out: &mut Vec<CommandPattern>, rule: &CommandPattern) {
    if !out.contains(rule) {
        out.push(rule.clone());
    }
}

/// Evaluate `command` against `policy`.
///
/// Precedence across segments: `Deny` beats `Prompt` beats `Allow` beats `Default`.
/// Within each segment, the first matching rule in `policy.patterns` wins.
#[must_use]
pub fn evaluate(policy: &CommandPolicy, command: &str) -> CommandVerdict {
    evaluate_with_depth(policy, command, 0)
}

const MAX_SUBSHELL_DEPTH: usize = 8;

fn evaluate_with_depth(policy: &CommandPolicy, command: &str, depth: usize) -> CommandVerdict {
    let segments = split_segments(command);
    if segments.is_empty() {
        return CommandVerdict::Default;
    }

    let mut first_prompt: Option<String> = None;
    let mut all_allowed = true;
    let mut compound_ok = policy.allow_compound_commands;

    // Extract whole-command subshells / command substitutions / `{ ... ; }` groups BEFORE
    // segment splitting so multi-statement bodies like `(rm -rf /; echo ok)` or `{ rm -rf /; }`
    // are evaluated intact.
    if depth < MAX_SUBSHELL_DEPTH {
        for sub in extract_subshell_commands(command) {
            if sub == command.trim() {
                continue;
            }
            match evaluate_with_depth(policy, &sub, depth + 1) {
                CommandVerdict::Deny { reason } => return CommandVerdict::Deny { reason },
                CommandVerdict::Prompt { reason } => {
                    all_allowed = false;
                    if first_prompt.is_none() {
                        first_prompt = Some(reason);
                    }
                }
                CommandVerdict::Allow => {}
                CommandVerdict::Default => {
                    all_allowed = false;
                }
            }
        }
    }

    for (idx, seg) in segments.iter().enumerate() {
        let (env_vars, bare) = peel_env_prefixes(&seg.text);
        if idx > 0 && seg.sep_before != Some(SegmentSep::And) {
            compound_ok = false;
        }
        if segments.len() > 1
            && (has_unquoted_subshell(&seg.text) || is_excluded_compound_builtin(&bare))
        {
            compound_ok = false;
        }
        match eval_segment(policy, &seg.text, &env_vars, &bare, depth) {
            CommandVerdict::Deny { reason } => return CommandVerdict::Deny { reason },
            CommandVerdict::Prompt { reason } => {
                all_allowed = false;
                if first_prompt.is_none() {
                    first_prompt = Some(reason);
                }
            }
            CommandVerdict::Allow => {}
            CommandVerdict::Default => {
                all_allowed = false;
            }
        }
    }

    if let Some(reason) = first_prompt {
        return CommandVerdict::Prompt { reason };
    }
    if all_allowed && (segments.len() == 1 || compound_ok) {
        return CommandVerdict::Allow;
    }
    CommandVerdict::Default
}

fn eval_segment(
    policy: &CommandPolicy,
    full_segment: &str,
    env_vars: &[String],
    bare: &str,
    depth: usize,
) -> CommandVerdict {
    let mut verdict = eval_direct_segment(policy, full_segment, env_vars, bare);
    if matches!(verdict, CommandVerdict::Deny { .. }) {
        return verdict;
    }
    // Check unwrapped inner command (e.g. `env rm -rf /`, `command git 'push'`, `sudo 'rm' -rf /`)
    // so an outer `allow "env *"` / `allow "command *"` can NEVER bypass an inner Deny or Prompt.
    if let Some(unwrapped) = unwrap_segment_command(bare) {
        let inner = eval_direct_segment(policy, &unwrapped, &[], &unwrapped);
        verdict = tighten_verdict(verdict, inner);
        if matches!(verdict, CommandVerdict::Deny { .. }) {
            return verdict;
        }
    }
    if depth < MAX_SUBSHELL_DEPTH {
        for sub in extract_subshell_commands(full_segment) {
            if sub == full_segment.trim() {
                continue;
            }
            let sub_verdict = evaluate_with_depth(policy, &sub, depth + 1);
            verdict = tighten_verdict(verdict, sub_verdict);
            if matches!(verdict, CommandVerdict::Deny { .. }) {
                return verdict;
            }
        }
    }
    verdict
}

fn eval_direct_segment(
    policy: &CommandPolicy,
    full_segment: &str,
    env_vars: &[String],
    bare: &str,
) -> CommandVerdict {
    let norm_word = normalize_command_word(bare);
    let norm_all = normalize_segment_words(bare);
    let Some(rule) = policy.patterns.iter().find(|rule| match rule.approval {
        CommandApproval::Allow => pattern_matches(&rule.pattern, bare),
        CommandApproval::Deny | CommandApproval::Prompt => {
            pattern_matches(&rule.pattern, bare)
                || pattern_matches(&rule.pattern, &norm_word)
                || pattern_matches(&rule.pattern, &norm_all)
        }
    }) else {
        return CommandVerdict::Default;
    };
    match rule.approval {
        CommandApproval::Deny => CommandVerdict::Deny {
            reason: format!("blocked by [shell] pattern `{}`", rule.pattern),
        },
        CommandApproval::Prompt => CommandVerdict::Prompt {
            reason: format!("matched [shell] prompt pattern `{}`", rule.pattern),
        },
        CommandApproval::Allow => {
            if has_unquoted_subshell(full_segment) {
                return CommandVerdict::Prompt {
                    reason:
                        "command contains subshell, command substitution, or process substitution"
                            .to_owned(),
                };
            }
            if has_unquoted_output_redirection(full_segment) && !rule.pattern.contains('>') {
                return CommandVerdict::Prompt {
                    reason: "command contains output redirection not allowed by matching pattern"
                        .to_owned(),
                };
            }
            for key in env_vars {
                let allowed = policy.allowed_env_vars.iter().any(|v| v == key)
                    || rule.env.iter().any(|v| v == key);
                if !allowed {
                    return CommandVerdict::Prompt {
                        reason: format!(
                            "environment variable `{key}` is not in allowed_env_vars or rule env list"
                        ),
                    };
                }
            }
            CommandVerdict::Allow
        }
    }
}

/// Inner/unwrapped matches may ONLY tighten (`Deny` > `Prompt`) and never auto-allow an outer call.
fn tighten_verdict(outer: CommandVerdict, inner: CommandVerdict) -> CommandVerdict {
    match inner {
        CommandVerdict::Deny { reason } => CommandVerdict::Deny { reason },
        CommandVerdict::Prompt { reason } => match outer {
            CommandVerdict::Deny { .. } | CommandVerdict::Prompt { .. } => outer,
            CommandVerdict::Allow | CommandVerdict::Default => CommandVerdict::Prompt { reason },
        },
        CommandVerdict::Allow | CommandVerdict::Default => outer,
    }
}

#[cfg(test)]
#[path = "command_policy_tests.rs"]
mod tests;
