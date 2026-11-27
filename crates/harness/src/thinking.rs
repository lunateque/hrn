//! Turn thinking resolution: omp's magic keywords (embedded data file) and the `auto` level's
//! per-prompt classifier (a cheap model call through the `fast` role, heuristics on failure).

use crate::runtime::AgentRuntime;
use hrn_kernel::{LlmRequest, Msg, ThinkingLevel, ToolSpec};
use serde::Deserialize;

const KEYWORDS_JSON: &str = include_str!("../data/magic-keywords.json");

/// One magic keyword from the embedded table.
#[derive(Debug, Deserialize)]
struct Keyword {
    #[serde(rename = "keyword")]
    word: String,
    level: Option<String>,
    /// `true` (ultrathink): only steers when thinking is automatic.
    raise: bool,
    notice: String,
    #[serde(default)]
    requires: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct KeywordTable {
    keywords: Vec<Keyword>,
}

/// The `auto` classifier question, omp's `prompts/judge/auto-thinking.json` reduced to one
/// ordinal choice a cheap model can answer in a word.
const CLASSIFY_SYSTEM: &str = "You estimate how much reasoning a coding-agent prompt needs. \
Answer with exactly one word: low, medium or high.\n\
low: a lookup, a one-line change, a direct question.\n\
medium: a scoped edit, a small bugfix, a focused explanation.\n\
high: multi-file work, debugging unknown causes, design or migration decisions.";

/// What the magic keywords decided for one prompt.
#[derive(Default)]
pub struct KeywordEffect {
    /// A pinned thinking level the keywords force (chillax), if any.
    pub level: Option<ThinkingLevel>,
    /// `ultrathink`/`chillax` fired with `auto` thinking: force max/min instead of classifying.
    pub auto_override: Option<ThinkingLevel>,
    /// Hidden per-turn instructions to append to the system prompt.
    pub notices: Vec<String>,
}

/// Standalone prose matching (omp's rules): exact lowercase, word-bounded by non-letter chars,
/// outside fenced and inline code.
fn keyword_present(prompt: &str, word: &str) -> bool {
    let stripped = strip_code(prompt);
    stripped.match_indices(word).any(|(at, _)| {
        let before_ok = at == 0
            || !stripped[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let rest = &stripped[at + word.len()..];
        // A trailing `.` only ends a sentence when it does not continue into an identifier:
        // `orchestrate.ts` is a file path, `ultrathink, then stop` is prose.
        let after_ok = match rest.chars().next() {
            None => true,
            Some('.') => !rest[1..].chars().next().is_some_and(char::is_alphanumeric),
            Some(c) => !(c.is_alphanumeric() || c == '_' || c == '-'),
        };
        before_ok && after_ok
    })
}

/// Drop fenced blocks and inline code spans so paths and snippets cannot trigger keywords.
fn strip_code(prompt: &str) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut in_fence = false;
    let mut in_span = false;
    for line in prompt.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push('\n');
            continue;
        }
        for ch in line.chars() {
            if ch == '`' {
                in_span = !in_span;
                continue;
            }
            if !in_span {
                out.push(ch);
            }
        }
        out.push('\n');
    }
    out
}

impl AgentRuntime {
    /// Apply the magic keywords of `prompt` given the session level and the callable tool set.
    pub fn keyword_effects(
        &self,
        prompt: &str,
        level: ThinkingLevel,
        tools: &[ToolSpec],
    ) -> hrn_kernel::Result<KeywordEffect> {
        let table: KeywordTable = serde_json::from_str(KEYWORDS_JSON)
            .map_err(|e| hrn_kernel::HrnError::Config(format!("magic-keyword table: {e}")))?;
        let mut eff = KeywordEffect::default();
        for kw in &table.keywords {
            if !keyword_present(prompt, &kw.word) {
                continue;
            }
            if !kw
                .requires
                .iter()
                .all(|t| tools.iter().any(|s| &s.name == t))
            {
                continue;
            }
            if let Some(raw) = &kw.level {
                let parsed = ThinkingLevel::parse(raw);
                if kw.raise {
                    // ultrathink: only steers automatic thinking (omp: picks the highest tier).
                    if level == ThinkingLevel::Auto {
                        eff.auto_override = parsed;
                    }
                } else {
                    eff.level = parsed;
                }
            }
            eff.notices.push(format!(
                "[magic · {}] {}",
                kw.word,
                kw.notice.replace('\n', " ")
            ));
        }
        Ok(eff)
    }

    /// Resolve the `auto` level for `prompt`: a cheap classifier call through the `fast` role
    /// (falling back to the session model), heuristics when no model can answer — omp's
    /// auto-thinking classifies per prompt and falls back when its judge throws.
    ///
    /// When `prompt` carries a subagent `[solution-space: ...]` trailer, only that solution-space
    /// string is fed to the classifier verbatim (never the rest of the task text).
    pub async fn classify_auto(&self, prompt: &str, model: Option<&str>) -> ThinkingLevel {
        let trimmed = extract_solution_space(prompt).unwrap_or_else(|| prompt.trim());
        let request = |ask_model: Option<String>| LlmRequest {
            model: ask_model,
            messages: vec![
                Msg::system(CLASSIFY_SYSTEM),
                Msg::user(format!(
                    "Model in use: {}.\nPrompt:\n{}",
                    model.unwrap_or("default"),
                    clipped(trimmed, 2000)
                )),
            ],
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            thinking_budgets: None,
            temperature: None,
            max_tokens: Some(4),
            session_id: None,
            service_tier: None,
        };
        let cancel = hrn_kernel::CancelFlag::new();
        for ask_model in [Some("fast".to_owned()), model.map(str::to_owned)] {
            let ask_model = match ask_model {
                Some(m) => Some(m),
                None => continue,
            };
            let Ok(resp) = self
                .llm
                .stream_chat(&request(ask_model), &cancel, &crate::runtime::NullSink)
                .await
            else {
                continue;
            };
            return match ThinkingLevel::parse(resp.content.trim()) {
                Some(
                    level @ (ThinkingLevel::Low | ThinkingLevel::Medium | ThinkingLevel::High),
                ) => level,
                _ => heuristic_level(trimmed),
            };
        }
        heuristic_level(trimmed)
    }
}

/// Heuristic ladder (omp's on-device fallback spirit): trivial lookups stay cheap, open-ended
/// or bulky work asks for high.
const HARD: [&str; 10] = [
    "refactor",
    "architect",
    "redesign",
    "migrat",
    "root cause",
    "why does",
    "concurrent",
    "race",
    "deadlock",
    "protocol",
];
const EASY: [&str; 8] = [
    "what is",
    "where is",
    "list ",
    "rename",
    "one-line",
    "typo",
    "show me",
    "how do i say",
];

fn heuristic_level(prompt: &str) -> ThinkingLevel {
    let lower = prompt.to_lowercase();
    if HARD.iter().any(|k| lower.contains(k)) || prompt.lines().count() > 30 {
        ThinkingLevel::High
    } else if EASY.iter().any(|k| lower.contains(k)) && prompt.len() < 220 {
        ThinkingLevel::Low
    } else {
        ThinkingLevel::Medium
    }
}

fn clipped(text: &str, max: usize) -> &str {
    let end = text
        .char_indices()
        .take_while(|(i, c)| *i + c.len_utf8() <= max || *i == 0)
        .fold(0usize, |_, (i, c)| i + c.len_utf8());
    &text[..end.min(text.len())]
}

/// Marker prefix for a subagent's `solutionSpace` trailer carried on the first prompt.
pub(crate) const SOLUTION_SPACE_PREFIX: &str = "\n\n[solution-space: ";

/// Extract the verbatim `solutionSpace` text from a subagent prompt trailer, if present.
pub(crate) fn extract_solution_space(prompt: &str) -> Option<&str> {
    let (_, tail) = prompt.rsplit_once(SOLUTION_SPACE_PREFIX)?;
    let inner = tail.trim_end().strip_suffix(']')?.trim();
    (!inner.is_empty()).then_some(inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_match_only_standalone_prose() {
        assert!(keyword_present(
            "please ultrathink about this",
            "ultrathink"
        ));
        assert!(keyword_present("ultrathink, then stop", "ultrathink"));
        assert!(!keyword_present("Ultrathink now", "ultrathink"));
        assert!(!keyword_present("orchestrated rollout", "orchestrate"));
        assert!(!keyword_present("see orchestrate.ts", "orchestrate"));
        assert!(!keyword_present("`ultrathink` in code", "ultrathink"));
        assert!(!keyword_present("```\nultrathink\n```", "ultrathink"));
        assert!(keyword_present("workflowz an audit", "workflowz"));
    }

    #[test]
    fn heuristics_span_easy_hard_and_middle() {
        assert_eq!(heuristic_level("list the files here"), ThinkingLevel::Low);
        assert_eq!(
            heuristic_level("find the root cause of the deadlock"),
            ThinkingLevel::High
        );
        assert_eq!(
            heuristic_level("add a retry to this http call"),
            ThinkingLevel::Medium
        );
    }
}
