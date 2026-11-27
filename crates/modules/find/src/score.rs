//! Scoring excerpts with a model: batches of excerpts per request, several requests at once.

use hrn_edge_rt::tokio::sync::Semaphore;
use hrn_edge_rt::tokio::task::JoinSet;
use hrn_edge_tools::Chunk;
use hrn_kernel::{
    BoxFuture, CancelFlag, ChunkSink, HrnError, LlmChunk, LlmProvider, LlmRequest, Msg, Result,
    ThinkingLevel,
};
use std::fmt::Write as _;
use std::sync::Arc;

const SYSTEM_PROMPT: &str = include_str!("../prompt.txt");
/// Output budget per excerpt, plus a fixed allowance for the rest of the reply. A reasoning
/// model spends part of it on its thinking trace, so the allowance is generous.
const TOKENS_PER_EXCERPT: u32 = 32;
const REPLY_ALLOWANCE: u32 = 256;

struct Silent;

impl ChunkSink for Silent {
    fn on_chunk(&self, _chunk: LlmChunk) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// How the excerpts are scored.
pub(crate) struct Scorer {
    pub(crate) llm: Arc<dyn LlmProvider>,
    /// The model alias or role asked for.
    pub(crate) model: String,
    /// Excerpts per request.
    pub(crate) batch: usize,
    /// Requests in flight at once.
    pub(crate) concurrency: usize,
}

/// What scoring produced.
pub(crate) struct Scored {
    /// One entry per excerpt, in order: `None` when no answer covered it.
    pub(crate) scores: Vec<Option<f64>>,
    /// One message per request that failed.
    pub(crate) failures: Vec<String>,
}

/// The score lines of `reply`: `<id> <score>` per line, ids 1-based within the request. Extra
/// words around the numbers (`[3]`, `3:`, `id 3 score 0.4`) do not matter.
fn parse_reply(reply: &str, count: usize) -> Vec<Option<f64>> {
    let mut scores = vec![None; count];
    let mut tokens = reply
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|t| t.trim_end_matches('.'))
        .filter(|t| !t.is_empty());
    while let Some(token) = tokens.next() {
        let Ok(id) = token.parse::<usize>() else {
            continue;
        };
        let Some(score) = tokens.next() else { break };
        let Some(score) = score.parse::<f64>().ok() else {
            // The next number may still pair with the next one; keep going.
            continue;
        };
        if id == 0 || id > count {
            continue;
        }
        if let Some(slot) = scores.get_mut(id - 1) {
            *slot = Some(score.clamp(0.0, 1.0));
        }
    }
    scores
}

fn render(query: &str, batch: &[Chunk]) -> String {
    let mut out = format!("Query: {query}\n\nExcerpts:\n");
    for (i, chunk) in batch.iter().enumerate() {
        let _ = write!(
            out,
            "\n[{}] {}:{}-{}\n{}\n",
            i + 1,
            chunk.path,
            chunk.start,
            chunk.end,
            chunk.text
        );
    }
    out
}

async fn score_batch(
    llm: Arc<dyn LlmProvider>,
    model: String,
    query: String,
    batch: Vec<Chunk>,
    cancel: CancelFlag,
) -> Result<Vec<Option<f64>>> {
    let count = u32::try_from(batch.len()).unwrap_or(u32::MAX);
    let request = LlmRequest {
        model: Some(model),
        messages: vec![
            Msg::system(SYSTEM_PROMPT),
            Msg::user(render(&query, &batch)),
        ],
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        thinking_budgets: None,
        temperature: Some(0.0),
        max_tokens: Some(count.saturating_mul(TOKENS_PER_EXCERPT) + REPLY_ALLOWANCE),
        session_id: None,
        service_tier: None,
    };
    let response = llm.stream_chat(&request, &cancel, &Silent).await?;
    // A reasoning model asked to say nothing may still put its answer in the reasoning trace;
    // that is the only scoring it ever gave, so take it rather than lose the batch.
    let reply = if response.content.trim().is_empty() {
        response.reasoning.as_str()
    } else {
        response.content.as_str()
    };
    Ok(parse_reply(reply, batch.len()))
}

impl Scorer {
    /// Score `chunks` against `query`. A request that fails leaves its excerpts unscored and is
    /// reported in [`Scored::failures`]; cancellation stops everything.
    ///
    /// # Errors
    ///
    /// `HrnError::Cancelled` when the call is cancelled.
    pub(crate) async fn score(
        &self,
        query: &str,
        chunks: &[Chunk],
        cancel: &CancelFlag,
    ) -> Result<Scored> {
        let gate = Arc::new(Semaphore::new(self.concurrency.max(1)));
        let mut jobs = JoinSet::new();
        for (n, batch) in chunks.chunks(self.batch.max(1)).enumerate() {
            let (llm, model, query) = (Arc::clone(&self.llm), self.model.clone(), query.to_owned());
            let (batch, cancel, gate) = (batch.to_vec(), cancel.clone(), Arc::clone(&gate));
            let first = n * self.batch.max(1);
            jobs.spawn(async move {
                let outcome = match gate.acquire().await {
                    Ok(_permit) => score_batch(llm, model, query, batch, cancel).await,
                    Err(e) => Err(HrnError::Internal(format!(
                        "find: scoring gate closed: {e}"
                    ))),
                };
                (first, outcome)
            });
        }
        let mut scores = vec![None; chunks.len()];
        let mut failures = Vec::new();
        while let Some(joined) = jobs.join_next().await {
            if cancel.is_cancelled() {
                return Err(HrnError::Cancelled);
            }
            match joined {
                Ok((first, Ok(batch))) => {
                    for (offset, score) in batch.into_iter().enumerate() {
                        if let Some(slot) = scores.get_mut(first + offset) {
                            *slot = score;
                        }
                    }
                }
                Ok((_, Err(HrnError::Cancelled))) => return Err(HrnError::Cancelled),
                Ok((_, Err(e))) => failures.push(e.to_string()),
                Err(e) => failures.push(format!("scoring task failed: {e}")),
            }
        }
        Ok(Scored { scores, failures })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_lines_survive_decoration_and_bad_lines() {
        let reply = "[1] 0.90\n2: 0.15\nid 3 score .4\nnonsense\n4 1.7\n9 0.5\n";
        assert_eq!(
            parse_reply(reply, 4),
            [Some(0.9), Some(0.15), Some(0.4), Some(1.0)]
        );
    }

    #[test]
    fn an_excerpt_without_a_line_stays_unscored() {
        assert_eq!(parse_reply("1 0.5", 2), [Some(0.5), None]);
    }
}
