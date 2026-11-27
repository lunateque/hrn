//! Host-authoritative live collaboration room owner (`CollabHost`).

use super::host_handlers::{handle_inbound_binary, send_host_frame};
use super::protocol::{HostFrame, MAX_COLLAB_FRAME_BYTES};
use hrn_edge_http::seal::{CollabLink, KEY_LEN, generate_key, generate_room_id};
use hrn_edge_http::{WsEvent, WsReader, WsWriter, connect_ws};
use hrn_kernel::{AgentEvent, DagNode, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot};

/// Snapshot of the host's active session sent to newly connecting guests.
#[derive(Clone, Debug, Default)]
pub struct HostSnapshot {
    /// Host session identifier.
    pub session_id: String,
    /// Session title.
    pub title: String,
    /// Active model alias, if any.
    pub model: Option<String>,
    /// Current head node ID, if any.
    pub head_node_id: Option<String>,
    /// Whether the host is currently executing a turn.
    pub busy: bool,
    /// Active branch nodes in chronological order.
    pub nodes: Vec<DagNode>,
}

/// Callback returning a fresh [`HostSnapshot`] whenever a guest completes `Hello`.
pub type SnapshotProvider = Arc<dyn Fn() -> HostSnapshot + Send + Sync>;

/// Inbound events emitted by [`CollabHost`] to the hosting runtime/UI.
#[derive(Clone, Debug)]
pub enum HostInbound {
    /// A guest completed `Hello` and received the back-transcript snapshot.
    GuestJoined {
        /// Relay-assigned non-zero peer identifier.
        peer_id: u32,
        /// Sanitized guest display name.
        name: String,
        /// Whether the guest authenticated with the room write token.
        can_write: bool,
    },
    /// A guest disconnected from the room.
    GuestLeft {
        /// Relay-assigned peer identifier.
        peer_id: u32,
        /// Sanitized guest display name.
        name: String,
    },
    /// An authorized interactive guest submitted a prompt.
    Prompt {
        /// Relay-assigned peer identifier.
        peer_id: u32,
        /// Sanitized guest display name.
        guest_name: String,
        /// Prompt prefixed with `[guest:<name>] `.
        badged_text: String,
        /// Raw prompt text without badge.
        raw_text: String,
    },
    /// An authorized interactive guest injected a mid-turn steer.
    Steer {
        /// Relay-assigned peer identifier.
        peer_id: u32,
        /// Sanitized guest display name.
        guest_name: String,
        /// Raw steering text.
        text: String,
    },
    /// An authorized interactive guest requested interrupting the active turn.
    Abort {
        /// Relay-assigned peer identifier.
        peer_id: u32,
        /// Sanitized guest display name.
        guest_name: String,
    },
    /// An authorized interactive guest answered a pending approval prompt.
    Approve {
        /// Relay-assigned peer identifier.
        peer_id: u32,
        /// Sanitized guest display name.
        guest_name: String,
        /// Pending tool call identifier.
        call_id: String,
        /// Whether the call was approved (`true`) or denied (`false`).
        approved: bool,
        /// Whether the guest requested session-wide tool approval.
        always: bool,
    },
    /// An authorized interactive guest answered a pending structured `ask` form.
    AskAnswer {
        /// Relay-assigned peer identifier.
        peer_id: u32,
        /// Sanitized guest display name.
        guest_name: String,
        /// Pending `AskRequest` identifier.
        id: String,
        /// Ordered answers corresponding to the form's fields.
        answers: Vec<String>,
    },
    /// The relay connection ended.
    Disconnected {
        /// Human-readable reason.
        reason: String,
    },
}

enum HostCmd {
    Broadcast(Box<HostFrame>),
    Close {
        reason: String,
        ack: Option<oneshot::Sender<()>>,
    },
}

pub(super) struct GuestPeer {
    pub(super) name: String,
    pub(super) can_write: bool,
}

/// Handle to a live host-authoritative collaboration room.
#[derive(Clone)]
pub struct CollabHost {
    room: String,
    key: [u8; KEY_LEN],
    write_token: [u8; KEY_LEN],
    interactive_link: String,
    view_only_link: String,
    guest_count: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<HostCmd>,
}

impl std::fmt::Debug for CollabHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollabHost")
            .field("room", &self.room)
            .field("key", &"<redacted>")
            .field("write_token", &"<redacted>")
            .field("interactive_link", &"<redacted>")
            .field("view_only_link", &"<redacted>")
            .field("guest_count", &self.guest_count())
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl CollabHost {
    /// Open a fresh encrypted collaboration room on `relay_base` (`http://`, `https://`, `ws://`,
    /// or `wss://`).
    pub async fn start(
        relay_base: &str,
        snapshot_fn: SnapshotProvider,
    ) -> Result<(Self, mpsc::Receiver<HostInbound>)> {
        Self::start_with_public_base(relay_base, relay_base, snapshot_fn).await
    }

    /// Open a fresh encrypted collaboration room connecting the host over `connect_base`
    /// (for example `http://127.0.0.1:<port>` for an in-process relay) while formatting
    /// [`Self::interactive_link`] and [`Self::view_only_link`] with `public_base`.
    pub async fn start_with_public_base(
        connect_base: &str,
        public_base: &str,
        snapshot_fn: SnapshotProvider,
    ) -> Result<(Self, mpsc::Receiver<HostInbound>)> {
        let room = generate_room_id()?;
        let key = generate_key()?;
        let write_token = generate_key()?;
        let public_link = CollabLink {
            relay_base: Some(public_base.trim().trim_end_matches('/').to_owned()),
            room: room.clone(),
            key,
            write_token: Some(write_token),
        };
        let connect_link = CollabLink {
            relay_base: Some(connect_base.trim().trim_end_matches('/').to_owned()),
            room: room.clone(),
            key,
            write_token: Some(write_token),
        };
        let interactive_link = public_link.to_url(None)?;
        let view_only_link = public_link.view_only().to_url(None)?;
        let ws_url = connect_link.to_ws_url("host", None)?;

        let mut ws = connect_ws(&ws_url).await?;
        ws.set_max_message_bytes(MAX_COLLAB_FRAME_BYTES);
        let (reader, writer) = ws.into_split();

        let (cmd_tx, cmd_rx) = mpsc::channel(128);
        let (inbound_tx, inbound_rx) = mpsc::channel(128);
        let guest_count = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));

        let ctx = HostTaskCtx {
            room: room.clone(),
            key,
            write_token,
            snapshot_fn,
            guest_count: Arc::clone(&guest_count),
            closed: Arc::clone(&closed),
            inbound_tx,
        };
        tokio::spawn(run_host_loop(reader, writer, cmd_rx, ctx));

        Ok((
            Self {
                room,
                key,
                write_token,
                interactive_link,
                view_only_link,
                guest_count,
                closed,
                cmd_tx,
            },
            inbound_rx,
        ))
    }

    /// Room identifier.
    pub fn room(&self) -> &str {
        &self.room
    }

    /// 32-byte room encryption key.
    pub fn key(&self) -> &[u8; KEY_LEN] {
        &self.key
    }

    /// 32-byte room write token.
    pub fn write_token(&self) -> &[u8; KEY_LEN] {
        &self.write_token
    }

    /// Full interactive join link (`<relay>/r/<room>#<key>.<write_token>`).
    pub fn interactive_link(&self) -> &str {
        &self.interactive_link
    }

    /// Full view-only join link (`<relay>/r/<room>#<key>`).
    pub fn view_only_link(&self) -> &str {
        &self.view_only_link
    }

    /// Format a join link for this room under an alternate `relay_base` URL.
    pub fn link_with_base(&self, relay_base: &str, interactive: bool) -> Result<String> {
        let link = CollabLink {
            relay_base: Some(relay_base.trim().trim_end_matches('/').to_owned()),
            room: self.room.clone(),
            key: self.key,
            write_token: interactive.then_some(self.write_token),
        };
        link.to_url(None)
    }

    /// Current number of connected guests.
    pub fn guest_count(&self) -> usize {
        self.guest_count.load(Ordering::Relaxed)
    }

    /// Whether the room has closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Broadcast a newly persisted authoritative DAG node to all connected guests.
    pub fn broadcast_entry(&self, node: DagNode) {
        let _ = self
            .cmd_tx
            .try_send(HostCmd::Broadcast(Box::new(HostFrame::Entry { node })));
    }

    /// Broadcast a live agent streaming event to all connected guests.
    pub fn broadcast_event(&self, event: AgentEvent) {
        let _ = self
            .cmd_tx
            .try_send(HostCmd::Broadcast(Box::new(HostFrame::Event { event })));
    }

    /// Broadcast updated host state to all connected guests.
    pub fn broadcast_state(&self, busy: bool, head_node_id: Option<String>) {
        let guests = self.guest_count();
        let _ = self
            .cmd_tx
            .try_send(HostCmd::Broadcast(Box::new(HostFrame::State {
                busy,
                head_node_id,
                guests,
            })));
    }

    /// Schedule sending `HostFrame::Goodbye` and closing the room without awaiting completion.
    pub fn close(&self, reason: impl Into<String>) {
        self.closed.store(true, Ordering::Release);
        let _ = self.cmd_tx.try_send(HostCmd::Close {
            reason: reason.into(),
            ack: None,
        });
    }

    /// Broadcast `HostFrame::Goodbye`, flush the socket, and await room shutdown.
    pub async fn close_and_wait(&self, reason: impl Into<String>) {
        self.closed.store(true, Ordering::Release);
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(HostCmd::Close {
                reason: reason.into(),
                ack: Some(ack_tx),
            })
            .await
            .is_ok()
        {
            let _ = ack_rx.await;
        }
    }
}

pub(super) struct HostTaskCtx {
    pub(super) room: String,
    pub(super) key: [u8; KEY_LEN],
    pub(super) write_token: [u8; KEY_LEN],
    pub(super) snapshot_fn: SnapshotProvider,
    pub(super) guest_count: Arc<AtomicUsize>,
    pub(super) closed: Arc<AtomicBool>,
    pub(super) inbound_tx: mpsc::Sender<HostInbound>,
}

async fn run_host_loop(
    mut reader: WsReader,
    mut writer: WsWriter,
    mut cmd_rx: mpsc::Receiver<HostCmd>,
    ctx: HostTaskCtx,
) {
    let mut guests: HashMap<u32, GuestPeer> = HashMap::new();
    let mut disconnect_reason = "relay disconnected".to_owned();

    loop {
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    Some(HostCmd::Broadcast(frame)) => {
                        if send_host_frame(&mut writer, 0, &frame, &ctx.key).await.is_err() {
                            break;
                        }
                    }
                    Some(HostCmd::Close { reason, ack }) => {
                        let _ = send_host_frame(
                            &mut writer,
                            0,
                            &HostFrame::Goodbye { reason: reason.clone() },
                            &ctx.key,
                        ).await;
                        let _ = writer.send_close().await;
                        disconnect_reason = reason;
                        if let Some(tx) = ack {
                            let _ = tx.send(());
                        }
                        break;
                    }
                    None => break,
                }
            }
            ev_res = reader.next_event() => {
                match ev_res {
                    Ok(WsEvent::Binary(raw)) => {
                        if handle_inbound_binary(
                            &raw,
                            &mut guests,
                            &mut writer,
                            &ctx,
                        ).await.is_err() {
                            break;
                        }
                    }
                    Ok(WsEvent::Ping(p)) => {
                        if writer.send_pong(&p).await.is_err() {
                            break;
                        }
                    }
                    Ok(WsEvent::Text(_)) => {}
                    Ok(WsEvent::Closed) => break,
                    Err(err) => {
                        disconnect_reason = err.to_string();
                        break;
                    }
                }
            }
        }
    }

    ctx.closed.store(true, Ordering::Release);
    ctx.guest_count.store(0, Ordering::Relaxed);
    let _ = ctx.inbound_tx.try_send(HostInbound::Disconnected {
        reason: disconnect_reason,
    });
}
