//! Operator-no-loosen security enforcement across layered TOML configurations so a project
//! `.harness/config.toml` can tighten security posture but NEVER weaken operator settings.

use super::{
    LayerSource, insert_dotted, lookup_dotted, lookup_dotted_mut, resolve_path_scoped_array,
};
use crate::settings_schema::schema;
use hrn_edge_tools::command_policy::{CommandApproval, CommandPattern, merge_patterns};
use std::collections::BTreeMap;
use std::path::Path;

/// Dangerous environment variables that a project config may NEVER add to `shell.allowed_env_vars`.
const DANGEROUS_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "BASH_ENV",
    "ENV",
    "PATH",
    "PROMPT_COMMAND",
    "IFS",
    "CDPATH",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    "PYTHONPATH",
    "NODE_OPTIONS",
    "RUBYOPT",
    "PERL5OPT",
];

/// Keys under `[llm]` that an untrusted project `.harness/config.toml` may NEVER define or
/// override (operator billing tiers, account pools, quota reserves, and transport lanes).
const PROJECT_BLOCKED_LLM_KEYS: &[&str] = &[
    "accounts",
    "allow_plaintext_http",
    "retry",
    "tier",
    "extended_context",
    "anthropic",
    "kimi",
];

/// Strip operator-only credential/provider definitions (`[[providers]]`), GC retention policies
/// (`[gc]`), share/collab endpoints & redaction flags (`[share]`, `[collab]`), self-update
/// coordinates (`[update]`), and billing/pool/tier settings (`[llm.*]`) from a
/// `LayerSource::Project` table before merging so a hostile repo cannot redirect OAuth/API tokens,
/// hijack share/collab endpoints or `hrn update`, disable secret redaction, wipe operator sessions
/// with `[gc] retain_days = 0`, or alter billing policies.
pub(super) fn sanitize_project_layer(tbl: &toml::Table) -> toml::Table {
    let mut clean = tbl.clone();
    clean.remove("providers");
    clean.remove("gc");
    clean.remove("share");
    clean.remove("collab");
    clean.remove("update");
    if let Some(toml::Value::Table(llm)) = clean.get_mut("llm") {
        for key in PROJECT_BLOCKED_LLM_KEYS {
            llm.remove(*key);
        }
        if llm.is_empty() {
            clean.remove("llm");
        }
    }
    clean
}

/// Enforce all schema-declared `operator_no_loosen` invariants after base 6-layer merge and
/// path-scoped resolution.
pub(super) fn enforce_operator_policies(
    root: &mut toml::Table,
    layers: &[(LayerSource, toml::Table)],
    cwd: &Path,
    home: Option<&Path>,
) {
    for spec in schema().all().iter().filter(|s| s.operator_no_loosen) {
        match spec.key.as_str() {
            "shell.patterns" => enforce_shell_patterns(root, layers, cwd, home),
            "tools.restricted_folders" => enforce_restricted_folders(root, layers),
            "tools.approval_mode" => enforce_approval_mode(root, layers),
            "tools.approval" => enforce_tool_approvals(root, layers),
            "shell.git_guard" => enforce_bool_lock(root, layers, "shell.git_guard", true),
            "shell.allow_compound_commands"
            | "lsp.format_on_write"
            | "edit.auto_repair"
            | "commit.changelog" => {
                enforce_bool_lock(root, layers, &spec.key, false);
            }
            "shell.allowed_env_vars" => enforce_allowed_env_vars(root, layers),
            "eval.py"
            | "eval.js"
            | "eval.tools_enabled"
            | "desktop.eval_prelude"
            | "lsp.enabled"
            | "commit.enabled" => {
                enforce_operator_disable_lock(root, layers, &spec.key);
            }
            "edit.block_auto_generated" | "edit.enforce_seen_lines" | "lsp.lazy" => {
                enforce_default_true_lock(root, layers, &spec.key);
            }
            "eval.default_timeout_secs" => {
                enforce_timeout_cap(root, layers, "eval.default_timeout_secs", 30);
            }
            "gc.retain_days" => enforce_min_floor(root, layers, "gc.retain_days", 90),
            "gc.retain_newest_per_cwd" => {
                enforce_min_floor(root, layers, "gc.retain_newest_per_cwd", 20);
            }
            "gc.retain_newest_global" => {
                enforce_min_floor(root, layers, "gc.retain_newest_global", 20);
            }
            "gc.vacuum_reclaim_bytes" => {
                enforce_min_floor(root, layers, "gc.vacuum_reclaim_bytes", 1_048_576);
            }
            "share.redact" => enforce_bool_lock(root, layers, "share.redact", true),
            _ => {}
        }
    }
}

fn is_operator_layer(src: &LayerSource) -> bool {
    !matches!(src, LayerSource::Project | LayerSource::Default)
}

fn has_higher_operator(layers: &[(LayerSource, toml::Table)], key: &str) -> bool {
    layers.iter().any(|(src, tbl)| {
        src.rank() > LayerSource::Project.rank() && lookup_dotted(tbl, key).is_some()
    })
}

/// Merge `shell.patterns` preserving exact operator rule order while ensuring project rules can
/// tighten via `deny`/`prompt` but can NEVER loosen operator `deny`/`prompt` rules.
fn enforce_shell_patterns(
    root: &mut toml::Table,
    layers: &[(LayerSource, toml::Table)],
    cwd: &Path,
    home: Option<&Path>,
) {
    let mut op_rules: Vec<CommandPattern> = Vec::new();
    let mut proj_rules: Vec<CommandPattern> = Vec::new();
    let mut saw_any = false;
    // Iterate highest-precedence operator layers first (prepend semantics) while preserving exact
    // in-layer ordering within each layer.
    for (src, tbl) in layers.iter().rev() {
        let Some(toml::Value::Array(arr)) = lookup_dotted(tbl, "shell.patterns") else {
            continue;
        };
        if matches!(src, LayerSource::Default) {
            continue;
        }
        saw_any = true;
        let resolved = resolve_path_scoped_array(arr, cwd, home);
        let parsed: Vec<CommandPattern> = resolved.iter().filter_map(value_to_pattern).collect();
        if matches!(src, LayerSource::Project) {
            proj_rules.extend(parsed);
        } else {
            op_rules.extend(parsed);
        }
    }
    if !saw_any || (op_rules.is_empty() && proj_rules.is_empty()) {
        return;
    }
    let merged = merge_patterns(&op_rules, &proj_rules);
    let values: Vec<toml::Value> = merged.iter().map(pattern_to_value).collect();
    insert_dotted(root, "shell.patterns", toml::Value::Array(values));
}

fn value_to_pattern(val: &toml::Value) -> Option<CommandPattern> {
    let tbl = val.as_table()?;
    let pattern = tbl.get("match")?.as_str()?.to_owned();
    let approval = match tbl.get("approval")?.as_str()? {
        "allow" => CommandApproval::Allow,
        "prompt" => CommandApproval::Prompt,
        "deny" => CommandApproval::Deny,
        _ => return None,
    };
    let env = tbl
        .get("env")
        .and_then(toml::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(CommandPattern {
        pattern,
        approval,
        env,
    })
}

fn pattern_to_value(rule: &CommandPattern) -> toml::Value {
    let mut tbl = toml::Table::new();
    tbl.insert(
        "match".to_owned(),
        toml::Value::String(rule.pattern.clone()),
    );
    let approval = match rule.approval {
        CommandApproval::Allow => "allow",
        CommandApproval::Prompt => "prompt",
        CommandApproval::Deny => "deny",
    };
    tbl.insert(
        "approval".to_owned(),
        toml::Value::String(approval.to_owned()),
    );
    if !rule.env.is_empty() {
        tbl.insert(
            "env".to_owned(),
            toml::Value::Array(
                rule.env
                    .iter()
                    .map(|e| toml::Value::String(e.clone()))
                    .collect(),
            ),
        );
    }
    toml::Value::Table(tbl)
}

/// Union operator and project `tools.restricted_folders` (deduplicated) so project can only add.
fn enforce_restricted_folders(root: &mut toml::Table, layers: &[(LayerSource, toml::Table)]) {
    let mut combined: Vec<toml::Value> = Vec::new();
    let mut saw_any = false;
    for (src, tbl) in layers {
        if matches!(src, LayerSource::Default) {
            continue;
        }
        if let Some(toml::Value::Array(arr)) = lookup_dotted(tbl, "tools.restricted_folders") {
            saw_any = true;
            for item in arr {
                if !combined.contains(item) {
                    combined.push(item.clone());
                }
            }
        }
    }
    if saw_any {
        insert_dotted(
            root,
            "tools.restricted_folders",
            toml::Value::Array(combined),
        );
    }
}

fn approval_mode_rank(raw: &str) -> Option<u8> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "plan" => Some(5),
        "always-ask" | "always_ask" | "default" => Some(4),
        "always-ask-cached" | "always_ask_cached" | "ask-cached" | "ask_cached" => Some(3),
        "write" | "auto-edit" | "auto_edit" | "accept-edits" => Some(2),
        "yolo" | "auto" => Some(1),
        _ => None,
    }
}

/// Project cannot loosen `tools.approval_mode` below an operator layer's strictness.
fn enforce_approval_mode(root: &mut toml::Table, layers: &[(LayerSource, toml::Table)]) {
    if has_higher_operator(layers, "tools.approval_mode") {
        return;
    }
    let op_val = layers.iter().rev().find_map(|(src, tbl)| {
        is_operator_layer(src)
            .then(|| lookup_dotted(tbl, "tools.approval_mode"))
            .flatten()
            .and_then(toml::Value::as_str)
    });
    let proj_val = layers.iter().rev().find_map(|(src, tbl)| {
        matches!(src, LayerSource::Project)
            .then(|| lookup_dotted(tbl, "tools.approval_mode"))
            .flatten()
            .and_then(toml::Value::as_str)
    });
    if let (Some(op), Some(proj)) = (op_val, proj_val)
        && let (Some(op_rank), Some(proj_rank)) = (approval_mode_rank(op), approval_mode_rank(proj))
        && proj_rank < op_rank
    {
        insert_dotted(
            root,
            "tools.approval_mode",
            toml::Value::String(op.to_owned()),
        );
    }
}

fn tool_policy_rank(raw: &str) -> u8 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "deny" => 3,
        "prompt" => 2,
        "allow" => 1,
        _ => 0,
    }
}

/// Project cannot loosen any operator `[tools.approval] <tool>` entry (`deny > prompt > allow`).
fn enforce_tool_approvals(root: &mut toml::Table, layers: &[(LayerSource, toml::Table)]) {
    let mut op_entries: BTreeMap<String, String> = BTreeMap::new();
    let mut higher_op_keys: BTreeMap<String, String> = BTreeMap::new();
    for (src, tbl) in layers {
        if !is_operator_layer(src) {
            continue;
        }
        let Some(toml::Value::Table(map)) = lookup_dotted(tbl, "tools.approval") else {
            continue;
        };
        for (tool, val) in map {
            if let Some(s) = val.as_str() {
                if src.rank() > LayerSource::Project.rank() {
                    higher_op_keys.insert(tool.clone(), s.to_owned());
                } else {
                    op_entries.insert(tool.clone(), s.to_owned());
                }
            }
        }
    }
    let Some(toml::Value::Table(merged)) = lookup_dotted_mut(root, "tools.approval") else {
        return;
    };
    for (tool, op_pol) in op_entries {
        if higher_op_keys.contains_key(&tool) {
            continue;
        }
        let cur_rank = merged
            .get(&tool)
            .and_then(toml::Value::as_str)
            .map_or(0, tool_policy_rank);
        if cur_rank < tool_policy_rank(&op_pol) {
            merged.insert(tool, toml::Value::String(op_pol));
        }
    }
}

/// Lock a boolean security flag so project cannot flip it away from `strict_val`.
fn enforce_bool_lock(
    root: &mut toml::Table,
    layers: &[(LayerSource, toml::Table)],
    key: &str,
    strict_val: bool,
) {
    if has_higher_operator(layers, key) {
        return;
    }
    let op_explicit = layers.iter().rev().find_map(|(src, tbl)| {
        is_operator_layer(src)
            .then(|| lookup_dotted(tbl, key))
            .flatten()
            .and_then(toml::Value::as_bool)
    });
    let proj_val = layers.iter().rev().find_map(|(src, tbl)| {
        matches!(src, LayerSource::Project)
            .then(|| lookup_dotted(tbl, key))
            .flatten()
            .and_then(toml::Value::as_bool)
    });
    // For `strict_val == false` (e.g. `allow_compound_commands`), even the implicit default `false`
    // cannot be flipped to `true` by an untrusted project config unless an operator opted in.
    let op_locked = match op_explicit {
        Some(v) => v == strict_val,
        None => !strict_val,
    };
    if op_locked && proj_val == Some(!strict_val) {
        insert_dotted(root, key, toml::Value::Boolean(strict_val));
    }
}

/// Filter dangerous env vars from project `shell.allowed_env_vars` and prevent widening past the
/// operator allowlist (when the operator sets no allowlist, the default is `[]`, so project config
/// may never introduce arbitrary allowed env vars).
fn enforce_allowed_env_vars(root: &mut toml::Table, layers: &[(LayerSource, toml::Table)]) {
    if has_higher_operator(layers, "shell.allowed_env_vars") {
        return;
    }
    let op_list: Vec<String> = layers
        .iter()
        .rev()
        .find_map(|(src, tbl)| {
            is_operator_layer(src)
                .then(|| lookup_dotted(tbl, "shell.allowed_env_vars"))
                .flatten()
                .and_then(toml::Value::as_array)
        })
        .map(|arr| {
            arr.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let proj_list: Option<Vec<String>> = layers
        .iter()
        .rev()
        .find_map(|(src, tbl)| {
            matches!(src, LayerSource::Project)
                .then(|| lookup_dotted(tbl, "shell.allowed_env_vars"))
                .flatten()
                .and_then(toml::Value::as_array)
        })
        .map(|arr| {
            arr.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        });
    let Some(proj) = proj_list else {
        return;
    };
    let filtered: Vec<toml::Value> = proj
        .into_iter()
        .filter(|var| !is_dangerous_env_var(var))
        .filter(|var| op_list.iter().any(|allowed| allowed == var))
        .map(toml::Value::String)
        .collect();
    insert_dotted(root, "shell.allowed_env_vars", toml::Value::Array(filtered));
}

fn is_dangerous_env_var(var: &str) -> bool {
    let upper = var.trim().to_ascii_uppercase();
    DANGEROUS_ENV_VARS.contains(&upper.as_str())
}

/// Prevent a project config from re-enabling a default-`true` capability (`eval.*`,
/// `desktop.eval_prelude`) that an operator layer explicitly set to `false`.
fn enforce_operator_disable_lock(
    root: &mut toml::Table,
    layers: &[(LayerSource, toml::Table)],
    key: &str,
) {
    if has_higher_operator(layers, key) {
        return;
    }
    let op_explicit = layers.iter().rev().find_map(|(src, tbl)| {
        is_operator_layer(src)
            .then(|| lookup_dotted(tbl, key))
            .flatten()
            .and_then(toml::Value::as_bool)
    });
    let proj_val = layers.iter().rev().find_map(|(src, tbl)| {
        matches!(src, LayerSource::Project)
            .then(|| lookup_dotted(tbl, key))
            .flatten()
            .and_then(toml::Value::as_bool)
    });
    if op_explicit == Some(false) && proj_val == Some(true) {
        insert_dotted(root, key, toml::Value::Boolean(false));
    }
}

/// Prevent a project config from disabling a default-`true` safety guard
/// (`edit.block_auto_generated`, `edit.enforce_seen_lines`) unless an operator layer explicitly
/// set it to `false`.
fn enforce_default_true_lock(
    root: &mut toml::Table,
    layers: &[(LayerSource, toml::Table)],
    key: &str,
) {
    if has_higher_operator(layers, key) {
        return;
    }
    let op_explicit = layers.iter().rev().find_map(|(src, tbl)| {
        is_operator_layer(src)
            .then(|| lookup_dotted(tbl, key))
            .flatten()
            .and_then(toml::Value::as_bool)
    });
    let proj_val = layers.iter().rev().find_map(|(src, tbl)| {
        matches!(src, LayerSource::Project)
            .then(|| lookup_dotted(tbl, key))
            .flatten()
            .and_then(toml::Value::as_bool)
    });
    if op_explicit != Some(false) && proj_val == Some(false) {
        insert_dotted(root, key, toml::Value::Boolean(true));
    }
}

/// Prevent a project config from disabling (`<= 0`) or raising an operator-capped timeout above
/// the operator's effective cap (or `default_cap` when unset).
fn enforce_timeout_cap(
    root: &mut toml::Table,
    layers: &[(LayerSource, toml::Table)],
    key: &str,
    default_cap: i64,
) {
    if has_higher_operator(layers, key) {
        return;
    }
    let op_explicit = layers.iter().rev().find_map(|(src, tbl)| {
        is_operator_layer(src)
            .then(|| lookup_dotted(tbl, key))
            .flatten()
            .and_then(toml::Value::as_integer)
    });
    let proj_val = layers.iter().rev().find_map(|(src, tbl)| {
        matches!(src, LayerSource::Project)
            .then(|| lookup_dotted(tbl, key))
            .flatten()
            .and_then(toml::Value::as_integer)
    });
    let Some(proj) = proj_val else {
        return;
    };
    let op_cap = op_explicit.unwrap_or(default_cap);
    if op_cap > 0 && (proj <= 0 || proj > op_cap) {
        insert_dotted(root, key, toml::Value::Integer(op_cap));
    }
}

/// Prevent a project layer from lowering a retention/vacuum floor below the operator or default value.
fn enforce_min_floor(
    root: &mut toml::Table,
    layers: &[(LayerSource, toml::Table)],
    key: &str,
    default_floor: i64,
) {
    if has_higher_operator(layers, key) {
        return;
    }
    let op_val = layers
        .iter()
        .rev()
        .find_map(|(src, tbl)| {
            is_operator_layer(src)
                .then(|| lookup_dotted(tbl, key))
                .flatten()
                .and_then(toml::Value::as_integer)
        })
        .unwrap_or(default_floor);
    let cur = lookup_dotted(root, key).and_then(toml::Value::as_integer);
    if cur.is_some_and(|v| v < op_val) {
        insert_dotted(root, key, toml::Value::Integer(op_val));
    }
}

#[cfg(test)]
#[path = "config_layers_policy_tests.rs"]
mod tests;
