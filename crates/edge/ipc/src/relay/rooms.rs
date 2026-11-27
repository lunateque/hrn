//! WebSocket room state machine: single host per room, bounded guest count, 4-byte LE target
//! routing, bounded per-connection send queues (dropping slow peers cleanly), and idle teardown.

use super::FrameObserver;
use hrn_edge_http::seal::is_valid_id;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

/// Peer role connected to `GET /r/<room>?role=host|guest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerRole {
    Host,
    Guest,
}

/// Reason a room join pre-check or registration was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinError {
    InvalidRoom,
    HostAlreadyPresent,
    MaxRoomsReached,
    MaxGuestsReached,
}

struct PeerEntry {
    conn_token: u64,
    tx: mpsc::Sender<Vec<u8>>,
}

struct Room {
    host: Option<PeerEntry>,
    guests: HashMap<u32, PeerEntry>,
    next_guest_id: u32,
    last_activity: Instant,
}

impl Room {
    fn new(now: Instant) -> Self {
        Self {
            host: None,
            guests: HashMap::new(),
            next_guest_id: 1,
            last_activity: now,
        }
    }

    fn alloc_guest_id(&mut self) -> Option<u32> {
        for _ in 0..1024 {
            let id = self.next_guest_id;
            self.next_guest_id = self.next_guest_id.wrapping_add(1).max(1);
            if id != 0 && !self.guests.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }
}

/// Room table shared across all relay WebSocket connections.
pub(crate) struct RoomHub {
    max_rooms: usize,
    max_guests_per_room: usize,
    send_queue_cap: usize,
    idle_timeout: Duration,
    observer: Option<FrameObserver>,
    next_conn_token: AtomicU64,
    rooms: Mutex<HashMap<String, Room>>,
}

impl RoomHub {
    pub(crate) fn new(
        max_rooms: usize,
        max_guests_per_room: usize,
        send_queue_cap: usize,
        idle_timeout: Duration,
        observer: Option<FrameObserver>,
    ) -> Self {
        Self {
            max_rooms,
            max_guests_per_room,
            send_queue_cap,
            idle_timeout,
            observer,
            next_conn_token: AtomicU64::new(1),
            rooms: Mutex::new(HashMap::new()),
        }
    }

    /// Validate that a join for `(room_id, role)` can proceed before committing HTTP 101 upgrade.
    pub(crate) fn precheck_join(&self, room_id: &str, role: PeerRole) -> Result<(), JoinError> {
        if !is_valid_id(room_id) {
            return Err(JoinError::InvalidRoom);
        }
        let rooms = self
            .rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match rooms.get(room_id) {
            Some(room) => match role {
                PeerRole::Host if room.host.is_some() => Err(JoinError::HostAlreadyPresent),
                PeerRole::Guest if room.guests.len() >= self.max_guests_per_room => {
                    Err(JoinError::MaxGuestsReached)
                }
                _ => Ok(()),
            },
            None => {
                if rooms.len() >= self.max_rooms {
                    Err(JoinError::MaxRoomsReached)
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Register a peer in `room_id`, returning `(peer_id, conn_token, outbound_rx)`.
    /// Host always receives `peer_id == 0`; guests receive unique non-zero `u32` IDs.
    pub(crate) fn join(
        &self,
        room_id: &str,
        role: PeerRole,
    ) -> Result<(u32, u64, mpsc::Receiver<Vec<u8>>), JoinError> {
        if !is_valid_id(room_id) {
            return Err(JoinError::InvalidRoom);
        }
        let now = Instant::now();
        let mut rooms = self
            .rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !rooms.contains_key(room_id) && rooms.len() >= self.max_rooms {
            return Err(JoinError::MaxRoomsReached);
        }
        let room = rooms
            .entry(room_id.to_owned())
            .or_insert_with(|| Room::new(now));
        room.last_activity = now;
        let conn_token = self.next_conn_token.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(self.send_queue_cap.max(1));
        let entry = PeerEntry { conn_token, tx };
        match role {
            PeerRole::Host => {
                if room.host.is_some() {
                    return Err(JoinError::HostAlreadyPresent);
                }
                room.host = Some(entry);
                Ok((0, conn_token, rx))
            }
            PeerRole::Guest => {
                if room.guests.len() >= self.max_guests_per_room {
                    return Err(JoinError::MaxGuestsReached);
                }
                let Some(peer_id) = room.alloc_guest_id() else {
                    return Err(JoinError::MaxGuestsReached);
                };
                room.guests.insert(peer_id, entry);
                Ok((peer_id, conn_token, rx))
            }
        }
    }

    /// Forward an opaque binary frame `[target_or_src_id: u32 LE][ciphertext...]`.
    ///
    /// - Frames without a ciphertext body (`raw.len() <= 4`) are dropped so guests cannot forge
    ///   the relay's 4-byte `[guest_id LE]` disconnect sentinel.
    /// - Host -> `target == 0`: broadcast to all guests (prefix `0u32 LE`).
    /// - Host -> `target != 0`: unicast to guest `target` (prefix `0u32 LE`).
    /// - Guest -> Host: rewrite prefix to `peer_id` (LE) and deliver to host only. If the host's
    ///   bounded send queue is momentarily full (`TrySendError::Full`), drop the excess frame
    ///   without tearing down the room; tear down only on `TrySendError::Closed`.
    pub(crate) fn route_frame(
        &self,
        room_id: &str,
        role: PeerRole,
        peer_id: u32,
        conn_token: u64,
        raw: &[u8],
    ) {
        if raw.len() <= 4 {
            return;
        }
        let target = u32::from_le_bytes([
            raw.first().copied().unwrap_or(0),
            raw.get(1).copied().unwrap_or(0),
            raw.get(2).copied().unwrap_or(0),
            raw.get(3).copied().unwrap_or(0),
        ]);
        let payload = raw.get(4..).unwrap_or_default();
        let now = Instant::now();
        let mut rooms = self
            .rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(room) = rooms.get_mut(room_id) else {
            return;
        };
        let sender_active = match role {
            PeerRole::Host => room
                .host
                .as_ref()
                .is_some_and(|h| h.conn_token == conn_token),
            PeerRole::Guest => room
                .guests
                .get(&peer_id)
                .is_some_and(|g| g.conn_token == conn_token),
        };
        if !sender_active {
            return;
        }
        if let Some(obs) = &self.observer {
            obs(room_id, raw);
        }
        room.last_activity = now;
        match role {
            PeerRole::Host => route_from_host(room, target, payload),
            PeerRole::Guest => {
                let mut forwarded = Vec::with_capacity(4 + payload.len());
                forwarded.extend_from_slice(&peer_id.to_le_bytes());
                forwarded.extend_from_slice(payload);
                if let Some(host) = &room.host {
                    match host.tx.try_send(forwarded) {
                        Ok(()) | Err(TrySendError::Full(_)) => {
                            // On `Full`, drop the excess guest frame cleanly so a single guest
                            // burst can never tear down the room for the host or other guests.
                        }
                        Err(TrySendError::Closed(_)) => {
                            room.host = None;
                            room.guests.clear();
                            rooms.remove(room_id);
                        }
                    }
                }
            }
        }
    }

    /// Remove a disconnecting peer when its `conn_token` still matches the live registration.
    ///
    /// Matching `conn_token` ensures a delayed `leave()` from a stale/evicted connection can never
    /// delete a newly recreated room or evict a newly registered guest with a reused ID.
    pub(crate) fn leave(&self, room_id: &str, role: PeerRole, peer_id: u32, conn_token: u64) {
        let mut rooms = self
            .rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match role {
            PeerRole::Host => {
                let matches_host = rooms
                    .get(room_id)
                    .and_then(|r| r.host.as_ref())
                    .is_some_and(|h| h.conn_token == conn_token);
                if matches_host {
                    rooms.remove(room_id);
                }
            }
            PeerRole::Guest => {
                if let Some(room) = rooms.get_mut(room_id) {
                    let matches_guest = room
                        .guests
                        .get(&peer_id)
                        .is_some_and(|g| g.conn_token == conn_token);
                    if matches_guest {
                        room.guests.remove(&peer_id);
                        if let Some(host) = &room.host {
                            let _ = host.tx.try_send(peer_id.to_le_bytes().to_vec());
                        }
                    }
                    if room.host.is_none() && room.guests.is_empty() {
                        rooms.remove(room_id);
                    }
                }
            }
        }
    }

    /// Reap idle or empty rooms and return active room count.
    pub(crate) fn reap_idle(&self) -> usize {
        let now = Instant::now();
        let mut rooms = self
            .rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rooms.retain(|_, room| {
            let active = room.host.is_some() || !room.guests.is_empty();
            active && now.duration_since(room.last_activity) < self.idle_timeout
        });
        rooms.len()
    }

    /// Number of active rooms.
    pub(crate) fn room_count(&self) -> usize {
        self.rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

fn route_from_host(room: &mut Room, target: u32, payload: &[u8]) {
    let mut forwarded = Vec::with_capacity(4 + payload.len());
    forwarded.extend_from_slice(&0u32.to_le_bytes());
    forwarded.extend_from_slice(payload);
    let mut evicted = Vec::new();
    if target == 0 {
        for (&gid, peer) in &room.guests {
            if peer.tx.try_send(forwarded.clone()).is_err() {
                evicted.push(gid);
            }
        }
    } else if let Some(peer) = room.guests.get(&target)
        && peer.tx.try_send(forwarded).is_err()
    {
        evicted.push(target);
    }
    for gid in evicted {
        room.guests.remove(&gid);
        if let Some(host) = &room.host {
            let _ = host.tx.try_send(gid.to_le_bytes().to_vec());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_burst_does_not_tear_down_room() {
        let hub = RoomHub::new(8, 8, 2, Duration::from_mins(5), None);
        let room = "room_burst_test";
        let (_, host_tok, mut host_rx) = hub.join(room, PeerRole::Host).expect("host join");
        let (g1, g1_tok, mut g1_rx) = hub.join(room, PeerRole::Guest).expect("g1 join");
        let (g2, g2_tok, mut g2_rx) = hub.join(room, PeerRole::Guest).expect("g2 join");

        // Guest 1 floods 20 frames into host's 2-slot queue -> excess frames drop, room survives!
        let mut flood = vec![0u8; 4];
        flood.extend_from_slice(b"ciphertext_payload");
        for _ in 0..20 {
            hub.route_frame(room, PeerRole::Guest, g1, g1_tok, &flood);
        }
        assert_eq!(hub.room_count(), 1, "burst must never destroy the room");

        // Host drains its 2 queued frames, then guest 2 and host broadcast still work normally.
        assert!(host_rx.try_recv().is_ok());
        assert!(host_rx.try_recv().is_ok());
        assert!(host_rx.try_recv().is_err());

        let mut g2_frame = vec![0u8; 4];
        g2_frame.extend_from_slice(b"from_guest_2");
        hub.route_frame(room, PeerRole::Guest, g2, g2_tok, &g2_frame);
        let received = host_rx.try_recv().expect("host receives guest 2 frame");
        assert_eq!(&received[..4], &g2.to_le_bytes());
        assert_eq!(&received[4..], b"from_guest_2");

        let mut host_bcast = vec![0u8; 4];
        host_bcast.extend_from_slice(b"host_broadcast");
        hub.route_frame(room, PeerRole::Host, 0, host_tok, &host_bcast);
        assert!(g1_rx.try_recv().is_ok());
        assert!(g2_rx.try_recv().is_ok());
    }

    #[test]
    fn stale_host_and_guest_leave_cannot_delete_recreated_room() {
        let hub = RoomHub::new(8, 8, 8, Duration::from_mins(5), None);
        let room = "room_stale_leave";
        let (_, host1_tok, host1_rx) = hub.join(room, PeerRole::Host).expect("host1");
        let (g1, g1_tok, _g1_rx) = hub.join(room, PeerRole::Guest).expect("g1");

        // Drop host1_rx so host1 is dead; a guest send closes room1, then host2 recreates `room`.
        drop(host1_rx);
        let mut frame = vec![0u8; 4];
        frame.extend_from_slice(b"ping_dead_host");
        hub.route_frame(room, PeerRole::Guest, g1, g1_tok, &frame);
        assert_eq!(hub.room_count(), 0);

        let (_, host2_tok, _host2_rx) = hub.join(room, PeerRole::Host).expect("host2");
        let (g2, g2_tok, mut g2_rx) = hub.join(room, PeerRole::Guest).expect("g2");
        assert_eq!(g2, g1, "recreated room starts guest IDs at 1");

        // Stale host1 and stale g1 call leave() -> MUST NOT delete host2's room or evict g2!
        hub.leave(room, PeerRole::Host, 0, host1_tok);
        hub.leave(room, PeerRole::Guest, g1, g1_tok);
        assert_eq!(hub.room_count(), 1, "stale host1 leave must not kill host2");

        let mut msg = vec![0u8; 4];
        msg.extend_from_slice(b"still_alive");
        hub.route_frame(room, PeerRole::Host, 0, host2_tok, &msg);
        assert!(g2_rx.try_recv().is_ok(), "stale g1 leave must not evict g2");

        hub.leave(room, PeerRole::Guest, g2, g2_tok);
        hub.leave(room, PeerRole::Host, 0, host2_tok);
        assert_eq!(hub.room_count(), 0);
    }

    #[test]
    fn guest_cannot_forge_four_byte_disconnect_sentinel() {
        let hub = RoomHub::new(8, 8, 8, Duration::from_mins(5), None);
        let room = "room_sentinel_test";
        let (_, _host_tok, mut host_rx) = hub.join(room, PeerRole::Host).expect("host");
        let (g1, g1_tok, _g1_rx) = hub.join(room, PeerRole::Guest).expect("g1");

        // Guest sends empty-body 4-byte frame -> MUST be dropped before reaching host.
        hub.route_frame(room, PeerRole::Guest, g1, g1_tok, &g1.to_le_bytes());
        assert!(
            host_rx.try_recv().is_err(),
            "guest must never be able to forge a 4-byte disconnect sentinel"
        );
    }
}
