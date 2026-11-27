//! What a tool call looks like at a glance: its key argument for the one-line card and the
//! preview shown under an approval prompt.

use crate::diff;
use serde_json::Value;

/// The argument that identifies a call, e.g. the command of `bash` or the path of `edit`.
pub(crate) fn key_arg(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let pick = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);
    let text = match name {
        "bash" => pick("command"),
        "read" | "write" => pick("path"),
        "grep" | "glob" => pick("pattern").map(|pattern| match pick("path") {
            Some(path) => format!("{pattern}  in {path}"),
            None => pattern,
        }),
        "web_fetch" => pick("url"),
        "web_search" => pick("query"),
        "apply_patch" => pick("patch").map(|patch| patch_files(&patch)),
        // `edit` is hashline (`input`) or replace mode (`path`, `old`, `new`).
        "edit" => pick("input")
            .map(|input| hashline_files(&input))
            .or_else(|| pick("path")),
        "todo" => args
            .get("items")
            .and_then(Value::as_array)
            .map(|items| format!("{} items", items.len())),
        _ => first_string(&args),
    };
    one_line(&text.unwrap_or_default())
}

/// Files named by an `apply_patch` envelope (`*** Update File: path` and friends).
fn patch_files(patch: &str) -> String {
    let files: Vec<&str> = patch
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("*** ")?;
            ["Update File: ", "Add File: ", "Delete File: "]
                .iter()
                .find_map(|tag| rest.strip_prefix(tag))
        })
        .collect();
    match files.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [first, more @ ..] => format!("{first} +{} more", more.len()),
    }
}

/// Files named by the `[PATH#TAG]` headers of a hashline `edit` payload.
fn hashline_files(input: &str) -> String {
    let files: Vec<&str> = input
        .lines()
        .filter_map(|line| {
            let inner = line.trim().strip_prefix('[')?.strip_suffix(']')?;
            inner.rsplit_once('#').map(|(path, _)| path)
        })
        .collect();
    match files.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [first, more @ ..] => format!("{first} +{} more", more.len()),
    }
}

fn first_string(args: &Value) -> Option<String> {
    args.as_object()?
        .values()
        .find_map(Value::as_str)
        .map(str::to_owned)
}

/// Collapse `text` to its first non-empty line, marking that more followed.
pub(crate) fn one_line(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or_default();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_owned()
    }
}

/// What to show under an approval prompt.
pub(crate) enum Preview {
    /// A unified diff of the change.
    Diff(String),
    /// Text shown verbatim (a shell command, a patch).
    Text(String),
    /// Nothing beyond the summary line.
    None,
}

pub(crate) fn preview(name: &str, arguments: &str) -> Preview {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let pick = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    match name {
        "edit" if args.get("input").is_some() => Preview::Text(pick("input").to_owned()),
        "edit" => Preview::Diff(diff::unified(pick("old"), pick("new"))),
        "write" => Preview::Diff(diff::unified("", pick("content"))),
        "apply_patch" => Preview::Text(pick("patch").to_owned()),
        "bash" if pick("command").contains('\n') => Preview::Text(pick("command").to_owned()),
        _ => Preview::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_args_pick_the_identifying_field() {
        assert_eq!(
            key_arg("bash", r#"{"command":"cargo test\n--all"}"#),
            "cargo test …"
        );
        assert_eq!(
            key_arg("grep", r#"{"pattern":"fn main","path":"src"}"#),
            "fn main  in src"
        );
        assert_eq!(key_arg("custom", r#"{"n":1,"q":"hello"}"#), "hello");
        let patch = "*** Begin Patch\n*** Update File: a.rs\n*** Add File: b.rs\n*** End Patch";
        let args = serde_json::json!({ "patch": patch }).to_string();
        assert_eq!(key_arg("apply_patch", &args), "a.rs +1 more");
        let input = serde_json::json!({"input": "[a.rs#1A2B]\nPUT 1.=1:\n+x\n[b.rs#0000]\nREM"});
        assert_eq!(key_arg("edit", &input.to_string()), "a.rs +1 more");
        assert_eq!(key_arg("bash", "not json"), "");
    }

    #[test]
    fn edit_previews_are_diffs() {
        let args = r#"{"path":"a","old":"x\n","new":"y\n"}"#;
        assert!(
            matches!(preview("edit", args), Preview::Diff(d) if d.contains("-x") && d.contains("+y"))
        );
        assert!(matches!(preview("read", r#"{"path":"a"}"#), Preview::None));
    }
}
