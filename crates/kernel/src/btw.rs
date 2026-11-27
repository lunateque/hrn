//! Shared data types for `/btw` side-thread topics stored in `btw_topics` (never in the DAG).

use serde::{Deserialize, Serialize};

/// Status of one `/btw` question/answer exchange.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtwStatus {
    /// Currently streaming from the LLM.
    Streaming,
    /// Finished cleanly.
    Done,
    /// Cancelled by the user (Esc); partial answer is preserved.
    Cancelled,
    /// Failed with an error message.
    Error(String),
}

/// One question + answer pair inside a [`BtwTopic`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtwExchange {
    /// User's side question.
    pub question: String,
    /// Assistant's response (or partial response if cancelled).
    pub answer: String,
    /// Completion state.
    pub status: BtwStatus,
    /// Whether an `<inject>…</inject>` payload from this exchange was already sent.
    #[serde(default)]
    pub injected: bool,
}

/// Persistent side-thread topic stored in `SQLite` v4 `btw_topics` (never enters the DAG).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtwTopic {
    /// Stable topic identifier (`btw_...`).
    pub id: String,
    /// Short display title (first question, clipped).
    pub title: String,
    /// Chronological exchanges within this topic.
    pub exchanges: Vec<BtwExchange>,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

impl BtwTopic {
    /// The latest exchange in this topic, if any.
    #[must_use]
    pub fn latest(&self) -> Option<&BtwExchange> {
        self.exchanges.last()
    }

    /// The latest exchange mutably, if any.
    pub fn latest_mut(&mut self) -> Option<&mut BtwExchange> {
        self.exchanges.last_mut()
    }
}

/// Extract the first non-empty `<inject>…</inject>` payload from a `/btw` answer.
#[must_use]
pub fn extract_inject_block(text: &str) -> Option<String> {
    let start_tag = "<inject>";
    let end_tag = "</inject>";
    let start = text.find(start_tag)? + start_tag.len();
    let rest = text.get(start..)?;
    let end = rest.find(end_tag)?;
    let body = rest.get(..end)?.trim();
    (!body.is_empty()).then(|| body.to_owned())
}
