//! Typed access to the loosely typed JSON arguments of the two tools.

use hrn_kernel::serde_json::Value;
use hrn_kernel::{HrnError, Result};

/// Non-empty trimmed string argument `key`.
pub(crate) fn opt<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Like [`opt`], but absence is an error naming the operation.
pub(crate) fn required<'a>(args: &'a Value, key: &str, op: &str) -> Result<&'a str> {
    opt(args, key).ok_or_else(|| HrnError::Tool(format!("`{op}` requires `{key}`")))
}

/// Boolean argument `key`, false when absent.
pub(crate) fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A string or array of strings, as a list of non-empty trimmed entries.
pub(crate) fn list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::String(s)) => vec![s.trim().to_owned()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_owned())
            .collect(),
        _ => Vec::new(),
    }
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect()
}

/// A positive count: floored, defaulting to `default`, clamped to `max`.
pub(crate) fn count(args: &Value, key: &str, default: usize, max: usize) -> Result<usize> {
    let Some(raw) = args.get(key).filter(|v| !v.is_null()) else {
        return Ok(default);
    };
    let whole = raw.as_u64().or_else(|| {
        raw.as_f64()
            .filter(|f| *f >= 1.0)
            .and_then(|f| f.floor().to_string().parse().ok())
    });
    whole
        .filter(|n| *n >= 1)
        .map(|n| usize::try_from(n).map_or(max, |n| n.min(max)))
        .ok_or_else(|| HrnError::Tool(format!("`{key}` must be a number greater than 0")))
}

/// An identifier that is passed to `gh` as a positional argument must not look like a flag.
pub(crate) fn no_flag<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    if value.starts_with('-') {
        Err(HrnError::Tool(format!(
            "{what} `{value}` must not start with `-`"
        )))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::serde_json::json;

    #[test]
    fn counts_are_floored_clamped_and_validated() {
        let a = json!({ "n": 7.9, "big": 900, "zero": 0, "neg": -2 });
        assert_eq!(count(&a, "n", 10, 50).expect("n"), 7);
        assert_eq!(count(&a, "big", 10, 50).expect("big"), 50);
        assert_eq!(count(&a, "missing", 10, 50).expect("default"), 10);
        assert!(count(&a, "zero", 10, 50).is_err());
        assert!(count(&a, "neg", 10, 50).is_err());
    }

    #[test]
    fn lists_accept_a_string_or_an_array() {
        let a = json!({ "one": " 5 ", "many": ["a", " ", "b"] });
        assert_eq!(list(&a, "one"), ["5"]);
        assert_eq!(list(&a, "many"), ["a", "b"]);
        assert!(list(&a, "none").is_empty());
    }
}
