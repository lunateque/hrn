//! `hrn stats`: sessions, calls, tokens and cost by day and model, from the session store.

use crate::term::out;
use hrn_edge_llm::LlmConfig;
use hrn_kernel::{Result, SessionStore, Usage, UsageRow};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// What a model was called by: usage rows store the name the provider answered with, which is
/// the bare upstream id. omp names a model `<provider>/<id>`, so the report turns a stored id
/// back into that name when exactly one configured model has it (an id served by two providers
/// stays as it was recorded, since neither name would be right).
#[derive(Default)]
pub(crate) struct ModelNames {
    by_id: BTreeMap<String, String>,
    ambiguous: Vec<String>,
}

impl ModelNames {
    /// The names of the configured models.
    fn from_llm(llm: &LlmConfig) -> Self {
        let mut this = Self::default();
        for preset in llm.presets.values() {
            let name = format!("{}/{}", preset.provider, preset.id);
            if this
                .by_id
                .insert(preset.id.clone(), name.clone())
                .is_some_and(|other| other != name)
            {
                this.ambiguous.push(preset.id.clone());
            }
        }
        for id in &this.ambiguous {
            this.by_id.remove(id);
        }
        this
    }

    /// The omp-style name of a stored model name.
    fn display(&self, stored: &str) -> String {
        self.by_id
            .get(stored)
            .cloned()
            .unwrap_or_else(|| stored.to_owned())
    }
}

/// Millionths of a dollar as `$0.0123`.
pub(crate) fn dollars(micros: u64) -> String {
    let ten_thousandths = micros.saturating_add(50) / 100;
    format!(
        "${}.{:04}",
        ten_thousandths / 10_000,
        ten_thousandths % 10_000
    )
}

fn cost(usage: &Usage) -> String {
    usage.cost_micros.map_or_else(|| "-".to_owned(), dollars)
}

fn cells(day: &str, model: &str, sessions: &str, calls: u64, usage: &Usage) -> Vec<String> {
    vec![
        day.to_owned(),
        model.to_owned(),
        sessions.to_owned(),
        calls.to_string(),
        usage.prompt_tokens.to_string(),
        usage.completion_tokens.to_string(),
        usage.cache_read_tokens.unwrap_or(0).to_string(),
        usage.cache_write_tokens.unwrap_or(0).to_string(),
        cost(usage),
    ]
}

/// The report as an aligned table: one row per day and model, then the totals.
fn table(rows: &[UsageRow], names: &ModelNames) -> String {
    let mut total = Usage::default();
    let mut calls = 0;
    let mut lines = vec![
        [
            "DAY", "MODEL", "SESSIONS", "CALLS", "IN", "OUT", "CACHE-R", "CACHE-W", "COST",
        ]
        .map(str::to_owned)
        .to_vec(),
    ];
    for row in rows {
        total.accumulate(row.usage);
        calls += row.calls;
        lines.push(cells(
            &row.day,
            &names.display(&row.model),
            &row.sessions.to_string(),
            row.calls,
            &row.usage,
        ));
    }
    lines.push(cells("total", "", "-", calls, &total));
    let widths: Vec<usize> = (0..9)
        .map(|col| {
            lines
                .iter()
                .filter_map(|line| line.get(col))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut text = String::new();
    for line in &lines {
        let mut row = String::new();
        for (col, (cell, width)) in line.iter().zip(&widths).enumerate() {
            if col > 0 {
                row.push_str("  ");
            }
            // Day and model read left to right, the figures line up on their last digit.
            let _ = if col < 2 {
                write!(row, "{cell:<width$}")
            } else {
                write!(row, "{cell:>width$}")
            };
        }
        text.push_str(row.trim_end());
        text.push('\n');
    }
    if rows.iter().any(|row| row.usage.cost_micros.is_none()) {
        text.push_str("\n- : no price configured for the model, or a subscription login\n");
    }
    text
}

/// The display names of the configured models.
pub(crate) fn names(llm: &LlmConfig) -> ModelNames {
    ModelNames::from_llm(llm)
}

/// Print the usage report.
///
/// # Errors
/// The store could not be read.
pub(crate) fn run(store: &dyn SessionStore, names: &ModelNames) -> Result {
    let rows = store.usage_report()?;
    if rows.is_empty() {
        out("no usage recorded yet\n");
    } else {
        out(&table(&rows, names));
    }
    Ok(())
}

/// What `/stats` shows in the TUI: the current session's usage by model, then the all-time
/// table of `hrn stats`.
///
/// # Errors
/// The store could not be read.
pub(crate) fn report(
    store: &dyn SessionStore,
    session: Option<&str>,
    names: &ModelNames,
) -> Result<String> {
    let mut text = String::from("this session\n");
    let own = match session {
        Some(id) => store.session_usage_report(id)?,
        None => Vec::new(),
    };
    if own.is_empty() {
        text.push_str("no model calls yet\n");
    } else {
        text.push_str(&table(&own, names));
    }
    text.push_str("\nall time\n");
    let all = store.usage_report()?;
    if all.is_empty() {
        text.push_str("no usage recorded yet\n");
    } else {
        text.push_str(&table(&all, names));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_edge_llm::ModelPreset;

    fn preset(provider: &str, id: &str) -> ModelPreset {
        ModelPreset {
            thinking_budgets: None,
            provider: provider.into(),
            id: id.into(),
            thinking: None,
            context_window: None,
            name: None,
            reasoning: None,
            prompt_variant: None,
            price: None,
            ..ModelPreset::default()
        }
    }

    fn llm(presets: Vec<(&str, ModelPreset)>) -> LlmConfig {
        LlmConfig {
            presets: presets
                .into_iter()
                .map(|(alias, p)| (alias.to_owned(), p))
                .collect(),
            ..LlmConfig::default()
        }
    }

    #[test]
    fn a_bare_id_is_reported_as_provider_slash_id() {
        let names = names(&llm(vec![
            ("flash", preset("gateway", "example-flash")),
            ("pro", preset("example", "org/model-pro")),
        ]));
        assert_eq!(names.display("example-flash"), "gateway/example-flash");
        // A name already in the omp form, and one no model has, stay as recorded.
        assert_eq!(
            names.display("gateway/example-flash"),
            "gateway/example-flash"
        );
        assert_eq!(names.display("some-old-model"), "some-old-model");
    }

    #[test]
    fn an_id_two_providers_serve_is_left_alone() {
        let names = names(&llm(vec![
            ("a", preset("first", "shared")),
            ("b", preset("second", "shared")),
        ]));
        assert_eq!(names.display("shared"), "shared");
    }

    #[test]
    fn dollars_keep_four_decimals_and_round() {
        assert_eq!(dollars(12_300), "$0.0123");
        assert_eq!(dollars(12_349), "$0.0123");
        assert_eq!(dollars(12_350), "$0.0124");
        assert_eq!(dollars(3_000_000), "$3.0000");
    }

    #[test]
    fn the_table_aligns_and_totals_priced_and_unpriced_rows() {
        let row = |day: &str, model: &str, cost| UsageRow {
            day: day.into(),
            model: model.into(),
            sessions: 1,
            calls: 2,
            usage: Usage {
                prompt_tokens: 1000,
                completion_tokens: 50,
                cost_micros: cost,
                ..Usage::default()
            },
        };
        let text = table(
            &[
                row("2026-05-02", "flash", Some(5_000)),
                row("2026-05-01", "claude", None),
            ],
            &ModelNames::default(),
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.first().map(|l| l.starts_with("DAY         MODEL")),
            Some(true),
            "{text}"
        );
        assert!(
            lines.get(1).is_some_and(|l| l.ends_with("$0.0050")),
            "{text}"
        );
        assert!(lines.get(2).is_some_and(|l| l.ends_with('-')), "{text}");
        assert!(
            lines
                .get(3)
                .is_some_and(|l| l.starts_with("total") && l.ends_with("$0.0050")),
            "{text}"
        );
        assert!(text.contains("no price configured"));
    }
}
