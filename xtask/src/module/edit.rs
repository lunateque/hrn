//! Pure text edits behind `cargo xtask module`: every function returns the new file content or an
//! error naming the file and the anchor line it could not find, so a failed edit leaves every file
//! untouched by construction.
//!
//! Adding a line goes into the sorted block that starts right after the exact anchor line;
//! insertion and removal are inverses because removal matches the exact line, wherever insertion
//! had sorted it.

use crate::Outcome;

pub(crate) const ROOT_MANIFEST: &str = "Cargo.toml";
pub(crate) const BINARY_MANIFEST: &str = "crates/bins/harness/Cargo.toml";

const TUI_ROOT_DEP: &str = "hrn-tui = { path = \"crates/front/tui\" }";
const KERNEL_BINARY_DEP: &str = "hrn-kernel.workspace = true";
const MEMBERS_ANCHOR: &str = "    \"crates/front/tui\",";
const MEMBERS_PREFIX: &str = "    \"crates/modules/";

pub(crate) fn root_dependency(id: &str) -> String {
    format!("hrn-mod-{id} = {{ path = \"crates/modules/{id}\" }}")
}

pub(crate) fn binary_dependency(id: &str) -> String {
    format!("hrn-mod-{id} = {{ workspace = true, optional = true }}")
}

pub(crate) fn feature(id: &str) -> String {
    format!("{id} = [\"dep:hrn-mod-{id}\"]")
}

pub(crate) fn default_line(ids: &[String]) -> String {
    let quoted: Vec<String> = ids.iter().map(|id| format!("\"{id}\"")).collect();
    format!("default = [{}]", quoted.join(", "))
}

pub(crate) fn insert_root_dependency(text: &str, id: &str) -> Outcome<String> {
    insert_line(
        text,
        ROOT_MANIFEST,
        TUI_ROOT_DEP,
        &root_dependency(id),
        |line| line.starts_with("hrn-mod-"),
    )
}

pub(crate) fn remove_root_dependency(text: &str, id: &str) -> Outcome<String> {
    remove_line(text, ROOT_MANIFEST, &root_dependency(id))
}

pub(crate) fn insert_binary_dependency(text: &str, id: &str) -> Outcome<String> {
    insert_line(
        text,
        BINARY_MANIFEST,
        KERNEL_BINARY_DEP,
        &binary_dependency(id),
        |line| line.starts_with("hrn-mod-"),
    )
}

pub(crate) fn remove_binary_dependency(text: &str, id: &str) -> Outcome<String> {
    remove_line(text, BINARY_MANIFEST, &binary_dependency(id))
}

/// The `[features]` section after adding `id` to the sorted `ids`: first the `default` line is
/// rewritten to the new list, then `<id> = ["dep:..."]` joins the sorted feature block directly
/// below it.
pub(crate) fn insert_feature(text: &str, id: &str, ids: &[String]) -> Outcome<String> {
    let text = set_default(text, ids)?;
    let anchor = default_line(ids);
    insert_line(&text, BINARY_MANIFEST, &anchor, &feature(id), |line| {
        line.contains("[\"dep:hrn-mod-")
    })
}

pub(crate) fn remove_feature(text: &str, id: &str, ids: &[String]) -> Outcome<String> {
    let text = remove_line(text, BINARY_MANIFEST, &feature(id))?;
    set_default(&text, ids)
}

/// The root `[workspace]` `members` block: the sorted `crates/modules/<id>` lines are
/// (re)inserted right after the `crates/front/tui` line. An explicit list, not a
/// `crates/modules/*` glob, because cargo treats an unmatched glob as a literal path and the whole
/// workspace stops loading once the last module is removed.
pub(crate) fn set_members(text: &str, ids: &[String]) -> Outcome<String> {
    let mut sorted = ids.to_vec();
    sorted.sort();
    let keep_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !line.starts_with(MEMBERS_PREFIX))
        .map(str::to_owned)
        .collect();
    let at = lines
        .iter()
        .position(|line| line == MEMBERS_ANCHOR)
        .ok_or_else(|| format!("{ROOT_MANIFEST}: the line `{MEMBERS_ANCHOR}` is missing"))?;
    for (offset, id) in sorted.iter().enumerate() {
        lines.insert(at + 1 + offset, format!("{MEMBERS_PREFIX}{id}\","));
    }
    Ok(join(&lines, keep_newline))
}

fn set_default(text: &str, ids: &[String]) -> Outcome<String> {
    let replacement = default_line(ids);
    let keep_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let slot = lines
        .iter_mut()
        .find(|line| line.starts_with("default = "))
        .ok_or_else(|| format!("{BINARY_MANIFEST}: the line `default = [...]` is missing"))?;
    *slot = replacement;
    Ok(join(&lines, keep_newline))
}

fn insert_line(
    text: &str,
    file: &str,
    anchor: &str,
    line: &str,
    in_block: impl Fn(&str) -> bool,
) -> Outcome<String> {
    let mut lines: Vec<&str> = text.lines().collect();
    let keep_newline = text.ends_with('\n');
    let at = lines
        .iter()
        .position(|candidate| *candidate == anchor)
        .ok_or_else(|| format!("{file}: the line `{anchor}` is missing"))?;
    if lines.contains(&line) {
        return Err(format!("{file}: the line `{line}` already exists"));
    }
    let mut at = at + 1;
    while let Some(candidate) = lines.get(at) {
        if in_block(candidate) && *candidate < line {
            at += 1;
        } else {
            break;
        }
    }
    lines.insert(at, line);
    Ok(join(&lines, keep_newline))
}

fn remove_line(text: &str, file: &str, line: &str) -> Outcome<String> {
    let mut lines: Vec<&str> = text.lines().collect();
    let keep_newline = text.ends_with('\n');
    let at = lines
        .iter()
        .position(|candidate| *candidate == line)
        .ok_or_else(|| format!("{file}: the line `{line}` is missing"))?;
    lines.remove(at);
    Ok(join(&lines, keep_newline))
}

fn join<S: AsRef<str>>(lines: &[S], ends_with_newline: bool) -> String {
    let parts: Vec<&str> = lines.iter().map(AsRef::as_ref).collect();
    let mut text = parts.join("\n");
    if ends_with_newline {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> String {
        "[workspace.dependencies]\nserde_json = \"1\"\nhrn-tui = { path = \"crates/front/tui\" }\nhrn-mod-fs = { path = \"crates/modules/fs\" }\n\n[workspace.lints.rust]\n".to_owned()
    }

    fn binary() -> String {
        "[dependencies]\nhrn-kernel.workspace = true\nhrn-mod-fs = { workspace = true, optional = true }\nhrn-tui.workspace = true\n\n[features]\ndefault = [\"fs\"]\nfs = [\"dep:hrn-mod-fs\"]\n".to_owned()
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn root_dependency_inserts_sorted_and_removes() {
        let with_demo = insert_root_dependency(&root(), "demo").expect("anchor exists");
        assert!(with_demo.contains("hrn-tui = { path = \"crates/front/tui\" }\nhrn-mod-demo = { path = \"crates/modules/demo\" }\nhrn-mod-fs ="));
        let with_both = insert_root_dependency(&with_demo, "web").expect("anchor exists");
        assert!(with_both.contains("hrn-mod-fs = { path = \"crates/modules/fs\" }\nhrn-mod-web"));
        let back = remove_root_dependency(
            &remove_root_dependency(&with_both, "web").expect("present"),
            "demo",
        )
        .expect("present");
        assert_eq!(back, root());
    }

    #[test]
    fn binary_edits_add_and_remove_symmetrically() {
        let with_demo = insert_binary_dependency(&binary(), "demo").expect("anchor exists");
        let with_demo =
            insert_feature(&with_demo, "demo", &ids(&["demo", "fs"])).expect("anchor exists");
        assert!(with_demo.contains("hrn-kernel.workspace = true\nhrn-mod-demo = { workspace = true, optional = true }\nhrn-mod-fs ="));
        assert!(with_demo.contains(
            "default = [\"demo\", \"fs\"]\ndemo = [\"dep:hrn-mod-demo\"]\nfs = [\"dep:hrn-mod-fs\"]"
        ));
        let back = remove_binary_dependency(
            &remove_feature(&with_demo, "demo", &ids(&["fs"])).expect("present"),
            "demo",
        )
        .expect("present");
        assert_eq!(back, binary());
    }

    #[test]
    fn removing_the_last_feature_empties_default() {
        let text = remove_feature(&binary(), "fs", &ids(&[])).expect("anchor exists");
        assert!(text.contains("default = []\n"));
        assert!(!text.contains("fs = [\"dep:hrn-mod-fs\"]"));
    }

    #[test]
    fn a_missing_anchor_is_an_error_naming_the_line() {
        let error =
            insert_root_dependency("[workspace.dependencies]\n", "demo").expect_err("no anchor");
        assert!(error.contains("Cargo.toml"), "{error}");
        assert!(error.contains(TUI_ROOT_DEP), "{error}");
        let error = remove_feature(&binary(), "demo", &ids(&["fs"])).expect_err("no demo");
        assert!(error.contains("demo = [\"dep:hrn-mod-demo\"]"), "{error}");
    }

    #[test]
    fn a_duplicate_line_is_an_error() {
        let error = insert_root_dependency(&root(), "fs").expect_err("already there");
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn members_block_is_rewritten_sorted() {
        let text = "[workspace]\nmembers = [\n    \"crates/kernel\",\n    \"crates/front/tui\",\n    \"crates/modules/old\",\n    \"crates/bins/harness\",\n]\n";
        let with = set_members(text, &ids(&["b", "a"])).expect("anchor exists");
        assert_eq!(
            with,
            "[workspace]\nmembers = [\n    \"crates/kernel\",\n    \"crates/front/tui\",\n    \"crates/modules/a\",\n    \"crates/modules/b\",\n    \"crates/bins/harness\",\n]\n"
        );
        let without = set_members(&with, &ids(&[])).expect("anchor exists");
        assert_eq!(
            without,
            "[workspace]\nmembers = [\n    \"crates/kernel\",\n    \"crates/front/tui\",\n    \"crates/bins/harness\",\n]\n"
        );
        assert!(set_members("[workspace]\n", &ids(&[])).is_err());
    }
}
