//! Where the workspace is and which files belong to it.
//!
//! Cargo reports Windows paths with backslashes and git always reports forward slashes, so every
//! path handed to a check is normalized to a workspace-relative path with `/`.

use crate::Outcome;
use std::path::{Path, PathBuf};

pub(crate) fn root() -> Outcome<PathBuf> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    let bytes = crate::process::output(
        "cargo",
        &["locate-project", "--workspace", "--message-format", "plain"],
        &cwd,
    )?;
    let text = String::from_utf8(bytes)
        .map_err(|error| format!("`cargo locate-project` wrote invalid UTF-8: {error}"))?;
    let manifest = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| "`cargo locate-project --workspace` printed no path".to_owned())?;
    let dir = Path::new(manifest)
        .parent()
        .ok_or_else(|| format!("`{manifest}` is not inside a directory"))?;
    std::path::absolute(dir).map_err(|error| format!("cannot resolve the workspace root: {error}"))
}

/// The tracked files, workspace-relative with `/` separators.
pub(crate) fn tracked_files(root: &Path) -> Outcome<Vec<String>> {
    let bytes = crate::process::output("git", &["ls-files", "-z"], root)?;
    let text = String::from_utf8(bytes)
        .map_err(|error| format!("`git ls-files` wrote invalid UTF-8: {error}"))?;
    let mut files = Vec::new();
    for entry in text.split('\0') {
        if entry.is_empty() {
            continue;
        }
        let normalized = entry.replace('\\', "/");
        if normalized.contains("/../") || normalized.starts_with("../") {
            return Err(format!(
                "`git ls-files` escaped the workspace: {normalized}"
            ));
        }
        files.push(normalized);
    }
    Ok(files)
}

pub(crate) fn read(root: &Path, relative: &str) -> Outcome<String> {
    let path = root.join(relative);
    std::fs::read_to_string(&path).map_err(|error| format!("cannot read {relative}: {error}"))
}

/// The ids of tracked `crates/modules/<id>/Cargo.toml` files, sorted.
pub(crate) fn module_ids(files: &[String]) -> Vec<String> {
    let mut ids: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let rest = path.strip_prefix("crates/modules/")?;
            let id = rest.strip_suffix("/Cargo.toml")?;
            (!id.is_empty() && !id.contains('/')).then(|| id.to_owned())
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}
