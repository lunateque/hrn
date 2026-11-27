//! The per-section system-prompt token budgets (spec `prompt-spec.md` §4.3) and the clipping
//! helpers every bounded section shares. The numbers are data — `data/prompt-budgets.json` —
//! so the gate (`cargo xtask promptstats`) and the memory module read the same table without
//! depending on the harness crate.

use serde::Deserialize;

/// One token is estimated as this many bytes — the same convention as the runtime's
/// `context_overhead` estimate.
pub const BYTES_PER_TOKEN: usize = 4;

/// The per-section budgets of §4.3, in tokens.
#[derive(Debug, Deserialize)]
pub struct Budgets {
    /// The base variant (or a project `SYSTEM.md`).
    pub base: usize,
    /// Module one-liners + tool commands + internal URLs (the JSON key is `static`).
    #[serde(rename = "static")]
    pub static_rest: usize,
    /// `APPEND_SYSTEM.md` (user + project layers together).
    pub append_system: usize,
    /// The `AGENTS.md` / `CLAUDE.md` chain in total.
    pub context_files: usize,
    /// How many context files the chain may hold at most.
    pub context_files_max: usize,
    /// The skills catalog.
    pub skills_catalog: usize,
    /// The subagent catalog.
    pub agents_catalog: usize,
    /// The deferred-groups section.
    pub deferred_groups: usize,
    /// The `## Memories` hook block.
    pub memories: usize,
    /// The tool schemas that ride on every request.
    pub tool_schemas: usize,
    /// The whole cold-turn fixed payload (system + schemas).
    pub cold_total: usize,
}

/// The embedded budget table.
///
/// # Panics
///
/// When the embedded JSON is malformed (a build-time mistake, not a runtime one).
#[must_use]
#[expect(
    clippy::expect_used,
    reason = "embedded data: a malformed table must fail loudly at startup"
)]
pub fn budgets() -> Budgets {
    static TABLE: &str = include_str!("../data/prompt-budgets.json");
    serde_json::from_str(TABLE).expect("the embedded prompt-budgets.json is valid")
}

/// Rough token count of `text` under [`BYTES_PER_TOKEN`].
#[must_use]
pub fn tokens(text: &str) -> usize {
    text.len().div_ceil(BYTES_PER_TOKEN)
}

/// `text` cut to at most `max` bytes on a char boundary, `[truncated]`-marked like the other
/// file caps in the harness.
#[must_use]
pub fn clip_bytes(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max {
        return trimmed.to_owned();
    }
    let cut = trimmed.floor_char_boundary(max);
    format!(
        "{}\n[truncated]",
        trimmed.get(..cut).unwrap_or_default().trim_end()
    )
}

/// Clip `text` to `budget` tokens at a line boundary with the visible cut marker. The marker
/// counts against the budget (a degenerate budget below the marker length still yields it).
#[must_use]
pub fn clip_at(text: &str, budget: usize) -> String {
    const MARKER_MAX: usize = 64;
    let ceiling = budget.saturating_mul(BYTES_PER_TOKEN);
    if text.len() <= ceiling {
        return text.to_owned();
    }
    let room = ceiling.saturating_sub(MARKER_MAX);
    let head = text.get(..room.min(text.len())).unwrap_or(text);
    let cut = head
        .rfind('\n')
        .unwrap_or_else(|| head.floor_char_boundary(room));
    let kept = text.get(..cut).unwrap_or_default().trim_end();
    format!(
        "{kept}\n… ({} bytes cut by the prompt budget)",
        text.len() - kept.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_table_has_every_section_of_the_spec() {
        let b = budgets();
        assert!(b.base > 0 && b.static_rest > 0 && b.append_system > 0);
        assert!(b.context_files > 0 && b.context_files_max > 0);
        assert!(b.skills_catalog > 0 && b.agents_catalog > 0 && b.deferred_groups > 0);
        assert!(b.memories > 0 && b.tool_schemas > 0 && b.cold_total > 0);
    }

    #[test]
    fn clip_keeps_line_boundaries_and_marks_the_cut() {
        let line = "x".repeat(40);
        let text = format!("{line}\n{line}\n{line}");
        let cut = clip_at(&text, 30);
        assert!(cut.len() <= 120, "{} bytes", cut.len());
        assert!(cut.starts_with(&line), "{cut}");
        assert!(cut.contains("bytes cut by the prompt budget"), "{cut}");
        assert_eq!(clip_at(&text, 100), text);
        // A degenerate budget still reports the cut.
        assert!(clip_at(&text, 3).contains("cut by the prompt budget"));
    }
}
