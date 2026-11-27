//! Search scopes shared by `grep` and `glob`: a `;`-separated `path` list whose entries are
//! files, directories or globs (`src/**/*.rs`), plus `!glob` exclusions, walked with the
//! `gitignore` / `hidden` toggles the tools expose.

use crate::paths::{display_rel, resolve};
use crate::selector::{self, Selector};
use crate::walk::{self, WalkFlags};
use globset::{GlobBuilder, GlobMatcher};
use hrn_kernel::{HrnError, Result};
use ignore::DirEntry;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One root to walk.
pub(crate) struct Target {
    /// The existing file or directory the walk starts at.
    pub(crate) base: PathBuf,
    /// Matches paths relative to `base` (`/`-separated); `None` walks everything below it.
    pub(crate) glob: Option<GlobMatcher>,
    /// A glob written with a trailing `/` matches directories only.
    pub(crate) dirs_only: bool,
    /// Line windows of a `file:N-M` entry.
    pub(crate) selector: Selector,
}

impl Target {
    /// The target is one named file (no glob, no directory walk).
    pub(crate) fn is_file(&self) -> bool {
        self.glob.is_none() && self.base.is_file()
    }

    /// Whether the path relative to the base satisfies the glob (always true without one).
    pub(crate) fn matches(&self, rel: &str) -> bool {
        self.glob.as_ref().is_none_or(|g| g.is_match(rel))
    }
}

/// A `!glob` entry: drops matching files and folders below a named path.
#[derive(Clone)]
pub(crate) struct Exclude {
    matcher: GlobMatcher,
    /// Written with a `/` or `./`: matched against the cwd-relative path rather than any name.
    anchored: bool,
}

impl Exclude {
    fn parse(text: &str) -> Result<Self> {
        let (body, anchored) = match text.strip_prefix("./") {
            Some(rest) => (rest, true),
            None => (text, text.contains('/')),
        };
        Ok(Self {
            matcher: compile(body.trim_end_matches('/'), false)?,
            anchored,
        })
    }

    pub(crate) fn hits(&self, name: &str, cwd_rel: &str) -> bool {
        if self.anchored {
            self.matcher.is_match(cwd_rel)
        } else {
            self.matcher.is_match(name)
        }
    }
}

/// A parsed `path` argument.
pub(crate) struct Scope {
    pub(crate) targets: Vec<Target>,
    pub(crate) excludes: Arc<[Exclude]>,
    /// Entries that did not exist (only reported when at least one target did).
    pub(crate) missing: Vec<String>,
}

fn compile(pattern: &str, literal_separator: bool) -> Result<GlobMatcher> {
    GlobBuilder::new(pattern)
        .literal_separator(literal_separator)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| HrnError::Tool(format!("invalid glob `{pattern}`: {e}")))
}

fn has_glob_chars(text: &str) -> bool {
    text.contains(['*', '?', '[', '{'])
}

impl Scope {
    /// Parse `raw` (default `.`). With `selectors`, a `file:N-M` entry keeps its line windows.
    ///
    /// # Errors
    /// An empty entry, an invalid glob or selector, or no entry naming anything that exists.
    pub(crate) fn parse(cwd: &Path, raw: Option<&str>, selectors: bool) -> Result<Self> {
        let raw = raw.unwrap_or(".");
        let mut targets = Vec::new();
        let mut excludes = Vec::new();
        let mut missing = Vec::new();
        let mut listed = 0usize;
        let mut only_excludes = true;
        for entry in raw.split(';') {
            let entry = entry.trim();
            if entry.is_empty() {
                return Err(HrnError::Tool(
                    "`path` entries must be non-empty paths or globs".into(),
                ));
            }
            listed += 1;
            if let Some(body) = entry.strip_prefix('!')
                && !resolve(cwd, entry).exists()
            {
                excludes.push(Exclude::parse(&body.replace('\\', "/"))?);
                continue;
            }
            only_excludes = false;
            match Self::target(cwd, entry, selectors)? {
                Some(target) => targets.push(target),
                None => missing.push(entry.to_owned()),
            }
        }
        if only_excludes {
            targets.push(Target {
                base: cwd.to_path_buf(),
                glob: None,
                dirs_only: false,
                selector: Selector::default(),
            });
        } else if targets.is_empty() {
            let hint = if listed > 1 {
                "; list each target in the semicolon-delimited `path`"
            } else {
                ""
            };
            return Err(HrnError::Tool(format!(
                "Path not found: {}{hint}",
                missing.join(", ")
            )));
        }
        Ok(Self {
            targets,
            excludes: excludes.into(),
            missing,
        })
    }

    /// `Ok(None)` when the entry names nothing that exists.
    fn target(cwd: &Path, entry: &str, selectors: bool) -> Result<Option<Target>> {
        let literal = resolve(cwd, entry);
        if literal.exists() {
            return Ok(Some(plain(literal, Selector::default())));
        }
        let unified = entry.replace('\\', "/");
        if selectors {
            let (base, sel) = selector::split(&unified)?;
            if !sel.spans.is_empty() {
                let file = resolve(cwd, base);
                if file.is_file() {
                    return Ok(Some(plain(file, sel)));
                }
                return Err(HrnError::Tool(if has_glob_chars(base) {
                    format!("Line-range selector requires a single file, not a glob: {entry}")
                } else if file.is_dir() {
                    format!("Line-range selector requires a single file: {entry} is a directory")
                } else {
                    format!("Path not found for line-range selector: {entry}")
                }));
            }
        }
        if !has_glob_chars(&unified) {
            return Ok(None);
        }
        let dirs_only = unified.ends_with('/');
        let unified = unified.trim_end_matches('/');
        let segments: Vec<&str> = unified.split('/').collect();
        let split_at = segments
            .iter()
            .position(|s| has_glob_chars(s))
            .unwrap_or_default();
        let (head, tail) = segments.split_at(split_at);
        let mut pattern = tail.join("/");
        let base = if head.is_empty() {
            if !pattern.starts_with("**/") && pattern != "**" {
                pattern.insert_str(0, "**/");
            }
            cwd.to_path_buf()
        } else {
            let joined = head.join("/");
            // `/src/*.rs` splits to an empty leading segment: that is the filesystem root.
            resolve(cwd, if joined.is_empty() { "/" } else { &joined })
        };
        if !base.is_dir() {
            return Ok(None);
        }
        Ok(Some(Target {
            base,
            glob: Some(compile(&pattern, true)?),
            dirs_only,
            selector: Selector::default(),
        }))
    }
}

fn plain(base: PathBuf, selector: Selector) -> Target {
    Target {
        base,
        glob: None,
        dirs_only: false,
        selector,
    }
}

/// Every existing file in the scope of `raw` (a `;` list of files, directories and globs with
/// `!glob` exclusions), gitignore-aware, hidden included, deduplicated across entries, in path
/// order. The shared entry point for callers that need file lists in hand (the `hrn-ast`
/// sidecar) rather than a streaming search.
///
/// # Errors
/// An unparseable or missing `path`, or cancellation.
pub fn scope_files(
    cwd: &Path,
    raw: Option<&str>,
    cancel: &hrn_kernel::CancelFlag,
) -> Result<Vec<PathBuf>> {
    let scope = Scope::parse(cwd, raw, false)?;
    let flags = WalkFlags::default();
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for target in &scope.targets {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        if target.is_file() {
            if seen.insert(target.base.clone()) {
                files.push(target.base.clone());
            }
            continue;
        }
        for (entry, rel) in walk_target(target, cwd, &scope.excludes, flags, &mut seen) {
            if cancel.is_cancelled() {
                return Err(HrnError::Cancelled);
            }
            if entry.file_type().is_some_and(|t| t.is_file()) && target.matches(&rel) {
                files.push(entry.path().to_path_buf());
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Walk one target, yielding every entry below its base (the base itself is not yielded for a
/// directory). Entries are deduplicated through `seen` across targets.
pub(crate) fn walk_target<'a>(
    target: &'a Target,
    cwd: &Path,
    excludes: &Arc<[Exclude]>,
    flags: WalkFlags,
    seen: &'a mut HashSet<PathBuf>,
) -> impl Iterator<Item = (DirEntry, String)> + 'a {
    let cwd = cwd.to_path_buf();
    let builder = walk::walker_with(&target.base, flags, {
        let excludes = Arc::clone(excludes);
        let cwd = cwd.clone();
        move |entry: &DirEntry| {
            if excludes.is_empty() {
                return false;
            }
            let name = entry.file_name().to_string_lossy();
            let rel = display_rel(&cwd, entry.path());
            excludes.iter().any(|e| e.hits(&name, &rel))
        }
    });
    builder.build().flatten().filter_map(move |entry| {
        if entry.depth() == 0 && !entry.file_type().is_some_and(|ft| ft.is_file()) {
            return None;
        }
        if !seen.insert(entry.path().to_path_buf()) {
            return None;
        }
        let rel = if entry.depth() == 0 {
            entry
                .path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            entry
                .path()
                .strip_prefix(&target.base)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/")
        };
        Some((entry, rel))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn names(dir: &TempDir, raw: &str) -> Vec<String> {
        let scope = Scope::parse(dir.path(), Some(raw), false).unwrap();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for target in &scope.targets {
            for (entry, rel) in walk_target(
                target,
                dir.path(),
                &scope.excludes,
                WalkFlags::default(),
                &mut seen,
            ) {
                if entry.file_type().is_some_and(|t| t.is_file()) && target.matches(&rel) {
                    out.push(display_rel(dir.path(), entry.path()));
                }
            }
        }
        out
    }

    #[test]
    fn exclusions_filter_below_named_paths_but_never_the_path_itself() {
        let dir = TempDir::new("scope-excl");
        dir.write("src/app.rs", "");
        dir.write("src/app.test.rs", "");
        dir.write("src/legacy/old.rs", "");
        dir.write("vendor/lib.rs", "");
        dir.write("gen/out.rs", "");
        assert_eq!(
            names(&dir, "src;!*.test.rs;!legacy"),
            ["src/app.rs"],
            "name exclusions match at any depth"
        );
        assert_eq!(
            names(&dir, "src/app.test.rs;!*.test.rs"),
            ["src/app.test.rs"],
            "a named file is never excluded"
        );
        let all = names(&dir, "!vendor;!./gen");
        assert!(
            !all.iter()
                .any(|p| p.starts_with("vendor/") || p.starts_with("gen/"))
        );
        assert!(all.contains(&"src/app.rs".to_owned()));
    }

    #[test]
    fn a_bare_glob_recurses_but_a_rooted_one_does_not() {
        let dir = TempDir::new("scope-glob");
        dir.write("a.rs", "");
        dir.write("src/b.rs", "");
        dir.write("src/deep/c.rs", "");
        assert_eq!(names(&dir, "*.rs"), ["a.rs", "src/b.rs", "src/deep/c.rs"]);
        assert_eq!(names(&dir, "src/*.rs"), ["src/b.rs"]);
        assert_eq!(names(&dir, "src/**/*.rs"), ["src/b.rs", "src/deep/c.rs"]);
    }

    #[test]
    fn missing_entries_are_reported_unless_all_are_missing() {
        let dir = TempDir::new("scope-missing");
        dir.write("a.txt", "");
        let scope = Scope::parse(dir.path(), Some("a.txt;nope"), false).unwrap();
        assert_eq!(scope.missing, ["nope"]);
        let err = Scope::parse(dir.path(), Some("nope;nada"), false)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("Path not found: nope, nada"), "{err}");
        assert!(Scope::parse(dir.path(), Some("a.txt;;b"), false).is_err());
    }
}
