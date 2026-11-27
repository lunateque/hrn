//! Shared auto-discovery locations: the ordered user/project layers every resource family
//! (`skills/`, `commands/`, `tools/`, `hooks/`, `rules/`, `agents/`) is read from. The kernel is
//! the only place that knows the list, so the harness (markdown resources) and module crates
//! (executables) agree on precedence.

use std::path::{Path, PathBuf};

/// The layer a discovered resource came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceLayer {
    /// User-global directory, labelled the way the user spells it (e.g. `~/.harness`).
    Global(String),
    /// Workspace-local directory (e.g. `.harness`).
    Project(String),
}

/// User-global roots, lowest precedence first; each is `<root>/<subdir>`.
const GLOBAL_ROOTS: [(&str, &[&str]); 6] = [
    ("~/.agents", &[".agents"]),
    ("~/.agent", &[".agent"]),
    ("~/.codex", &[".codex"]),
    ("~/.claude", &[".claude"]),
    ("~/.omp/agent", &[".omp", "agent"]),
    ("~/.harness", &[".harness"]),
];

/// Project roots, lowest precedence first; each is `<cwd>/<root>/<subdir>`.
const PROJECT_ROOTS: [&str; 4] = [".claude", ".agent", ".omp", ".harness"];

/// Ordered discovery directories for `subdir`, from lowest to highest precedence, so `~/.harness`
/// and the project `.harness` always win over the omp (`~/.omp/agent`, `.omp`) and foreign-tool
/// (`~/.claude`, `.claude`, `~/.agents`, `~/.agent`, `~/.codex`) layers. Only resource
/// subdirectories are ever named — never auth or secret files.
#[must_use]
pub fn resource_dirs(home: Option<&Path>, cwd: &Path, subdir: &str) -> Vec<(SourceLayer, PathBuf)> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        for (label, segments) in GLOBAL_ROOTS {
            let mut path = home.to_path_buf();
            for seg in segments {
                path.push(seg);
            }
            path.push(subdir);
            dirs.push((SourceLayer::Global(label.to_owned()), path));
        }
    }
    for label in PROJECT_ROOTS {
        dirs.push((
            SourceLayer::Project(label.to_owned()),
            cwd.join(label).join(subdir),
        ));
    }
    dirs
}
