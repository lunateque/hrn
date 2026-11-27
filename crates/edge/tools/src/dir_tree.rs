//! Directory tree rendering for `read`: two levels deep, `.gitignore`-aware, most recently
//! modified first, with a cap on every child listing (`… N more`).

use crate::walk::walker;
use hrn_kernel::{HrnError, Result};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Levels listed below the root.
const MAX_DEPTH: usize = 2;
/// Entries listed per directory below the root; the root itself is complete.
const CHILD_LIMIT: usize = 12;

struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes < KB {
        format!("{bytes}B")
    } else if bytes < MB {
        format!("{}KB", bytes / KB)
    } else {
        let tenths = (bytes * 10) / MB;
        format!("{}.{}MB", tenths / 10, tenths % 10)
    }
}

fn age(modified: Option<SystemTime>, now: SystemTime) -> String {
    let Some(secs) = modified
        .and_then(|m| now.duration_since(m).ok())
        .map(|d| d.as_secs())
    else {
        return "just now".to_owned();
    };
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=2_591_999 => format!("{}d ago", secs / 86_400),
        2_592_000..=31_535_999 => format!("{}mo ago", secs / 2_592_000),
        _ => format!("{}y ago", secs / 31_536_000),
    }
}

/// Render `root` as tree lines; the first line is `shown/`.
///
/// # Errors
/// The root cannot be listed at all.
pub(crate) fn render(root: &Path, shown: &str) -> Result<Vec<String>> {
    let mut groups: HashMap<PathBuf, Vec<Entry>> = HashMap::new();
    let mut unreadable = 0usize;
    for item in walker(root).max_depth(Some(MAX_DEPTH)).build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(err) if entry_is_root_failure(&err) => {
                return Err(HrnError::Tool(format!("cannot list `{shown}`: {err}")));
            }
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let Some(parent) = entry.path().parent() else {
            continue;
        };
        let meta = entry.metadata().ok();
        groups.entry(parent.to_path_buf()).or_default().push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: entry.file_type().is_some_and(|t| t.is_dir()),
            size: meta.as_ref().map_or(0, std::fs::Metadata::len),
            modified: meta.and_then(|m| m.modified().ok()),
        });
    }
    for entries in groups.values_mut() {
        entries.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.name.cmp(&b.name))
        });
    }

    let mut lines = vec![format!("{}/", shown.trim_end_matches('/'))];
    if groups.is_empty() {
        lines.push("(empty directory)".to_owned());
    } else {
        let ctx = Ctx {
            groups: &groups,
            now: SystemTime::now(),
        };
        ctx.level(root, "", 1, &mut lines);
    }
    if unreadable > 0 {
        lines.push(format!("[{unreadable} entries could not be read]"));
    }
    Ok(lines)
}

/// A walk error at depth 0 means the root itself is unusable.
fn entry_is_root_failure(err: &ignore::Error) -> bool {
    matches!(err.depth(), Some(0))
        || err
            .io_error()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

struct Ctx<'a> {
    groups: &'a HashMap<PathBuf, Vec<Entry>>,
    now: SystemTime,
}

impl Ctx<'_> {
    fn level(&self, dir: &Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
        let Some(entries) = self.groups.get(dir) else {
            return;
        };
        let limit = if depth == 1 { usize::MAX } else { CHILD_LIMIT };
        let hidden = entries.len().saturating_sub(limit);
        let shown = entries.len() - hidden;
        for (i, entry) in entries.iter().take(shown).enumerate() {
            let last = i + 1 == shown && hidden == 0;
            let (branch, pad) = if last {
                ("└── ", "    ")
            } else {
                ("├── ", "│   ")
            };
            let mut line = format!("{prefix}{branch}{}", entry.name);
            if entry.is_dir {
                line.push('/');
                let _ = write!(line, " ({})", age(entry.modified, self.now));
            } else {
                let _ = write!(
                    line,
                    " ({}, {})",
                    format_size(entry.size),
                    age(entry.modified, self.now)
                );
            }
            out.push(line);
            if entry.is_dir && depth < MAX_DEPTH {
                self.level(
                    &dir.join(&entry.name),
                    &format!("{prefix}{pad}"),
                    depth + 1,
                    out,
                );
            }
        }
        if hidden > 0 {
            out.push(format!("{prefix}└── … {hidden} more"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn caps_child_listings_and_honors_gitignore() {
        let dir = TempDir::new("tree");
        dir.write(".gitignore", "secret/\n");
        dir.write("secret/key.txt", "x");
        dir.write("src/main.rs", "fn main() {}\n");
        for i in 0..15 {
            dir.write(&format!("src/f{i:02}.rs"), "x");
        }
        let text = render(dir.path(), "proj").unwrap().join("\n");
        assert!(text.starts_with("proj/\n"), "{text}");
        assert!(text.contains("src/"), "{text}");
        assert!(text.contains("… 4 more"), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains(".gitignore"), "{text}");
    }

    #[test]
    fn empty_directory_says_so() {
        let dir = TempDir::new("tree-empty");
        let lines = render(dir.path(), "e").unwrap();
        assert_eq!(lines, vec!["e/", "(empty directory)"]);
    }
}
