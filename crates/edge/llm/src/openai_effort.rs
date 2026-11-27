//! HTTP 400 recovery for OpenAI-compatible gateways that reject an unsupported
//! `reasoning_effort` value (e.g. `qwen3.8-flash-next` rejecting `high` while advertising
//! `xhigh (default), medium, and low`).

use hrn_kernel::ThinkingLevel;
use serde_json::Value;

const EFFORT_RANK: &[(&str, i32)] = &[
    ("none", 0),
    ("minimal", 1),
    ("low", 2),
    ("medium", 3),
    ("high", 4),
    ("xhigh", 5),
    ("max", 6),
];

const fn level_rank(level: ThinkingLevel) -> i32 {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Auto => 0,
        ThinkingLevel::Minimal => 1,
        ThinkingLevel::Low => 2,
        ThinkingLevel::Medium => 3,
        ThinkingLevel::High => 4,
        ThinkingLevel::Xhigh => 5,
        ThinkingLevel::Max => 6,
    }
}

const fn level_effort_str(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Auto => None,
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::Xhigh => Some("xhigh"),
        ThinkingLevel::Max => Some("max"),
    }
}

/// Proactively clamp `requested` against a model's declared `thinking.efforts` list, reusing
/// the same rank/nearest tie-breaking as [`pick_nearest_effort`].
///
/// Returns `None` for [`ThinkingLevel::Off`], [`ThinkingLevel::Auto`], or an empty `supported` list.
pub(crate) fn clamp_effort_to_supported(
    requested: ThinkingLevel,
    supported: &[ThinkingLevel],
) -> Option<&'static str> {
    if matches!(requested, ThinkingLevel::Off | ThinkingLevel::Auto) || supported.is_empty() {
        return None;
    }
    let names: Vec<&'static str> = supported
        .iter()
        .copied()
        .filter_map(level_effort_str)
        .collect();
    pick_nearest_effort(&names, requested)
}

/// Recover once from HTTP 400 rejecting `reasoning_effort` or `reasoning.effort`:
/// remap to the nearest advertised effort, or strip the field when none is advertised.
pub(crate) fn adjust_rejected_reasoning_effort(
    payload: &mut Value,
    requested: ThinkingLevel,
    error_body: &str,
) -> bool {
    let Some(obj) = payload.as_object_mut() else {
        return false;
    };
    let current = obj
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| {
            obj.get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned);
    let Some(current) = current else {
        return false;
    };
    let lower = error_body.to_ascii_lowercase();
    let mentions_effort = lower.contains("reasoning effort")
        || lower.contains("reasoning_effort")
        || lower.contains("reasoning.effort");
    let mentions_reject = lower.contains("unexpected")
        || lower.contains("unsupported")
        || lower.contains("invalid")
        || lower.contains("not supported")
        || lower.contains("supported")
        || lower.contains("allowed")
        || lower.contains("unrecognized");
    if !mentions_effort || !mentions_reject {
        return false;
    }

    let advertised = extract_supported_efforts(&lower, &current);
    let replacement = pick_nearest_effort(&advertised, requested);
    if obj.contains_key("reasoning_effort") {
        match replacement {
            Some(effort) => {
                obj.insert("reasoning_effort".into(), Value::String(effort.to_owned()));
            }
            None => {
                obj.remove("reasoning_effort");
            }
        }
    } else if let Some(r) = obj.get_mut("reasoning").and_then(Value::as_object_mut) {
        match replacement {
            Some(effort) => {
                r.insert("effort".into(), Value::String(effort.to_owned()));
            }
            None => {
                obj.remove("reasoning");
            }
        }
    }
    true
}

fn extract_supported_efforts(lower_body: &str, rejected: &str) -> Vec<&'static str> {
    let bytes = lower_body.as_bytes();
    let mut starts: Vec<usize> = Vec::new();
    for needle in [
        "supported",
        "allowed",
        "expected",
        "valid",
        "one of",
        "must be",
    ] {
        let mut offset = 0;
        while let Some(rel) = lower_body.get(offset..).and_then(|s| s.find(needle)) {
            let idx = offset + rel;
            let preceded_by_alpha = idx
                .checked_sub(1)
                .and_then(|p| bytes.get(p))
                .is_some_and(u8::is_ascii_alphabetic);
            if !preceded_by_alpha {
                starts.push(idx);
            }
            offset = idx + needle.len();
        }
    }
    starts.sort_unstable();
    starts.dedup();
    for start in starts {
        let Some(tail) = lower_body.get(start..) else {
            continue;
        };
        let clause = tail.split(['"', '.', ';', '}']).next().unwrap_or(tail);
        let mut found = Vec::new();
        for raw in clause.split(|c: char| !c.is_ascii_alphanumeric()) {
            if raw.is_empty() || raw.eq_ignore_ascii_case(rejected) {
                continue;
            }
            if let Some(&(canon, _)) = EFFORT_RANK.iter().find(|&&(k, _)| k == raw)
                && !found.contains(&canon)
            {
                found.push(canon);
            }
        }
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

fn pick_nearest_effort<'a>(supported: &[&'a str], requested: ThinkingLevel) -> Option<&'a str> {
    let target = level_rank(requested);
    supported
        .iter()
        .copied()
        .filter(|&k| k != "none" || target == 0)
        .min_by_key(|&k| {
            let rank = EFFORT_RANK
                .iter()
                .find(|&&(name, _)| name == k)
                .map_or(0, |&(_, r)| r);
            let dist = (rank - target).abs();
            // Break ties upward for high/xhigh/max, downward otherwise.
            let tie = if target >= 4 {
                i32::from(rank < target)
            } else {
                i32::from(rank > target)
            };
            (dist, tie)
        })
}
