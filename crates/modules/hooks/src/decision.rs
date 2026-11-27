//! The hook-decision protocol: how a subprocess's stdout/exit code becomes one of hrn's
//! verdicts. Understands omp's shape (`{"blocked":"true","reason":…}`, `{"context":…}`,
//! `{"args":…}`) and Claude Code's (`hookSpecificOutput.permissionDecision`, `updatedInput`,
//! `additionalContext`, exit-2 blocks with stderr).

use hrn_kernel::serde_json::{self, Value};

/// A hook's outcome for one invocation.
pub(crate) enum Decision {
    Block(String),
    Proceed {
        arguments: Option<Value>,
        context: Option<String>,
    },
}

/// Parse one hook's stdout (+ exit code) into a decision, understanding the omp shape
/// (`{"blocked":"true","reason":…}`, `{"context":…}`, `{"args":…}`) and the Claude shape
/// (`{"hookSpecificOutput":{"permissionDecision":"deny","updatedInput":…}}`); exit code 2 blocks
/// with stderr as the reason.
pub(crate) fn interpret(code: Option<i32>, stdout: &str, stderr: &str) -> Decision {
    let value: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    let specific = value.get("hookSpecificOutput");
    let field = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .filter_map(|key| value.get(*key).or_else(|| specific?.get(*key)))
            .find_map(Value::as_str)
            .map(str::to_owned)
    };
    let blocked = matches!(
        value.get("decision").and_then(Value::as_str),
        Some("block" | "deny")
    ) || value
        .get("blocked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value.get("blocked").and_then(Value::as_str) == Some("true")
        || matches!(
            specific
                .and_then(|s| s.get("permissionDecision"))
                .and_then(Value::as_str),
            Some("deny")
        )
        || code == Some(2);
    let context = field(&["additionalContext", "context"]);
    let arguments = specific
        .and_then(|s| s.get("updatedInput"))
        .or_else(|| value.get("updatedInput"))
        .or_else(|| value.get("args"))
        .filter(|v| v.is_object())
        .cloned();
    if !blocked {
        // Claude's exit-1 semantics: a failed hook that did not block still gets its stderr
        // reported (as context), never silently dropped.
        let note = match (code, context.clone()) {
            (Some(0) | None, _) => context,
            (Some(other), existing) => {
                let stderr = stderr.trim();
                let detail = if stderr.is_empty() {
                    format!("hook exited with code {other}")
                } else {
                    format!("hook exited with code {other}: {stderr}")
                };
                Some(existing.map_or_else(|| detail.clone(), |c| format!("{c}\n{detail}")))
            }
        };
        return Decision::Proceed {
            arguments,
            context: note,
        };
    }
    let reason =
        field(&["reason", "permissionDecisionReason", "systemMessage"]).unwrap_or_else(|| {
            let err = stderr.trim();
            if err.is_empty() {
                "the hook asked for it".to_owned()
            } else {
                err.to_owned()
            }
        });
    Decision::Block(reason)
}
