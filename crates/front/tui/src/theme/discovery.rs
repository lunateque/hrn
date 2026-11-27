//! Theme discovery (`<data>/themes/*.toml`), lookup precedence (built-ins > named user themes >
//! explicit file paths), and a pollable custom-theme file watcher that keeps last-good on error.

use super::palette::{built_in, built_in_names, reload_from_path};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

static THEMES_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Configure the profile-aware directory searched for named user themes (`<data>/themes`).
pub fn set_themes_dir(dir: Option<PathBuf>) {
    *THEMES_DIR
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = dir;
}

/// Currently configured user themes directory, if any.
#[must_use]
pub fn themes_dir() -> Option<PathBuf> {
    THEMES_DIR
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Metadata for one discoverable theme (built-in or user `<data>/themes/<name>.toml`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeEntry {
    /// Theme identifier accepted by `/theme <name>` and `[tui.theme]`.
    pub name: String,
    /// `true` for themes compiled into the binary; built-ins always win on name collision.
    pub built_in: bool,
    /// On-disk `.toml` path for user themes (`None` for built-in themes).
    pub path: Option<PathBuf>,
}

/// Where a resolved theme spec came from.
pub(crate) enum ThemeOrigin {
    BuiltIn { name: String, source: &'static str },
    UserNamed { name: String, path: PathBuf },
    FilePath(PathBuf),
}

/// Check whether `spec` is a plain theme name (safe to join with `<themes_dir>/<name>.toml`).
fn is_plain_name(spec: &str) -> bool {
    !spec.is_empty()
        && !spec.contains('/')
        && !spec.contains('\\')
        && !spec.contains("..")
        && !spec.contains(':')
}

/// Discover all built-in themes followed by sorted `*.toml` user themes in `dir`
/// (skipping any user file whose stem collides with a built-in name).
#[must_use]
pub fn discover_in(dir: Option<&Path>) -> Vec<ThemeEntry> {
    let built_ins = built_in_names();
    let mut entries: Vec<ThemeEntry> = built_ins
        .iter()
        .map(|name| ThemeEntry {
            name: name.clone(),
            built_in: true,
            path: None,
        })
        .collect();
    let Some(dir) = dir else {
        return entries;
    };
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return entries;
    };
    let mut user: Vec<ThemeEntry> = Vec::new();
    for item in read_dir.flatten() {
        let path = item.path();
        if !path.is_file() {
            continue;
        }
        let is_toml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("toml"));
        if !is_toml {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let stem = stem.trim();
        if stem.is_empty() || built_ins.iter().any(|b| b == stem) {
            continue;
        }
        user.push(ThemeEntry {
            name: stem.to_owned(),
            built_in: false,
            path: Some(path),
        });
    }
    user.sort_by(|a, b| a.name.cmp(&b.name));
    entries.extend(user);
    entries
}

/// Discover all available themes using the currently configured themes directory.
#[must_use]
pub fn available_themes() -> Vec<ThemeEntry> {
    let dir = themes_dir();
    discover_in(dir.as_deref())
}

/// Names of all available built-in and discovered user themes.
#[must_use]
pub fn available_theme_names() -> Vec<String> {
    available_themes().into_iter().map(|e| e.name).collect()
}

/// Names of all available built-in and user themes inside an explicit directory.
#[must_use]
pub fn available_theme_names_in(dir: Option<&Path>) -> Vec<String> {
    discover_in(dir).into_iter().map(|e| e.name).collect()
}

/// Resolve `spec` in strict lookup order:
/// 1. Built-in themes (name collision -> built-in wins)
/// 2. Named user theme (`<dir>/<spec>.toml`)
/// 3. Explicit file path
pub(crate) fn resolve_spec(spec: &str, dir: Option<&Path>) -> Result<ThemeOrigin, String> {
    if let Some(source) = built_in(spec) {
        return Ok(ThemeOrigin::BuiltIn {
            name: spec.to_owned(),
            source,
        });
    }
    if let Some(dir) = dir
        && is_plain_name(spec)
    {
        let stem = spec.strip_suffix(".toml").unwrap_or(spec);
        let candidate = dir.join(format!("{stem}.toml"));
        if candidate.is_file() {
            return Ok(ThemeOrigin::UserNamed {
                name: stem.to_owned(),
                path: candidate,
            });
        }
    }
    let path = PathBuf::from(spec);
    if path.is_file() {
        return Ok(ThemeOrigin::FilePath(path));
    }
    let available = available_theme_names_in(dir).join(", ");
    Err(format!(
        "theme `{spec}` not found (available: {available}) and cannot be read as a file"
    ))
}

/// Return `true` if `spec` resolves to a built-in theme, a named theme in `dir`, or an existing file.
#[must_use]
pub fn is_known_theme(spec: &str, dir: Option<&Path>) -> bool {
    let trimmed = spec.trim();
    !trimmed.is_empty() && resolve_spec(trimmed, dir).is_ok()
}

/// Fingerprint of a watched custom theme file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    exists: bool,
    len: u64,
    modified: Option<SystemTime>,
}

fn stamp_of(path: &Path) -> FileStamp {
    match std::fs::metadata(path) {
        Ok(meta) => FileStamp {
            exists: true,
            len: meta.len(),
            modified: meta.modified().ok(),
        },
        Err(_) => FileStamp {
            exists: false,
            len: 0,
            modified: None,
        },
    }
}

/// Pollable watcher for a single on-disk custom theme file.
/// Reloads valid edits into the active palette and keeps the last-good palette on error.
pub struct ThemeWatcher {
    path: PathBuf,
    themes_dir: Option<PathBuf>,
    last_stamp: FileStamp,
}

impl ThemeWatcher {
    /// Start watching `path`, recording its current metadata stamp.
    #[must_use]
    pub fn new(path: PathBuf, themes_dir: Option<PathBuf>) -> Self {
        let last_stamp = stamp_of(&path);
        Self {
            path,
            themes_dir,
            last_stamp,
        }
    }

    /// The path currently watched by this watcher.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-check the file on disk. Returns `None` when unchanged, `Some(Ok(()))` when reloaded,
    /// or `Some(Err(msg))` on read/parse failure (keeping the previously installed palette intact).
    pub fn poll(&mut self) -> Option<Result<(), String>> {
        let now = stamp_of(&self.path);
        if now == self.last_stamp {
            return None;
        }
        self.last_stamp = now;
        Some(reload_from_path(&self.path, self.themes_dir.as_deref()))
    }
}
