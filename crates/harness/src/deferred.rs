//! Deferred tools: their schemas stay out of LLM requests until the model finds them with
//! `tool_search`, which activates the best matches for the rest of the session.

use hrn_kernel::{
    BoxFuture, DeferredTool, Result, Tool, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

/// Name of the discovery tool.
pub(crate) const SEARCH_TOOL: &str = "tool_search";
/// The `tool_search` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/tools/tool-search.md");
const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 25;
/// Longest description excerpt echoed back per match.
const BLURB_CHARS: usize = 110;

/// What `tool_search` and the prompt listing need to know about one deferred tool.
pub(crate) struct Entry {
    group: String,
    short: String,
    /// Qualified name minus `short`, e.g. `mcp__telegram__`.
    prefix: String,
    /// First line of the description, clipped for display.
    blurb: String,
    /// Lowercased qualified name.
    name: String,
    /// The qualified name's alphanumeric words.
    words: Vec<String>,
    /// Lowercased description.
    description: String,
}

/// Deferred tools by qualified name.
pub(crate) type Catalog = BTreeMap<String, Arc<Entry>>;

impl Entry {
    pub(crate) fn new(deferred: &DeferredTool, spec: &ToolSpec) -> Self {
        let first_line = spec.description.lines().next().unwrap_or_default().trim();
        let cut = first_line.floor_char_boundary(BLURB_CHARS);
        let name = spec.name.to_lowercase();
        Self {
            group: deferred.group.clone(),
            short: deferred.short_name.clone(),
            prefix: spec
                .name
                .strip_suffix(&deferred.short_name)
                .unwrap_or_default()
                .to_owned(),
            blurb: first_line.get(..cut).unwrap_or_default().to_owned(),
            words: words(&name),
            name,
            description: spec.description.to_lowercase(),
        }
    }
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Relevance of `entry` to the query `terms`: a whole word of the name beats a name substring,
/// which beats a description hit. One- and two-letter terms only count as whole name words; as
/// substrings they would match nearly everything.
fn score(entry: &Entry, terms: &[String]) -> u32 {
    terms
        .iter()
        .map(|term| {
            if entry.words.iter().any(|w| w == term) {
                6
            } else if term.len() < 3 {
                0
            } else if entry.name.contains(term.as_str()) {
                4
            } else {
                u32::from(entry.description.contains(term.as_str()))
            }
        })
        .sum()
}

type Hits<'a> = Vec<(&'a str, &'a Entry)>;

/// The best `limit` matches of `query`, best first, and the total number of matches.
fn search<'a>(catalog: &'a Catalog, query: &str, limit: usize) -> (Hits<'a>, usize) {
    let mut terms = words(&query.to_lowercase());
    terms.sort();
    terms.dedup();
    let mut hits: Vec<(u32, &str, &Entry)> = catalog
        .iter()
        .filter_map(|(name, entry)| {
            let s = score(entry, &terms);
            (s > 0).then_some((s, name.as_str(), entry.as_ref()))
        })
        .collect();
    // Equal scores: the shorter name is the closer match (`telegram-status` over
    // `telegram-get-boosts-status`), then alphabetical for a stable order.
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.2.words.len().cmp(&b.2.words.len()))
            .then_with(|| a.1.cmp(b.1))
    });
    let total = hits.len();
    hits.truncate(limit);
    (hits.into_iter().map(|(_, n, e)| (n, e)).collect(), total)
}

/// Per-group `name (count)` summary, for the "nothing matched" hint.
fn group_counts(catalog: &Catalog) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in catalog.values() {
        *counts.entry(entry.group.as_str()).or_default() += 1;
    }
    counts
        .iter()
        .map(|(group, n)| format!("{group} ({n})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The system-prompt section: one summary line per deferred group (names stay out; the model
/// pulls exact ones with `tool_search`).
pub(crate) fn prompt_section(catalog: &Catalog, budget: usize) -> Option<String> {
    if catalog.is_empty() {
        return None;
    }
    let mut groups: BTreeMap<&str, (&str, Vec<&str>)> = BTreeMap::new();
    for entry in catalog.values() {
        groups
            .entry(entry.group.as_str())
            .or_insert_with(|| (entry.prefix.as_str(), Vec::new()))
            .1
            .push(entry.short.as_str());
    }
    let mut out = format!(
        "## Deferred tools\n{} tools load on demand: `{SEARCH_TOOL}` by name or keywords makes \
         matches callable next step.\n",
        catalog.len()
    );
    let ceiling = budget
        .saturating_mul(crate::prompt_data::BYTES_PER_TOKEN)
        .saturating_sub(64);
    let total = groups.len();
    for (index, (group, (prefix, shorts))) in groups.iter().enumerate() {
        let line = if shorts.len() <= GROUP_NAME_LIMIT {
            let list = shorts.join(", ");
            if prefix.is_empty() {
                format!("- {group}: {list}\n")
            } else {
                format!("- {group}: `{prefix}<name>` — {list}\n")
            }
        } else {
            format!(
                "- {group}: {} tools `{prefix}<name>` ({}); find them with `{SEARCH_TOOL}`\n",
                shorts.len(),
                topic_words(catalog, group)
            )
        };
        if out.len() + line.len() > ceiling {
            let _ = writeln!(
                out,
                "- … ({} more groups; `{SEARCH_TOOL}` still finds them)",
                total - index
            );
            break;
        }
        out.push_str(&line);
    }
    Some(out)
}

/// A group names up to this many tools inline before it collapses to a summary line.
const GROUP_NAME_LIMIT: usize = 6;

/// A few topic words for a large group: the most frequent non-filler words of its blurbs.
fn topic_words(catalog: &Catalog, group: &str) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in catalog.values().filter(|e| e.group == group) {
        for word in entry.blurb.split_whitespace() {
            let clean: String = word
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .flat_map(char::to_lowercase)
                .collect();
            if clean.len() >= 4 {
                *counts.entry(clean).or_default() += 1;
            }
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .iter()
        .take(4)
        .map(|(word, _)| word.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `tool_search`: find deferred tools and activate the best matches.
pub(crate) struct ToolSearchTool {
    catalog: Arc<Catalog>,
}

impl ToolSearchTool {
    pub(crate) fn new(catalog: Arc<Catalog>) -> Self {
        Self { catalog }
    }
}

impl Tool for ToolSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SEARCH_TOOL.to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Tool name or keywords, e.g. `telegram status`."
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!(
                            "Most matches to return and activate (default {DEFAULT_LIMIT}, max {MAX_LIMIT})."
                        )
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn execute(&self, _ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let catalog = Arc::clone(&self.catalog);
        Box::pin(async move {
            let Some(query) = args
                .get("query")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|q| !q.is_empty())
            else {
                return Ok(ToolOutput::err("`query` must be a non-empty string"));
            };
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_LIMIT, |n| {
                    usize::try_from(n).unwrap_or(MAX_LIMIT).clamp(1, MAX_LIMIT)
                });
            let (hits, total) = search(&catalog, query, limit);
            if hits.is_empty() {
                return Ok(ToolOutput::ok(format!(
                    "No deferred tool matches `{query}`. Deferred groups: {}.",
                    group_counts(&catalog)
                )));
            }
            let mut out = format!(
                "Activated {} tool(s); call them by these exact names from your next step:\n",
                hits.len()
            );
            let mut activated = Vec::with_capacity(hits.len());
            for (name, entry) in hits {
                let _ = writeln!(out, "- {name} — {}", entry.blurb);
                activated.push(name.to_owned());
            }
            if total > activated.len() {
                let _ = write!(
                    out,
                    "{} more matched; refine the query or raise `limit` to load them.",
                    total - activated.len()
                );
            }
            let mut output = ToolOutput::ok(out.trim_end());
            output.activated_tools = activated;
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(&'static str, &'static str);

    impl Tool for Stub {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.to_owned(),
                description: self.1.to_owned(),
                parameters: json!({"type": "object"}),
            }
        }

        fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
            Box::pin(async { Ok(ToolOutput::ok("")) })
        }
    }

    fn catalog() -> Catalog {
        [
            ("mcp__tg__tg-status", "Check the connection status"),
            ("mcp__tg__tg-send-message", "Send a message to a chat"),
            ("mcp__tg__tg-get-boosts-status", "Show the boost level"),
            (
                "mcp__tg__tg-get-contacts",
                "List contacts with status flags",
            ),
            ("mcp__photos__list_albums", "List photo albums"),
        ]
        .into_iter()
        .map(|(name, desc)| {
            let group = name.split("__").nth(1).unwrap_or_default();
            let deferred = DeferredTool {
                group: group.to_owned(),
                short_name: name.rsplit("__").next().unwrap_or_default().to_owned(),
                tool: Arc::new(Stub(name, desc)),
            };
            let entry = Entry::new(&deferred, &deferred.tool.spec());
            (name.to_owned(), Arc::new(entry))
        })
        .collect()
    }

    #[test]
    fn name_words_outrank_description_hits() {
        let catalog = catalog();
        let (hits, total) = search(&catalog, "tg status", 3);
        assert_eq!(total, 4);
        let names: Vec<&str> = hits.iter().map(|h| h.0).collect();
        assert_eq!(
            names,
            [
                "mcp__tg__tg-status",
                "mcp__tg__tg-get-boosts-status",
                "mcp__tg__tg-get-contacts"
            ],
            "equal scores: the shorter name first; a description hit ranks last"
        );
    }

    #[test]
    fn short_terms_match_whole_name_words_only() {
        let catalog = catalog();
        assert!(search(&catalog, "to", 5).0.is_empty());
        assert_eq!(search(&catalog, "tg", 5).1, 4);
    }

    #[test]
    fn prompt_lists_one_summary_line_per_group() {
        let section = prompt_section(&catalog(), 200).expect("section");
        assert!(
            section.contains("- tg: `mcp__tg__<name>` — tg-get-boosts-status, tg-get-contacts, tg-send-message, tg-status"),
            "{section}"
        );
        assert!(
            section.contains("- photos: `mcp__photos__<name>` —"),
            "{section}"
        );
        assert!(section.contains("tool_search"), "{section}");
    }

    #[test]
    fn a_large_group_collapses_to_a_topic_line() {
        let mut entries: Vec<hrn_kernel::DeferredTool> = Vec::new();
        for i in 0..10 {
            let name = format!("mcp__big__tool-{i}");
            entries.push(hrn_kernel::DeferredTool {
                group: "big".into(),
                short_name: format!("tool-{i}"),
                tool: std::sync::Arc::new(BigStub(name)),
            });
        }
        let mut catalog: Catalog = Catalog::new();
        for d in &entries {
            let spec = d.tool.spec();
            catalog.insert(spec.name.clone(), std::sync::Arc::new(Entry::new(d, &spec)));
        }
        let section = prompt_section(&catalog, 200).expect("section");
        assert!(section.contains("- big: 10 tools"), "{section}");
        assert!(!section.contains("tool-3,"), "{section}");
    }

    struct BigStub(String);
    impl BigStub {
        fn name(&self) -> &str {
            &self.0
        }
    }
    impl hrn_kernel::Tool for BigStub {
        fn spec(&self) -> hrn_kernel::ToolSpec {
            hrn_kernel::ToolSpec {
                name: self.name().to_owned(),
                description: "send messages and contacts".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        fn execute(
            &self,
            _ctx: &hrn_kernel::ToolContext,
            _args: serde_json::Value,
        ) -> BoxFuture<hrn_kernel::Result<hrn_kernel::ToolOutput>> {
            Box::pin(async { Ok(hrn_kernel::ToolOutput::ok("")) })
        }
    }
}
