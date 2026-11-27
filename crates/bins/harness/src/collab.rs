//! Live collaboration host broadcaster bridge and headless `hrn join <link>` CLI driver.

use crate::cli::JoinArgs;
use crate::config::Config;
use crate::term::{err_line, out};
use hrn_edge_http::seal::CollabLink;
use hrn_edge_ipc::collab::{
    CollabGuest, CollabHost, GuestEvent, ReplicaConfig, sanitize_guest_name,
};
use hrn_kernel::{
    AgentEvent, BoxFuture, DagNode, DagPayload, EventSink, Result, Role, SessionStore,
};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Active host room bridge tracking which DAG nodes have already been broadcast to guests.
#[derive(Clone)]
pub(crate) struct CollabHostBridge {
    pub(crate) host: CollabHost,
    pub(crate) session_id: String,
    seen_nodes: Arc<Mutex<HashSet<String>>>,
}

impl CollabHostBridge {
    pub(crate) fn new(host: CollabHost, session_id: String, initial_nodes: &[DagNode]) -> Self {
        let seen: HashSet<String> = initial_nodes.iter().map(|n| n.id.clone()).collect();
        Self {
            host,
            session_id,
            seen_nodes: Arc::new(Mutex::new(seen)),
        }
    }

    /// Broadcast any newly persisted DAG nodes on `self.session_id` since the last sync.
    pub(crate) fn sync_new_nodes(&self, store: &dyn SessionStore) {
        let Ok(Some(meta)) = store.get_session(&self.session_id) else {
            return;
        };
        let Some(head) = meta.head_node_id.as_deref() else {
            return;
        };
        let Ok(branch) = store.load_branch(&self.session_id, head) else {
            return;
        };
        let mut seen = self
            .seen_nodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for node in branch {
            if seen.insert(node.id.clone()) {
                self.host.broadcast_entry(node);
            }
        }
    }

    /// Broadcast a live agent event and flush any newly persisted DAG nodes.
    pub(crate) fn on_agent_event(&self, store: &dyn SessionStore, event: &AgentEvent) {
        self.host.broadcast_event(event.clone());
        match event {
            AgentEvent::TurnStart { .. } => {
                self.sync_new_nodes(store);
                self.host
                    .broadcast_state(true, current_head(store, &self.session_id));
            }
            AgentEvent::ToolDone { .. }
            | AgentEvent::Compacted { .. }
            | AgentEvent::TodosUpdated { .. } => {
                self.sync_new_nodes(store);
            }
            AgentEvent::TurnDone { .. } | AgentEvent::TurnError { .. } => {
                self.sync_new_nodes(store);
                self.host
                    .broadcast_state(false, current_head(store, &self.session_id));
            }
            _ => {}
        }
    }
}

fn current_head(store: &dyn SessionStore, session_id: &str) -> Option<String> {
    store
        .get_session(session_id)
        .ok()
        .flatten()
        .and_then(|m| m.head_node_id)
}

/// Tees live runtime [`AgentEvent`]s to both the local UI sink and an active [`CollabHostBridge`].
pub(crate) struct CollabEventSink {
    pub(crate) inner: Arc<dyn EventSink>,
    pub(crate) bridge: CollabHostBridge,
    pub(crate) store: Arc<dyn SessionStore>,
}

impl EventSink for CollabEventSink {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        self.bridge.on_agent_event(self.store.as_ref(), &event);
        self.inner.emit(event)
    }
}

/// Resolve a sanitized default guest display name from the local environment.
pub(crate) fn default_guest_name() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "guest".to_owned());
    sanitize_guest_name(&raw)
}

/// CLI entry point for `hrn join <link> [--name <name>]`.
pub(crate) async fn run_join_cli(
    cfg: &Config,
    store: Arc<dyn SessionStore>,
    cwd: &Path,
    args: &JoinArgs,
) -> Result<()> {
    let link = CollabLink::parse(&args.link)?;
    let default_relay = (!cfg.collab.relay.is_empty()).then_some(cfg.collab.relay.as_str());
    let guest_name = args
        .name
        .as_deref()
        .map_or_else(default_guest_name, sanitize_guest_name);
    let replica = ReplicaConfig {
        store,
        cwd: cwd.to_string_lossy().into_owned(),
    };
    let (guest, mut events) =
        CollabGuest::connect(link, default_relay, &guest_name, Some(replica)).await?;
    let mode_label = if guest.can_write() {
        "interactive (type a prompt, `/abort`, or `/leave`)"
    } else {
        "view-only (prompting disabled; type `/leave` to exit)"
    };
    out(&format!(
        "joined collab room `{}` as `{guest_name}` · {mode_label}\n",
        guest.room()
    ));

    let event_task = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if render_guest_event(&ev) {
                break;
            }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if matches!(trimmed, "/leave" | "/quit" | "/exit") {
            break;
        }
        if trimmed == "/abort" {
            if let Err(err) = guest.send_abort() {
                err_line(&format!("error: {err}"));
            }
            continue;
        }
        if !guest.can_write() {
            err_line("view-only collab link: prompting is disabled");
            continue;
        }
        if let Err(err) = guest.send_prompt(trimmed) {
            err_line(&format!("error: {err}"));
        }
    }

    guest.leave_and_wait().await;
    event_task.abort();
    Ok(())
}

/// Render one [`GuestEvent`] to stdout/stderr; returns `true` when the room ended.
fn render_guest_event(ev: &GuestEvent) -> bool {
    match ev {
        GuestEvent::SnapshotReady {
            replica_session_id,
            meta,
            nodes,
        } => {
            out(&format!(
                "[collab] synced {} node(s) into local replica `{replica_session_id}` (title: {})\n",
                nodes.len(),
                meta.title
            ));
            for node in nodes {
                print_node_line(node);
            }
            false
        }
        GuestEvent::Agent(agent_ev) => {
            render_agent_event(agent_ev);
            false
        }
        GuestEvent::Error { message } => {
            err_line(&format!("[collab error] {message}"));
            false
        }
        GuestEvent::Goodbye { reason } => {
            out(&format!("\n[collab] host closed room: {reason}\n"));
            true
        }
        GuestEvent::Disconnected { reason } => {
            err_line(&format!("\n[collab] disconnected: {reason}"));
            true
        }
        GuestEvent::Welcome { .. }
        | GuestEvent::SnapshotChunk { .. }
        | GuestEvent::Entry { .. }
        | GuestEvent::State { .. } => false,
    }
}

fn print_node_line(node: &DagNode) {
    if let DagPayload::Message { msg, .. } = &node.payload {
        let label = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        };
        if !msg.content.trim().is_empty() {
            out(&format!("[{label}] {}\n", msg.content));
        }
    }
}

fn render_agent_event(ev: &AgentEvent) {
    match ev {
        AgentEvent::TurnStart { user_text, .. } => out(&format!("\n[user] {user_text}\n")),
        AgentEvent::TextDelta { delta } => out(delta),
        AgentEvent::ToolStart {
            name, arguments, ..
        } => {
            let preview: String = arguments.chars().take(120).collect();
            out(&format!("\n[tool:{name}] {preview}\n"));
        }
        AgentEvent::ToolDone { name, is_error, .. } => {
            let tag = if *is_error { "ERR" } else { "ok" };
            out(&format!("[tool:{name} {tag}]\n"));
        }
        AgentEvent::TurnDone { steps, .. } => out(&format!("\n[turn done in {steps} step(s)]\n")),
        AgentEvent::TurnError { code, message, .. } => {
            err_line(&format!("[turn error {code}] {message}"));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use hrn_edge_http::connect_ws;
    use hrn_edge_http::seal::{CollabLink, encode_key};
    use hrn_edge_ipc::collab::{
        CollabHost, GuestFrame, HostInbound, HostSnapshot, MAX_GUEST_FRAME_UNSEALED_BYTES,
        encode_routed_frame,
    };
    use hrn_relay::{RelayConfig, serve};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::net::TcpListener;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_blocks_duplicate_hello_gzip_bomb_and_forged_sentinel() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let relay = serve(listener, RelayConfig::default())
            .await
            .expect("serve");

        let snap_calls = Arc::new(AtomicUsize::new(0));
        let snap_counter = Arc::clone(&snap_calls);
        let (host, mut host_in) = CollabHost::start(
            &relay.base_url(),
            Arc::new(move || {
                snap_counter.fetch_add(1, Ordering::Relaxed);
                HostSnapshot {
                    session_id: "s_dos".to_owned(),
                    title: "DoS Guard".to_owned(),
                    ..HostSnapshot::default()
                }
            }),
        )
        .await
        .expect("start host");

        // Debug on CollabHost must redact key and write_token.
        let dbg = format!("{host:?}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        assert!(!dbg.contains(&encode_key(host.key())), "{dbg}");
        assert!(!dbg.contains(&encode_key(host.write_token())), "{dbg}");

        let link = CollabLink::parse(host.interactive_link()).expect("link");
        let ws_url = link.to_ws_url("guest", None).expect("ws url");
        let mut raw_ws = connect_ws(&ws_url).await.expect("raw ws");

        let hello = GuestFrame::Hello {
            name: "flooder".to_owned(),
            write_token: Some(encode_key(host.write_token())),
        };
        let hello_wire = encode_routed_frame(0, &hello, &link.key).expect("hello");
        // Send Hello 5 times -> host must only accept the first Hello and run snapshot_fn ONCE!
        for _ in 0..5 {
            raw_ws.send_binary(&hello_wire).await.expect("send hello");
        }

        // Send a compressed gzip-bomb guest frame (>128 KiB unsealed) -> host ignores cleanly.
        let bomb = GuestFrame::Prompt {
            text: "Z".repeat(MAX_GUEST_FRAME_UNSEALED_BYTES + 8192),
        };
        let bomb_wire = encode_routed_frame(0, &bomb, &link.key).expect("bomb");
        raw_ws.send_binary(&bomb_wire).await.expect("send bomb");

        // Attempt to forge a 4-byte disconnect sentinel -> relay drops it; guest remains registered.
        raw_ws
            .send_binary(&1u32.to_le_bytes())
            .await
            .expect("forged sentinel");

        // Send a normal prompt afterward -> proves host is still healthy and guest was not dropped.
        let ok_wire = encode_routed_frame(
            0,
            &GuestFrame::Prompt {
                text: "still alive".to_owned(),
            },
            &link.key,
        )
        .expect("ok prompt");
        raw_ws.send_binary(&ok_wire).await.expect("send ok");

        let mut joined = 0usize;
        let mut left = 0usize;
        let mut prompts = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && prompts.is_empty() {
            if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(2), host_in.recv()).await
            {
                match ev {
                    HostInbound::GuestJoined { .. } => joined += 1,
                    HostInbound::GuestLeft { .. } => left += 1,
                    HostInbound::Prompt { raw_text, .. } => prompts.push(raw_text),
                    _ => {}
                }
            }
        }
        assert_eq!(joined, 1, "duplicate Hello must not trigger extra joins");
        assert_eq!(
            snap_calls.load(Ordering::Relaxed),
            1,
            "duplicate Hello must not reload snapshot"
        );
        assert_eq!(left, 0, "forged 4-byte sentinel must not trigger GuestLeft");
        assert_eq!(prompts, vec!["still alive".to_owned()]);

        let _ = raw_ws.send_close().await;
        host.close_and_wait("done").await;
        relay.shutdown().await;
    }

    async fn assert_relay_serves_pwa(addr: std::net::SocketAddr, room: &str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let mut http = TcpStream::connect(addr).await.expect("http");
        http.write_all(
            format!("GET /r/{room} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write");
        let mut pwa_raw = Vec::new();
        http.read_to_end(&mut pwa_raw).await.expect("read");
        let pwa = String::from_utf8_lossy(&pwa_raw);
        assert!(
            pwa.starts_with("HTTP/1.1 200") && pwa.contains("hrn remote"),
            "{pwa}"
        );
        assert!(pwa.contains("content-security-policy:"), "{pwa}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_mobile_supervision_routes_control_and_blocks_view_only() {
        use hrn_edge_ipc::collab::{HostFrame, decode_routed_frame};

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let relay = serve(listener, RelayConfig::default())
            .await
            .expect("serve");
        let (host, mut host_in) = CollabHost::start(
            &relay.base_url(),
            Arc::new(|| HostSnapshot {
                session_id: "s_mob".to_owned(),
                title: "Mobile Relay".to_owned(),
                ..HostSnapshot::default()
            }),
        )
        .await
        .expect("start host");

        // Relay serves mobile PWA on non-WS GET /r/<room> with strict CSP.
        assert_relay_serves_pwa(relay.addr(), host.room()).await;

        let mut ctrl_ws = send_guest_frames(
            host.interactive_link(),
            &[
                GuestFrame::Hello {
                    name: "phone-ctrl".to_owned(),
                    write_token: Some(encode_key(host.write_token())),
                },
                GuestFrame::Steer {
                    text: "focus on tests".to_owned(),
                },
                GuestFrame::Approve {
                    call_id: "call-42".to_owned(),
                    approved: true,
                    always: true,
                },
                GuestFrame::AskAnswer {
                    id: "ask-7".to_owned(),
                    answers: vec!["prod".to_owned()],
                },
            ],
        )
        .await;
        let view_link = CollabLink::parse(host.view_only_link()).expect("view link");
        let mut view_ws = send_guest_frames(
            host.view_only_link(),
            &[
                GuestFrame::Hello {
                    name: "phone-view".to_owned(),
                    write_token: None,
                },
                GuestFrame::Steer {
                    text: "blocked steer".to_owned(),
                },
                GuestFrame::Approve {
                    call_id: "call-99".to_owned(),
                    approved: true,
                    always: false,
                },
                GuestFrame::AskAnswer {
                    id: "ask-99".to_owned(),
                    answers: vec!["nope".to_owned()],
                },
            ],
        )
        .await;

        let (got_steer, got_approve, got_ask, unexpected) =
            collect_mobile_inbound(&mut host_in).await;
        assert_eq!(got_steer.as_deref(), Some("focus on tests"));
        assert_eq!(got_approve, Some(("call-42".to_owned(), true, true)));
        assert_eq!(got_ask, Some(("ask-7".to_owned(), vec!["prod".to_owned()])));
        assert!(unexpected.is_empty(), "view-only leaked: {unexpected:?}");

        let mut view_errors = 0usize;
        let err_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < err_deadline && view_errors < 3 {
            if let Ok(Ok(Some(bytes))) =
                tokio::time::timeout(Duration::from_millis(400), view_ws.recv_binary()).await
                && let Ok((_, Some(HostFrame::Error { .. }))) =
                    decode_routed_frame::<HostFrame>(&bytes, &view_link.key)
            {
                view_errors += 1;
            }
        }
        assert_eq!(
            view_errors, 3,
            "view-only guest must receive 3 error frames"
        );

        let _ = ctrl_ws.send_close().await;
        let _ = view_ws.send_close().await;
        host.close_and_wait("done").await;
        relay.shutdown().await;
    }

    async fn send_guest_frames(link_str: &str, frames: &[GuestFrame]) -> hrn_edge_http::WsStream {
        let link = CollabLink::parse(link_str).expect("link");
        let mut ws = connect_ws(&link.to_ws_url("guest", None).expect("ws"))
            .await
            .expect("ws");
        for frame in frames {
            let wire = encode_routed_frame(0, frame, &link.key).expect("encode");
            ws.send_binary(&wire).await.expect("send");
        }
        ws
    }

    type MobileInboundTuple = (
        Option<String>,
        Option<(String, bool, bool)>,
        Option<(String, Vec<String>)>,
        Vec<String>,
    );

    async fn collect_mobile_inbound(
        host_in: &mut tokio::sync::mpsc::Receiver<HostInbound>,
    ) -> MobileInboundTuple {
        let (mut steer, mut approve, mut ask, mut unexpected) = (None, None, None, Vec::new());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline
            && (steer.is_none() || approve.is_none() || ask.is_none())
        {
            if let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_millis(500), host_in.recv()).await
            {
                match ev {
                    HostInbound::Steer {
                        guest_name, text, ..
                    } if guest_name == "phone-ctrl" => {
                        steer = Some(text);
                    }
                    HostInbound::Approve {
                        guest_name,
                        call_id,
                        approved,
                        always,
                        ..
                    } if guest_name == "phone-ctrl" => {
                        approve = Some((call_id, approved, always));
                    }
                    HostInbound::AskAnswer {
                        guest_name,
                        id,
                        answers,
                        ..
                    } if guest_name == "phone-ctrl" => {
                        ask = Some((id, answers));
                    }
                    HostInbound::Steer { guest_name, .. }
                    | HostInbound::Approve { guest_name, .. }
                    | HostInbound::AskAnswer { guest_name, .. } => {
                        unexpected.push(guest_name);
                    }
                    _ => {}
                }
            }
        }
        (steer, approve, ask, unexpected)
    }
}
