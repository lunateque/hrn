//! In-process and standalone `hrn-relay` server for sealed share blobs, live collab rooms,
//! and E2E-encrypted mobile supervision (plus zero-config auto-relay + optional HTTPS tunnel).
//!
//! Zero authentication logic on the relay: possession of the room/share URL fragment encryption
//! key is the cryptographic boundary; the relay handles strictly opaque ciphertext.

mod auto;
mod auto_probe;
mod blobs;
mod rooms;
mod server;
mod ws_server;

pub use auto::{AutoRelay, AutoRelayMode, cloudflared_available, parse_trycloudflare_url};

use hrn_kernel::{HrnError, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, oneshot};

/// Default maximum single sealed share blob accepted on `POST /s` (1 MiB).
pub const DEFAULT_MAX_BLOB_BYTES: usize = 1024 * 1024;
/// Default aggregate memory budget across all stored blobs (64 MiB).
pub const DEFAULT_MAX_TOTAL_BLOB_BYTES: usize = 64 * 1024 * 1024;
/// Default maximum stored blob count.
pub const DEFAULT_MAX_BLOBS: usize = 4096;
/// Default blob retention TTL (30 days).
pub const DEFAULT_BLOB_TTL: Duration = Duration::from_hours(720);
/// Maximum accepted blob retention TTL (365 days) to prevent monotonic clock overflow.
pub const MAX_BLOB_TTL: Duration = Duration::from_hours(365 * 24);
/// Default maximum concurrent collab rooms.
pub const DEFAULT_MAX_ROOMS: usize = 256;
/// Default maximum concurrent guests per room.
pub const DEFAULT_MAX_GUESTS_PER_ROOM: usize = 32;
/// Default maximum single WebSocket frame size (256 KiB).
pub const DEFAULT_MAX_WS_FRAME_BYTES: usize = 256 * 1024;
/// Default bounded per-connection outbound frame queue capacity.
pub const DEFAULT_SEND_QUEUE_CAP: usize = 64;
/// Default idle room teardown timeout (5 minutes).
pub const DEFAULT_ROOM_IDLE_TIMEOUT: Duration = Duration::from_mins(5);
/// Default maximum concurrent TCP connections accepted by the relay.
pub const DEFAULT_MAX_CONNECTIONS: usize = 512;

/// Optional observer called with `(room_id, raw_ws_frame_bytes)` for every forwarded relay frame.
pub type FrameObserver = Arc<dyn Fn(&str, &[u8]) + Send + Sync>;

/// Runtime configuration for [`serve`].
#[derive(Clone)]
pub struct RelayConfig {
    /// Maximum single blob size on `POST /s`.
    pub max_blob_bytes: usize,
    /// Maximum total bytes held in the blob store.
    pub max_total_blob_bytes: usize,
    /// Maximum number of stored blobs.
    pub max_blobs: usize,
    /// Retention TTL for uploaded blobs.
    pub blob_ttl: Duration,
    /// Maximum active collaboration rooms.
    pub max_rooms: usize,
    /// Maximum guests per collaboration room.
    pub max_guests_per_room: usize,
    /// Maximum inbound WebSocket frame size in bytes.
    pub max_ws_frame_bytes: usize,
    /// Bounded outbound send queue length per peer (slow peers are disconnected).
    pub send_queue_cap: usize,
    /// Idle room inactivity duration before teardown.
    pub room_idle_timeout: Duration,
    /// Maximum concurrent TCP connections handled simultaneously.
    pub max_connections: usize,
    /// Optional wire-frame capture hook for auditing/testing.
    pub on_frame: Option<FrameObserver>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            max_total_blob_bytes: DEFAULT_MAX_TOTAL_BLOB_BYTES,
            max_blobs: DEFAULT_MAX_BLOBS,
            blob_ttl: DEFAULT_BLOB_TTL,
            max_rooms: DEFAULT_MAX_ROOMS,
            max_guests_per_room: DEFAULT_MAX_GUESTS_PER_ROOM,
            max_ws_frame_bytes: DEFAULT_MAX_WS_FRAME_BYTES,
            send_queue_cap: DEFAULT_SEND_QUEUE_CAP,
            room_idle_timeout: DEFAULT_ROOM_IDLE_TIMEOUT,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            on_frame: None,
        }
    }
}

/// Running `hrn-relay` server handle.
pub struct RelayHandle {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    reaper: Option<tokio::task::JoinHandle<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RelayHandle {
    /// Local bound socket address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Base HTTP URL (`http://<addr>`).
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop accepting new connections and wait for the server loop to exit.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(reaper) = self.reaper.take() {
            reaper.abort();
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(reaper) = self.reaper.take() {
            reaper.abort();
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Start serving `hrn-relay` on an already-bound [`TcpListener`].
pub async fn serve(listener: TcpListener, cfg: RelayConfig) -> Result<RelayHandle> {
    let addr = listener
        .local_addr()
        .map_err(|e| HrnError::Io(format!("relay local_addr: {e}")))?;
    let conn_limit = Arc::new(Semaphore::new(cfg.max_connections.max(1)));
    let state = Arc::new(server::RelayState::new(cfg));
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    let reaper_state = Arc::clone(&state);
    let reaper = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let _ = reaper_state.blobs.count_and_reap();
            let _ = reaper_state.rooms.reap_idle();
        }
    });

    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else {
                        continue;
                    };
                    let Ok(permit) = Arc::clone(&conn_limit).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let st = Arc::clone(&state);
                    tokio::spawn(async move {
                        let _permit = permit;
                        server::handle_connection(stream, st).await;
                    });
                }
            }
        }
    });

    tokio::task::yield_now().await;
    Ok(RelayHandle {
        addr,
        shutdown_tx: Some(shutdown_tx),
        reaper: Some(reaper),
        task: Some(task),
    })
}

/// Parse a human-friendly TTL string such as `30d`, `24h`, `15m`, or `60s`.
pub fn parse_ttl(raw: &str) -> Result<Duration> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(HrnError::Config("empty duration".to_owned()));
    }
    let (num_str, mult) = if let Some(n) = s.strip_suffix('d') {
        (n, 86_400u64)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else {
        (s, 1u64)
    };
    let count: u64 = num_str
        .parse()
        .map_err(|_| HrnError::Config(format!("invalid TTL duration `{raw}`")))?;
    let secs = count
        .checked_mul(mult)
        .filter(|&v| v > 0 && v <= MAX_BLOB_TTL.as_secs())
        .ok_or_else(|| {
            HrnError::Config(format!(
                "invalid TTL duration `{raw}` (must be between 1s and 365d)"
            ))
        })?;
    Ok(Duration::from_secs(secs))
}
