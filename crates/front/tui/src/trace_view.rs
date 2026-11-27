//! `/trace` and `/trace export <file>`: render per-turn LLM and tool trace spans as an aligned
//! table, Markdown, or JSON.

use hrn_kernel::TraceRecord;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Render `records` as an aligned plain-text table grouped by turn for the TUI scrollable view.
pub(crate) fn format_trace_table(records: &[TraceRecord]) -> String {
    if records.is_empty() {
        return "No trace spans recorded in this session yet.\nRun a prompt to record LLM and tool spans."
            .to_owned();
    }
    let rows: Vec<[String; 6]> = records.iter().map(row_cells).collect();
    let headers = ["TURN", "KIND", "TARGET", "DETAILS", "LATENCY", "COST"];
    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
        headers[4].len(),
        headers[5].len(),
    ];
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:>w4$}  {:>w5$}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        headers[5],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        w5 = widths[5],
    );
    let rule_len = widths.iter().sum::<usize>() + 10;
    let _ = writeln!(out, "{}", "-".repeat(rule_len.min(120)));
    for row in &rows {
        let _ = writeln!(
            out,
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:>w4$}  {:>w5$}",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            row[5],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
            w5 = widths[5],
        );
    }
    let _ = write!(out, "\n{}", summary_line(records));
    out
}

/// Render `records` as a Markdown table plus totals for `/trace export <file>.md`.
pub(crate) fn format_trace_markdown(records: &[TraceRecord]) -> String {
    let mut out = String::from("# Session Trace\n\n");
    if records.is_empty() {
        out.push_str("_No trace spans recorded._\n");
        return out;
    }
    out.push_str("| Turn | Kind | Target | Details | Latency | Cost |\n");
    out.push_str("|------|------|--------|---------|--------:|-----:|\n");
    for rec in records {
        let [turn, kind, target, details, latency, cost] = row_cells(rec);
        let _ = writeln!(
            out,
            "| `{}` | `{}` | `{}` | {} | {} | {} |",
            escape_md(&turn),
            escape_md(&kind),
            escape_md(&target),
            escape_md(&details),
            escape_md(&latency),
            escape_md(&cost),
        );
    }
    let _ = writeln!(out, "\n{}", summary_line(records));
    out
}

/// Export `records` to `raw_path` (resolved against `cwd`), using pretty JSON for `.json` and
/// Markdown otherwise.
pub(crate) fn export_traces(
    records: &[TraceRecord],
    cwd: &Path,
    raw_path: &str,
) -> Result<PathBuf, String> {
    let trimmed = raw_path.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err("usage: /trace export <file>".to_owned());
    }
    let path = Path::new(trimmed);
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create `{}`: {e}", parent.display()))?;
    }
    let is_json = target
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    let body = if is_json {
        serde_json::to_string_pretty(records).map_err(|e| format!("serialize trace: {e}"))?
    } else {
        format_trace_markdown(records)
    };
    std::fs::write(&target, body).map_err(|e| format!("write `{}`: {e}", target.display()))?;
    Ok(target)
}

fn row_cells(rec: &TraceRecord) -> [String; 6] {
    let turn_raw = rec
        .payload
        .get("turn_id")
        .and_then(serde_json::Value::as_str)
        .or(rec.node_id.as_deref())
        .unwrap_or("-");
    let turn = short_turn(turn_raw);
    let latency = format_ms(u64::try_from(rec.dur_ms).ok());
    match rec.kind.as_str() {
        "llm" => {
            let model = rec
                .payload
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
                .to_owned();
            let prompt = rec
                .payload
                .get("prompt_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let completion = rec
                .payload
                .get("completion_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let reasoning = rec
                .payload
                .get("reasoning_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let mut details = format!(
                "in {} out {}",
                crate::render::tokens(prompt),
                crate::render::tokens(completion)
            );
            if reasoning > 0 {
                let _ = write!(details, " rsn {}", crate::render::tokens(reasoning));
            }
            let cost = rec
                .payload
                .get("cost_micros")
                .and_then(serde_json::Value::as_u64)
                .map_or_else(|| "-".to_owned(), crate::render::money);
            [turn, "llm".to_owned(), model, details, latency, cost]
        }
        "tool" => {
            let tool = rec
                .payload
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
                .to_owned();
            let is_err = rec
                .payload
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let status = if is_err { "error" } else { "ok" };
            let call_id = rec
                .payload
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let details = if call_id.is_empty() {
                status.to_owned()
            } else {
                format!("{status} ({})", short_turn(call_id))
            };
            [
                turn,
                "tool".to_owned(),
                tool,
                details,
                latency,
                "-".to_owned(),
            ]
        }
        other => [
            turn,
            other.to_owned(),
            "-".to_owned(),
            rec.payload.to_string(),
            latency,
            "-".to_owned(),
        ],
    }
}

fn summary_line(records: &[TraceRecord]) -> String {
    let mut llm_count = 0usize;
    let mut tool_count = 0usize;
    let mut tool_errors = 0usize;
    let mut total_ms = 0u64;
    let mut total_cost = 0u64;
    let mut priced = false;
    for rec in records {
        total_ms = total_ms.saturating_add(u64::try_from(rec.dur_ms).unwrap_or(0));
        if rec.kind == "llm" {
            llm_count += 1;
            if let Some(micros) = rec
                .payload
                .get("cost_micros")
                .and_then(serde_json::Value::as_u64)
            {
                total_cost = total_cost.saturating_add(micros);
                priced = true;
            }
        } else if rec.kind == "tool" {
            tool_count += 1;
            if rec
                .payload
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                tool_errors += 1;
            }
        }
    }
    let cost = if priced {
        format!(" · cost {}", crate::render::money(total_cost))
    } else {
        String::new()
    };
    let errs = if tool_errors > 0 {
        format!(" ({tool_errors} err)")
    } else {
        String::new()
    };
    format!(
        "Totals: {} spans ({llm_count} llm, {tool_count} tool{errs}) · sum duration {}{cost}",
        records.len(),
        format_ms(Some(total_ms))
    )
}

fn short_turn(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 10 {
        id.to_owned()
    } else {
        chars.into_iter().take(10).collect()
    }
}

fn format_ms(ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return "-".to_owned();
    };
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        format!("{}.{}s", ms / 1_000, (ms % 1_000) / 100)
    }
}

fn escape_md(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_traces() -> Vec<TraceRecord> {
        vec![
            TraceRecord {
                session_id: "s1".into(),
                node_id: Some("n1".into()),
                kind: "llm".into(),
                started_ms: 100,
                dur_ms: 420,
                payload: serde_json::json!({
                    "turn_id": "turn-1",
                    "model": "claude-sonnet",
                    "prompt_tokens": 1200,
                    "completion_tokens": 250,
                    "cost_micros": 12_500,
                }),
            },
            TraceRecord {
                session_id: "s1".into(),
                node_id: Some("n2".into()),
                kind: "tool".into(),
                started_ms: 520,
                dur_ms: 1500,
                payload: serde_json::json!({
                    "turn_id": "turn-1",
                    "tool": "read",
                    "call_id": "call-1",
                    "is_error": false,
                }),
            },
        ]
    }

    #[test]
    fn formats_table_and_exports_json_and_markdown() {
        let traces = sample_traces();
        let table = format_trace_table(&traces);
        assert!(table.contains("claude-sonnet"), "{table}");
        assert!(table.contains("read"), "{table}");
        assert!(table.contains("420ms"), "{table}");
        assert!(table.contains("1.5s"), "{table}");
        assert!(table.contains("Totals: 2 spans (1 llm, 1 tool)"), "{table}");

        let dir = std::env::temp_dir().join(format!("hrn-trace-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let json_path = export_traces(&traces, &dir, "out.json").expect("json");
        let json_text = std::fs::read_to_string(&json_path).expect("read json");
        let roundtrip: Vec<TraceRecord> = serde_json::from_str(&json_text).expect("parse json");
        assert_eq!(roundtrip.len(), 2);

        let md_path = export_traces(&traces, &dir, "out.md").expect("md");
        let md_text = std::fs::read_to_string(&md_path).expect("read md");
        assert!(md_text.contains("| Turn | Kind | Target |"), "{md_text}");
        assert!(md_text.contains("claude-sonnet"), "{md_text}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
