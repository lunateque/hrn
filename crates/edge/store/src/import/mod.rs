//! Read-only transcript import from Claude Code (`~/.claude`), Codex CLI (`~/.codex`), and OMP (`~/.omp/agent`).
//!
//! Security & immutability invariants:
//! - Only invoked explicitly by the user (`hrn sessions import`, `hrn chat --from-claude|--from-codex|--from-omp`,
//!   or `I` / `Import` in the `/sessions` picker).
//! - Source directories, JSONL logs, and Codex `state_*.sqlite` files are NEVER modified and NEVER
//!   opened for writing (`SQLITE_OPEN_READ_ONLY` + `immutable=1` + `PRAGMA query_only = ON`).
//! - Credential files (`~/.claude/.credentials.json`, `~/.codex/auth.json`, `~/.gemini`,
//!   `auth-gateway-tokens/`, `.env`, `agent.db`, etc.) are strictly forbidden and refused before opening.
//! - Discovery scans read at most 64 KiB per JSONL file; full imports cap line and file sizes.
//! - Every import assigns fresh `hrn` session/node IDs and commits in a single `SQLite` transaction.

pub mod claude;
mod claude_parse;
pub mod codex;
mod codex_discover;
mod commit;
mod io;
pub mod omp;
mod omp_parse;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_omp;
#[cfg(test)]
mod tests_security;

use crate::SqliteStore;
use commit::commit_import;
use hrn_kernel::{
    AgentStatus, DagPayload, FinishReason, HrnError, Result, SessionMeta, TitleSource, Usage,
};
use std::path::{Path, PathBuf};

/// External harness transcript source supported for read-only import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportSource {
    /// Claude Code (`~/.claude/projects/<slug>/<uuid>.jsonl`).
    Claude,
    /// OpenAI Codex CLI (`~/.codex/sessions/**`, `~/.codex/archived_sessions/**`).
    Codex,
    /// OMP (`~/.omp/agent/sessions/<slug>/*.jsonl`).
    Omp,
}

impl ImportSource {
    /// Lowercase display/CLI identifier (`"claude"`, `"codex"`, or `"omp"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Omp => "omp",
        }
    }

    /// Parse a source name (`"claude"`, `"codex"`, or `"omp"`, case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "omp" => Some(Self::Omp),
            _ => None,
        }
    }
}

/// Summary row for one discovered external session (read-only metadata).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalSession {
    /// External source (`Claude` or `Codex`).
    pub source: ImportSource,
    /// Source-native session/thread identifier.
    pub id: String,
    /// Display title (custom title, AI title, thread title, or first prompt snippet).
    pub title: String,
    /// Resolved working directory of the session.
    pub cwd: String,
    /// Last updated timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Source JSONL path on disk.
    pub path: PathBuf,
    /// Whether this session came from an archived directory or archived thread row.
    pub archived: bool,
}

/// Result returned after committing one imported transcript into [`SqliteStore`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportOutcome {
    /// Newly created `hrn` session metadata (fresh session ID).
    pub session: SessionMeta,
    /// External transcript source.
    pub source: ImportSource,
    /// Source-native session identifier.
    pub source_id: String,
    /// Number of DAG nodes persisted in the transaction.
    pub node_count: usize,
    /// Number of usage rows persisted in the transaction.
    pub usage_records: usize,
    /// Normalized stop reason from the last assistant turn, if known.
    pub last_finish_reason: Option<FinishReason>,
}

/// One node prepared for transactional import (using local temporary IDs before remapping).
#[derive(Clone, Debug)]
pub(crate) struct ImportedNode {
    pub(crate) local_id: String,
    pub(crate) parent_local_id: Option<String>,
    pub(crate) created_at_ms: i64,
    pub(crate) payload: DagPayload,
}

/// One token-usage row prepared for transactional import.
#[derive(Clone, Debug)]
pub(crate) struct ImportedUsage {
    pub(crate) at_ms: i64,
    pub(crate) model: String,
    pub(crate) usage: Usage,
    /// Index into `ImportedTranscript::nodes` when recorded (supports rollback truncation).
    pub(crate) after_node_count: usize,
}

/// Complete parsed external transcript ready for atomic single-transaction commit.
#[derive(Clone, Debug)]
pub(crate) struct ImportedTranscript {
    pub(crate) source: ImportSource,
    pub(crate) source_id: String,
    pub(crate) title: String,
    pub(crate) title_source: Option<TitleSource>,
    pub(crate) cwd: String,
    pub(crate) branch: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) status: AgentStatus,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) head_local_id: Option<String>,
    pub(crate) nodes: Vec<ImportedNode>,
    pub(crate) usage: Vec<ImportedUsage>,
    pub(crate) last_finish_reason: Option<FinishReason>,
}

/// Sanitize untrusted external metadata for safe single-line terminal/TUI display by stripping
/// ANSI escape sequences, Unicode bidi overrides, and control characters, and collapsing runs of
/// whitespace/tabs/newlines into single spaces.
#[must_use]
pub fn sanitize_display_text(raw: &str) -> String {
    let cleaned = io::strip_control_and_escapes(raw, true);
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Resolve the default Claude configuration directory (`CLAUDE_CONFIG_DIR` else `<home>/.claude`).
#[must_use]
pub fn default_claude_dir(home: Option<&Path>) -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    home.map(|h| h.join(".claude"))
}

/// Resolve the default Codex home directory (`CODEX_HOME` else `<home>/.codex`).
#[must_use]
pub fn default_codex_dir(home: Option<&Path>) -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("CODEX_HOME") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    home.map(|h| h.join(".codex"))
}

/// Resolve the default OMP agent directory (`OMP_CODING_AGENT_DIR` / `OMP_AGENT_DIR` else `<home>/.omp/agent`).
#[must_use]
pub fn default_omp_dir(home: Option<&Path>) -> Option<PathBuf> {
    for var in ["OMP_CODING_AGENT_DIR", "OMP_AGENT_DIR"] {
        if let Ok(raw) = std::env::var(var) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    home.map(|h| h.join(".omp").join("agent"))
}

/// Discover Claude Code sessions under `claude_dir` (newest first).
///
/// # Errors
/// Returns an error if `claude_dir` is a forbidden path or cannot be read.
pub fn discover_claude(claude_dir: &Path) -> Result<Vec<ExternalSession>> {
    claude::discover(claude_dir)
}

/// Discover Codex CLI sessions under `codex_dir` (newest first).
///
/// # Errors
/// Returns an error if `codex_dir` is a forbidden path or cannot be read.
pub fn discover_codex(codex_dir: &Path) -> Result<Vec<ExternalSession>> {
    codex::discover(codex_dir)
}

/// Discover all OMP sessions under `omp_dir` (newest first).
///
/// # Errors
/// Returns an error if `omp_dir` is a forbidden path or cannot be read.
pub fn discover_omp(omp_dir: &Path) -> Result<Vec<ExternalSession>> {
    omp::discover(omp_dir, None)
}

/// Discover at most `limit` newest OMP sessions under `omp_dir` (fast mtime pre-truncation).
///
/// # Errors
/// Returns an error if `omp_dir` is a forbidden path or cannot be read.
pub fn discover_omp_limited(omp_dir: &Path, limit: usize) -> Result<Vec<ExternalSession>> {
    omp::discover(omp_dir, Some(limit))
}

/// Discover sessions from both optional Claude and Codex roots, sorted newest first.
#[must_use]
pub fn discover_all(claude_dir: Option<&Path>, codex_dir: Option<&Path>) -> Vec<ExternalSession> {
    discover_all_with_omp(claude_dir, codex_dir, None, None)
}

/// Discover sessions across optional Claude, Codex, and OMP roots, sorted newest first.
#[must_use]
pub fn discover_all_with_omp(
    claude_dir: Option<&Path>,
    codex_dir: Option<&Path>,
    omp_dir: Option<&Path>,
    omp_limit: Option<usize>,
) -> Vec<ExternalSession> {
    let mut out = Vec::new();
    if let Some(dir) = claude_dir
        && let Ok(list) = discover_claude(dir)
    {
        out.extend(list);
    }
    if let Some(dir) = codex_dir
        && let Ok(list) = discover_codex(dir)
    {
        out.extend(list);
    }
    if let Some(dir) = omp_dir
        && let Ok(list) = omp::discover(dir, omp_limit)
    {
        out.extend(list);
    }
    out.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Import a Claude Code session (latest when `session_id` is `None`, else exact or unique prefix)
/// into `store` inside one `SQLite` transaction.
///
/// # Errors
/// Returns an error if no matching session exists, the source fails security checks, or `SQLite` fails.
pub fn import_claude(
    store: &SqliteStore,
    claude_dir: &Path,
    session_id: Option<&str>,
) -> Result<ImportOutcome> {
    let sessions = discover_claude(claude_dir)?;
    let chosen = pick_external(&sessions, session_id, ImportSource::Claude)?;
    import_claude_file(store, claude_dir, &chosen.path)
}

/// Import a specific Claude Code JSONL file under `claude_dir` into `store` in one transaction.
///
/// # Errors
/// Returns an error if `jsonl_path` escapes `claude_dir`, targets a forbidden file, or `SQLite` fails.
pub fn import_claude_file(
    store: &SqliteStore,
    claude_dir: &Path,
    jsonl_path: &Path,
) -> Result<ImportOutcome> {
    let transcript = claude::parse_session(claude_dir, jsonl_path)?;
    commit_import(store, transcript)
}

/// Import a Codex CLI session (latest when `session_id` is `None`, else exact or unique prefix)
/// into `store` inside one `SQLite` transaction.
///
/// # Errors
/// Returns an error if no matching session exists, the source fails security checks, or `SQLite` fails.
pub fn import_codex(
    store: &SqliteStore,
    codex_dir: &Path,
    session_id: Option<&str>,
) -> Result<ImportOutcome> {
    let sessions = discover_codex(codex_dir)?;
    let chosen = pick_external(&sessions, session_id, ImportSource::Codex)?;
    let db_title = (!chosen.title.is_empty()).then_some(chosen.title.as_str());
    let db_cwd = (!chosen.cwd.is_empty()).then_some(chosen.cwd.as_str());
    let transcript = codex::parse_session(codex_dir, &chosen.path, db_title, db_cwd)?;
    commit_import(store, transcript)
}

/// Import a specific Codex rollout JSONL file under `codex_dir` into `store` in one transaction.
///
/// # Errors
/// Returns an error if `jsonl_path` escapes `codex_dir`, targets a forbidden file, or `SQLite` fails.
pub fn import_codex_file(
    store: &SqliteStore,
    codex_dir: &Path,
    jsonl_path: &Path,
) -> Result<ImportOutcome> {
    let transcript = codex::parse_session(codex_dir, jsonl_path, None, None)?;
    commit_import(store, transcript)
}

/// Import an OMP session (latest when `session_id` is `None`, else exact or unique prefix)
/// into `store` inside one `SQLite` transaction.
///
/// # Errors
/// Returns an error if no matching session exists, the source fails security checks, or `SQLite` fails.
pub fn import_omp(
    store: &SqliteStore,
    omp_dir: &Path,
    session_id: Option<&str>,
) -> Result<ImportOutcome> {
    let path = omp::resolve_session_path(omp_dir, session_id)?;
    import_omp_file(store, omp_dir, &path)
}

/// Import a specific OMP JSONL file under `omp_dir` into `store` in one transaction.
///
/// # Errors
/// Returns an error if `jsonl_path` escapes `omp_dir`, targets a forbidden file, or `SQLite` fails.
pub fn import_omp_file(
    store: &SqliteStore,
    omp_dir: &Path,
    jsonl_path: &Path,
) -> Result<ImportOutcome> {
    let transcript = omp::parse_session(omp_dir, jsonl_path)?;
    commit_import(store, transcript)
}

fn pick_external<'a>(
    sessions: &'a [ExternalSession],
    wanted: Option<&str>,
    source: ImportSource,
) -> Result<&'a ExternalSession> {
    if sessions.is_empty() {
        return Err(HrnError::Store(format!(
            "no {} sessions found to import",
            source.as_str()
        )));
    }
    let Some(raw) = wanted.map(str::trim).filter(|s| !s.is_empty()) else {
        return sessions
            .first()
            .ok_or_else(|| HrnError::Store(format!("no {} sessions found", source.as_str())));
    };
    if raw.eq_ignore_ascii_case("latest") {
        return sessions
            .first()
            .ok_or_else(|| HrnError::Store(format!("no {} sessions found", source.as_str())));
    }
    if let Some(exact) = sessions.iter().find(|s| s.id == raw) {
        return Ok(exact);
    }
    let matches: Vec<&ExternalSession> =
        sessions.iter().filter(|s| s.id.starts_with(raw)).collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => Err(HrnError::Store(format!(
            "no {} session matches `{raw}`",
            source.as_str()
        ))),
        many => Err(HrnError::Store(format!(
            "`{raw}` matches {} {} sessions; pass a longer id prefix",
            many.len(),
            source.as_str()
        ))),
    }
}
