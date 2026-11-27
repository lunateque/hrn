//! Bounded in-memory ciphertext blob store with TTL expiry and total-capacity eviction.

use bytes::Bytes;
use hrn_edge_http::seal::{generate_room_id, is_valid_id};
use hrn_kernel::{HrnError, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct BlobEntry {
    data: Bytes,
    expires_at: Instant,
}

struct BlobState {
    entries: HashMap<String, BlobEntry>,
    order: VecDeque<String>,
    total_bytes: usize,
}

/// Bounded opaque-blob store behind `POST /s` and `GET /s/<id>/raw`.
pub(crate) struct BlobStore {
    max_blob_bytes: usize,
    max_total_bytes: usize,
    max_blobs: usize,
    ttl: Duration,
    state: Mutex<BlobState>,
}

impl BlobStore {
    pub(crate) fn new(
        max_blob_bytes: usize,
        max_total_bytes: usize,
        max_blobs: usize,
        ttl: Duration,
    ) -> Self {
        Self {
            max_blob_bytes,
            max_total_bytes,
            max_blobs,
            ttl,
            state: Mutex::new(BlobState {
                entries: HashMap::new(),
                order: VecDeque::new(),
                total_bytes: 0,
            }),
        }
    }

    /// Store opaque sealed bytes and return a freshly generated random blob ID.
    pub(crate) fn insert(&self, data: Bytes) -> Result<String> {
        if data.len() > self.max_blob_bytes {
            return Err(HrnError::Config(format!(
                "blob exceeds {} byte limit",
                self.max_blob_bytes
            )));
        }
        if data.len() > self.max_total_bytes {
            return Err(HrnError::Config(
                "blob exceeds relay total storage capacity".to_owned(),
            ));
        }
        let now = Instant::now();
        let expires_at = now
            .checked_add(self.ttl)
            .ok_or_else(|| HrnError::Config("blob TTL overflows monotonic clock".to_owned()))?;
        let id = generate_room_id()?;
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        evict_expired(&mut st, now);
        while (st.total_bytes.saturating_add(data.len()) > self.max_total_bytes
            || st.entries.len() >= self.max_blobs)
            && let Some(oldest) = st.order.pop_front()
        {
            if let Some(removed) = st.entries.remove(&oldest) {
                st.total_bytes = st.total_bytes.saturating_sub(removed.data.len());
            }
        }
        if st.total_bytes.saturating_add(data.len()) > self.max_total_bytes
            || st.entries.len() >= self.max_blobs
        {
            return Err(HrnError::Store("relay blob storage full".to_owned()));
        }
        st.total_bytes = st.total_bytes.saturating_add(data.len());
        st.entries
            .insert(id.clone(), BlobEntry { data, expires_at });
        st.order.push_back(id.clone());
        Ok(id)
    }

    /// Fetch non-expired ciphertext bytes for `id`.
    pub(crate) fn get(&self, id: &str) -> Option<Bytes> {
        if !is_valid_id(id) {
            return None;
        }
        let now = Instant::now();
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expired = st.entries.get(id).is_some_and(|e| e.expires_at <= now);
        if expired {
            if let Some(removed) = st.entries.remove(id) {
                st.total_bytes = st.total_bytes.saturating_sub(removed.data.len());
            }
            return None;
        }
        st.entries.get(id).map(|e| e.data.clone())
    }

    /// Evict expired blobs and return current active count.
    pub(crate) fn count_and_reap(&self) -> usize {
        let now = Instant::now();
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        evict_expired(&mut st, now);
        st.entries.len()
    }
}

fn evict_expired(st: &mut BlobState, now: Instant) {
    while let Some(front) = st.order.front() {
        let drop_it = match st.entries.get(front) {
            Some(entry) => entry.expires_at <= now,
            None => true,
        };
        if !drop_it {
            break;
        }
        if let Some(id) = st.order.pop_front()
            && let Some(removed) = st.entries.remove(&id)
        {
            st.total_bytes = st.total_bytes.saturating_sub(removed.data.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_rejects_overflowing_ttl_without_panicking() {
        let store = BlobStore::new(1024, 4096, 8, Duration::MAX);
        assert!(store.insert(Bytes::from_static(&[1u8; 32])).is_err());
        assert_eq!(store.count_and_reap(), 0);
    }
}
