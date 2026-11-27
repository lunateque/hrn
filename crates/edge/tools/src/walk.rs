//! Shared directory walking for `grep`, `glob` and directory trees: `.gitignore`-aware,
//! `.git`-skipping, and deterministic.

use ignore::{DirEntry, WalkBuilder};
use std::path::Path;

/// Heavy dependency, build-cache, and OS directories skipped during recursive walks unless
/// explicitly passed as the root path (`depth == 0`) or `gitignore` is off.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".nuget",
    ".next",
    ".nuxt",
    ".gradle",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "AppData",
    "Application Data",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// The switches `grep` and `glob` expose.
#[derive(Clone, Copy)]
pub(crate) struct WalkFlags {
    /// Honour `.gitignore` / `.ignore` and skip the heavy directories in [`SKIP_DIRS`].
    pub(crate) gitignore: bool,
    /// Include dotfiles and dot-directories.
    pub(crate) hidden: bool,
}

impl Default for WalkFlags {
    fn default() -> Self {
        Self {
            gitignore: true,
            hidden: true,
        }
    }
}

/// A walker over `root` that honors `.gitignore` / `.ignore` files **even outside a git
/// repository** (the `ignore` crate otherwise only applies them inside one), includes hidden
/// files, skips `.git` and heavy dependency/OS cache directories, and yields entries in path order.
pub(crate) fn walker(root: &Path) -> WalkBuilder {
    walker_with(root, WalkFlags::default(), |_| false)
}

/// [`walker`] with the caller's switches; `excluded` prunes matching entries below the root.
pub(crate) fn walker_with(
    root: &Path,
    flags: WalkFlags,
    excluded: impl Fn(&DirEntry) -> bool + Send + Sync + 'static,
) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!flags.hidden)
        .require_git(false)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
            if is_dir && let Some(name) = entry.file_name().to_str() {
                if name.eq_ignore_ascii_case(".git") {
                    return false;
                }
                if flags.gitignore && SKIP_DIRS.iter().any(|s| name.eq_ignore_ascii_case(s)) {
                    return false;
                }
            }
            !excluded(entry)
        })
        .sort_by_file_path(Ord::cmp);
    if !flags.gitignore {
        builder
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false);
    }
    builder
}
