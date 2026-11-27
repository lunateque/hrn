//! The `find` tool: keyword prefilter, model scoring, ranked `path:start-end score snippet` hits.

use crate::FindConfig;
use crate::score::Scorer;
use hrn_edge_tools::args::{blocking, opt_str, required_str, summary_of};
use hrn_edge_tools::paths::resolve;
use hrn_edge_tools::{Chunk, FindOpts, prefilter};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, HrnError, LlmProvider, Result, Tool, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

/// The `find` description, as data.
const DESCRIPTION: &str = include_str!("../prompts/find.md");

/// Query words kept as keywords when the caller gave none, and how long a word must be.
const DERIVED_KEYWORDS: usize = 8;
const DERIVED_MIN_CHARS: usize = 4;

/// Semantic code search over the live tree.
pub(crate) struct FindTool {
    cfg: FindConfig,
    llm: Option<Arc<dyn LlmProvider>>,
}

impl FindTool {
    pub(crate) const fn new(cfg: FindConfig, llm: Option<Arc<dyn LlmProvider>>) -> Self {
        Self { cfg, llm }
    }
}

/// The caller's keywords, else the longer words of the query.
fn keywords(args: &Value, query: &str) -> Vec<String> {
    let given: Vec<String> = args
        .get("grep_keywords")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|k| k.as_str().map(str::to_owned))
        .filter(|k| !k.trim().is_empty())
        .collect();
    if !given.is_empty() {
        return given;
    }
    query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| w.chars().count() >= DERIVED_MIN_CHARS)
        .take(DERIVED_KEYWORDS)
        .map(str::to_owned)
        .collect()
}

struct Hit<'a> {
    score: f64,
    chunk: &'a Chunk,
}

fn render(hits: &[Hit<'_>]) -> String {
    let mut out = String::new();
    for hit in hits {
        let _ = writeln!(
            out,
            "{}:{}-{} {:.2} {}",
            hit.chunk.path, hit.chunk.start, hit.chunk.end, hit.score, hit.chunk.hint
        );
    }
    out
}

impl Tool for FindTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "find".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to find, in plain language." },
                    "grep_keywords": { "type": "array", "items": { "type": "string" }, "description": "Likely verbatim terms that appear in the code; case-insensitive, matched literally. [] if unsure." },
                    "path": { "type": "string", "description": "Directory or file to search (default: the working directory)." }
                },
                "required": ["query", "grep_keywords"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        summary_of(args, "query")
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let cfg = self.cfg.clone();
        let llm = self.llm.clone();
        let cwd = ctx.cwd.clone();
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            let started = Instant::now();
            let query = required_str(&args, "query", "find")?.to_owned();
            let llm = llm.ok_or_else(|| {
                HrnError::Config("find: this process has no model to score with".to_owned())
            })?;
            let words = keywords(&args, &query);
            if words.is_empty() {
                return Err(HrnError::Tool(
                    "find: give `grep_keywords`, or a `query` with words of 4+ letters".to_owned(),
                ));
            }
            let root = opt_str(&args, "path").map_or_else(|| cwd.clone(), |p| resolve(&cwd, p));
            let opts = FindOpts {
                max_files: cfg.max_files,
                max_chunk_lines: cfg.max_chunk_lines,
                max_chunks: cfg.max_chunks,
            };
            let scan_cancel = cancel.clone();
            let scan_words = words.clone();
            let found = blocking("find", move || {
                prefilter(&root, &cwd, &scan_words, opts, &scan_cancel)
            })
            .await?;
            let mut notes = Vec::new();
            if let Some(why) = found.stopped {
                notes.push(format!(
                    "scan stopped early: {why} after {} files; narrow `path`",
                    found.scanned
                ));
            }
            if found.unreadable > 0 {
                notes.push(format!("{} file(s) could not be read", found.unreadable));
            }
            if found.chunks.is_empty() {
                notes.push(format!(
                    "no file among {} scanned holds any of the keywords [{}]; try other \
                     `grep_keywords` or a wider `path`",
                    found.scanned,
                    words.join(", ")
                ));
                return Ok(ToolOutput::ok(notes.join("\n")));
            }
            let scorer = Scorer {
                llm,
                model: cfg.role.clone(),
                batch: cfg.batch_chunks,
                concurrency: cfg.concurrency,
            };
            let answers = scorer.score(&query, &found.chunks, &cancel).await?;
            if !answers.failures.is_empty() && answers.scores.iter().all(Option::is_none) {
                return Err(HrnError::Tool(format!(
                    "find: the scoring model failed: {}",
                    answers.failures.join("; ")
                )));
            }
            for failure in &answers.failures {
                notes.push(format!("a scoring request failed: {failure}"));
            }
            let mut hits: Vec<Hit<'_>> = found
                .chunks
                .iter()
                .zip(&answers.scores)
                .filter_map(|(chunk, score)| score.map(|score| Hit { score, chunk }))
                .collect();
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            let best = hits.first().map(|h| h.score);
            hits.retain(|h| h.score >= cfg.min_score);
            hits.truncate(cfg.max_results);
            let mut out = if hits.is_empty() {
                format!(
                    "No excerpt scored {:.2} or more (best: {}).",
                    cfg.min_score,
                    best.map_or_else(|| "none".to_owned(), |b| format!("{b:.2}"))
                )
            } else {
                render(&hits)
            };
            let _ = write!(
                out,
                "[scored {} excerpts from {} of {} files in {:.1}s]",
                found.chunks.len(),
                found.matched,
                found.scanned,
                started.elapsed().as_secs_f64()
            );
            for note in notes {
                let _ = write!(out, "\n[{note}]");
            }
            Ok(ToolOutput::ok(out))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_fall_back_to_the_longer_query_words() {
        let args = json!({"query": "where is the PID file written", "grep_keywords": []});
        assert_eq!(
            keywords(&args, "where is the PID file written"),
            ["where", "file", "written"]
        );
        let given = json!({"grep_keywords": ["spawn_agent", " "]});
        assert_eq!(keywords(&given, "x"), ["spawn_agent"]);
    }
}
