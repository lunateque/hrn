//! CLI and TUI bridge for read-only Claude Code, Codex CLI, and OMP transcript import.

use crate::cli::{ImportArgs, ImportSourceArg};
use crate::commands::format_utc;
use crate::term::out;
use hrn_edge_store::import::sanitize_display_text;
use hrn_edge_store::{
    ExternalSession, ImportSource, SqliteStore, default_claude_dir, default_codex_dir,
    default_omp_dir, discover_all_with_omp, discover_claude, discover_codex, discover_omp_limited,
    import_claude, import_codex, import_omp,
};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_kernel::{HrnError, Result, WorkspaceFs};
use hrn_tui::ExternalSessionRow;
use std::path::PathBuf;

/// Maximum OMP transcripts scanned for interactive/CLI listing (newest by mtime).
const OMP_DISCOVER_LIMIT: usize = 200;

fn resolve_source_dir(source: ImportSource, override_dir: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(dir.clone());
    }
    let home = StdWorkspaceFs.home_dir();
    let dir = match source {
        ImportSource::Claude => default_claude_dir(home.as_deref()),
        ImportSource::Codex => default_codex_dir(home.as_deref()),
        ImportSource::Omp => default_omp_dir(home.as_deref()),
    };
    dir.ok_or_else(|| {
        HrnError::Config(format!(
            "cannot locate home directory for {} import; pass --dir <path>",
            source.as_str()
        ))
    })
}

fn format_external_row(s: &ExternalSession) -> String {
    let clean_id = sanitize_display_text(&s.id);
    let clean_cwd = sanitize_display_text(&s.cwd);
    let clean_title = sanitize_display_text(&s.title);
    let title = if clean_title.is_empty() {
        "(untitled)"
    } else {
        clean_title.as_str()
    };
    let cwd = if clean_cwd.is_empty() {
        "."
    } else {
        clean_cwd.as_str()
    };
    format!(
        "{}\t{}\t{}\t{}\t{}\n",
        s.source.as_str(),
        clean_id,
        format_utc(s.updated_at_ms),
        cwd,
        title
    )
}

/// `hrn sessions import claude|codex|omp [--latest] [--id <source_id>] [--dir <path>]`.
pub(crate) fn run_sessions_import(store: &SqliteStore, args: &ImportArgs) -> Result {
    let source = match args.source {
        ImportSourceArg::Claude => ImportSource::Claude,
        ImportSourceArg::Codex => ImportSource::Codex,
        ImportSourceArg::Omp => ImportSource::Omp,
    };
    let dir = resolve_source_dir(source, args.dir.as_ref())?;
    if !args.latest && args.id.is_none() {
        let sessions = match source {
            ImportSource::Claude => discover_claude(&dir)?,
            ImportSource::Codex => discover_codex(&dir)?,
            ImportSource::Omp => discover_omp_limited(&dir, OMP_DISCOVER_LIMIT)?,
        };
        if sessions.is_empty() {
            out(&format!(
                "no {} sessions found in {}\n",
                source.as_str(),
                dir.display()
            ));
            return Ok(());
        }
        out("SOURCE\tID\tUPDATED (UTC)\tCWD\tTITLE\n");
        for s in &sessions {
            out(&format_external_row(s));
        }
        out(&format!(
            "\nimport latest with: hrn sessions import {} --latest\n",
            source.as_str()
        ));
        return Ok(());
    }

    let wanted = if args.latest {
        None
    } else {
        args.id.as_deref()
    };
    let outcome = match source {
        ImportSource::Claude => import_claude(store, &dir, wanted)?,
        ImportSource::Codex => import_codex(store, &dir, wanted)?,
        ImportSource::Omp => import_omp(store, &dir, wanted)?,
    };
    out(&format!(
        "imported {} session {} -> {} ({} node(s))\nresume with: hrn chat --resume {}\n",
        outcome.source.as_str(),
        sanitize_display_text(&outcome.source_id),
        outcome.session.id,
        outcome.node_count,
        outcome.session.id
    ));
    Ok(())
}

/// `hrn chat --from-claude|--from-codex|--from-omp [id_or_latest]`.
pub(crate) fn import_for_chat(
    store: &SqliteStore,
    source: ImportSource,
    selector: &str,
) -> Result<String> {
    let dir = resolve_source_dir(source, None)?;
    let trimmed = selector.trim();
    let wanted =
        (!trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("latest")).then_some(trimmed);
    let outcome = match source {
        ImportSource::Claude => import_claude(store, &dir, wanted)?,
        ImportSource::Codex => import_codex(store, &dir, wanted)?,
        ImportSource::Omp => import_omp(store, &dir, wanted)?,
    };
    Ok(outcome.session.id)
}

/// Discover Claude, Codex, and OMP sessions for the TUI `/sessions` import sub-view (`I`).
pub(crate) fn discover_for_tui() -> Vec<ExternalSessionRow> {
    let home = StdWorkspaceFs.home_dir();
    let claude_dir = default_claude_dir(home.as_deref());
    let codex_dir = default_codex_dir(home.as_deref());
    let omp_dir = default_omp_dir(home.as_deref());
    discover_all_with_omp(
        claude_dir.as_deref(),
        codex_dir.as_deref(),
        omp_dir.as_deref(),
        Some(OMP_DISCOVER_LIMIT),
    )
    .into_iter()
    .map(|s| ExternalSessionRow {
        source: s.source.as_str().to_owned(),
        id: sanitize_display_text(&s.id),
        title: sanitize_display_text(&s.title),
        cwd: sanitize_display_text(&s.cwd),
        updated_at_ms: s.updated_at_ms,
        archived: s.archived,
    })
    .collect()
}

/// Import one external session selected inside the TUI `/sessions` import sub-view.
pub(crate) fn import_for_tui(store: &SqliteStore, source: &str, id: &str) -> Result<String> {
    let parsed = ImportSource::parse(source)
        .ok_or_else(|| HrnError::Config(format!("unknown import source `{source}`")))?;
    import_for_chat(store, parsed, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_external_row_strips_terminal_escapes_and_control_chars() {
        let row = ExternalSession {
            source: ImportSource::Claude,
            id: "sess\x1b[31m-1\tspoof".to_owned(),
            title: "\x1b]0;pwn\x07Safe\nTitle\x1b[0m".to_owned(),
            cwd: "/work/\x1b]52;c;abc\x1b\\repo\r\n".to_owned(),
            updated_at_ms: 1_775_000_000_000,
            path: PathBuf::from("dummy.jsonl"),
            archived: false,
        };
        let line = format_external_row(&row);
        assert!(!line.contains('\x1b'), "ESC leaked: {line:?}");
        assert!(!line.contains('\x07'), "BEL leaked: {line:?}");
        assert!(!line.contains('\r'), "CR leaked: {line:?}");
        let cols: Vec<&str> = line.trim_end_matches('\n').split('\t').collect();
        assert_eq!(cols.len(), 5, "tabs inside fields must not split TSV cols");
        assert_eq!(cols[1], "sess-1 spoof");
        assert_eq!(cols[3], "/work/repo");
        assert_eq!(cols[4], "Safe Title");
    }
}
