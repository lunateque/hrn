//! Grouped output of `grep` and `glob`: a prefix-folded directory tree, one `#` per nesting
//! level, directory headers ending in `/`, single-child directory chains folded into one header.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// One file of a grouped listing: its display path (`/`-separated, a trailing `/` marks a
/// matched directory), an optional snapshot tag, and its body rows.
pub(crate) struct Entry {
    pub(crate) path: String,
    pub(crate) tag: Option<String>,
    pub(crate) rows: Vec<String>,
}

/// A directory level: sub-directories by name (so listings come out ordered), and the indexes of
/// the entries that live in it.
#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    files: Vec<usize>,
    /// The directory itself was a match (a glob result ending in `/`).
    matched: bool,
}

fn build(entries: &[Entry]) -> Node {
    let mut root = Node::default();
    for (idx, entry) in entries.iter().enumerate() {
        let is_dir = entry.path.ends_with('/');
        let mut node = &mut root;
        let segments: Vec<&str> = entry.path.trim_end_matches('/').split('/').collect();
        let Some((last, parents)) = segments.split_last() else {
            continue;
        };
        for seg in parents {
            node = node.dirs.entry((*seg).to_owned()).or_default();
        }
        if is_dir {
            node.dirs.entry((*last).to_owned()).or_default().matched = true;
        } else {
            node.files.push(idx);
        }
    }
    root
}

/// How rows are laid out.
#[derive(Clone, Copy)]
pub(crate) enum Style {
    /// `grep`: file headers (`## name#TAG`) with body rows, blank line before each directory.
    Files,
    /// `glob`: bare file names under directory headers.
    Paths,
}

fn render(node: &Node, level: usize, entries: &[Entry], style: Style, out: &mut String) {
    for &idx in &node.files {
        let Some(entry) = entries.get(idx) else {
            continue;
        };
        let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
        match style {
            Style::Paths => {
                let _ = writeln!(out, "{name}");
            }
            Style::Files => {
                let tag = entry
                    .tag
                    .as_deref()
                    .map_or_else(String::new, |t| format!("#{t}"));
                let _ = writeln!(out, "{} {name}{tag}", "#".repeat(level + 1));
                for row in &entry.rows {
                    let _ = writeln!(out, "{row}");
                }
            }
        }
    }
    for (name, dir) in &node.dirs {
        let (name, dir) = fold(name, dir);
        if matches!(style, Style::Files) && !out.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(out, "{} {name}/", "#".repeat(level + 1));
        render(dir, level + 1, entries, style, out);
    }
}

/// Collapse `a/b/c/` chains that hold no entries of their own into one header.
fn fold<'a>(name: &str, dir: &'a Node) -> (String, &'a Node) {
    let mut name = name.to_owned();
    let mut cur = dir;
    while cur.files.is_empty() && cur.dirs.len() == 1 && !cur.matched {
        let Some((child, next)) = cur.dirs.first_key_value() else {
            break;
        };
        name.push('/');
        name.push_str(child);
        cur = next;
    }
    (name, cur)
}

/// The grouped text of `entries`, trailing newline removed.
pub(crate) fn grouped(entries: &[Entry], style: Style) -> String {
    let mut out = String::new();
    render(&build(entries), 0, entries, style, &mut out);
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, rows: &[&str]) -> Entry {
        Entry {
            path: path.to_owned(),
            tag: None,
            rows: rows.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    #[test]
    fn single_child_chains_fold_and_files_precede_subdirectories() {
        let entries = [
            file("crates/edge/tools/src/a.rs", &["*1:x"]),
            file("crates/edge/tools/src/hashline/mod.rs", &["*2:y"]),
            file("crates/kernel/src/ir.rs", &["*3:z"]),
        ];
        assert_eq!(
            grouped(&entries, Style::Files),
            "# crates/\n\n## edge/tools/src/\n### a.rs\n*1:x\n\n### hashline/\n#### mod.rs\n*2:y\n\n## kernel/src/\n### ir.rs\n*3:z"
        );
    }

    #[test]
    fn glob_paths_list_bare_names_and_matched_directories_become_headers() {
        let entries = [
            file("src/html.rs", &[]),
            file("src/hashline/", &[]),
            file("Cargo.toml", &[]),
        ];
        assert_eq!(
            grouped(&entries, Style::Paths),
            "Cargo.toml\n# src/\nhtml.rs\n## hashline/"
        );
    }
}
