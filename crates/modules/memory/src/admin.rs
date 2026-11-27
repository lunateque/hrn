//! Administrative `/memory` command surface (`view`, `stats`, `search`, `enqueue`, `clear`).

use crate::fmt::{PREVIEW_CHARS, bullet, utc_minute};
use crate::project::{GLOBAL, project_key};
use crate::query::match_expr;
use crate::redact::redact;
use crate::store::{Memory, NewMemory};
use hrn_kernel::{HrnError, Result};
use std::fmt::Write as _;
use std::path::Path;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// Execute a `/memory [view|stats|search|enqueue|clear] [args]` command against `db_path`.
///
/// Does not create `db_path` on read-only commands (`view`, `stats`, `search`, `clear`).
///
/// # Errors
///
/// Returns an error when the subcommand or arguments are invalid, or when the `SQLite` store fails.
pub fn run_command(
    db_path: &Path,
    cwd: &Path,
    session_id: Option<&str>,
    arg: Option<&str>,
    session_note_fallback: Option<&str>,
) -> Result<String> {
    let memory = Memory::new(db_path.to_path_buf());
    let proj = project_key(cwd);
    let trimmed = arg.map(str::trim).unwrap_or_default();
    let (sub, rest) = if trimmed.is_empty() {
        ("view", "")
    } else {
        trimmed
            .split_once(char::is_whitespace)
            .map_or((trimmed, ""), |(s, r)| (s, r.trim()))
    };
    match sub.to_ascii_lowercase().as_str() {
        "view" | "list" => view(&memory, &proj, rest),
        "stats" => stats(&memory, &proj),
        "search" => search(&memory, &proj, rest),
        "enqueue" => enqueue(&memory, &proj, cwd, session_id, rest, session_note_fallback),
        "clear" => clear(&memory, &proj, rest),
        other => Err(HrnError::Config(format!(
            "unknown /memory subcommand `{other}` — try: view, stats, search <query>, enqueue [note], clear [project|global|all]"
        ))),
    }
}

fn view(memory: &Memory, proj: &str, rest: &str) -> Result<String> {
    let limit = if rest.is_empty() {
        DEFAULT_LIMIT
    } else {
        rest.parse::<usize>()
            .map_err(|_| HrnError::Config(format!("invalid /memory view limit `{rest}`")))?
            .clamp(1, MAX_LIMIT)
    };
    let entries = memory.list_recent([proj, GLOBAL], limit)?;
    if entries.is_empty() {
        return Ok("No memories stored for this project or globally.".to_owned());
    }
    let mut out = format!("Memories ({} shown, newest first):\n", entries.len());
    for entry in &entries {
        let _ = writeln!(out, "{}", bullet(entry, PREVIEW_CHARS));
    }
    Ok(out.trim_end().to_owned())
}

fn stats(memory: &Memory, proj: &str) -> Result<String> {
    let st = memory.stats(proj)?;
    let status = if st.exists {
        format!("present ({} bytes)", st.db_bytes)
    } else {
        "not created yet".to_owned()
    };
    let updated = st.latest_updated_ms.map_or_else(
        || "never".to_owned(),
        |ms| format!("{} UTC", utc_minute(ms)),
    );
    Ok(format!(
        "Memory store: {}\nStatus: {status}\nProject scope: {proj}\nActive: {} ({} project, {} global)\nInvalidated: {}\nLast updated: {updated}",
        memory.path().display(),
        st.total_active,
        st.project_active,
        st.global_active,
        st.invalidated,
    ))
}

fn search(memory: &Memory, proj: &str, query: &str) -> Result<String> {
    if query.is_empty() {
        return Err(HrnError::Config("usage: /memory search <query>".to_owned()));
    }
    let Some(expr) = match_expr(query) else {
        return Ok(format!("No searchable terms in `{query}`."));
    };
    let entries = memory.search(&expr, [proj, GLOBAL], DEFAULT_LIMIT)?;
    if entries.is_empty() {
        return Ok(format!("No memories matching `{query}`."));
    }
    let mut out = format!("{} memory match(es) for `{query}`:\n", entries.len());
    for entry in &entries {
        let _ = writeln!(out, "{}", bullet(entry, PREVIEW_CHARS));
    }
    Ok(out.trim_end().to_owned())
}

fn enqueue(
    memory: &Memory,
    proj: &str,
    cwd: &Path,
    session_id: Option<&str>,
    rest: &str,
    fallback: Option<&str>,
) -> Result<String> {
    let raw = if !rest.is_empty() {
        rest
    } else if let Some(note) = fallback.map(str::trim).filter(|s| !s.is_empty()) {
        note
    } else {
        return Err(HrnError::Config(
            "nothing to enqueue — pass a note: /memory enqueue <note>".to_owned(),
        ));
    };
    let clean = redact(raw);
    if clean.trim().is_empty() {
        return Err(HrnError::Config("memory note cannot be empty".to_owned()));
    }
    let cwd_text = cwd.to_string_lossy();
    let ids = memory.retain(&[NewMemory {
        scope: proj,
        content: clean.trim(),
        context: "enqueued via /memory",
        source: "enqueue",
        importance: 0.6,
        session_id: session_id.unwrap_or(""),
        cwd: &cwd_text,
    }])?;
    let id = ids.first().map_or("?", String::as_str);
    Ok(format!("Enqueued memory {id} (project scope)."))
}

fn clear(memory: &Memory, proj: &str, rest: &str) -> Result<String> {
    let (label, scopes): (&str, Vec<&str>) = match rest.to_ascii_lowercase().as_str() {
        "" | "project" => ("project", vec![proj]),
        "global" => ("global", vec![GLOBAL]),
        "all" => ("project + global", vec![proj, GLOBAL]),
        other => {
            return Err(HrnError::Config(format!(
                "unknown /memory clear scope `{other}` — use project, global, or all"
            )));
        }
    };
    let count = memory.clear_scopes(&scopes)?;
    if count == 0 {
        Ok(format!("No memory entries to clear ({label} scope)."))
    } else {
        Ok(format!(
            "Cleared {count} memory entry(ies) ({label} scope)."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::run_command;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_db(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "hrn-mem-admin-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        (dir.join("memory.db"), dir)
    }

    #[test]
    fn read_commands_are_lazy_and_full_lifecycle_works() {
        let (db, dir) = temp_db("lifecycle");

        // View and stats do not create the SQLite file when absent.
        let empty_view = run_command(&db, &dir, None, None, None).expect("view");
        assert!(empty_view.contains("No memories"));
        let empty_stats = run_command(&db, &dir, None, Some("stats"), None).expect("stats");
        assert!(empty_stats.contains("not created yet"));
        assert!(!db.exists());

        // Enqueue stores redacted text and creates the database.
        let secret = format!("sk-{}", "a".repeat(24));
        let enqueued = run_command(
            &db,
            &dir,
            Some("sess-1"),
            Some(&format!("enqueue cargo test uses {secret} flag")),
            None,
        )
        .expect("enqueue");
        assert!(enqueued.contains("Enqueued memory"));
        assert!(db.exists());

        // View & search return the redacted entry.
        let shown = run_command(&db, &dir, Some("sess-1"), Some("view"), None).expect("view");
        assert!(shown.contains("[redacted]"), "{shown}");
        assert!(!shown.contains(&secret), "{shown}");
        let found = run_command(&db, &dir, None, Some("search cargo"), None).expect("search");
        assert!(found.contains("1 memory match"), "{found}");

        // Stats reflect the stored entry.
        let st = run_command(&db, &dir, None, Some("stats"), None).expect("stats");
        assert!(st.contains("Active: 1 (1 project, 0 global)"), "{st}");

        // Clear removes project entries.
        let cleared = run_command(&db, &dir, None, Some("clear"), None).expect("clear");
        assert!(cleared.contains("Cleared 1"), "{cleared}");
        let after = run_command(&db, &dir, None, Some("view"), None).expect("view after clear");
        assert!(after.contains("No memories"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enqueue_uses_session_fallback_when_no_note_given() {
        let (db, dir) = temp_db("fallback");
        let msg = run_command(
            &db,
            &dir,
            Some("s2"),
            Some("enqueue"),
            Some("remember to run just ci"),
        )
        .expect("enqueue fallback");
        assert!(msg.contains("Enqueued memory"));
        let shown = run_command(&db, &dir, None, Some("view"), None).expect("view");
        assert!(shown.contains("just ci"), "{shown}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
