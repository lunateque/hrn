//! Acceptance integration tests for Wave 11h `ShareCollab`:
//! - `share_blob_is_ciphertext`
//! - `join_two_process_back_transcript_then_live`
//! - `join_viewonly_cannot_prompt`
//! - `host_resume_closes_room`
//! - `no_plaintext_on_wire`

use crate::chat::ChatBackend;
use crate::cli::ChatArgs;
use crate::config::{Config, ShareConfig, ShareStoreKind};
use crate::share::{ShareSnapshot, share_session};
use hrn_edge_http::seal::{CollabLink, parse_share_url, unseal};
use hrn_edge_http::{HttpClient, connect_ws};
use hrn_edge_ipc::collab::{
    CollabGuest, CollabHost, GuestEvent, GuestFrame, HostInbound, HostSnapshot, ReplicaConfig,
    collab_branch_tag, encode_routed_frame,
};
use hrn_edge_store::SqliteStore;
use hrn_kernel::{
    AgentEvent, AgentStatus, DagNode, DagPayload, Msg, PermissionMode, SessionMeta, SessionStore,
    ThinkingLevel,
};
use hrn_relay::{RelayConfig, RelayHandle, serve};
use hrn_tui::Backend as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

async fn start_relay(cfg: RelayConfig) -> RelayHandle {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
    serve(listener, cfg).await.expect("serve relay")
}

fn msg_node(id: &str, session_id: &str, parent_id: Option<&str>, ts: i64, msg: Msg) -> DagNode {
    DagNode {
        id: id.to_owned(),
        session_id: session_id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        created_at_ms: ts,
        payload: DagPayload::Message {
            turn_id: format!("t_{ts}"),
            msg,
        },
    }
}

fn seed_session(store: &dyn SessionStore, id: &str, title: &str, msgs: &[(&str, Msg)]) {
    let meta = SessionMeta {
        id: id.to_owned(),
        title: title.to_owned(),
        cwd: "/repo".to_owned(),
        branch: None,
        head_node_id: None,
        model: Some("flash".to_owned()),
        thinking: ThinkingLevel::Medium,
        goal: None,
        permission_mode: PermissionMode::Default,
        parent_session_id: None,
        parent_node_id: None,
        status: AgentStatus::Idle,
        pinned: false,
        title_source: None,
        exit_ms: None,
        created_at_ms: 100,
        updated_at_ms: 100,
    };
    store.create_session(&meta).expect("create session");
    let mut parent: Option<&str> = None;
    for (idx, (node_id, msg)) in msgs.iter().enumerate() {
        let ts = 100 + i64::try_from(idx).unwrap_or(0);
        store
            .append_node(&msg_node(node_id, id, parent, ts, msg.clone()))
            .expect("append");
        parent = Some(*node_id);
    }
}

fn snapshot_from_store(store: &Arc<dyn SessionStore>, session_id: &str) -> HostSnapshot {
    let meta = store.get_session(session_id).expect("get").expect("exists");
    let nodes = meta
        .head_node_id
        .as_deref()
        .map(|h| store.load_branch(session_id, h).expect("branch"))
        .unwrap_or_default();
    HostSnapshot {
        session_id: meta.id,
        title: meta.title,
        model: meta.model,
        head_node_id: meta.head_node_id,
        busy: false,
        nodes,
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

async fn recv_guest<T>(
    rx: &mut mpsc::Receiver<GuestEvent>,
    mut pick: impl FnMut(GuestEvent) -> Option<T>,
) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Some(ev) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("guest recv timeout")
            && let Some(val) = pick(ev)
        {
            return val;
        }
    }
    panic!("expected guest event before deadline");
}

async fn recv_host_prompt(rx: &mut mpsc::Receiver<HostInbound>) -> String {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("host timeout")
            .expect("host event")
        {
            HostInbound::Prompt { badged_text, .. } => return badged_text,
            HostInbound::GuestJoined { can_write, .. } => assert!(can_write),
            _ => {}
        }
    }
}

async fn open_test_backend(relay_url: &str) -> (ChatBackend, hrn_edge_mcp::McpManager) {
    let cfg_toml = format!(
        r#"
[[providers]]
name = "local"
kind = "openai_chat"
base_url = "http://127.0.0.1:1/v1"

[models.m]
provider = "local"
id = "stub"
context_window = 16000

[roles]
default = "m"

[collab]
relay = "{relay_url}"
"#
    );
    let cfg = Config::parse(&cfg_toml).expect("parse cfg");
    let (tx, _rx) = hrn_tui::channel();
    let handle = tokio::runtime::Handle::current();
    let args = ChatArgs {
        no_session: true,
        ..ChatArgs::default()
    };
    let (backend, mcp, _warnings) = ChatBackend::open(
        &cfg,
        PathBuf::from("test-config.toml"),
        std::path::Path::new(":memory:"),
        std::env::temp_dir(),
        &args,
        tx,
        handle,
        None,
    )
    .await
    .expect("open backend");
    (backend, mcp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn share_blob_is_ciphertext() {
    let relay = start_relay(RelayConfig::default()).await;
    let store = SqliteStore::open_in_memory().expect("store");
    let secret = "sk-ant-api03-SECRETTOKEN1234567890abcdefghijklmnop";
    seed_session(
        &store,
        "sess_share_1",
        "Sensitive Debug Title",
        &[
            (
                "n1",
                Msg::user(format!("debug key {secret} and UNIQUE_MARKER_ALPHA")),
            ),
            ("n2", Msg::assistant("UNIQUE_ANSWER_BETA".to_owned())),
        ],
    );

    let cfg = ShareConfig {
        store: ShareStoreKind::Blob,
        server: relay.base_url(),
        redact: true,
    };
    let (url, _report) = share_session(
        &store,
        &cfg,
        "sess_share_1",
        Some("SYSTEM_PROMPT_GAMMA".to_owned()),
        false,
        std::path::Path::new("."),
    )
    .await
    .expect("share");

    let (base, id, key) = parse_share_url(&url).expect("parse share url");
    assert_eq!(base, relay.base_url());
    assert!(url.contains(&format!("/s/{id}#")));

    let http = HttpClient::new().expect("http");
    let raw_resp = http
        .get(
            &format!("{base}/s/{id}/raw"),
            &[],
            Duration::from_secs(5),
            2 * 1024 * 1024,
        )
        .await
        .expect("get raw");
    assert_eq!(raw_resp.status.as_u16(), 200);
    let raw = raw_resp.body.as_ref();
    for forbidden in [
        b"UNIQUE_MARKER_ALPHA".as_slice(),
        b"UNIQUE_ANSWER_BETA".as_slice(),
        b"SYSTEM_PROMPT_GAMMA".as_slice(),
        b"Sensitive Debug Title".as_slice(),
        b"sk-ant-api03".as_slice(),
    ] {
        assert!(!contains_subslice(raw, forbidden), "ciphertext leak");
    }

    let plain = unseal(raw, &key).expect("unseal");
    let snap: ShareSnapshot = serde_json::from_slice(&plain).expect("json");
    assert_eq!(snap.version, 1);
    assert!(snap.meta.redacted);
    assert_eq!(snap.nodes.len(), 2);
    let snap_str = String::from_utf8(plain).expect("utf8");
    assert!(snap_str.contains("UNIQUE_MARKER_ALPHA"));
    assert!(snap_str.contains("UNIQUE_ANSWER_BETA"));
    assert!(snap_str.contains("[REDACTED:ANTHROPIC_KEY]"));
    assert!(!snap_str.contains("SECRETTOKEN1234567890"));
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_two_process_back_transcript_then_live() {
    let relay = start_relay(RelayConfig::default()).await;
    let host_store: Arc<dyn SessionStore> =
        Arc::new(SqliteStore::open_in_memory().expect("host store"));
    let guest_store: Arc<dyn SessionStore> =
        Arc::new(SqliteStore::open_in_memory().expect("guest store"));
    seed_session(
        host_store.as_ref(),
        "host_s1",
        "Pair Debugging",
        &[
            ("h_n1", Msg::user("historical question")),
            ("h_n2", Msg::assistant("historical answer")),
        ],
    );

    let snap_store = Arc::clone(&host_store);
    let (host, mut host_in) = CollabHost::start(
        &relay.base_url(),
        Arc::new(move || snapshot_from_store(&snap_store, "host_s1")),
    )
    .await
    .expect("start host");

    let link = CollabLink::parse(host.interactive_link()).expect("parse link");
    let (guest, mut guest_ev) = CollabGuest::connect(
        link,
        None,
        "alice",
        Some(ReplicaConfig {
            store: Arc::clone(&guest_store),
            cwd: "/guest".to_owned(),
        }),
    )
    .await
    .expect("connect guest");

    let (replica_id, meta, ready_nodes) = recv_guest(&mut guest_ev, |ev| match ev {
        GuestEvent::SnapshotReady {
            replica_session_id,
            meta,
            nodes,
        } => Some((replica_session_id, meta, nodes)),
        _ => None,
    })
    .await;
    assert_eq!(
        meta.branch.as_deref(),
        Some(collab_branch_tag(host.room()).as_str())
    );
    assert_eq!(replica_id, guest.replica_session_id());
    assert_eq!(ready_nodes.len(), 2);

    guest.send_prompt("check the parser").expect("send prompt");
    let badged = recv_host_prompt(&mut host_in).await;
    assert_eq!(badged, "[guest:alice] check the parser");

    let live_user = msg_node("h_n3", "host_s1", Some("h_n2"), 200, Msg::user(&badged));
    let live_asst = msg_node(
        "h_n4",
        "host_s1",
        Some("h_n3"),
        201,
        Msg::assistant("parser fixed"),
    );
    host_store.append_node(&live_user).expect("append u");
    host_store.append_node(&live_asst).expect("append a");
    host.broadcast_entry(live_user);
    host.broadcast_event(AgentEvent::TextDelta {
        delta: "parser fixed".to_owned(),
    });
    host.broadcast_entry(live_asst);

    let mut saw_delta = false;
    let mut live_entries = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && (!saw_delta || live_entries < 2) {
        match tokio::time::timeout(Duration::from_secs(3), guest_ev.recv())
            .await
            .expect("live timeout")
            .expect("live event")
        {
            GuestEvent::Agent(AgentEvent::TextDelta { delta }) if delta == "parser fixed" => {
                saw_delta = true;
            }
            GuestEvent::Entry { .. } => live_entries += 1,
            _ => {}
        }
    }
    assert!(saw_delta && live_entries == 2);

    let rep_meta = guest_store
        .get_session(guest.replica_session_id())
        .expect("get replica")
        .expect("replica exists");
    let rep_head = rep_meta.head_node_id.as_deref().expect("replica head");
    assert_eq!(
        guest_store
            .load_branch(&rep_meta.id, rep_head)
            .expect("replica branch")
            .len(),
        4
    );

    guest.leave_and_wait().await;
    host.close_and_wait("done").await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_viewonly_cannot_prompt() {
    let relay = start_relay(RelayConfig::default()).await;
    let host_store: Arc<dyn SessionStore> =
        Arc::new(SqliteStore::open_in_memory().expect("host store"));
    seed_session(
        host_store.as_ref(),
        "s_ro",
        "ReadOnly",
        &[("n1", Msg::user("hi"))],
    );

    let snap_store = Arc::clone(&host_store);
    let (host, mut host_in) = CollabHost::start(
        &relay.base_url(),
        Arc::new(move || snapshot_from_store(&snap_store, "s_ro")),
    )
    .await
    .expect("start host");

    let ro_link = CollabLink::parse(host.view_only_link()).expect("parse ro link");
    assert!(!ro_link.can_write());
    let (guest, mut guest_ev) = CollabGuest::connect(ro_link.clone(), None, "viewer", None)
        .await
        .expect("connect viewer");
    let can_write = recv_guest(&mut guest_ev, |ev| match ev {
        GuestEvent::Welcome { can_write, .. } => Some(can_write),
        _ => None,
    })
    .await;
    assert!(!can_write);
    assert!(guest.send_prompt("blocked locally").is_err());
    assert!(guest.send_abort().is_err());

    // Host-side check: forged WS guest with wrong token cannot inject prompts or abort.
    let ws_url = ro_link.to_ws_url("guest", None).expect("ws url");
    let mut raw_ws = connect_ws(&ws_url).await.expect("raw ws");
    for frame in [
        GuestFrame::Hello {
            name: "attacker".to_owned(),
            write_token: Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned()),
        },
        GuestFrame::Prompt {
            text: "forged prompt".to_owned(),
        },
        GuestFrame::Abort,
    ] {
        let wire = encode_routed_frame(0, &frame, &ro_link.key).expect("encode");
        raw_ws.send_binary(&wire).await.expect("send");
    }

    let mut saw_error_reply = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            Some(ev) = host_in.recv() => {
                assert!(
                    !matches!(ev, HostInbound::Prompt { .. } | HostInbound::Abort { .. }),
                    "host accepted forged write frame: {ev:?}"
                );
            }
            Ok(Some(raw)) = raw_ws.recv_binary() => {
                if let Ok((_, Some(hrn_edge_ipc::collab::HostFrame::Error { message }))) =
                    hrn_edge_ipc::collab::decode_routed_frame(&raw, &ro_link.key)
                    && message.contains("view-only")
                {
                    saw_error_reply = true;
                }
            }
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
    assert!(saw_error_reply, "host must reject view-only prompt");

    let _ = raw_ws.send_close().await;
    guest.leave_and_wait().await;
    host.close_and_wait("done").await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_resume_closes_room() {
    let relay = start_relay(RelayConfig::default()).await;
    let (backend, mcp) = open_test_backend(&relay.base_url()).await;
    seed_session(
        backend.store.as_ref(),
        "s_host_1",
        "Room 1",
        &[("n1", Msg::user("one"))],
    );
    seed_session(
        backend.store.as_ref(),
        "s_host_2",
        "Room 2",
        &[("n2", Msg::user("two"))],
    );

    let (mut backend, link_str) = tokio::task::spawn_blocking(move || {
        let mut backend = backend;
        let _ = backend.resume("s_host_1").expect("resume s1");
        let report = backend.collab_command(None).expect("start collab");
        let link = report.split_whitespace().last().expect("link").to_owned();
        (backend, link)
    })
    .await
    .expect("blocking host start");

    let link = CollabLink::parse(&link_str).expect("parse collab link");
    let (guest, mut guest_ev) = CollabGuest::connect(link, None, "bob", None)
        .await
        .expect("guest connect");
    recv_guest(&mut guest_ev, |ev| {
        matches!(ev, GuestEvent::Welcome { .. }).then_some(())
    })
    .await;

    let backend = tokio::task::spawn_blocking(move || {
        let _ = backend.resume("s_host_2").expect("resume s2");
        backend
    })
    .await
    .expect("blocking resume");

    recv_guest(&mut guest_ev, |ev| {
        matches!(ev, GuestEvent::Goodbye { .. }).then_some(())
    })
    .await;

    drop(backend);
    guest.leave_and_wait().await;
    mcp.shutdown().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_plaintext_on_wire() {
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let cap_clone = Arc::clone(&captured);
    let relay = start_relay(RelayConfig {
        on_frame: Some(Arc::new(move |_room, frame| {
            cap_clone.lock().expect("cap lock").push(frame.to_vec());
        })),
        ..RelayConfig::default()
    })
    .await;

    let host_store: Arc<dyn SessionStore> =
        Arc::new(SqliteStore::open_in_memory().expect("host store"));
    seed_session(
        host_store.as_ref(),
        "s_wire",
        "TOP_SECRET_TITLE_XYZ",
        &[("w1", Msg::user("TOP_SECRET_BACK_TRANSCRIPT"))],
    );

    let snap_store = Arc::clone(&host_store);
    let (host, mut host_in) = CollabHost::start(
        &relay.base_url(),
        Arc::new(move || snapshot_from_store(&snap_store, "s_wire")),
    )
    .await
    .expect("host");

    let link = CollabLink::parse(host.interactive_link()).expect("link");
    let ws_url = link.to_ws_url("guest", None).expect("ws url");
    assert!(!ws_url.contains('#'));
    assert!(!ws_url.contains(&hrn_edge_http::seal::encode_key(host.key())));
    assert!(!ws_url.contains(&hrn_edge_http::seal::encode_key(host.write_token())));

    let (guest, mut guest_ev) = CollabGuest::connect(link, None, "SECRET_GUEST_NAME", None)
        .await
        .expect("guest");
    recv_guest(&mut guest_ev, |ev| {
        matches!(ev, GuestEvent::SnapshotReady { .. }).then_some(())
    })
    .await;

    guest
        .send_prompt("TOP_SECRET_GUEST_PROMPT")
        .expect("send prompt");
    let _ = recv_host_prompt(&mut host_in).await;

    host.broadcast_event(AgentEvent::TextDelta {
        delta: "TOP_SECRET_LIVE_DELTA".to_owned(),
    });
    host.broadcast_entry(msg_node(
        "w2",
        "s_wire",
        Some("w1"),
        200,
        Msg::assistant("TOP_SECRET_LIVE_ENTRY"),
    ));
    host.close_and_wait("TOP_SECRET_GOODBYE").await;
    recv_guest(&mut guest_ev, |ev| {
        matches!(
            ev,
            GuestEvent::Goodbye { .. } | GuestEvent::Disconnected { .. }
        )
        .then_some(())
    })
    .await;

    let frames = captured.lock().expect("frames").clone();
    assert!(frames.len() >= 6, "captured only {} frames", frames.len());
    let forbidden: &[&[u8]] = &[
        b"TOP_SECRET_TITLE_XYZ",
        b"TOP_SECRET_BACK_TRANSCRIPT",
        b"SECRET_GUEST_NAME",
        b"TOP_SECRET_GUEST_PROMPT",
        b"TOP_SECRET_LIVE_DELTA",
        b"TOP_SECRET_LIVE_ENTRY",
        b"TOP_SECRET_GOODBYE",
        b"welcome",
        b"snapshot-chunk",
        b"write_token",
        b"goodbye",
        b"s_wire",
    ];
    for (idx, frame) in frames.iter().enumerate() {
        for needle in forbidden {
            assert!(
                !contains_subslice(frame, needle),
                "frame #{idx} leaked `{}`",
                String::from_utf8_lossy(needle)
            );
        }
    }

    guest.leave_and_wait().await;
    relay.shutdown().await;
}
