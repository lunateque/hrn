//! Per-root-session agent artifacts stored under `<data>/sessions/<root>/artifacts/`:
//! `<id>.md` (final output), `<id>.json` (structured yield when present), `<id>.patch` (worktree
//! patch reserved for 10a), plus `.meta/<id>.json` metadata sidecars for resurrecting parked rows
//! and parent/child lineage across restarts.

use crate::agents::{AgentUsage, JobStatus};
use hrn_kernel::{HrnError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Longest agent slug kept per ID segment.
pub const NAME_MAX: usize = 32;

/// Sanitize a root session identifier into a safe single directory component.
pub fn sanitize_segment(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session".to_owned()
    } else {
        cleaned
    }
}

/// Convert a caller-chosen or profile name into one safe agent ID segment (no `.`, `/`, or `?`).
pub fn slugify_agent_name(raw: &str) -> String {
    let mut slug = String::with_capacity(raw.len().min(NAME_MAX));
    let mut last_dash = false;
    for ch in raw.trim().chars() {
        if slug.len() >= NAME_MAX {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_matches(|c| c == '-' || c == '_');
    if trimmed.is_empty() {
        "agent".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Allocate a unique agent ID (`slug`, uniquified with `-2`, `-3`, …; nested children are
/// `<parent>.<child>`). Top-level `"all"` is rewritten so it never collides with `agent://all`.
pub fn allocate_id(name: &str, parent: Option<&str>, taken: impl Fn(&str) -> bool) -> String {
    let mut leaf = slugify_agent_name(name);
    if parent.is_none() && leaf.eq_ignore_ascii_case("all") {
        "agent-all".clone_into(&mut leaf);
    }
    let base = match parent {
        Some(p) if !p.is_empty() => format!("{p}.{leaf}"),
        _ => leaf,
    };
    if !taken(&base) {
        return base;
    }
    for n in 2u64.. {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{base}-overflow")
}

/// Parent agent ID implied by a nested `<parent>.<child>` identifier.
pub fn parent_of_id(id: &str) -> Option<&str> {
    id.rsplit_once('.').map(|(parent, _)| parent)
}

/// Validate that `id` is a safe flat artifact stem (`[a-zA-Z0-9_-]` dot-separated segments).
fn validate_id(id: &str) -> Result<&str> {
    if id.is_empty() {
        return Err(HrnError::Tool("agent artifact id must not be empty".into()));
    }
    for seg in id.split('.') {
        if seg.is_empty()
            || !seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(HrnError::Tool(format!(
                "invalid agent artifact id `{id}`: segments must be non-empty ASCII alphanumerics, `-`, or `_`"
            )));
        }
    }
    Ok(id)
}

fn io_err(action: &str, path: &Path, err: &io::Error) -> HrnError {
    HrnError::Io(format!("cannot {action} `{}`: {err}", path.display()))
}

/// Serializable metadata sidecar stored at `<artifacts>/.meta/<id>.json` for restart resurrection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    /// Unique agent id (`<id>` or `<parent>.<child>`).
    pub id: String,
    /// Display/chosen name.
    pub name: String,
    /// Subagent profile name.
    pub profile: String,
    /// Session id that spawned this agent.
    pub owner: String,
    /// Root session id of this agent tree.
    pub root: String,
    /// Parent agent id if nested under another subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Short task description.
    pub task: String,
    /// Child session id backing this agent's transcript (`history://<session_id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Last recorded status (`parked` or `aborted` on resurrection).
    pub status: JobStatus,
    /// Creation timestamp (ms since epoch).
    pub started_ms: i64,
    /// Last settle timestamp (ms since epoch).
    pub finished_ms: i64,
    /// Generation counter (`>= 1`).
    pub generation: u32,
    /// Accumulated LLM/tool usage counters.
    #[serde(default)]
    pub usage: AgentUsage,
    /// Recorded `?progress` notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress_notes: Vec<String>,
    /// Path to captured `<id>.patch` worktree diff, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_path: Option<PathBuf>,
    /// Committed worktree branch (`hrn/task/<id>`), when `merge == "branch"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolated_branch: Option<String>,
}

/// One discovered artifact group on disk for a single agent ID.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredArtifact {
    /// Agent ID derived from the artifact filename stem.
    pub id: String,
    /// Root session ID passed to [`ArtifactStore::list_for_root`].
    pub root: String,
    /// Path to `<id>.md`, if present.
    pub md_path: Option<PathBuf>,
    /// Path to `<id>.json`, if present.
    pub json_path: Option<PathBuf>,
    /// Path to `<id>.patch`, if present.
    pub patch_path: Option<PathBuf>,
    /// Parsed `.meta/<id>.json` sidecar, if present and valid.
    pub meta: Option<ArtifactRecord>,
}

/// Filesystem-backed artifact store rooted at `<data>/sessions/<root>/artifacts/`.
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    data_dir: PathBuf,
}

impl ArtifactStore {
    /// Create a store rooted under `data_dir` (artifacts live at `<data_dir>/sessions/<root>/artifacts/`).
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    /// The configured data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Directory holding artifacts for `root`: `<data>/sessions/<root>/artifacts`.
    pub fn artifacts_dir(&self, root: &str) -> PathBuf {
        self.data_dir
            .join("sessions")
            .join(sanitize_segment(root))
            .join("artifacts")
    }

    /// Path to `<id>.md` (final output) for `root`.
    pub fn md_path(&self, root: &str, id: &str) -> Result<PathBuf> {
        let id = validate_id(id)?;
        Ok(self.artifacts_dir(root).join(format!("{id}.md")))
    }

    /// Path to `<id>.json` (structured yield) for `root`.
    pub fn json_path(&self, root: &str, id: &str) -> Result<PathBuf> {
        let id = validate_id(id)?;
        Ok(self.artifacts_dir(root).join(format!("{id}.json")))
    }

    /// Path to `<id>.patch` (worktree diff, reserved for 10a) for `root`.
    pub fn patch_path(&self, root: &str, id: &str) -> Result<PathBuf> {
        let id = validate_id(id)?;
        Ok(self.artifacts_dir(root).join(format!("{id}.patch")))
    }

    /// Path to `.meta/<id>.json` (resurrection metadata sidecar) for `root`.
    pub fn meta_path(&self, root: &str, id: &str) -> Result<PathBuf> {
        let id = validate_id(id)?;
        Ok(self
            .artifacts_dir(root)
            .join(".meta")
            .join(format!("{id}.json")))
    }

    /// Write `<id>.md` (always) and `<id>.json` (when `json_yield` is `Some`), returning the
    /// path to `<id>.md`.
    pub fn write_output(
        &self,
        root: &str,
        id: &str,
        markdown: &str,
        json_yield: Option<&Value>,
    ) -> Result<PathBuf> {
        let dir = self.artifacts_dir(root);
        fs::create_dir_all(&dir).map_err(|e| io_err("create", &dir, &e))?;
        let md = self.md_path(root, id)?;
        fs::write(&md, markdown).map_err(|e| io_err("write", &md, &e))?;
        if let Some(value) = json_yield {
            let jp = self.json_path(root, id)?;
            let pretty = serde_json::to_string_pretty(value)?;
            fs::write(&jp, pretty).map_err(|e| io_err("write", &jp, &e))?;
        }
        Ok(md)
    }

    /// Write `<id>.patch` (worktree unified diff) for `root`, returning its path.
    pub fn write_patch(&self, root: &str, id: &str, patch: &str) -> Result<PathBuf> {
        let dir = self.artifacts_dir(root);
        fs::create_dir_all(&dir).map_err(|e| io_err("create", &dir, &e))?;
        let file = self.patch_path(root, id)?;
        fs::write(&file, patch).map_err(|e| io_err("write", &file, &e))?;
        Ok(file)
    }

    /// Read `<id>.patch` if it exists on disk.
    pub fn read_patch(&self, root: &str, id: &str) -> Result<Option<String>> {
        let path = self.patch_path(root, id)?;
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err("read", &path, &e)),
        }
    }

    /// Write the resurrection metadata sidecar at `.meta/<id>.json`.
    pub fn write_meta(&self, root: &str, record: &ArtifactRecord) -> Result {
        let path = self.meta_path(root, &record.id)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err("create", parent, &e))?;
        }
        let pretty = serde_json::to_string_pretty(record)?;
        fs::write(&path, pretty).map_err(|e| io_err("write", &path, &e))
    }

    /// Read `<id>.md` if it exists on disk.
    pub fn read_md(&self, root: &str, id: &str) -> Result<Option<String>> {
        let path = self.md_path(root, id)?;
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err("read", &path, &e)),
        }
    }

    /// Read and parse `<id>.json` if it exists on disk.
    pub fn read_json(&self, root: &str, id: &str) -> Result<Option<Value>> {
        let path = self.json_path(root, id)?;
        let raw = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("read", &path, &e)),
        };
        let value = serde_json::from_str(&raw)
            .map_err(|e| HrnError::Tool(format!("invalid JSON in `{}`: {e}", path.display())))?;
        Ok(Some(value))
    }

    /// Read and parse `.meta/<id>.json` if it exists on disk.
    pub fn read_meta(&self, root: &str, id: &str) -> Result<Option<ArtifactRecord>> {
        let path = self.meta_path(root, id)?;
        let raw = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("read", &path, &e)),
        };
        let rec = serde_json::from_str(&raw)?;
        Ok(Some(rec))
    }

    /// Whether any artifact file (`.md`, `.json`, `.patch`, or `.meta`) already uses `id`.
    pub fn exists_id(&self, root: &str, id: &str) -> bool {
        self.md_path(root, id).is_ok_and(|p| p.exists())
            || self.json_path(root, id).is_ok_and(|p| p.exists())
            || self.patch_path(root, id).is_ok_and(|p| p.exists())
            || self.meta_path(root, id).is_ok_and(|p| p.exists())
    }

    /// Scan `<data>/sessions/<root>/artifacts/` and return every stored agent artifact, sorted by
    /// lineage depth (parents before children) then ID.
    pub fn list_for_root(&self, root: &str) -> Result<Vec<StoredArtifact>> {
        let dir = self.artifacts_dir(root);
        let mut by_id: BTreeMap<String, StoredArtifact> = BTreeMap::new();
        scan_artifacts_dir(&dir, root, &mut by_id)?;
        scan_meta_dir(&dir.join(".meta"), root, &mut by_id)?;
        let mut out: Vec<StoredArtifact> = by_id.into_values().collect();
        out.sort_by(|a, b| {
            let da = a.id.matches('.').count();
            let db = b.id.matches('.').count();
            da.cmp(&db).then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }
}

fn scan_artifacts_dir(
    dir: &Path,
    root: &str,
    by_id: &mut BTreeMap<String, StoredArtifact>,
) -> Result {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err("list", dir, &e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err("list", dir, &e))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        let Some((stem, ext)) = split_known_ext(file_name) else {
            continue;
        };
        if validate_id(stem).is_err() {
            continue;
        }
        let item = by_id
            .entry(stem.to_owned())
            .or_insert_with(|| StoredArtifact {
                id: stem.to_owned(),
                root: root.to_owned(),
                md_path: None,
                json_path: None,
                patch_path: None,
                meta: None,
            });
        match ext {
            "md" => item.md_path = Some(path),
            "json" => item.json_path = Some(path),
            "patch" => item.patch_path = Some(path),
            _ => {}
        }
    }
    Ok(())
}

fn scan_meta_dir(dir: &Path, root: &str, by_id: &mut BTreeMap<String, StoredArtifact>) -> Result {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err("list", dir, &e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err("list", dir, &e))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if validate_id(stem).is_err() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<ArtifactRecord>(&raw) else {
            continue;
        };
        let item = by_id
            .entry(stem.to_owned())
            .or_insert_with(|| StoredArtifact {
                id: stem.to_owned(),
                root: root.to_owned(),
                md_path: None,
                json_path: None,
                patch_path: None,
                meta: None,
            });
        item.meta = Some(meta);
    }
    Ok(())
}

fn split_known_ext(file_name: &str) -> Option<(&str, &'static str)> {
    if let Some(stem) = file_name.strip_suffix(".md") {
        Some((stem, "md"))
    } else if let Some(stem) = file_name.strip_suffix(".json") {
        Some((stem, "json"))
    } else if let Some(stem) = file_name.strip_suffix(".patch") {
        Some((stem, "patch"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("hrn-artifacts-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn slugify_uniquify_and_nested_ids() {
        assert_eq!(slugify_agent_name("Lint Run"), "lint-run");
        assert_eq!(slugify_agent_name("  foo.bar/baz  "), "foo-bar-baz");
        assert_eq!(slugify_agent_name("quick_task"), "quick_task");
        assert_eq!(slugify_agent_name("   "), "agent");

        let mut taken = BTreeSet::new();
        let a = allocate_id("Explore", None, |id| taken.contains(id));
        assert_eq!(a, "explore");
        taken.insert(a.clone());

        let b = allocate_id("Explore", None, |id| taken.contains(id));
        assert_eq!(b, "explore-2");
        taken.insert(b);

        let reserved = allocate_id("all", None, |id| taken.contains(id));
        assert_eq!(reserved, "agent-all");

        let child = allocate_id("worker", Some(&a), |id| taken.contains(id));
        assert_eq!(child, "explore.worker");
        assert_eq!(parent_of_id(&child), Some("explore"));
        taken.insert(child);

        let child2 = allocate_id("worker", Some(&a), |id| taken.contains(id));
        assert_eq!(child2, "explore.worker-2");
    }

    #[test]
    fn write_read_and_list_artifacts_per_root() {
        let dir = temp_dir("rw");
        let store = ArtifactStore::new(dir.clone());
        let md_path = store
            .write_output(
                "sess_root",
                "lead",
                "# Lead output",
                Some(&json!({"status": "ok", "items": [10, 20]})),
            )
            .expect("write lead");
        assert!(md_path.ends_with(Path::new("sessions/sess_root/artifacts/lead.md")));
        store
            .write_output("sess_root", "lead.worker", "child output", None)
            .expect("write child");

        assert_eq!(
            store.read_md("sess_root", "lead").unwrap().as_deref(),
            Some("# Lead output")
        );
        assert_eq!(
            store.read_json("sess_root", "lead").unwrap(),
            Some(json!({"status": "ok", "items": [10, 20]}))
        );
        assert_eq!(store.read_json("sess_root", "lead.worker").unwrap(), None);

        let listed = store.list_for_root("sess_root").unwrap();
        let ids: Vec<&str> = listed.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["lead", "lead.worker"]);
        assert!(listed[0].md_path.is_some() && listed[0].json_path.is_some());
        assert!(listed[1].md_path.is_some() && listed[1].json_path.is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}

// ===== merged from artifacts_gc.rs =====
// Confined orphan-artifact discovery and pruning for [`ArtifactStore`].
//
// Security invariant: every target directory and child entry is checked with
// [`std::fs::symlink_metadata`] and canonicalized inside `<data_dir>/sessions` before removal so
// symlinks can NEVER be followed outside `<data_dir>`.

use std::collections::HashSet;

/// One orphan artifact directory discovered under `<data_dir>/sessions/<root>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrphanArtifact {
    /// Sanitized root-session directory name.
    pub dir_name: String,
    /// Canonical path to `<data_dir>/sessions/<dir_name>`.
    pub path: PathBuf,
    /// Total bytes of regular files inside this directory (symlinks not followed).
    pub bytes: u64,
}

impl ArtifactStore {
    /// Scan `<data_dir>/sessions/*` for directories whose root session id is NOT in
    /// `live_session_ids`. Strictly confined inside `<data_dir>/sessions` with zero disk writes.
    pub fn scan_orphans(&self, live_session_ids: &[String]) -> Result<Vec<OrphanArtifact>> {
        let Some((sessions_canon, live_dirs)) = self.confined_sessions_root(live_session_ids)?
        else {
            return Ok(Vec::new());
        };
        let entries = match fs::read_dir(&sessions_canon) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(HrnError::Io(format!(
                    "list `{}`: {e}",
                    sessions_canon.display()
                )));
            }
        };
        let mut orphans = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|e| HrnError::Io(format!("read `{}`: {e}", sessions_canon.display())))?;
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            // Never follow symlinks when scanning or pruning orphan artifact directories.
            if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                continue;
            }
            let Ok(canon) = fs::canonicalize(&path) else {
                continue;
            };
            if !canon.starts_with(&sessions_canon) || canon == sessions_canon {
                continue;
            }
            let Some(name) = canon.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with('.') || live_dirs.contains(name) {
                continue;
            }
            let bytes = measure_dir_confined(&sessions_canon, &canon);
            orphans.push(OrphanArtifact {
                dir_name: name.to_owned(),
                path: canon,
                bytes,
            });
        }
        orphans.sort_by(|a, b| a.dir_name.cmp(&b.dir_name));
        Ok(orphans)
    }

    /// Remove all orphan artifact directories strictly confined inside `<data_dir>/sessions`,
    /// returning the count of removed directories.
    pub fn prune_orphans(&self, live_session_ids: &[String]) -> Result<usize> {
        let Some((sessions_canon, _)) = self.confined_sessions_root(live_session_ids)? else {
            return Ok(0);
        };
        let orphans = self.scan_orphans(live_session_ids)?;
        let mut removed = 0usize;
        for orphan in orphans {
            if remove_dir_confined(&sessions_canon, &orphan.path)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn confined_sessions_root(
        &self,
        live_session_ids: &[String],
    ) -> Result<Option<(PathBuf, HashSet<String>)>> {
        let data_dir = self.data_dir();
        if !data_dir.exists() {
            return Ok(None);
        }
        let data_canon = fs::canonicalize(data_dir)
            .map_err(|e| HrnError::Io(format!("canonicalize `{}`: {e}", data_dir.display())))?;
        let sessions_dir = data_dir.join("sessions");
        let meta = match fs::symlink_metadata(&sessions_dir) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(HrnError::Io(format!(
                    "stat `{}`: {e}",
                    sessions_dir.display()
                )));
            }
        };
        if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
            return Err(HrnError::Io(format!(
                "refused non-directory or symlink sessions root `{}`",
                sessions_dir.display()
            )));
        }
        let sessions_canon = fs::canonicalize(&sessions_dir)
            .map_err(|e| HrnError::Io(format!("canonicalize `{}`: {e}", sessions_dir.display())))?;
        if !sessions_canon.starts_with(&data_canon) {
            return Err(HrnError::Io(format!(
                "sessions root `{}` escapes data_dir `{}`",
                sessions_canon.display(),
                data_canon.display()
            )));
        }
        let live_dirs: HashSet<String> = live_session_ids
            .iter()
            .map(|id| sanitize_segment(id))
            .collect();
        Ok(Some((sessions_canon, live_dirs)))
    }
}

/// Measure regular-file bytes inside `target` without following any symlinks.
fn measure_dir_confined(base_canon: &Path, target: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(target) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.file_type().is_file() {
        return meta.len();
    }
    if !meta.file_type().is_dir() {
        return 0;
    }
    let Ok(canon) = fs::canonicalize(target) else {
        return 0;
    };
    if !canon.starts_with(base_canon) {
        return 0;
    }
    let Ok(entries) = fs::read_dir(&canon) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        total = total.saturating_add(measure_dir_confined(base_canon, &entry.path()));
    }
    total
}

/// Recursively remove `target` while re-verifying every step stays inside `base_canon` and never
/// follows symlinks into external directories.
fn remove_dir_confined(base_canon: &Path, target: &Path) -> Result<bool> {
    let meta = match fs::symlink_metadata(target) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(HrnError::Io(format!("stat `{}`: {e}", target.display()))),
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Ok(false);
    }
    let canon = fs::canonicalize(target)
        .map_err(|e| HrnError::Io(format!("canonicalize `{}`: {e}", target.display())))?;
    if !canon.starts_with(base_canon) || canon == base_canon {
        return Err(HrnError::Io(format!(
            "refused to prune path outside sessions root `{}`",
            target.display()
        )));
    }
    for entry in fs::read_dir(&canon)
        .map_err(|e| HrnError::Io(format!("read `{}`: {e}", canon.display())))?
    {
        let entry = entry.map_err(|e| HrnError::Io(format!("entry `{}`: {e}", canon.display())))?;
        let child = entry.path();
        let child_meta = fs::symlink_metadata(&child)
            .map_err(|e| HrnError::Io(format!("stat `{}`: {e}", child.display())))?;
        if child_meta.file_type().is_symlink() {
            fs::remove_file(&child)
                .or_else(|_| fs::remove_dir(&child))
                .map_err(|e| HrnError::Io(format!("remove symlink `{}`: {e}", child.display())))?;
        } else if child_meta.file_type().is_file() {
            fs::remove_file(&child)
                .map_err(|e| HrnError::Io(format!("remove `{}`: {e}", child.display())))?;
        } else if child_meta.file_type().is_dir() {
            remove_dir_confined(base_canon, &child)?;
        }
    }
    fs::remove_dir(&canon)
        .map_err(|e| HrnError::Io(format!("rmdir `{}`: {e}", canon.display())))?;
    Ok(true)
}
