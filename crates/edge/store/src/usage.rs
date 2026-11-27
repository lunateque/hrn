//! Per-call token and cost records: the data behind the status bar's `$` and `hrn stats`.

use crate::{SqliteStore, store_err};
use hrn_kernel::{Result, Usage, UsageRow};
use rusqlite::{Row, params};

/// A stored count (`SQLite` integers are signed) back to a token count.
fn count(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<u64>> {
    Ok(row
        .get::<_, Option<i64>>(idx)?
        .map(|v| u64::try_from(v).unwrap_or(0)))
}

/// A token count to the signed integer `SQLite` stores.
fn stored(tokens: u64) -> i64 {
    i64::try_from(tokens).unwrap_or(i64::MAX)
}

/// The summed columns `prompt, completion, total, cache_read, cache_write, reasoning, cost`
/// starting at `first`.
fn usage_columns(row: &Row<'_>, first: usize) -> rusqlite::Result<Usage> {
    Ok(Usage {
        prompt_tokens: count(row, first)?.unwrap_or(0),
        completion_tokens: count(row, first + 1)?.unwrap_or(0),
        total_tokens: count(row, first + 2)?.unwrap_or(0),
        cache_read_tokens: count(row, first + 3)?,
        cache_write_tokens: count(row, first + 4)?,
        reasoning_tokens: count(row, first + 5)?,
        cost_micros: count(row, first + 6)?,
    })
}

/// Sums of the usage columns, `NULL` (so `None`) where no call reported the figure.
const SUMS: &str = "SUM(prompt_tokens), SUM(completion_tokens), SUM(total_tokens), \
     SUM(cache_read_tokens), SUM(cache_write_tokens), SUM(reasoning_tokens), SUM(cost_micros)";

impl SqliteStore {
    pub(crate) fn insert_usage(
        &self,
        session_id: &str,
        at_ms: i64,
        model: &str,
        usage: &Usage,
    ) -> Result {
        self.lock()
            .execute(
                "INSERT INTO usage (session_id, at_ms, model, prompt_tokens, completion_tokens,
                     total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                     cost_micros)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    session_id,
                    at_ms,
                    model,
                    stored(usage.prompt_tokens),
                    stored(usage.completion_tokens),
                    stored(usage.total_tokens),
                    usage.cache_read_tokens.map(stored),
                    usage.cache_write_tokens.map(stored),
                    usage.reasoning_tokens.map(stored),
                    usage.cost_micros.map(stored),
                ],
            )
            .map_err(|e| store_err("record_usage", e))?;
        Ok(())
    }

    pub(crate) fn sum_session_usage(&self, session_id: &str) -> Result<Usage> {
        self.lock()
            .query_row(
                &format!("SELECT {SUMS} FROM usage WHERE session_id = ?1"),
                params![session_id],
                |row| usage_columns(row, 0),
            )
            .map_err(|e| store_err("session_usage", e))
    }

    /// Usage by day and model: of every call, or of `only_session`'s own calls.
    pub(crate) fn report_usage(&self, only_session: Option<&str>) -> Result<Vec<UsageRow>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT strftime('%Y-%m-%d', u.at_ms / 1000, 'unixepoch') AS day, u.model,
                        COUNT(DISTINCT CASE WHEN s.parent_session_id IS NULL
                                            THEN u.session_id END),
                        COUNT(*), {SUMS}
                 FROM usage u LEFT JOIN sessions s ON s.id = u.session_id
                 WHERE ?1 IS NULL OR u.session_id = ?1
                 GROUP BY day, u.model
                 ORDER BY day DESC, u.model"
            ))
            .map_err(|e| store_err("usage_report prepare", e))?;
        let rows = stmt
            .query_map(params![only_session], |row| {
                Ok(UsageRow {
                    day: row.get(0)?,
                    model: row.get(1)?,
                    sessions: count(row, 2)?.unwrap_or(0),
                    calls: count(row, 3)?.unwrap_or(0),
                    usage: usage_columns(row, 4)?,
                })
            })
            .map_err(|e| store_err("usage_report query", e))?;
        rows.map(|row| row.map_err(|e| store_err("usage_report row", e)))
            .collect()
    }
}
