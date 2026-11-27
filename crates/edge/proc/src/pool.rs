//! Persistent shells keyed by agent session.

use crate::lock;
use crate::session::ShellSession;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Live shells kept per pool; the least recently used idle one is evicted beyond this.
const MAX_SHELLS: usize = 8;

struct PoolEntry {
    session: Arc<tokio::sync::Mutex<ShellSession>>,
    last_used: Instant,
}

/// Persistent shells keyed by agent session. A call that finds its session's shell busy (parallel
/// tool calls) is told so and is expected to fall back to a one-shot run.
#[derive(Default)]
pub struct ShellPool {
    sessions: Mutex<BTreeMap<String, PoolEntry>>,
}

impl ShellPool {
    /// An empty pool.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    /// Lock the shell session for `key`, or `None` while another call is running in it. Evicts
    /// the least recently used idle shell once the pool holds more than eight.
    pub fn acquire(&self, key: &str) -> Option<tokio::sync::OwnedMutexGuard<ShellSession>> {
        let mut map = lock(&self.sessions);
        if !map.contains_key(key) {
            if map.len() >= MAX_SHELLS {
                let idle_oldest = map
                    .iter()
                    .filter(|(_, e)| e.session.try_lock().is_ok())
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, _)| k.clone());
                if let Some(victim) = idle_oldest {
                    map.remove(&victim);
                }
            }
            map.insert(
                key.to_owned(),
                PoolEntry {
                    session: Arc::new(tokio::sync::Mutex::new(ShellSession::new())),
                    last_used: Instant::now(),
                },
            );
        }
        let entry = map.get_mut(key)?;
        entry.last_used = Instant::now();
        Arc::clone(&entry.session).try_lock_owned().ok()
    }
}
