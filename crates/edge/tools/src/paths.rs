//! Path helpers shared by the file tools.

use std::path::{Path, PathBuf};

/// Resolve a user path against the session `cwd`.
pub fn resolve(cwd: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// `path` relative to `base` (when inside it) with forward slashes, for model-facing output.
pub fn display_rel(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
