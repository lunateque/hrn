//! Shared edit-mode coordination, generated-file glob matching, and TOML-to-JSON parsing
//! helpers exposed through `hrn-edge-tools` so `hrn-mod-fs`, `hrn-mod-edit`, and `hrn-mod-lsp`
//! stay strictly decoupled and individually removable.

use crate::edit_ladder::DEFAULT_FUZZY_THRESHOLD;
use crate::fs_edit::EditOpts;
use crate::paths::display_rel;
use globset::GlobBuilder;
use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{HrnError, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

/// Process-wide edit mode hints and shared edit guard policy stored in [`hrn_kernel::Shared`] by
/// `hrn-mod-edit` and consumed by `hrn-mod-fs` (`read` / `grep` / `write` / `apply_patch`),
/// `hrn-mod-lsp` (`format_on_write`), and binary wiring (`recover_inline_edits`).
pub struct EditModeHint {
    hashline_anchors: AtomicBool,
    recover_inline_edits: AtomicBool,
    enforce_seen_lines: AtomicBool,
    fuzzy_match: AtomicBool,
    fuzzy_threshold: Mutex<f32>,
    blocked_globs: Mutex<Vec<String>>,
    active_model: Mutex<Option<String>>,
    debug_notes: Mutex<Vec<String>>,
}

impl Default for EditModeHint {
    fn default() -> Self {
        Self {
            hashline_anchors: AtomicBool::new(true),
            recover_inline_edits: AtomicBool::new(true),
            enforce_seen_lines: AtomicBool::new(false),
            fuzzy_match: AtomicBool::new(true),
            fuzzy_threshold: Mutex::new(DEFAULT_FUZZY_THRESHOLD),
            blocked_globs: Mutex::new(Vec::new()),
            active_model: Mutex::new(None),
            debug_notes: Mutex::new(Vec::new()),
        }
    }
}

impl EditModeHint {
    /// Create a default hint (`hashline_anchors = true`, `recover_inline_edits = true`, default
    /// permissive guard policy when `hrn-mod-edit` is absent).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish `[edit]` guard & matching policy for sibling modules (`fs`, `lsp`) without
    /// introducing any cross-module compile-time dependency.
    pub fn set_edit_policy(
        &self,
        enforce_seen_lines: bool,
        blocked_globs: Vec<String>,
        fuzzy_match: bool,
        fuzzy_threshold: f32,
    ) {
        self.enforce_seen_lines
            .store(enforce_seen_lines, Ordering::Release);
        self.fuzzy_match.store(fuzzy_match, Ordering::Release);
        *self
            .fuzzy_threshold
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = fuzzy_threshold;
        *self
            .blocked_globs
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = blocked_globs;
    }

    /// Replace only the blocked auto-generated glob list (useful in unit tests).
    pub fn set_blocked_globs(&self, globs: Vec<String>) {
        *self
            .blocked_globs
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = globs;
    }

    /// Current auto-generated / lockfile globs refused by `write`, `apply_patch`, `edit`, and LSP formatting.
    #[must_use]
    pub fn blocked_globs(&self) -> Vec<String> {
        self.blocked_globs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether `apply_patch` / `edit` must refuse touching lines not yet displayed by `read`.
    #[must_use]
    pub fn enforce_seen_lines(&self) -> bool {
        self.enforce_seen_lines.load(Ordering::Acquire)
    }

    /// Snapshot full [`EditOpts`] from the shared `[edit]` policy.
    #[must_use]
    pub fn edit_opts(&self) -> EditOpts {
        EditOpts {
            fuzzy_match: self.fuzzy_match.load(Ordering::Acquire),
            fuzzy_threshold: *self
                .fuzzy_threshold
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            enforce_seen_lines: self.enforce_seen_lines(),
            blocked_globs: self.blocked_globs(),
        }
    }

    /// Record whether `read` and `grep` should emit `[path#TAG]` hashline anchors.
    pub fn set_hashline_anchors(&self, enabled: bool) {
        self.hashline_anchors.store(enabled, Ordering::Release);
    }

    /// Whether `read` and `grep` should emit `[path#TAG]` hashline anchors.
    #[must_use]
    pub fn hashline_anchors(&self) -> bool {
        self.hashline_anchors.load(Ordering::Acquire)
    }

    /// Record whether `turn.rs` should recover accidental inline edit blocks from assistant text.
    pub fn set_recover_inline_edits(&self, enabled: bool) {
        self.recover_inline_edits.store(enabled, Ordering::Release);
    }

    /// Whether inline edit recovery is enabled for turns.
    #[must_use]
    pub fn recover_inline_edits(&self) -> bool {
        self.recover_inline_edits.load(Ordering::Acquire)
    }

    /// Override the active model string inspected by `hrn-mod-edit` during registration.
    pub fn set_active_model(&self, model: Option<String>) {
        *self
            .active_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = model;
    }

    /// Current active model override, if explicitly set.
    #[must_use]
    pub fn active_model(&self) -> Option<String> {
        self.active_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Append a non-fatal diagnostic/debug note (e.g. ignored `magic` edit mode).
    pub fn push_debug_note(&self, note: impl Into<String>) {
        self.debug_notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(note.into());
    }

    /// Snapshot accumulated debug notes.
    #[must_use]
    pub fn debug_notes(&self) -> Vec<String> {
        self.debug_notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Canonicalize `path` (or its nearest existing ancestor + remaining tail) so symlinks pointing
/// to lockfiles or inside generated directories resolve to their real destination.
#[must_use]
pub(crate) fn canonicalize_for_guard(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    while let Some(name) = cur.file_name() {
        tail.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            break;
        };
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Ok(mut canon) = std::fs::canonicalize(parent) {
            for seg in tail.into_iter().rev() {
                canon.push(seg);
            }
            return canon;
        }
        cur = parent;
    }
    path.to_path_buf()
}

/// Match BOTH the lexical `path` and its canonicalized target path against `globs` so symlinks
/// pointing to lockfiles or auto-generated files cannot bypass `block_auto_generated`.
#[must_use]
pub(crate) fn match_generated_glob_canonical<'a>(
    path: &Path,
    cwd: &Path,
    globs: &'a [String],
) -> Option<&'a str> {
    if globs.is_empty() {
        return None;
    }
    if let Some(hit) = match_generated_glob(path, cwd, globs) {
        return Some(hit);
    }
    let canon_path = canonicalize_for_guard(path);
    let canon_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    match_generated_glob(&canon_path, &canon_cwd, globs)
}

/// Match `path` (relative to `cwd`, plus its basename) against a list of glob patterns for
/// lockfiles / auto-generated files. Returns the first matched glob verbatim.
#[must_use]
pub fn match_generated_glob<'a>(path: &Path, cwd: &Path, globs: &'a [String]) -> Option<&'a str> {
    let rel = display_rel(cwd, path).replace('\\', "/");
    let rel_trimmed = rel.trim_start_matches("./");
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(rel_trimmed);
    for raw in globs {
        let pat = raw.trim();
        if pat.is_empty() || pat.starts_with('#') {
            continue;
        }
        if glob_hits(pat, rel_trimmed, file_name) {
            return Some(raw.as_str());
        }
    }
    None
}

fn glob_hits(pat: &str, rel: &str, file_name: &str) -> bool {
    if let Some(dir) = pat.strip_suffix('/') {
        let direct = format!("{dir}/**");
        let nested = format!("**/{dir}/**");
        return compile_glob(&direct, false).is_some_and(|m| m.is_match(rel))
            || compile_glob(&nested, false).is_some_and(|m| m.is_match(rel));
    }
    if pat.contains('/') {
        let normalized = pat.trim_start_matches("./");
        return compile_glob(normalized, false).is_some_and(|m| m.is_match(rel));
    }
    compile_glob(pat, true).is_some_and(|m| m.is_match(file_name) || m.is_match(rel))
}

fn compile_glob(pattern: &str, literal_separator: bool) -> Option<globset::GlobMatcher> {
    GlobBuilder::new(pattern)
        .literal_separator(literal_separator)
        .case_insensitive(cfg!(windows))
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

/// Parse TOML text into a [`serde_json::Value`] so modules (`Layer::Module`) can read TOML files
/// cleanly without adding direct external crate dependencies.
///
/// # Errors
/// Returns `HrnError::Config` when `text` is invalid TOML or fails JSON value conversion.
pub fn parse_toml_to_json(text: &str) -> Result<Value> {
    let val: toml::Value =
        toml::from_str(text).map_err(|e| HrnError::Config(format!("invalid TOML: {e}")))?;
    serde_json::to_value(&val)
        .map_err(|e| HrnError::Config(format!("TOML-to-JSON conversion failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn generated_glob_matches_basename_and_nested_paths() {
        let globs = vec![
            "Cargo.lock".to_owned(),
            "*.pb.rs".to_owned(),
            "*.generated.*".to_owned(),
            "node_modules/".to_owned(),
        ];
        let cwd = PathBuf::from("/repo");
        assert_eq!(
            match_generated_glob(Path::new("/repo/Cargo.lock"), &cwd, &globs),
            Some("Cargo.lock")
        );
        assert_eq!(
            match_generated_glob(Path::new("/repo/sub/Cargo.lock"), &cwd, &globs),
            Some("Cargo.lock")
        );
        assert_eq!(
            match_generated_glob(Path::new("/repo/src/api.pb.rs"), &cwd, &globs),
            Some("*.pb.rs")
        );
        assert_eq!(
            match_generated_glob(Path::new("/repo/src/types.generated.ts"), &cwd, &globs),
            Some("*.generated.*")
        );
        assert_eq!(
            match_generated_glob(Path::new("/repo/node_modules/pkg/index.js"), &cwd, &globs),
            Some("node_modules/")
        );
        assert_eq!(
            match_generated_glob(Path::new("/repo/src/lib.rs"), &cwd, &globs),
            None
        );
    }

    #[test]
    fn toml_to_json_parses_tables_and_arrays() {
        let parsed = parse_toml_to_json(
            "idle_timeout_secs = 120\n[[servers]]\nname = \"demo\"\ncommand = \"demo-lsp\"\n",
        )
        .expect("parse");
        assert_eq!(parsed["idle_timeout_secs"], 120);
        assert_eq!(parsed["servers"][0]["name"], "demo");
    }
}
