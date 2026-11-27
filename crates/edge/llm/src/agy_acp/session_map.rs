//! Canonical SHA-256 prefix map linking `hrn` DAG conversation branches to `agy` ACP sessions.

use hrn_kernel::{Msg, Role, ThinkingLevel};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Instant;

/// Maximum number of `(hrn_session_id, prefix_hash)` entries kept in memory per runtime.
pub const MAX_SESSION_ENTRIES: usize = 256;

/// One synchronized point on an `hrn` DAG branch backed by an `agy` ACP session.
#[derive(Clone, Debug)]
pub struct AgySessionEntry {
    /// ACP session UUID owned by `agy`.
    pub agy_session_id: String,
    /// SHA-256 digest of the canonical message prefix synchronized through `synced_msg_count`.
    pub synced_prefix_hash: [u8; 32],
    /// Number of canonical [`Msg`] items already synchronized into `agy_session_id`.
    pub synced_msg_count: usize,
    /// Last configured ACP model id on this session.
    pub model_id: String,
    /// Last configured effort level on this session.
    pub effort: ThinkingLevel,
    /// Last activity timestamp for LRU eviction.
    pub last_used_at: Instant,
}

/// Routing plan returned by [`SessionMap::route`] for a new non-tool-continuation request.
#[derive(Clone, Debug)]
pub enum RouteDecision {
    /// Active ACP session is an exact prefix of `messages` -> send only delta `[delta_start..]`.
    ContinueLinear {
        /// Active entry to reuse without calling `session/resume`.
        entry: AgySessionEntry,
        /// Index in `messages` where unsent messages begin.
        delta_start: usize,
    },
    /// Active session diverged, but another stored DAG branch matches a prefix -> call `session/resume`.
    ResumeBranch {
        /// Prior branch entry to resume via ACP `session/resume`.
        entry: AgySessionEntry,
        /// Index in `messages` where unsent messages begin.
        delta_start: usize,
    },
    /// No stored prefix matched (rewind, `/fork`, or cross-provider switch into `agy_acp`).
    ForkOrBootstrap {
        /// Most recent known ACP session for this `hrn_session_id`, if native `session/fork` is enabled.
        fork_from: Option<AgySessionEntry>,
    },
}

/// Compute a deterministic SHA-256 hash over a canonical slice of [`Msg`] values.
///
/// Only conversation-defining fields (role, content, tool calls, tool result linkage, image
/// fingerprints) are hashed so cosmetic metadata differences never spuriously invalidate cache hits.
pub fn hash_messages_prefix(messages: &[Msg]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((messages.len() as u64).to_le_bytes());
    for msg in messages {
        let role_tag: u8 = match msg.role {
            Role::System => 1,
            Role::User => 2,
            Role::Assistant => 3,
            Role::Tool => 4,
        };
        hasher.update([role_tag]);
        feed_str(&mut hasher, &msg.content);
        hasher.update([u8::from(msg.is_error)]);
        feed_opt_str(&mut hasher, msg.tool_call_id.as_deref());
        feed_opt_str(&mut hasher, msg.tool_name.as_deref());

        hasher.update((msg.tool_calls.len() as u64).to_le_bytes());
        for tc in &msg.tool_calls {
            feed_str(&mut hasher, &tc.id);
            feed_str(&mut hasher, &tc.name);
            feed_str(&mut hasher, &tc.arguments);
        }

        hasher.update((msg.images.len() as u64).to_le_bytes());
        for img in &msg.images {
            feed_str(&mut hasher, &img.media_type);
            feed_str(&mut hasher, &img.data_base64);
        }
    }
    hasher.finalize().into()
}

fn feed_str(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn feed_opt_str(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(s) => {
            hasher.update([1u8]);
            feed_str(hasher, s);
        }
        None => hasher.update([0u8]),
    }
}

/// Bounded in-memory DAG session map keyed by `(hrn_session_id, synced_prefix_hash)`.
#[derive(Debug)]
pub struct SessionMap {
    entries: HashMap<(String, [u8; 32]), AgySessionEntry>,
    active: HashMap<String, [u8; 32]>,
    max_entries: usize,
}

impl Default for SessionMap {
    fn default() -> Self {
        Self::with_capacity(MAX_SESSION_ENTRIES)
    }
}

impl SessionMap {
    /// Construct a session map with an explicit maximum entry count (for tests or tuning).
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            active: HashMap::new(),
            max_entries: max_entries.max(4),
        }
    }

    /// Number of tracked branch checkpoints currently stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map holds zero checkpoints.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Determine how to route `messages` for `hrn_session_id`.
    pub fn route(&self, hrn_session_id: &str, messages: &[Msg]) -> RouteDecision {
        // Case 2: active session is an exact prefix of the incoming message slice.
        if let Some(active_hash) = self.active.get(hrn_session_id)
            && let Some(entry) = self.entries.get(&(hrn_session_id.to_owned(), *active_hash))
            && prefix_matches(messages, entry)
        {
            return RouteDecision::ContinueLinear {
                entry: entry.clone(),
                delta_start: entry.synced_msg_count,
            };
        }

        // Case 3: find the longest matching prefix among any previously seen branch for this session.
        let mut best_match: Option<&AgySessionEntry> = None;
        for ((sid, _), entry) in &self.entries {
            if sid != hrn_session_id || !prefix_matches(messages, entry) {
                continue;
            }
            let better = best_match.is_none_or(|cur| {
                entry.synced_msg_count > cur.synced_msg_count
                    || (entry.synced_msg_count == cur.synced_msg_count
                        && entry.last_used_at > cur.last_used_at)
            });
            if better {
                best_match = Some(entry);
            }
        }
        if let Some(entry) = best_match {
            return RouteDecision::ResumeBranch {
                entry: entry.clone(),
                delta_start: entry.synced_msg_count,
            };
        }

        // Case 4: no saved prefix matched (rewind, /fork, or cross-provider switch).
        let fork_from = self
            .active
            .get(hrn_session_id)
            .and_then(|h| self.entries.get(&(hrn_session_id.to_owned(), *h)))
            .cloned();
        RouteDecision::ForkOrBootstrap { fork_from }
    }

    /// Record a synchronized branch checkpoint and mark it as the active head for `hrn_session_id`.
    pub fn record(&mut self, hrn_session_id: &str, entry: AgySessionEntry) {
        let key = (hrn_session_id.to_owned(), entry.synced_prefix_hash);
        self.active
            .insert(hrn_session_id.to_owned(), entry.synced_prefix_hash);
        self.entries.insert(key, entry);
        self.evict_if_needed();
    }

    /// Drop every cached checkpoint referencing a dead or invalid `agy_session_id`.
    pub fn invalidate_agy_session(&mut self, agy_session_id: &str) {
        self.entries
            .retain(|_, entry| entry.agy_session_id != agy_session_id);
        self.active
            .retain(|sid, hash| self.entries.contains_key(&(sid.clone(), *hash)));
    }

    fn evict_if_needed(&mut self) {
        while self.entries.len() > self.max_entries {
            let oldest_key = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used_at)
                .map(|(k, _)| k.clone());
            let Some(victim) = oldest_key else {
                break;
            };
            self.entries.remove(&victim);
            if self.active.get(&victim.0) == Some(&victim.1) {
                self.active.remove(&victim.0);
            }
        }
    }
}

fn prefix_matches(messages: &[Msg], entry: &AgySessionEntry) -> bool {
    if entry.synced_msg_count == 0 || entry.synced_msg_count > messages.len() {
        return false;
    }
    let Some(prefix) = messages.get(..entry.synced_msg_count) else {
        return false;
    };
    hash_messages_prefix(prefix) == entry.synced_prefix_hash
}
