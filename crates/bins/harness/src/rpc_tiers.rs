//! RPC service-tier, slow-mode, and compaction-recorder helpers split out of `rpc.rs`.

use super::{Shared, lock};
use hrn_kernel::{AgentEvent, BoxFuture, DagPayload, EventSink, Role, SessionStore, Tier};
use std::sync::{Arc, Mutex};

/// Saved characters of the newest `Compacted` event.
#[derive(Default)]
pub(super) struct Recorder(pub(super) Mutex<u64>);

/// An event sink that also remembers the compaction's saved-character count.
pub(super) struct Recording {
    pub(super) inner: Arc<dyn EventSink>,
    pub(super) recorder: Arc<Recorder>,
}

impl EventSink for Recording {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        let recorder = Arc::clone(&self.recorder);
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            if let AgentEvent::Compacted { saved_chars, .. } = &event {
                *lock(&recorder.0) = u64::try_from(*saved_chars).unwrap_or(u64::MAX);
            }
            inner.emit(event).await
        })
    }
}

/// The newest assistant text on the session's branch (what an aborted run leaves behind).
pub(super) fn text_of(store: &Arc<dyn SessionStore>, id: &str) -> String {
    let Ok(Some(meta)) = store.get_session(id) else {
        return String::new();
    };
    let Some(head) = meta.head_node_id else {
        return String::new();
    };
    store
        .load_branch(id, &head)
        .map(|branch| {
            branch
                .iter()
                .rev()
                .find_map(|node| match &node.payload {
                    DagPayload::Message { msg, .. } if msg.role == Role::Assistant => {
                        Some(msg.content.clone())
                    }
                    _ => None,
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

impl Shared {
    pub(super) fn supports_service_tier_live(&self, alias: &str) -> bool {
        self.runtime().supports_service_tier(Some(alias))
    }

    pub(super) fn supports_slow_mode_live(&self, alias: &str) -> bool {
        let llm = lock(&self.llm);
        hrn_edge_llm::supports_slow_mode(&llm, Some(alias))
    }

    pub(super) fn set_fast_mode_live(&self, enabled: bool) {
        let tier = if enabled { Tier::Priority } else { Tier::None };
        self.overrides.set_service_tier(Some(tier));
    }

    pub(super) fn set_slow_mode_live(&self, enabled: bool) {
        self.overrides.set_slow_mode(Some(enabled));
    }

    pub(super) fn fast_mode_enabled_live(&self) -> bool {
        matches!(
            self.overrides.service_tier(),
            Some(Tier::Priority | Tier::Ultrafast)
        )
    }

    pub(super) fn slow_mode_enabled_live(&self) -> bool {
        self.overrides
            .slow_mode()
            .unwrap_or_else(|| lock(&self.llm).anthropic.slow_mode)
    }
}
