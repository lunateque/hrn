//! Approval resolution ladder, restricted-folder checks, path/selector normalization, and
//! session + persistent grant caching (`approval_grants`).

use hrn_kernel::{
    ApprovalGrantScope, PermissionMode, ToolApproval, ToolApprovalPolicy, ToolTier, WorkspaceFs,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// User-configured approval overrides (`[tools] restricted_folders` and `[tools.approval]`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserApprovalPolicy {
    /// Directories where any matching tool call forces a plain Approve/Deny prompt (no persistent
    /// remember, even in `yolo`; headless runs fail closed).
    #[serde(default)]
    pub restricted_folders: Vec<String>,
    /// Per-tool overrides (`[tools.approval] <tool> = "allow" | "deny" | "prompt"`).
    /// Supports MCP qualified names (`mcp__<server>_<tool>`).
    #[serde(default)]
    pub tool_policies: BTreeMap<String, ToolApprovalPolicy>,
}

impl UserApprovalPolicy {
    /// Look up an explicit user policy for `tool_name`.
    #[must_use]
    pub fn policy_for(&self, tool_name: &str) -> Option<ToolApprovalPolicy> {
        self.tool_policies.get(tool_name).copied()
    }
}

/// Outcome of the 5-step approval resolution ladder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalResolution {
    /// Execute without asking the user.
    Allow,
    /// Refuse immediately with a model-facing reason.
    Deny {
        /// Human-readable reason for refusal.
        reason: String,
    },
    /// Prompt the user interactively.
    Prompt {
        /// Optional explanation shown in the approval dock.
        reason: Option<String>,
        /// Permitted grant scopes (`[Once]` = plain Approve/Deny only).
        choices: Vec<ApprovalGrantScope>,
        /// `true` when forced by a restricted folder (never cached or remembered).
        restricted: bool,
    },
}

/// Tier dominance: `Write` covers `Read`; `Read` NEVER covers `Write` or `Exec`.
#[must_use]
pub const fn tier_covers(granted: ToolTier, needed: ToolTier) -> bool {
    granted.covers(needed)
}

/// Permitted interactive grant scopes for `mode` and `targets`.
#[must_use]
pub fn mode_choices(
    mode: PermissionMode,
    tool_name: &str,
    targets: &[String],
) -> Vec<ApprovalGrantScope> {
    match mode {
        PermissionMode::AskCached => {
            if !targets.is_empty() && tool_name != "bash" {
                vec![
                    ApprovalGrantScope::Once,
                    ApprovalGrantScope::File,
                    ApprovalGrantScope::Directory,
                    ApprovalGrantScope::ToolSession,
                    ApprovalGrantScope::Persistent,
                ]
            } else {
                vec![
                    ApprovalGrantScope::Once,
                    ApprovalGrantScope::ToolSession,
                    ApprovalGrantScope::Persistent,
                ]
            }
        }
        PermissionMode::Default
        | PermissionMode::AutoEdit
        | PermissionMode::Yolo
        | PermissionMode::Plan => vec![ApprovalGrantScope::Once, ApprovalGrantScope::ToolSession],
    }
}

/// Run the 5-step approval resolution ladder:
/// 1. Tool `Deny` / user `[tools.approval] = deny` / tool `force_prompt`
/// 2. `[tools] restricted_folders` (plain Approve/Deny only, beats `yolo`)
/// 3. `[tools.approval] <tool> = allow | deny | prompt` and tool-level policy overrides
/// 4. Mode tier (`always-ask`, `always-ask-cached`, `write`, `yolo`, `plan`) + grant cache
/// 5. No UI (`!ui_available`) -> fail closed (`Deny`)
#[expect(
    clippy::too_many_arguments,
    reason = "explicit pure inputs for deterministic approval ladder evaluation"
)]
#[must_use]
pub fn resolve(
    tool_name: &str,
    tool_approval: &ToolApproval,
    user_policy: &UserApprovalPolicy,
    mode: PermissionMode,
    targets: &[String],
    cwd: &Path,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
    ui_available: bool,
    grants: &GrantCache,
) -> ApprovalResolution {
    resolve_for_session(
        "",
        tool_name,
        tool_approval,
        user_policy,
        mode,
        targets,
        cwd,
        home,
        fs,
        ui_available,
        grants,
    )
}

/// Session-scoped 5-step approval resolution ladder (used by the runtime so session grants never
/// leak across sessions).
#[expect(
    clippy::too_many_arguments,
    reason = "explicit pure inputs for deterministic approval ladder evaluation"
)]
#[must_use]
pub fn resolve_for_session(
    session_id: &str,
    tool_name: &str,
    tool_approval: &ToolApproval,
    user_policy: &UserApprovalPolicy,
    mode: PermissionMode,
    targets: &[String],
    cwd: &Path,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
    ui_available: bool,
    grants: &GrantCache,
) -> ApprovalResolution {
    let call_targets = CallTargets {
        grant: targets.to_vec(),
        restricted: targets.to_vec(),
        ..CallTargets::default()
    };
    resolve_for_call(
        session_id,
        tool_name,
        tool_approval,
        user_policy,
        mode,
        &call_targets,
        cwd,
        home,
        fs,
        ui_available,
        grants,
    )
}

/// Session-scoped 5-step approval ladder using rich [`CallTargets`] (including `bash` `cd`
/// ancestor-overlap and unbounded `cd` checks).
#[expect(
    clippy::too_many_arguments,
    reason = "explicit pure inputs for deterministic approval ladder evaluation"
)]
#[must_use]
pub(crate) fn resolve_for_call(
    session_id: &str,
    tool_name: &str,
    tool_approval: &ToolApproval,
    user_policy: &UserApprovalPolicy,
    mode: PermissionMode,
    call_targets: &CallTargets,
    cwd: &Path,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
    ui_available: bool,
    grants: &GrantCache,
) -> ApprovalResolution {
    // Step 1: Tool Deny, explicit user Deny (beats yolo & restricted prompt), or force_prompt.
    if tool_approval.policy == Some(ToolApprovalPolicy::Deny) {
        let reason = tool_approval
            .reason
            .clone()
            .unwrap_or_else(|| format!("blocked by tool approval policy for `{tool_name}`"));
        return ApprovalResolution::Deny { reason };
    }
    if user_policy.policy_for(tool_name) == Some(ToolApprovalPolicy::Deny) {
        return ApprovalResolution::Deny {
            reason: format!("blocked by [tools.approval] deny policy for `{tool_name}`"),
        };
    }
    if tool_approval.force_prompt {
        return prompt_or_fail_closed(
            ui_available,
            tool_name,
            tool_approval.reason.clone(),
            vec![ApprovalGrantScope::Once],
            false,
        );
    }

    // Step 2: `[tools] restricted_folders` always forces plain Approve/Deny (even in yolo).
    if let Some(folder) = matching_restricted_folder_for_call(
        tool_name,
        &user_policy.restricted_folders,
        call_targets,
        cwd,
        home,
        fs,
    ) {
        return prompt_or_fail_closed(
            ui_available,
            tool_name,
            Some(format!("target is inside restricted folder `{folder}`")),
            vec![ApprovalGrantScope::Once],
            true,
        );
    }

    // Step 3: `[tools.approval] <tool>` and tool-level policy (`Prompt` / `Allow`).
    if let Some(res) = resolve_explicit_policy(
        tool_name,
        user_policy.policy_for(tool_name),
        tool_approval,
        ui_available,
    ) {
        return res;
    }

    // Step 4: Mode tier + session/persistent grant cache.
    let norm_targets: Vec<String> = call_targets
        .grant
        .iter()
        .map(|t| normalize_target(cwd, t, home, fs))
        .collect();
    match mode {
        PermissionMode::Plan => {
            if tool_approval.tier == ToolTier::Read {
                ApprovalResolution::Allow
            } else {
                ApprovalResolution::Deny {
                    reason: format!(
                        "blocked by `plan` mode: `{tool_name}` cannot run until the mode is switched"
                    ),
                }
            }
        }
        PermissionMode::Yolo => ApprovalResolution::Allow,
        PermissionMode::AutoEdit if tool_approval.tier != ToolTier::Exec => {
            ApprovalResolution::Allow
        }
        PermissionMode::Default if tool_approval.tier == ToolTier::Read => {
            ApprovalResolution::Allow
        }
        PermissionMode::Default | PermissionMode::AutoEdit | PermissionMode::AskCached => {
            if grants.covers_for_session(session_id, tool_name, tool_approval.tier, &norm_targets) {
                ApprovalResolution::Allow
            } else {
                prompt_or_fail_closed(
                    ui_available,
                    tool_name,
                    tool_approval.reason.clone(),
                    mode_choices(mode, tool_name, &norm_targets),
                    false,
                )
            }
        }
    }
}

/// Step 3: Evaluate `[tools.approval]` and tool-level `Prompt` / `Allow` overrides.
fn resolve_explicit_policy(
    tool_name: &str,
    user_override: Option<ToolApprovalPolicy>,
    tool_approval: &ToolApproval,
    ui_available: bool,
) -> Option<ApprovalResolution> {
    match user_override {
        Some(ToolApprovalPolicy::Prompt) => {
            return Some(prompt_or_fail_closed(
                ui_available,
                tool_name,
                Some(format!(
                    "`{tool_name}` requires confirmation per [tools.approval]"
                )),
                vec![ApprovalGrantScope::Once],
                false,
            ));
        }
        Some(ToolApprovalPolicy::Allow) => return Some(ApprovalResolution::Allow),
        Some(ToolApprovalPolicy::Deny) => unreachable!("handled in step 1"),
        None => {}
    }
    match tool_approval.policy {
        Some(ToolApprovalPolicy::Prompt) => Some(prompt_or_fail_closed(
            ui_available,
            tool_name,
            tool_approval.reason.clone(),
            vec![ApprovalGrantScope::Once],
            false,
        )),
        Some(ToolApprovalPolicy::Allow) => Some(ApprovalResolution::Allow),
        Some(ToolApprovalPolicy::Deny) => unreachable!("handled in step 1"),
        None => None,
    }
}

/// Step 5: Fail closed when no interactive UI / approval channel is attached.
fn prompt_or_fail_closed(
    ui_available: bool,
    tool_name: &str,
    reason: Option<String>,
    choices: Vec<ApprovalGrantScope>,
    restricted: bool,
) -> ApprovalResolution {
    if !ui_available {
        let why = reason
            .as_deref()
            .map_or_else(String::new, |r| format!(" ({r})"));
        return ApprovalResolution::Deny {
            reason: format!(
                "`{tool_name}` requires user approval{why} but no approval channel is attached \
                 (headless run); switch to a permission mode that allows it"
            ),
        };
    }
    ApprovalResolution::Prompt {
        reason,
        choices,
        restricted,
    }
}

#[cfg(test)]
#[path = "approvals_tests.rs"]
mod tests;

// ===== merged from approvals_bash_lex.rs =====
// Quote-aware segment splitting and operator-splitting tokenization (`<`, `>`, `(`, `)`, `` ` ``,
// `'...'`, `$'...'` with `\xHH`, `"..."`, and `\` escapes) for `bash` path inspection.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    AnsiSingle,
    Double,
}

/// Split a shell script into segments on unquoted `;`, `|`, `||`, `&`, `&&`, and `\n` while
/// ignoring `#` comments at word start and preserving `'...'`, `$'...'`, and `"..."`.
pub(super) fn split_bash_segments(script: &str) -> Vec<String> {
    let chars: Vec<char> = script.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = Quote::None;
    let mut at_word_start = true;
    let mut i = 0usize;
    while let Some(&ch) = chars.get(i) {
        if quote != Quote::None {
            i = step_quoted_segment(&chars, i, &mut cur, &mut quote);
            at_word_start = false;
            continue;
        }
        if ch == '#' && at_word_start {
            while chars.get(i).is_some_and(|&c| c != '\n') {
                i += 1;
            }
            continue;
        }
        if ch == '\\' {
            cur.push(ch);
            if let Some(&esc) = chars.get(i + 1) {
                cur.push(esc);
                i += 2;
            } else {
                i += 1;
            }
            at_word_start = false;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            quote = Quote::AnsiSingle;
            cur.push_str("$'");
            at_word_start = false;
            i += 2;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = if ch == '\'' {
                Quote::Single
            } else {
                Quote::Double
            };
            cur.push(ch);
            at_word_start = false;
            i += 1;
            continue;
        }
        if is_segment_break(&chars, i, &cur) {
            push_trimmed(&mut out, &mut cur);
            at_word_start = true;
            i += if matches!(ch, '&' | '|') && chars.get(i + 1) == Some(&ch) {
                2
            } else {
                1
            };
            continue;
        }
        at_word_start = ch.is_ascii_whitespace();
        cur.push(ch);
        i += 1;
    }
    push_trimmed(&mut out, &mut cur);
    out
}

fn step_quoted_segment(chars: &[char], i: usize, cur: &mut String, quote: &mut Quote) -> usize {
    let Some(&ch) = chars.get(i) else {
        return i + 1;
    };
    cur.push(ch);
    match *quote {
        Quote::Single => {
            if ch == '\'' {
                *quote = Quote::None;
            }
            i + 1
        }
        Quote::AnsiSingle | Quote::Double => {
            if ch == '\\'
                && let Some(&esc) = chars.get(i + 1)
            {
                cur.push(esc);
                return i + 2;
            }
            if (*quote == Quote::AnsiSingle && ch == '\'') || (*quote == Quote::Double && ch == '"')
            {
                *quote = Quote::None;
            }
            i + 1
        }
        Quote::None => i + 1,
    }
}

fn is_segment_break(chars: &[char], i: usize, cur: &str) -> bool {
    let Some(&ch) = chars.get(i) else {
        return false;
    };
    matches!(ch, ';' | '|' | '\n')
        || (ch == '&'
            && chars.get(i + 1) != Some(&'>')
            && !cur.ends_with('>')
            && !cur.ends_with('<'))
}

fn push_trimmed(out: &mut Vec<String>, cur: &mut String) {
    let trimmed = cur.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_owned());
    }
    cur.clear();
}

/// Tokenize one bash segment, splitting on whitespace and shell operators `<`, `>`, `(`, `)`, `` ` ``
/// while decoding `'...'`, `$'...'` (including `\xHH`), `"..."`, and `\` escapes.
pub(super) fn tokenize_bash_segment(segment: &str) -> Vec<String> {
    let chars: Vec<char> = segment.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = Quote::None;
    let mut i = 0usize;
    while let Some(&ch) = chars.get(i) {
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    cur.push(ch);
                }
                i += 1;
            }
            Quote::AnsiSingle => {
                if ch == '\\' {
                    i = decode_ansi_escape(&chars, i, &mut cur);
                } else if ch == '\'' {
                    quote = Quote::None;
                    i += 1;
                } else {
                    cur.push(ch);
                    i += 1;
                }
            }
            Quote::Double => {
                if ch == '\\'
                    && let Some(&esc) = chars.get(i + 1)
                    && matches!(esc, '"' | '\\' | '$' | '`' | '\n')
                {
                    cur.push(esc);
                    i += 2;
                } else if ch == '"' {
                    quote = Quote::None;
                    i += 1;
                } else {
                    cur.push(ch);
                    i += 1;
                }
            }
            Quote::None => {
                if ch == '$' && chars.get(i + 1) == Some(&'\'') {
                    quote = Quote::AnsiSingle;
                    i += 2;
                } else if ch == '\'' {
                    quote = Quote::Single;
                    i += 1;
                } else if ch == '"' {
                    quote = Quote::Double;
                    i += 1;
                } else if ch == '\\' {
                    if has_drive_prefix(&cur) {
                        cur.push('\\');
                        i += 1;
                    } else if let Some(&esc) = chars.get(i + 1) {
                        cur.push(esc);
                        i += 2;
                    } else {
                        cur.push('\\');
                        i += 1;
                    }
                } else if matches!(ch, '>' | '<') {
                    flush_tok(&mut out, &mut cur);
                    if chars.get(i + 1) == Some(&ch) {
                        out.push(if ch == '>' { ">>" } else { "<<" }.to_owned());
                        i += 2;
                    } else {
                        out.push(ch.to_string());
                        i += 1;
                    }
                } else if ch.is_ascii_whitespace()
                    || matches!(ch, '(' | ')' | '`' | ';' | '|' | '&' | '\n')
                {
                    flush_tok(&mut out, &mut cur);
                    i += 1;
                } else {
                    cur.push(ch);
                    i += 1;
                }
            }
        }
    }
    flush_tok(&mut out, &mut cur);
    out
}

fn decode_ansi_escape(chars: &[char], i: usize, cur: &mut String) -> usize {
    let Some(&esc) = chars.get(i + 1) else {
        cur.push('\\');
        return i + 1;
    };
    match esc {
        'a' => {
            cur.push('\x07');
            i + 2
        }
        'b' => {
            cur.push('\x08');
            i + 2
        }
        'e' | 'E' => {
            cur.push('\x1b');
            i + 2
        }
        'f' => {
            cur.push('\x0c');
            i + 2
        }
        'n' => {
            cur.push('\n');
            i + 2
        }
        'r' => {
            cur.push('\r');
            i + 2
        }
        't' => {
            cur.push('\t');
            i + 2
        }
        'v' => {
            cur.push('\x0b');
            i + 2
        }
        '\\' | '\'' | '"' | '?' => {
            cur.push(esc);
            i + 2
        }
        'x' => decode_hex_escape(chars, i, 2, true, cur),
        'u' => decode_hex_escape(chars, i, 4, false, cur),
        'U' => decode_hex_escape(chars, i, 8, false, cur),
        '0'..='7' => decode_octal_escape(chars, i + 1, cur),
        'c' => {
            if let Some(&ctrl) = chars.get(i + 2)
                && let Ok(byte) = u8::try_from(u32::from(ctrl) & 0x1f)
            {
                cur.push(char::from(byte));
                i + 3
            } else {
                cur.push('\\');
                cur.push('c');
                i + 2
            }
        }
        other => {
            cur.push('\\');
            cur.push(other);
            i + 2
        }
    }
}

fn decode_hex_escape(
    chars: &[char],
    slash_idx: usize,
    max_digits: usize,
    as_byte: bool,
    cur: &mut String,
) -> usize {
    let start = slash_idx + 2;
    let mut val = 0u32;
    let mut j = start;
    let limit = start + max_digits;
    while j < limit
        && let Some(&c) = chars.get(j)
        && let Some(d) = c.to_digit(16)
    {
        val = (val << 4) | d;
        j += 1;
    }
    if j == start {
        cur.push('\\');
        if let Some(&esc) = chars.get(slash_idx + 1) {
            cur.push(esc);
        }
        return start;
    }
    if as_byte {
        if let Ok(byte) = u8::try_from(val & 0xff) {
            cur.push(char::from(byte));
        }
    } else if let Some(ch) = char::from_u32(val) {
        cur.push(ch);
    }
    j
}

fn decode_octal_escape(chars: &[char], start: usize, cur: &mut String) -> usize {
    let mut val = 0u32;
    let mut j = start;
    let limit = start + 3;
    while j < limit
        && let Some(&c) = chars.get(j)
        && let Some(d) = c.to_digit(8)
    {
        val = (val << 3) | d;
        j += 1;
    }
    if let Ok(byte) = u8::try_from(val & 0xff) {
        cur.push(char::from(byte));
    }
    j
}

fn flush_tok(out: &mut Vec<String>, cur: &mut String) {
    if !cur.is_empty() {
        out.push(std::mem::take(cur));
    }
}

fn has_drive_prefix(cur: &str) -> bool {
    let b = cur.as_bytes();
    b.len() >= 2 && b.first().is_some_and(u8::is_ascii_alphabetic) && b.get(1) == Some(&b':')
}

// ===== merged from approvals_bash.rs =====
// Quote-aware `bash` path, redirection, nested sub-shell (`sh -c`, `bash -c`, `cmd /c`,
// `powershell -c`), and inline `cd` extraction for `[tools] restricted_folders` enforcement.
use serde_json::Value;
use std::path::PathBuf;

const MAX_NESTED_SHELL_DEPTH: usize = 6;

/// Populate `out` with normalized restricted targets, `cd` ancestor checks, and `next_cwd` for a
/// `bash` tool call.
pub(super) fn collect_bash_call_targets(
    args: &Value,
    cwd: &Path,
    session_bash_cwd: Option<&Path>,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
    out: &mut CallTargets,
) {
    let explicit_cwd = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(|d| resolve_against_cwd(cwd, &expand_tilde(d.trim(), home)));
    let push_norm = |vec: &mut Vec<String>, base: &Path, raw: &str| {
        let norm = normalize_target(base, raw, home, fs);
        if !norm.is_empty() && !vec.contains(&norm) {
            vec.push(norm);
        }
    };
    if let Some(dir) = args.get("cwd").and_then(Value::as_str) {
        push_norm(&mut out.restricted, cwd, dir);
    }
    push_norm(&mut out.restricted, cwd, &cwd.to_string_lossy());
    if explicit_cwd.is_none()
        && let Some(prev) = session_bash_cwd
    {
        push_norm(&mut out.restricted, cwd, &prev.to_string_lossy());
    }
    let Some(cmd) = args.get("command").and_then(Value::as_str) else {
        return;
    };
    let primary_base = explicit_cwd.as_deref().or(session_bash_cwd).unwrap_or(cwd);
    let primary = inspect_bash_command(cmd, primary_base, home);
    out.bash_next_cwd = primary.next_cwd;
    out.bash_unknown_cwd = primary.unknown_cd;
    for p in primary.resolved_paths {
        push_norm(&mut out.restricted, primary_base, &p.to_string_lossy());
    }
    for cd_dir in primary.cd_targets {
        let s = cd_dir.to_string_lossy();
        push_norm(&mut out.ancestor_check, primary_base, &s);
        push_norm(&mut out.restricted, primary_base, &s);
    }
    if explicit_cwd.is_none() && session_bash_cwd.is_some() && primary_base != cwd {
        let fallback = inspect_bash_command(cmd, cwd, home);
        out.bash_unknown_cwd |= fallback.unknown_cd;
        for p in fallback.resolved_paths {
            push_norm(&mut out.restricted, cwd, &p.to_string_lossy());
        }
        for cd_dir in fallback.cd_targets {
            let s = cd_dir.to_string_lossy();
            push_norm(&mut out.ancestor_check, cwd, &s);
            push_norm(&mut out.restricted, cwd, &s);
        }
    }
}

/// Extracted path and working-directory inspection results for one `bash` command line.
#[derive(Clone, Debug, Default)]
pub(crate) struct BashPathInspection {
    /// Concrete path candidates resolved against the evolving inline `virtual_cwd`.
    pub resolved_paths: Vec<PathBuf>,
    /// Directories targeted by `cd` / `pushd` (checked for both inside-restricted and
    /// ancestor-of-restricted overlap).
    pub cd_targets: Vec<PathBuf>,
    /// Final resulting working directory if any static `cd` / `pushd` ran in this command.
    pub next_cwd: Option<PathBuf>,
    /// `true` when `cd -`, dynamic `cd $VAR`, or bare `cd` without a known home directory prevents
    /// statically bounding the target directory.
    pub unknown_cd: bool,
}

/// Inspect a `bash` command line for path candidates, inline `cd` transitions, redirections,
/// ANSI-C `$'...'` strings, and nested `sh -c` / `bash -c` / `cmd /c` / `powershell -c` payloads.
#[must_use]
pub(crate) fn inspect_bash_command(
    command: &str,
    base_cwd: &Path,
    home: Option<&Path>,
) -> BashPathInspection {
    let mut out = BashPathInspection::default();
    let mut virtual_cwd = base_cwd.to_path_buf();
    inspect_script(command, &mut virtual_cwd, home, &mut out, 0, true);
    out
}

/// Extract raw path-like tokens from a `bash` command line (convenience helper for callers that
/// only need un-resolved path strings).
#[must_use]
pub fn extract_bash_path_candidates(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect_raw_candidates(command, &mut out, 0);
    out
}

fn collect_raw_candidates(script: &str, out: &mut Vec<String>, depth: usize) {
    for seg in split_bash_segments(script) {
        let tokens = tokenize_bash_segment(&seg);
        if depth < MAX_NESTED_SHELL_DEPTH
            && let Some(nested) = extract_nested_shell_script(&tokens)
        {
            collect_raw_candidates(&nested, out, depth + 1);
        }
        for tok in extract_segment_path_tokens(&tokens) {
            if !out.contains(&tok) {
                out.push(tok);
            }
        }
    }
}

fn inspect_script(
    script: &str,
    virtual_cwd: &mut PathBuf,
    home: Option<&Path>,
    out: &mut BashPathInspection,
    depth: usize,
    track_next_cwd: bool,
) {
    for seg in split_bash_segments(script) {
        let tokens = tokenize_bash_segment(&seg);
        if tokens.is_empty() {
            continue;
        }
        if depth < MAX_NESTED_SHELL_DEPTH
            && let Some(nested) = extract_nested_shell_script(&tokens)
        {
            let mut nested_cwd = virtual_cwd.clone();
            inspect_script(&nested, &mut nested_cwd, home, out, depth + 1, false);
        }
        if handle_cd_segment(&tokens, virtual_cwd, home, out, track_next_cwd) {
            // Still inspect any redirections or extra path flags on the same segment.
        }
        for raw in extract_segment_path_tokens(&tokens) {
            let resolved = resolve_against_cwd(virtual_cwd, &expand_tilde(&raw, home));
            push_unique_path(&mut out.resolved_paths, resolved);
        }
    }
}

fn handle_cd_segment(
    tokens: &[String],
    virtual_cwd: &mut PathBuf,
    home: Option<&Path>,
    out: &mut BashPathInspection,
    track_next_cwd: bool,
) -> bool {
    let idx = skip_segment_head(tokens);
    let Some(cmd) = tokens.get(idx).map(String::as_str) else {
        return false;
    };
    if !matches!(cmd, "cd" | "pushd") {
        return false;
    }
    let mut arg_idx = idx + 1;
    while let Some(tok) = tokens.get(arg_idx).map(String::as_str) {
        if tok == "--" {
            arg_idx += 1;
            break;
        }
        if matches!(tok, "-L" | "-P" | "-e" | "-@") {
            arg_idx += 1;
            continue;
        }
        break;
    }
    let arg = tokens
        .get(arg_idx)
        .map(String::as_str)
        .filter(|t| !matches!(*t, ">" | ">>" | "<" | "<<"));
    match arg {
        None => {
            if let Some(h) = home {
                let target = h.to_path_buf();
                virtual_cwd.clone_from(&target);
                push_unique_path(&mut out.cd_targets, target.clone());
                push_unique_path(&mut out.resolved_paths, target.clone());
                if track_next_cwd {
                    out.next_cwd = Some(target);
                }
            } else {
                out.unknown_cd = true;
            }
        }
        Some("-") => {
            out.unknown_cd = true;
        }
        Some(dir) if dir.contains('$') => {
            out.unknown_cd = true;
        }
        Some(dir) => {
            let resolved = resolve_against_cwd(virtual_cwd, &expand_tilde(dir, home));
            virtual_cwd.clone_from(&resolved);
            push_unique_path(&mut out.cd_targets, resolved.clone());
            push_unique_path(&mut out.resolved_paths, resolved.clone());
            if track_next_cwd {
                out.next_cwd = Some(resolved);
            }
        }
    }
    true
}

fn skip_segment_head(tokens: &[String]) -> usize {
    let mut i = 0usize;
    while let Some(tok) = tokens.get(i).map(String::as_str) {
        if is_env_assignment(tok) {
            i += 1;
            continue;
        }
        let base = cmd_basename(tok);
        if matches!(base, "builtin" | "nohup") {
            i += 1;
            if tokens.get(i).map(String::as_str) == Some("--") {
                i += 1;
            }
            continue;
        }
        if matches!(base, "command" | "time" | "env" | "sudo" | "exec") {
            i += 1;
            while let Some(flag) = tokens.get(i).map(String::as_str) {
                if flag == "--" {
                    i += 1;
                    break;
                }
                if matches!(flag, "-u" | "-g" | "-C" | "-a" | "--unset" | "--chdir") {
                    i += 2;
                } else if (flag.starts_with('-') && flag.len() > 1) || is_env_assignment(flag) {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }
        break;
    }
    i
}

fn extract_nested_shell_script(tokens: &[String]) -> Option<String> {
    let idx = skip_segment_head(tokens);
    let cmd = cmd_basename(tokens.get(idx)?.as_str()).to_ascii_lowercase();
    let rest = tokens.get(idx + 1..)?;
    if matches!(cmd.as_str(), "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash") {
        for (i, tok) in rest.iter().enumerate() {
            if tok == "--" {
                break;
            }
            if tok.starts_with('-')
                && !tok.starts_with("--")
                && tok.len() >= 2
                && tok.chars().skip(1).any(|c| c == 'c')
            {
                return rest.get(i + 1).cloned();
            }
        }
        return None;
    }
    if matches!(cmd.as_str(), "cmd" | "cmd.exe") {
        for (i, tok) in rest.iter().enumerate() {
            if tok.eq_ignore_ascii_case("/c") || tok.eq_ignore_ascii_case("/k") {
                let tail = rest.get(i + 1..)?;
                return (!tail.is_empty()).then(|| tail.join(" "));
            }
        }
        return None;
    }
    if matches!(
        cmd.as_str(),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    ) {
        for (i, tok) in rest.iter().enumerate() {
            if tok.eq_ignore_ascii_case("-c")
                || tok.eq_ignore_ascii_case("-cmd")
                || tok.eq_ignore_ascii_case("-command")
            {
                let tail = rest.get(i + 1..)?;
                return (!tail.is_empty()).then(|| tail.join(" "));
            }
        }
    }
    None
}

fn cmd_basename(tok: &str) -> &str {
    tok.rsplit(['/', '\\']).next().unwrap_or(tok)
}

fn is_env_assignment(tok: &str) -> bool {
    let Some((key, _)) = tok.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn extract_segment_path_tokens(tokens: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |tok: &str| {
        let t = tok
            .trim()
            .trim_matches(|c| matches!(c, '"' | '\'' | ';' | ',' | ')' | '('));
        if !t.is_empty() && !is_url_target(t) && !out.iter().any(|s| s == t) {
            out.push(t.to_owned());
        }
    };
    let mut prev_expects_path = false;
    for raw in tokens {
        let tok = raw.as_str();
        if matches!(tok, ">" | ">>" | "<" | "<<") {
            prev_expects_path = true;
            continue;
        }
        let redir_target = tok
            .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '>' | '<' | '&' | '|'));
        if redir_target != tok && !redir_target.is_empty() {
            push(redir_target);
            prev_expects_path = false;
            continue;
        }
        if let Some((_, rhs)) = tok.split_once('=')
            && looks_like_path(rhs)
        {
            push(rhs);
        }
        if (prev_expects_path && !tok.starts_with('-')) || looks_like_path(tok) {
            push(tok);
        }
        prev_expects_path = matches!(
            cmd_basename(tok).to_ascii_lowercase().as_str(),
            "cd" | "pushd"
                | "cat"
                | "head"
                | "tail"
                | "ls"
                | "rm"
                | "cp"
                | "mv"
                | "touch"
                | "tee"
                | "find"
                | "grep"
                | "rg"
                | "sed"
                | "awk"
                | "chmod"
                | "chown"
                | "source"
                | "."
                | "type"
                | "get-content"
                | "gc"
        );
    }
    out
}

fn looks_like_path(tok: &str) -> bool {
    if tok.is_empty() || is_url_target(tok) || tok.starts_with("--") {
        return false;
    }
    tok == "~"
        || tok.starts_with("~/")
        || tok.starts_with("~\\")
        || tok.starts_with('/')
        || tok.starts_with('\\')
        || tok.starts_with("./")
        || tok.starts_with("../")
        || tok.starts_with(".\\")
        || tok.starts_with("..\\")
        || drive_letter(tok).is_some()
        || (tok.contains(['/', '\\']) && !tok.starts_with('-'))
}

fn push_unique_path(out: &mut Vec<PathBuf>, p: PathBuf) {
    if !out.contains(&p) {
        out.push(p);
    }
}

#[cfg(test)]
#[path = "approvals_bash_tests.rs"]
mod approvals_bash_inline_tests;

// ===== merged from approvals_grants.rs =====
// Session-memory (partitioned by `session_id`) + optional persistent (`SessionStore`) cache of
// user approval grants and per-session tracked `bash` working directories.

use hrn_kernel::{ApprovalGrant, Clock, Result, SessionStore, SystemClock};
use std::sync::{Arc, Mutex};

/// Session-memory (scoped by `session_id`) + optional persistent (`SessionStore`) cache of user
/// approval grants.
pub struct GrantCache {
    session: Mutex<BTreeMap<String, Vec<ApprovalGrant>>>,
    bash_cwd: Mutex<BTreeMap<String, PathBuf>>,
    store: Option<Arc<dyn SessionStore>>,
    clock: Arc<dyn Clock>,
    on_clear: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for GrantCache {
    fn default() -> Self {
        Self::memory()
    }
}

impl GrantCache {
    /// Pure in-memory session grant cache (no persistence backing).
    #[must_use]
    pub fn memory() -> Self {
        Self {
            session: Mutex::new(BTreeMap::new()),
            bash_cwd: Mutex::new(BTreeMap::new()),
            store: None,
            clock: Arc::new(SystemClock),
            on_clear: Mutex::new(Vec::new()),
        }
    }

    /// Grant cache backed by `store` for cross-session `Persistent` grants.
    #[must_use]
    pub fn with_store(store: Arc<dyn SessionStore>, clock: Arc<dyn Clock>) -> Self {
        Self {
            session: Mutex::new(BTreeMap::new()),
            bash_cwd: Mutex::new(BTreeMap::new()),
            store: Some(store),
            clock,
            on_clear: Mutex::new(Vec::new()),
        }
    }

    /// Register a callback invoked whenever [`Self::clear`] runs (e.g. to clear `cfg://` session
    /// grants alongside `/forget-approvals`).
    pub fn register_on_clear(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.on_clear
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(hook);
    }

    /// Last working directory established by an executed `bash` `cd`/`pushd` in `session_id`.
    #[must_use]
    pub fn session_bash_cwd(&self, session_id: &str) -> Option<PathBuf> {
        self.bash_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }

    /// Record the working directory established by an executed `bash` `cd`/`pushd` in `session_id`.
    pub fn set_session_bash_cwd(&self, session_id: &str, dir: PathBuf) {
        self.bash_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_owned(), dir);
    }

    /// Record a user's approval decision into session memory (and persistent storage when
    /// `scope == ApprovalGrantScope::Persistent`).
    ///
    /// # Errors
    /// Returns a store error if persisting a `Persistent` grant fails.
    pub fn record(
        &self,
        scope: ApprovalGrantScope,
        tool_name: &str,
        tier: ToolTier,
        normalized_targets: &[String],
    ) -> Result {
        self.record_for_session("", scope, tool_name, tier, normalized_targets)
    }

    /// Record a user's approval decision scoped to `session_id` (or persisted cross-session when
    /// `scope == ApprovalGrantScope::Persistent`).
    ///
    /// # Errors
    /// Returns a store error if persisting a `Persistent` grant fails.
    pub fn record_for_session(
        &self,
        session_id: &str,
        scope: ApprovalGrantScope,
        tool_name: &str,
        tier: ToolTier,
        normalized_targets: &[String],
    ) -> Result {
        let now = self.clock.now_ms();
        match scope {
            ApprovalGrantScope::Once => Ok(()),
            ApprovalGrantScope::File => {
                for target in normalized_targets {
                    self.upsert_session(session_id, target, "file", tier, now);
                }
                Ok(())
            }
            ApprovalGrantScope::Directory => {
                for target in normalized_targets {
                    if let Some(dir) = parent_dir(target) {
                        self.upsert_session(session_id, &dir, "directory", tier, now);
                    }
                }
                Ok(())
            }
            ApprovalGrantScope::ToolSession => {
                self.upsert_session(session_id, tool_name, "tool", tier, now);
                Ok(())
            }
            ApprovalGrantScope::Persistent => {
                if normalized_targets.is_empty() || tool_name == "bash" {
                    self.upsert_session(session_id, tool_name, "tool", tier, now);
                    self.persist_grant(tool_name, "tool", tier, now)?;
                } else {
                    for target in normalized_targets {
                        self.upsert_session(session_id, target, "file", tier, now);
                        self.persist_grant(target, "file", tier, now)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Explicitly record a session-scoped file grant on `normalized_file`.
    pub fn grant_file(&self, normalized_file: &str, tier: ToolTier) {
        self.upsert_session("", normalized_file, "file", tier, self.clock.now_ms());
    }

    /// Explicitly record a session-scoped direct-children-only directory grant on `normalized_dir`.
    pub fn grant_directory(&self, normalized_dir: &str, tier: ToolTier) {
        self.upsert_session("", normalized_dir, "directory", tier, self.clock.now_ms());
    }

    /// Explicitly record a session-scoped tool grant on `tool_name`.
    pub fn grant_tool(&self, tool_name: &str, tier: ToolTier) {
        self.upsert_session("", tool_name, "tool", tier, self.clock.now_ms());
    }

    /// Whether cached session or persistent grants cover `(tool_name, tier, normalized_targets)`.
    #[must_use]
    pub fn covers(&self, tool_name: &str, tier: ToolTier, normalized_targets: &[String]) -> bool {
        self.covers_for_session("", tool_name, tier, normalized_targets)
    }

    /// Whether session grants for `session_id` (or persistent grants) cover
    /// `(tool_name, tier, normalized_targets)`. For multi-target calls, EVERY target must be
    /// covered.
    #[must_use]
    pub fn covers_for_session(
        &self,
        session_id: &str,
        tool_name: &str,
        tier: ToolTier,
        normalized_targets: &[String],
    ) -> bool {
        let mut all = {
            let map = self
                .session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut entries = map.get(session_id).cloned().unwrap_or_default();
            if !session_id.is_empty()
                && let Some(global_test) = map.get("")
            {
                entries.extend(global_test.iter().cloned());
            }
            entries
        };
        if let Some(store) = &self.store
            && let Ok(persisted) = store.list_approval_grants()
        {
            all.extend(persisted);
        }
        if all
            .iter()
            .any(|g| g.scope == "tool" && g.key == tool_name && tier_covers(g.tier, tier))
        {
            return true;
        }
        if normalized_targets.is_empty() {
            return false;
        }
        normalized_targets.iter().all(|target| {
            all.iter().any(|g| {
                tier_covers(g.tier, tier)
                    && ((g.scope == "file" && g.key == *target)
                        || (g.scope == "directory" && is_direct_child(target, &g.key)))
            })
        })
    }

    /// Clear in-memory session grants and tracked bash cwd belonging only to `session_id`.
    pub fn clear_session(&self, session_id: &str) {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        self.bash_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
    }

    /// Clear all in-memory session grants and all persisted grants in the store (`/forget-approvals`).
    ///
    /// # Errors
    /// Returns a store error if clearing persistent grants fails.
    pub fn clear(&self) -> Result {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.bash_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        if let Some(store) = &self.store {
            store.clear_approval_grants()?;
        }
        let hooks = self
            .on_clear
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for hook in hooks {
            hook();
        }
        Ok(())
    }

    fn upsert_session(
        &self,
        session_id: &str,
        key: &str,
        scope: &str,
        tier: ToolTier,
        created_ms: i64,
    ) {
        let mut map = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let grants = map.entry(session_id.to_owned()).or_default();
        if let Some(existing) = grants.iter_mut().find(|g| g.key == key && g.scope == scope) {
            existing.tier = existing.tier.stronger(tier);
            existing.created_ms = created_ms;
            return;
        }
        grants.push(ApprovalGrant {
            key: key.to_owned(),
            tier,
            scope: scope.to_owned(),
            created_ms,
        });
    }

    fn persist_grant(&self, key: &str, scope: &str, tier: ToolTier, created_ms: i64) -> Result {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let mut final_tier = tier;
        if let Ok(existing) = store.list_approval_grants()
            && let Some(prev) = existing.iter().find(|g| g.key == key && g.scope == scope)
        {
            final_tier = prev.tier.stronger(tier);
        }
        store.put_approval_grant(&ApprovalGrant {
            key: key.to_owned(),
            tier: final_tier,
            scope: scope.to_owned(),
            created_ms,
        })
    }
}

// ===== merged from approvals_path.rs =====
// Path/selector normalization, target extraction, and restricted-folder matching for approvals.

use crate::eval_support::extract_eval_load_targets;
use crate::scope::{extract_patch_paths, hashline_paths, lexical_normalize};

const PATH_ARG_KEYS: [&str; 10] = [
    "path",
    "file_path",
    "target_path",
    "old_path",
    "new_path",
    "notebook_path",
    "dir",
    "directory",
    "program",
    "file",
];

const ARRAY_PATH_ARG_KEYS: [&str; 2] = ["paths", "add_dirs"];

/// Tools that accept `;`-delimited path lists (matching `Scope::parse`).
fn splits_semicolons(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "glob" | "grep" | "find" | "ast_grep" | "ast_edit"
    )
}

/// Tools that traverse, list, or commit workspace-wide edits (must match when a target directory
/// or glob prefix is an ancestor of a restricted child folder).
#[must_use]
pub fn is_dir_scan_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "glob" | "grep" | "find" | "ast_grep" | "ast_edit" | "read" | "resolve"
    )
}

/// Peel trailing line/mode selectors (`:10-20`, `:10+5`, `:-20`, `:raw`, `:conflicts`, `:img`,
/// `#L10`, `#L10-L20`) from `raw`, keeping Windows drive letters (`C:\...`) intact.
#[must_use]
pub fn peel_selector(raw: &str) -> &str {
    let mut s = raw.trim();
    if let Some((head, frag)) = s.rsplit_once('#')
        && !head.is_empty()
        && is_line_anchor(frag)
    {
        s = head;
    }
    for _ in 0..3 {
        let Some((head, tail)) = s.rsplit_once(':') else {
            break;
        };
        if head.is_empty() || is_windows_drive_head(head) {
            break;
        }
        if matches!(tail, "raw" | "conflicts" | "img") || is_line_span_selector(tail) {
            s = head;
        } else {
            break;
        }
    }
    s
}

fn is_windows_drive_head(head: &str) -> bool {
    head.len() == 1 && head.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
}

fn is_line_anchor(frag: &str) -> bool {
    let Some(rest) = frag.strip_prefix('L').or_else(|| frag.strip_prefix('l')) else {
        return false;
    };
    if let Some((a, b)) = rest.split_once('-') {
        let b = b
            .strip_prefix('L')
            .or_else(|| b.strip_prefix('l'))
            .unwrap_or(b);
        !a.is_empty()
            && a.bytes().all(|c| c.is_ascii_digit())
            && !b.is_empty()
            && b.bytes().all(|c| c.is_ascii_digit())
    } else {
        !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit())
    }
}

fn is_line_span_selector(tail: &str) -> bool {
    !tail.is_empty()
        && tail
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b','))
        && tail.bytes().any(|b| b.is_ascii_digit())
}

/// Normalize a path or URL target for grant cache lookup and restricted-folder matching:
/// peels selectors, expands `~`, resolves against `cwd` (including Windows drive-relative
/// `C:foo` and drive-root-relative `\foo`), canonicalizes existing prefixes via `fs`, lexically
/// normalizes slashes/`..`, and case-folds on Windows.
#[must_use]
pub fn normalize_target(
    cwd: &Path,
    raw: &str,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
) -> String {
    let trimmed = raw.trim();
    if is_url_target(trimmed) {
        return peel_selector(trimmed).to_owned();
    }
    let peeled = peel_selector(trimmed);
    let expanded = expand_tilde(peeled, home);
    let combined = resolve_against_cwd(cwd, &expanded);
    let resolved = canonicalize_with_ancestors(&combined, fs);
    format_normalized_path(&resolved)
}

fn is_url_target(s: &str) -> bool {
    if let Some((scheme, _)) = s.split_once("://") {
        !scheme.is_empty()
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    } else {
        false
    }
}

fn expand_tilde(raw: &str, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if raw == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn drive_letter(path: &str) -> Option<char> {
    let b = path.as_bytes();
    if b.len() >= 2 && b.first().is_some_and(u8::is_ascii_alphabetic) && b.get(1) == Some(&b':') {
        path.chars().next().map(|c| c.to_ascii_lowercase())
    } else {
        None
    }
}

/// Resolve `expanded` against `cwd`, handling full absolute paths, UNC paths, Windows
/// drive-relative (`C:foo`) and drive-root-relative (`\vault\secret`) paths cleanly.
fn resolve_against_cwd(cwd: &Path, expanded: &Path) -> PathBuf {
    let s = expanded.to_string_lossy();
    let cwd_s = cwd.to_string_lossy();
    let cwd_drive = drive_letter(&cwd_s);

    // UNC path (`\\server\share` or `//server/share`).
    if s.starts_with(r"\\") || s.starts_with("//") {
        return expanded.to_path_buf();
    }
    // Windows drive prefix (`X:...`).
    if let Some(drv) = drive_letter(&s) {
        let third = s.as_bytes().get(2).copied();
        if matches!(third, Some(b'/' | b'\\')) {
            return expanded.to_path_buf();
        }
        // Drive-relative `C:foo`: join onto `cwd` if on same drive, else root of `drv`.
        let rest = s.get(2..).unwrap_or("");
        if cwd_drive == Some(drv) {
            return cwd.join(rest);
        }
        return PathBuf::from(format!("{drv}:/{rest}"));
    }
    // Leading `/` or `\`: drive-root-relative when `cwd` carries a Windows drive prefix.
    if s.starts_with('\\') || (s.starts_with('/') && cwd_drive.is_some()) {
        if let Some(drv) = cwd_drive {
            return PathBuf::from(format!("{drv}:{s}"));
        }
        return expanded.to_path_buf();
    }
    if s.starts_with('/') || expanded.is_absolute() {
        return expanded.to_path_buf();
    }
    cwd.join(expanded)
}

/// Canonicalize `path` via `fs`, falling back to canonicalizing the nearest existing ancestor so
/// non-existent targets beneath a symlink still resolve to their real destination.
pub(crate) fn canonicalize_with_ancestors(path: &Path, fs: Option<&dyn WorkspaceFs>) -> PathBuf {
    let Some(fs) = fs else {
        return path.to_path_buf();
    };
    if let Some(canon) = fs.canonicalize(path) {
        return canon;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    while let Some(name) = cur.file_name() {
        tail.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            break;
        };
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Some(mut canon) = fs.canonicalize(parent) {
            for seg in tail.into_iter().rev() {
                canon.push(seg);
            }
            return canon;
        }
        cur = parent;
    }
    path.to_path_buf()
}

fn format_normalized_path(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let parts = lexical_normalize(Path::new(&raw));
    let mut out = String::new();
    for (idx, part) in parts.iter().enumerate() {
        if idx == 0 {
            out.push_str(part);
        } else if part == "/" {
            if !out.ends_with('/') {
                out.push('/');
            }
        } else {
            if !out.is_empty() && !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(part);
        }
    }
    while out.len() > 1 && out.ends_with('/') && !is_drive_root(&out) {
        out.pop();
    }
    if cfg!(windows) {
        out.to_ascii_lowercase()
    } else if out.len() >= 2
        && out.as_bytes().get(1) == Some(&b':')
        && out.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
    {
        let mut chars = out.chars();
        let first = chars.next().map_or('c', |c| c.to_ascii_lowercase());
        format!("{first}{}", chars.as_str())
    } else {
        out
    }
}

fn is_drive_root(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 3 && b.first().is_some_and(u8::is_ascii_alphabetic) && b.get(1..3) == Some(b":/")
}

/// Parent directory of a normalized slash-separated path (`None` for roots).
#[must_use]
pub fn parent_dir(normalized: &str) -> Option<String> {
    if normalized == "/" || is_drive_root(normalized) {
        return None;
    }
    let trimmed = normalized.trim_end_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;
    if parent.is_empty() {
        return Some("/".to_owned());
    }
    if parent.len() == 2 && parent.as_bytes().get(1) == Some(&b':') {
        return Some(format!("{parent}/"));
    }
    Some(parent.to_owned())
}

pub(super) fn is_direct_child(target: &str, dir: &str) -> bool {
    parent_dir(target).is_some_and(|p| p == dir)
}

/// Whether `target` equals `folder` or is nested inside `folder`.
#[must_use]
pub fn path_within_folder(target: &str, folder: &str) -> bool {
    if folder.is_empty() {
        return false;
    }
    if target == folder {
        return true;
    }
    if folder == "/" || is_drive_root(folder) {
        return target.starts_with(folder);
    }
    target
        .strip_prefix(folder)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Extract raw target paths from `args` for a tool call (excluding `bash`, whose command/cwd
/// path candidates are checked separately against restricted folders).
#[must_use]
pub fn extract_targets(tool_name: &str, args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let split_semi = splits_semicolons(tool_name);
    let mut push = |val: &str| {
        if split_semi && !is_url_target(val.trim()) {
            for part in val.split(';') {
                let trimmed = part.trim();
                if !trimmed.is_empty() && !out.iter().any(|s| s == trimmed) {
                    out.push(trimmed.to_owned());
                }
            }
        } else {
            let trimmed = val.trim();
            if !trimmed.is_empty() && !out.iter().any(|s| s == trimmed) {
                out.push(trimmed.to_owned());
            }
        }
    };
    if tool_name == "bash" {
        return out;
    }
    if tool_name == "eval"
        && let Some(code) = args.get("code").and_then(Value::as_str)
    {
        for target in extract_eval_load_targets(code) {
            push(&target);
        }
    }
    if tool_name == "edit" {
        if let Some(input) = args.get("input").and_then(Value::as_str) {
            for p in hashline_paths(input) {
                push(&p);
            }
        }
        if let Some(patch) = args.get("patch").and_then(Value::as_str) {
            for p in extract_patch_paths(patch) {
                push(&p);
            }
        }
    }
    if tool_name == "apply_patch" {
        for key in ["patch", "input"] {
            if let Some(patch) = args.get(key).and_then(Value::as_str) {
                for p in extract_patch_paths(patch) {
                    push(&p);
                }
            }
        }
    }
    for key in PATH_ARG_KEYS {
        if let Some(val) = args.get(key).and_then(Value::as_str) {
            push(val);
        }
    }
    for key in ARRAY_PATH_ARG_KEYS {
        if let Some(val) = args.get(key).and_then(Value::as_str) {
            push(val);
        } else if let Some(arr) = args.get(key).and_then(Value::as_array) {
            for item in arr {
                if let Some(val) = item.as_str() {
                    push(val);
                }
            }
        }
    }
    out
}

/// Normalized target sets extracted for one tool call.
#[derive(Clone, Debug, Default)]
pub(crate) struct CallTargets {
    /// Targets used for grant cache lookup and `ApprovalRequest.targets` (excludes `bash`).
    pub grant: Vec<String>,
    /// Targets checked against `restricted_folders`.
    pub restricted: Vec<String>,
    /// Directories (e.g. `bash` `cd`/`pushd` targets) checked for BOTH inside-restricted and
    /// ancestor-of-restricted overlap.
    pub ancestor_check: Vec<String>,
    /// Resulting bash working directory when a static `cd`/`pushd` runs in this call.
    pub bash_next_cwd: Option<PathBuf>,
    /// `true` when `bash` runs an unbounded `cd` (`cd -`, `cd $VAR`, bare `cd` without `home`).
    pub bash_unknown_cwd: bool,
}

/// Normalized target lists `(grant_targets, restricted_targets)` for a tool call.
#[cfg(test)]
pub(crate) fn normalized_targets_for_call(
    tool_name: &str,
    args: &Value,
    cwd: &Path,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
) -> (Vec<String>, Vec<String>) {
    let ct = normalized_call_targets(tool_name, args, cwd, None, home, fs);
    (ct.grant, ct.restricted)
}

/// Extract full normalized target sets for a tool call, including inline & cross-call `bash` `cd`
/// tracking, redirections, ANSI-C quoting, and nested sub-shell command inspection.
pub(crate) fn normalized_call_targets(
    tool_name: &str,
    args: &Value,
    cwd: &Path,
    session_bash_cwd: Option<&Path>,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
) -> CallTargets {
    let grant: Vec<String> = extract_targets(tool_name, args)
        .into_iter()
        .map(|raw| normalize_target(cwd, &raw, home, fs))
        .collect();
    let mut out = CallTargets {
        restricted: grant.clone(),
        grant,
        ..CallTargets::default()
    };
    if tool_name == "bash" {
        collect_bash_call_targets(args, cwd, session_bash_cwd, home, fs, &mut out);
    }
    out
}

/// Strip from the first path segment containing glob metacharacters (`*`, `?`, `[`) to produce
/// the static ancestor directory for a normalized glob target (e.g. `/repo/sec*` -> `/repo`,
/// `/repo/src/*.rs` -> `/repo/src`).
fn strip_glob_tail(normalized: &str) -> String {
    if !normalized.contains(['*', '?', '[']) {
        return normalized.to_owned();
    }
    let mut prefix = String::new();
    let has_leading_slash = normalized.starts_with('/');
    for (idx, seg) in normalized.split('/').enumerate() {
        if seg.contains(['*', '?', '[']) {
            break;
        }
        if idx == 0 {
            if has_leading_slash {
                prefix.push('/');
            } else {
                prefix.push_str(seg);
            }
        } else if !seg.is_empty() {
            if !prefix.is_empty() && !prefix.ends_with('/') {
                prefix.push('/');
            }
            prefix.push_str(seg);
        }
    }
    if prefix.is_empty() {
        if has_leading_slash {
            return "/".to_owned();
        }
        return normalized.to_owned();
    }
    if prefix.len() == 2 && prefix.as_bytes().get(1) == Some(&b':') {
        prefix.push('/');
    }
    prefix
}

/// Find the first configured restricted folder matching `targets` (or `cwd` when `targets` is
/// empty).
#[must_use]
pub fn matching_restricted_folder(
    restricted_folders: &[String],
    targets: &[String],
    cwd: &Path,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
) -> Option<String> {
    matching_restricted_folder_for_tool("", restricted_folders, targets, cwd, home, fs)
}

/// Find the first configured restricted folder matching `targets` (or `cwd` when `targets` is
/// empty) for `tool_name`. For directory-scanning tools (`glob`, `grep`, `find`, `ast_grep`,
/// `ast_edit`, `read`, `resolve`), also resolves glob prefixes and matches when a target directory
/// is an ancestor of a restricted folder.
#[must_use]
pub fn matching_restricted_folder_for_tool(
    tool_name: &str,
    restricted_folders: &[String],
    targets: &[String],
    cwd: &Path,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
) -> Option<String> {
    matching_restricted_folder_inner(
        tool_name,
        restricted_folders,
        targets,
        &[],
        false,
        cwd,
        home,
        fs,
    )
}

/// Full restricted-folder matcher using [`CallTargets`] (including `bash` `cd` ancestor checks and
/// unbounded `cd` fail-closed prompting).
#[must_use]
pub(crate) fn matching_restricted_folder_for_call(
    tool_name: &str,
    restricted_folders: &[String],
    call_targets: &CallTargets,
    cwd: &Path,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
) -> Option<String> {
    matching_restricted_folder_inner(
        tool_name,
        restricted_folders,
        &call_targets.restricted,
        &call_targets.ancestor_check,
        call_targets.bash_unknown_cwd,
        cwd,
        home,
        fs,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "shared pure matcher backing both public target-slice and CallTargets checks"
)]
fn matching_restricted_folder_inner(
    tool_name: &str,
    restricted_folders: &[String],
    targets: &[String],
    ancestor_targets: &[String],
    unknown_cd: bool,
    cwd: &Path,
    home: Option<&Path>,
    fs: Option<&dyn WorkspaceFs>,
) -> Option<String> {
    if restricted_folders.is_empty() {
        return None;
    }
    let normalized_folders: Vec<(&str, String)> = restricted_folders
        .iter()
        .map(|raw| (raw.as_str(), normalize_target(cwd, raw, home, fs)))
        .filter(|(_, norm)| !norm.is_empty())
        .collect();
    if normalized_folders.is_empty() {
        return None;
    }
    if unknown_cd {
        return normalized_folders.first().map(|(raw, _)| (*raw).to_owned());
    }
    let candidates: Vec<String> = if targets.is_empty() {
        vec![normalize_target(cwd, &cwd.to_string_lossy(), home, fs)]
    } else {
        targets
            .iter()
            .map(|t| normalize_target(cwd, t, home, fs))
            .collect()
    };
    let dir_scan = is_dir_scan_tool(tool_name);
    for cand in &candidates {
        let effective = if dir_scan && cand.contains(['*', '?', '[']) {
            strip_glob_tail(cand)
        } else {
            cand.clone()
        };
        for (raw, folder) in &normalized_folders {
            if path_within_folder(&effective, folder)
                || (dir_scan && path_within_folder(folder, &effective))
            {
                return Some((*raw).to_owned());
            }
        }
    }
    for anc in ancestor_targets {
        let norm = normalize_target(cwd, anc, home, fs);
        for (raw, folder) in &normalized_folders {
            if path_within_folder(&norm, folder) || path_within_folder(folder, &norm) {
                return Some((*raw).to_owned());
            }
        }
    }
    None
}
