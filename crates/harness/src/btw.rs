//! `/btw [question]`: side-thread questions over projected session context with no tools and the
//! session's model, persisted in `btw_topics` without touching the conversation DAG.

use crate::compaction::project_branch;
use crate::runtime::AgentRuntime;
use hrn_kernel::{
    BoxFuture, CancelFlag, ChunkSink, HrnError, LlmChunk, LlmRequest, Msg, Result, ThinkingLevel,
};
pub use hrn_kernel::{BtwExchange, BtwStatus, BtwTopic, extract_inject_block};
use std::sync::{Arc, Mutex};

const BTW_SYSTEM_NOTE: &str = "You are answering a side-thread (`/btw`) question about the ongoing session. \
Answer concisely using the conversation context provided. You have NO tools in this side thread; do not emit tool calls. \
If the user might want to feed a short action or note back into the main session, you may optionally wrap it in <inject>...</inject>.";

struct BtwDeltaSink {
    partial: Arc<Mutex<String>>,
    on_delta: Arc<dyn Fn(String) + Send + Sync>,
}

impl ChunkSink for BtwDeltaSink {
    fn on_chunk(&self, chunk: LlmChunk) -> BoxFuture<()> {
        if let LlmChunk::ContentDelta(delta) = chunk {
            if let Ok(mut buf) = self.partial.lock() {
                buf.push_str(&delta);
            }
            (self.on_delta)(delta);
        }
        Box::pin(async {})
    }
}

fn short_title(question: &str) -> String {
    let single = question
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("btw");
    if single.chars().count() <= 60 {
        single.to_owned()
    } else {
        let mut s: String = single.chars().take(59).collect();
        s.push('…');
        s
    }
}

impl AgentRuntime {
    /// Load all persisted `/btw` topics for `session_id`, newest first.
    ///
    /// # Errors
    /// Fails if the store cannot be read.
    pub fn list_btw_topics(&self, session_id: &str) -> Result<Vec<BtwTopic>> {
        let rows = self.store.get_btw_topics(session_id)?;
        let mut out = Vec::with_capacity(rows.len());
        for (_, json, _) in rows {
            if let Ok(topic) = serde_json::from_str::<BtwTopic>(&json) {
                out.push(topic);
            }
        }
        Ok(out)
    }

    /// Persist a `/btw` topic in `btw_topics` (never modifies DAG nodes).
    ///
    /// # Errors
    /// Fails if serialization or store persistence fails.
    pub fn save_btw_topic(&self, session_id: &str, topic: &BtwTopic) -> Result {
        let json = serde_json::to_string(topic)?;
        self.store
            .put_btw_topic(session_id, &topic.id, &json, topic.updated_at_ms)
    }

    fn load_or_init_btw_topic(
        &self,
        session_id: &str,
        topic_id: Option<&str>,
        question: &str,
    ) -> Result<BtwTopic> {
        let now = self.clock.now_ms();
        match topic_id {
            Some(wanted) => Ok(self
                .list_btw_topics(session_id)?
                .into_iter()
                .find(|t| t.id == wanted)
                .unwrap_or_else(|| BtwTopic {
                    id: wanted.to_owned(),
                    title: short_title(question),
                    exchanges: Vec::new(),
                    updated_at_ms: now,
                })),
            None => Ok(BtwTopic {
                id: self.next_id("btw"),
                title: short_title(question),
                exchanges: Vec::new(),
                updated_at_ms: now,
            }),
        }
    }

    /// Run a `/btw` side question over `session_id`'s projected context with **no tools** and the
    /// session's model. Persists the updated [`BtwTopic`] in `btw_topics` (including partial text
    /// on cancellation) without touching the session DAG.
    ///
    /// # Errors
    /// Fails if the session does not exist or persistence fails. Provider errors are recorded on
    /// the returned exchange (`BtwStatus::Error`) or returned if nothing could be run.
    pub async fn run_btw(
        &self,
        session_id: &str,
        topic_id: Option<&str>,
        question: &str,
        cancel: &CancelFlag,
        on_delta: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<BtwTopic> {
        let meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let branch = self.load_head_branch(&meta)?;
        let policy = self.policy_for(meta.model.as_deref());
        let projected = project_branch(&branch, &policy);
        let mut topic = self.load_or_init_btw_topic(session_id, topic_id, question)?;

        let mut messages =
            Vec::with_capacity(projected.messages.len() + topic.exchanges.len() * 2 + 2);
        messages.push(Msg::system(BTW_SYSTEM_NOTE));
        messages.extend(projected.messages);
        for ex in &topic.exchanges {
            messages.push(Msg::user(format!("[/btw] {}", ex.question)));
            if !ex.answer.is_empty() {
                messages.push(Msg::assistant(&ex.answer));
            }
        }
        messages.push(Msg::user(format!("[/btw] {question}")));

        let request = LlmRequest {
            model: meta.model.clone(),
            messages,
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            thinking_budgets: None,
            temperature: None,
            max_tokens: None,
            session_id: None,
            service_tier: None,
        };

        let partial = Arc::new(Mutex::new(String::new()));
        let sink = BtwDeltaSink {
            partial: Arc::clone(&partial),
            on_delta,
        };

        let exchange = match self.llm.stream_chat(&request, cancel, &sink).await {
            Ok(resp) => {
                let _ = self.record_usage(&meta.id, request.model.as_deref(), &resp);
                let answer = if resp.content.is_empty() {
                    partial
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                } else {
                    resp.content
                };
                BtwExchange {
                    question: question.to_owned(),
                    answer,
                    status: BtwStatus::Done,
                    injected: false,
                }
            }
            Err(HrnError::Cancelled) => {
                let answer = partial
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                BtwExchange {
                    question: question.to_owned(),
                    answer,
                    status: BtwStatus::Cancelled,
                    injected: false,
                }
            }
            Err(err) => {
                let answer = partial
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                BtwExchange {
                    question: question.to_owned(),
                    answer,
                    status: BtwStatus::Error(err.to_string()),
                    injected: false,
                }
            }
        };

        topic.exchanges.push(exchange);
        topic.updated_at_ms = self.clock.now_ms();
        self.save_btw_topic(session_id, &topic)?;
        Ok(topic)
    }
}
