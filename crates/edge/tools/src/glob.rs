//! `glob` tool: `.gitignore`-aware discovery over a `;`-separated list of paths and globs,
//! newest first, printed as a prefix-folded directory tree.

use crate::group::{self, Entry, Style};
use crate::paths::display_rel;
use crate::scope::{Scope, walk_target};
use crate::walk::WalkFlags;
use hrn_kernel::{CancelFlag, HrnError, Result, ToolOutput};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// Paths listed when the caller does not say.
pub const DEFAULT_LIMIT: usize = 200;
/// Most paths one call may ask for.
pub const MAX_LIMIT: usize = 200;
const SCAN_BUDGET: Duration = Duration::from_secs(5);

/// What the model asked for.
pub struct GlobRequest<'a> {
    /// The `;`-separated globs, files and directories (default `.`).
    pub path: Option<&'a str>,
    /// Include dotfiles.
    pub hidden: bool,
    /// Respect `.gitignore`.
    pub gitignore: bool,
    /// The requested cap (clamped to `1..=MAX_LIMIT`).
    pub limit: usize,
}

fn mtime(path: &Path) -> SystemTime {
    path.metadata()
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Paths matching the scope of `req.path`, newest first, at most `limit`.
///
/// # Errors
/// An unparseable or missing `path`, or cancellation.
pub fn run_glob(cwd: &Path, req: &GlobRequest<'_>, cancel: &CancelFlag) -> Result<ToolOutput> {
    let scope = Scope::parse(cwd, req.path, false)?;
    let limit = req.limit.clamp(1, MAX_LIMIT);
    let flags = WalkFlags {
        gitignore: req.gitignore,
        hidden: req.hidden,
    };
    let started = Instant::now();
    let mut timed_out = false;
    let mut seen = HashSet::new();
    let mut found: Vec<(SystemTime, String)> = Vec::new();
    'scan: for target in &scope.targets {
        if target.is_file() {
            if seen.insert(target.base.clone()) {
                found.push((mtime(&target.base), display_rel(cwd, &target.base)));
            }
            continue;
        }
        for (entry, rel) in walk_target(target, cwd, &scope.excludes, flags, &mut seen) {
            if cancel.is_cancelled() {
                return Err(HrnError::Cancelled);
            }
            if started.elapsed() >= SCAN_BUDGET {
                timed_out = true;
                break 'scan;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let wanted = if target.glob.is_none() {
                !is_dir
            } else {
                target.matches(&rel) && (is_dir || !target.dirs_only)
            };
            if wanted {
                let mut shown = display_rel(cwd, entry.path());
                if is_dir {
                    shown.push('/');
                }
                found.push((mtime(entry.path()), shown));
            }
        }
    }

    let mut sections: Vec<String> = Vec::new();
    if found.is_empty() {
        sections.push(if timed_out {
            "Scan timed out after 5s before any file matched; this is NOT proof of absence. \
             Search a deeper directory."
                .to_owned()
        } else {
            "No files found matching pattern".to_owned()
        });
    } else {
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total = found.len();
        found.truncate(limit);
        let entries: Vec<Entry> = found
            .into_iter()
            .map(|(_, path)| Entry {
                path,
                tag: None,
                rows: Vec::new(),
            })
            .collect();
        sections.push(group::grouped(&entries, Style::Paths));
        if total > limit {
            sections.push(if limit < MAX_LIMIT {
                format!(
                    "[{limit} results limit reached. Use limit={} for more]",
                    (limit * 2).min(MAX_LIMIT)
                )
            } else {
                format!("[{limit} results limit reached]")
            });
        }
        if timed_out {
            sections.push(
                "Scan timed out after 5s; results are partial. Search a deeper directory."
                    .to_owned(),
            );
        }
    }
    if req.limit > MAX_LIMIT {
        sections.push(format!(
            "Requested limit {} clamped to the max of {MAX_LIMIT}",
            req.limit
        ));
    }
    if !scope.missing.is_empty() {
        sections.push(format!(
            "Skipped missing paths: {}",
            scope.missing.join(", ")
        ));
    }
    Ok(ToolOutput::ok(sections.join("\n\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn glob(dir: &TempDir, path: &str, limit: usize, gitignore: bool, hidden: bool) -> String {
        run_glob(
            dir.path(),
            &GlobRequest {
                path: Some(path),
                hidden,
                gitignore,
                limit,
            },
            &CancelFlag::new(),
        )
        .unwrap()
        .content
    }

    #[test]
    fn globs_group_by_directory_and_directories_end_in_a_slash() {
        let dir = TempDir::new("glob-tree");
        dir.write("Cargo.toml", "");
        dir.write("src/lib.rs", "");
        dir.write("src/read_fmt/mod.rs", "");
        let all = glob(&dir, "**/*.rs", 50, true, true);
        assert!(all.starts_with("# src/\n"), "{all}");
        assert!(
            all.contains("lib.rs") && all.contains("## read_fmt/\nmod.rs"),
            "{all}"
        );
        let dirs = glob(&dir, "src/*_fmt/", 50, true, true);
        assert_eq!(dirs, "# src/read_fmt/");
        assert_eq!(
            glob(&dir, "Cargo.toml;nope/*.rs", 50, true, true),
            "Cargo.toml\n\nSkipped missing paths: nope/*.rs"
        );
        assert_eq!(
            glob(&dir, "*.zzz", 50, true, true),
            "No files found matching pattern"
        );
    }

    #[test]
    fn limit_hidden_and_gitignore_toggles() {
        let dir = TempDir::new("glob-flags");
        dir.write(".gitignore", "ignored.txt\n");
        dir.write("a.txt", "");
        dir.write("ignored.txt", "");
        dir.write(".hidden.txt", "");
        dir.write("node_modules/pkg/index.js", "");
        let default = glob(&dir, "*.txt", 50, true, true);
        assert!(
            default.contains("a.txt") && default.contains(".hidden.txt"),
            "{default}"
        );
        assert!(!default.contains("ignored.txt"), "{default}");
        let all = glob(&dir, "*.*", 50, false, true);
        assert!(
            all.contains("ignored.txt") && all.contains("index.js"),
            "{all}"
        );
        let visible = glob(&dir, "*.txt", 50, true, false);
        assert!(!visible.contains(".hidden.txt"), "{visible}");
        let limited = glob(&dir, "*.txt", 1, true, true);
        assert!(
            limited.contains("[1 results limit reached. Use limit=2 for more]"),
            "{limited}"
        );
        assert!(glob(&dir, "*.txt", 500, true, true).contains("clamped to the max of 200"));
    }
}
