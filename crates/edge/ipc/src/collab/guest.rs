//! Live collaboration guest client (`CollabGuest`) with local replica session sync
//! (`collab-of=<room>`).

use super::protocol::{
    GuestFrame, HostFrame, MAX_COLLAB_FRAME_BYTES, collab_branch_tag, decode_routed_frame,
    encode_routed_frame, replica_node_id, replica_session_id, sanitize_guest_name,
};
use hrn_edge_http::seal::{CollabLink, KEY_LEN, encode_key};
use hrn_edge_http::{WsEvent, WsReader, WsWriter, connect_ws};
use hrn_kernel::{
    AgentEvent, AgentStatus, Clock, DagNode, HrnError, PermissionMode, Result, SessionMeta,
    SessionStore, SystemClock, ThinkingLevel,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, oneshot};

/// Events received by a guest from the authoritative host.
#[derive(Clone, Debug)]
pub enum GuestEvent {
    /// Initial handshake acknowledgment from the host.
    Welcome {
        /// Room identifier.
        room: String,
        /// Authoritative host session ID.
        session_id: String,
        /// Session title.
        title: String,
        /// Active host model alias.
        model: Option<String>,
        /// Whether this guest is permitted to prompt/abort.
        can_write: bool,
        /// Number of `SnapshotChunk` frames to follow.
        total_chunks: u32,
        /// Current authoritative head node ID.
        head_node_id: Option<String>,
    },
    /// Back-transcript chunk received from host.
    SnapshotChunk {
        /// Zero-based chunk index.
        index: u32,
        /// Total chunk count.
        total: u32,
        /// Host nodes in this chunk.
        nodes: Vec<DagNode>,
    },
    /// Initial back-transcript fully applied to the guest's local replica session.
    SnapshotReady {
        /// Local replica session ID (`collab_<room>`).
        replica_session_id: String,
        /// Local replica session metadata (`branch = Some("collab-of=<room>")`).
        meta: SessionMeta,
        /// Loaded chronological branch nodes of the replica.
        nodes: Vec<DagNode>,
    },
    /// Authoritative live DAG node appended on host (and mirrored into local replica).
    Entry {
        /// Host DAG node.
        node: DagNode,
    },
    /// Live streaming agent event from host.
    Agent(AgentEvent),
    /// Host busy/guest-count state update.
    State {
        /// Whether the host is currently running a turn.
        busy: bool,
        /// Current host head node ID.
        head_node_id: Option<String>,
        /// Connected guest count.
        guests: usize,
    },
    /// Host closed the room cleanly.
    Goodbye {
        /// Reason sent by the host.
        reason: String,
    },
    /// Host rejected a guest action (e.g. view-only prompt attempt).
    Error {
        /// Error explanation.
        message: String,
    },
    /// Relay connection ended.
    Disconnected {
        /// Human-readable reason.
        reason: String,
    },
}

enum GuestCmd {
    Send(GuestFrame),
    Leave(Option<oneshot::Sender<()>>),
}

/// Optional local replica session store binding for a [`CollabGuest`].
#[derive(Clone)]
pub struct ReplicaConfig {
    /// Local session store where the `collab-of=<room>` replica is persisted.
    pub store: Arc<dyn SessionStore>,
    /// Working directory recorded on the replica session metadata.
    pub cwd: String,
}

/// Handle to a connected collaboration guest session.
#[derive(Clone)]
pub struct CollabGuest {
    room: String,
    replica_id: String,
    can_write: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<GuestCmd>,
}

impl CollabGuest {
    /// Connect to a live collaboration room described by `link` as a guest.
    pub async fn connect(
        link: CollabLink,
        default_relay: Option<&str>,
        guest_name: &str,
        replica: Option<ReplicaConfig>,
    ) -> Result<(Self, mpsc::Receiver<GuestEvent>)> {
        let ws_url = link.to_ws_url("guest", default_relay)?;
        let mut ws = connect_ws(&ws_url).await?;
        ws.set_max_message_bytes(MAX_COLLAB_FRAME_BYTES);
        let (reader, mut writer) = ws.into_split();

        let hello = GuestFrame::Hello {
            name: sanitize_guest_name(guest_name),
            write_token: link.write_token.as_ref().map(encode_key),
        };
        let hello_wire = encode_routed_frame(0, &hello, &link.key)?;
        writer.send_binary(&hello_wire).await?;

        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (event_tx, event_rx) = mpsc::channel(256);
        let can_write = Arc::new(AtomicBool::new(link.can_write()));
        let closed = Arc::new(AtomicBool::new(false));
        let replica_id = replica_session_id(&link.room);

        let ctx = GuestTaskCtx {
            room: link.room.clone(),
            key: link.key,
            link_has_write_token: link.can_write(),
            can_write: Arc::clone(&can_write),
            closed: Arc::clone(&closed),
            event_tx,
            replica: replica.map(|r| ReplicaSync::new(link.room.clone(), r.store, r.cwd)),
        };
        tokio::spawn(run_guest_loop(reader, writer, cmd_rx, ctx));

        Ok((
            Self {
                room: link.room,
                replica_id,
                can_write,
                closed,
                cmd_tx,
            },
            event_rx,
        ))
    }

    /// Room identifier.
    pub fn room(&self) -> &str {
        &self.room
    }

    /// Deterministic local replica session ID (`collab_<room>`).
    pub fn replica_session_id(&self) -> &str {
        &self.replica_id
    }

    /// Whether this guest link allows prompting and aborting.
    pub fn can_write(&self) -> bool {
        self.can_write.load(Ordering::Acquire)
    }

    /// Whether the guest connection has closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Submit a prompt to the host (rejected locally if this is a view-only link).
    pub fn send_prompt(&self, text: &str) -> Result<()> {
        if !self.can_write() {
            return Err(HrnError::Auth(
                "view-only collab link: prompting is disabled".to_owned(),
            ));
        }
        if self.is_closed() {
            return Err(HrnError::Tool("collab room is closed".to_owned()));
        }
        self.cmd_tx
            .try_send(GuestCmd::Send(GuestFrame::Prompt {
                text: text.to_owned(),
            }))
            .map_err(|_| HrnError::Tool("collab send queue full or disconnected".to_owned()))
    }

    /// Request aborting the host's active turn (rejected locally if view-only).
    pub fn send_abort(&self) -> Result<()> {
        if !self.can_write() {
            return Err(HrnError::Auth(
                "view-only collab link: abort is disabled".to_owned(),
            ));
        }
        if self.is_closed() {
            return Err(HrnError::Tool("collab room is closed".to_owned()));
        }
        self.cmd_tx
            .try_send(GuestCmd::Send(GuestFrame::Abort))
            .map_err(|_| HrnError::Tool("collab send queue full or disconnected".to_owned()))
    }

    /// Disconnect from the room cleanly without waiting.
    pub fn leave(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.cmd_tx.try_send(GuestCmd::Leave(None));
    }

    /// Disconnect from the room and wait until the socket has closed.
    pub async fn leave_and_wait(&self) {
        self.closed.store(true, Ordering::Release);
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(GuestCmd::Leave(Some(tx))).await.is_ok() {
            let _ = rx.await;
        }
    }
}

struct GuestTaskCtx {
    room: String,
    key: [u8; KEY_LEN],
    link_has_write_token: bool,
    can_write: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    event_tx: mpsc::Sender<GuestEvent>,
    replica: Option<ReplicaSync>,
}

async fn run_guest_loop(
    mut reader: WsReader,
    mut writer: WsWriter,
    mut cmd_rx: mpsc::Receiver<GuestCmd>,
    mut ctx: GuestTaskCtx,
) {
    let mut expected_chunks: Option<u32> = None;
    let mut received_chunks = 0u32;
    let mut snapshot_ready_emitted = false;
    let mut reason = "relay disconnected".to_owned();

    loop {
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    Some(GuestCmd::Send(frame)) => {
                        match encode_routed_frame(0, &frame, &ctx.key) {
                            Ok(wire) => {
                                if writer.send_binary(&wire).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                reason = err.to_string();
                                break;
                            }
                        }
                    }
                    Some(GuestCmd::Leave(ack)) => {
                        let _ = writer.send_close().await;
                        "left room".clone_into(&mut reason);
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
                        let Ok((_, Some(frame))) = decode_routed_frame::<HostFrame>(&raw, &ctx.key) else {
                            continue;
                        };
                        let stop = handle_host_frame(
                            frame,
                            &mut ctx,
                            &mut expected_chunks,
                            &mut received_chunks,
                            &mut snapshot_ready_emitted,
                            &mut reason,
                        ).await;
                        if stop {
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
                        reason = err.to_string();
                        break;
                    }
                }
            }
        }
    }

    ctx.closed.store(true, Ordering::Release);
    let _ = ctx.event_tx.try_send(GuestEvent::Disconnected { reason });
}

async fn handle_host_frame(
    frame: HostFrame,
    ctx: &mut GuestTaskCtx,
    expected_chunks: &mut Option<u32>,
    received_chunks: &mut u32,
    snapshot_ready_emitted: &mut bool,
    reason: &mut String,
) -> bool {
    match frame {
        HostFrame::Welcome {
            room,
            session_id,
            title,
            model,
            can_write,
            total_chunks,
            head_node_id,
        } => {
            let effective_write = ctx.link_has_write_token && can_write;
            ctx.can_write.store(effective_write, Ordering::Release);
            *expected_chunks = Some(total_chunks);
            if let Some(rep) = ctx.replica.as_mut() {
                let _ = rep.ensure_session(&title, model.as_deref());
            }
            let _ = ctx
                .event_tx
                .send(GuestEvent::Welcome {
                    room,
                    session_id,
                    title,
                    model,
                    can_write: effective_write,
                    total_chunks,
                    head_node_id,
                })
                .await;
            if total_chunks == 0 && !*snapshot_ready_emitted {
                *snapshot_ready_emitted = true;
                emit_snapshot_ready(ctx).await;
            }
            false
        }
        HostFrame::SnapshotChunk {
            index,
            total,
            nodes,
        } => {
            if let Some(rep) = ctx.replica.as_mut() {
                for node in &nodes {
                    let _ = rep.apply_node(node);
                }
            }
            *received_chunks = received_chunks.saturating_add(1);
            let _ = ctx
                .event_tx
                .send(GuestEvent::SnapshotChunk {
                    index,
                    total,
                    nodes,
                })
                .await;
            if expected_chunks.is_some_and(|exp| *received_chunks >= exp)
                && !*snapshot_ready_emitted
            {
                *snapshot_ready_emitted = true;
                emit_snapshot_ready(ctx).await;
            }
            false
        }
        HostFrame::Entry { node } => {
            if let Some(rep) = ctx.replica.as_mut() {
                let _ = rep.apply_node(&node);
            }
            let _ = ctx.event_tx.send(GuestEvent::Entry { node }).await;
            false
        }
        HostFrame::Event { event } => {
            let _ = ctx.event_tx.send(GuestEvent::Agent(event)).await;
            false
        }
        HostFrame::State {
            busy,
            head_node_id,
            guests,
        } => {
            let _ = ctx
                .event_tx
                .send(GuestEvent::State {
                    busy,
                    head_node_id,
                    guests,
                })
                .await;
            false
        }
        HostFrame::Goodbye { reason: why } => {
            reason.clone_from(&why);
            let _ = ctx.event_tx.send(GuestEvent::Goodbye { reason: why }).await;
            true
        }
        HostFrame::Error { message } => {
            let _ = ctx.event_tx.send(GuestEvent::Error { message }).await;
            false
        }
    }
}

async fn emit_snapshot_ready(ctx: &GuestTaskCtx) {
    if let Some(rep) = &ctx.replica
        && let Ok(Some((meta, nodes))) = rep.load_replica()
    {
        let _ = ctx
            .event_tx
            .send(GuestEvent::SnapshotReady {
                replica_session_id: rep.session_id.clone(),
                meta,
                nodes,
            })
            .await;
    } else {
        let now = SystemClock.now_ms();
        let meta = SessionMeta {
            id: replica_session_id(&ctx.room),
            title: format!("collab {}", ctx.room),
            cwd: ".".to_owned(),
            branch: Some(collab_branch_tag(&ctx.room)),
            created_at_ms: now,
            updated_at_ms: now,
            ..SessionMeta::default()
        };
        let _ = ctx
            .event_tx
            .send(GuestEvent::SnapshotReady {
                replica_session_id: meta.id.clone(),
                meta,
                nodes: Vec::new(),
            })
            .await;
    }
}

/// Manages idempotent persistence of host DAG entries into a local replica session tagged
/// `collab-of=<room>`.
struct ReplicaSync {
    room: String,
    session_id: String,
    store: Arc<dyn SessionStore>,
    cwd: String,
    known_ids: HashSet<String>,
    last_head: Option<String>,
}

impl ReplicaSync {
    fn new(room: String, store: Arc<dyn SessionStore>, cwd: String) -> Self {
        let session_id = replica_session_id(&room);
        Self {
            room,
            session_id,
            store,
            cwd,
            known_ids: HashSet::new(),
            last_head: None,
        }
    }

    fn ensure_session(&mut self, title: &str, model: Option<&str>) -> Result<()> {
        if let Some(mut existing) = self.store.get_session(&self.session_id)? {
            title.clone_into(&mut existing.title);
            existing.branch = Some(collab_branch_tag(&self.room));
            if model.is_some() {
                existing.model = model.map(str::to_owned);
            }
            existing.updated_at_ms = SystemClock.now_ms();
            self.store.update_session(&existing)?;
            self.last_head = existing.head_node_id;
            for node in self.store.load_tree(&self.session_id)? {
                self.known_ids.insert(node.id);
            }
            return Ok(());
        }
        let now = SystemClock.now_ms();
        let meta = SessionMeta {
            id: self.session_id.clone(),
            title: title.to_owned(),
            cwd: self.cwd.clone(),
            branch: Some(collab_branch_tag(&self.room)),
            head_node_id: None,
            model: model.map(str::to_owned),
            thinking: ThinkingLevel::Medium,
            goal: None,
            permission_mode: PermissionMode::Default,
            parent_session_id: None,
            parent_node_id: None,
            status: AgentStatus::Idle,
            pinned: false,
            title_source: None,
            exit_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.store.create_session(&meta)
    }

    fn apply_node(&mut self, host_node: &DagNode) -> Result<()> {
        let local_id = replica_node_id(&self.room, &host_node.id);
        if self.known_ids.contains(&local_id) {
            return Ok(());
        }
        let mapped_parent = host_node
            .parent_id
            .as_deref()
            .map(|p| replica_node_id(&self.room, p))
            .filter(|pid| self.known_ids.contains(pid))
            .or_else(|| self.last_head.clone());
        let local_node = DagNode {
            id: local_id.clone(),
            session_id: self.session_id.clone(),
            parent_id: mapped_parent,
            created_at_ms: host_node.created_at_ms,
            payload: host_node.payload.clone(),
        };
        self.store.append_node(&local_node)?;
        self.known_ids.insert(local_id.clone());
        self.last_head = Some(local_id);
        Ok(())
    }

    fn load_replica(&self) -> Result<Option<(SessionMeta, Vec<DagNode>)>> {
        let Some(meta) = self.store.get_session(&self.session_id)? else {
            return Ok(None);
        };
        let nodes = match &meta.head_node_id {
            Some(head) => self.store.load_branch(&self.session_id, head)?,
            None => Vec::new(),
        };
        Ok(Some((meta, nodes)))
    }
}
