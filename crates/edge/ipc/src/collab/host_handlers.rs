//! Inbound frame handlers for [`super::host::CollabHost`].

use super::host::{GuestPeer, HostInbound, HostTaskCtx};
use super::protocol::{
    GuestFrame, HostFrame, MAX_GUEST_FRAME_UNSEALED_BYTES, MAX_GUEST_PROMPT_BYTES, chunk_nodes,
    decode_routed_frame_with_limit, encode_routed_frame, format_badged_prompt, sanitize_guest_name,
};
use hrn_edge_http::WsWriter;
use hrn_edge_http::seal::{KEY_LEN, ct_eq, decode_key};
use hrn_kernel::Result;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

/// Local maximum concurrent guests enforced on the host independent of the relay.
const MAX_HOST_GUESTS: usize = 32;

pub(super) async fn handle_inbound_binary(
    raw: &[u8],
    guests: &mut HashMap<u32, GuestPeer>,
    writer: &mut WsWriter,
    ctx: &HostTaskCtx,
) -> Result<()> {
    let Ok((peer_id, maybe_frame)) =
        decode_routed_frame_with_limit::<GuestFrame>(raw, &ctx.key, MAX_GUEST_FRAME_UNSEALED_BYTES)
    else {
        // Ignore frames that fail AES-GCM auth or exceed the guest unsealed cap.
        return Ok(());
    };
    if peer_id == 0 {
        return Ok(());
    }
    let Some(frame) = maybe_frame else {
        handle_disconnect(peer_id, guests, writer, ctx).await;
        return Ok(());
    };

    match frame {
        GuestFrame::Hello { name, write_token } => {
            handle_hello(peer_id, &name, write_token.as_deref(), guests, writer, ctx).await
        }
        GuestFrame::Prompt { text } => handle_prompt(peer_id, text, guests, writer, ctx).await,
        GuestFrame::Steer { text } => handle_steer(peer_id, text, guests, writer, ctx).await,
        GuestFrame::Abort => handle_abort(peer_id, guests, writer, ctx).await,
        GuestFrame::Approve {
            call_id,
            approved,
            always,
        } => handle_approve(peer_id, call_id, approved, always, guests, writer, ctx).await,
        GuestFrame::AskAnswer { id, answers } => {
            handle_ask_answer(peer_id, id, answers, guests, writer, ctx).await
        }
    }
}

async fn handle_disconnect(
    peer_id: u32,
    guests: &mut HashMap<u32, GuestPeer>,
    writer: &mut WsWriter,
    ctx: &HostTaskCtx,
) {
    if let Some(left) = guests.remove(&peer_id) {
        ctx.guest_count.store(guests.len(), Ordering::Relaxed);
        let _ = ctx.inbound_tx.try_send(HostInbound::GuestLeft {
            peer_id,
            name: left.name,
        });
        let snap = (ctx.snapshot_fn)();
        let _ = send_host_frame(
            writer,
            0,
            &HostFrame::State {
                busy: snap.busy,
                head_node_id: snap.head_node_id,
                guests: guests.len(),
            },
            &ctx.key,
        )
        .await;
    }
}

async fn handle_hello(
    peer_id: u32,
    raw_name: &str,
    provided_token: Option<&str>,
    guests: &mut HashMap<u32, GuestPeer>,
    writer: &mut WsWriter,
    ctx: &HostTaskCtx,
) -> Result<()> {
    if guests.contains_key(&peer_id) {
        // Ignore duplicate Hello from an already-registered peer to block snapshot-flood DoS.
        return Ok(());
    }
    if guests.len() >= MAX_HOST_GUESTS {
        return send_error(writer, peer_id, "room guest limit reached", &ctx.key).await;
    }
    let name = sanitize_guest_name(raw_name);
    let can_write = provided_token
        .and_then(|tok| decode_key(tok).ok())
        .is_some_and(|decoded| ct_eq(&decoded, &ctx.write_token));
    guests.insert(
        peer_id,
        GuestPeer {
            name: name.clone(),
            can_write,
        },
    );
    ctx.guest_count.store(guests.len(), Ordering::Relaxed);

    let snap = (ctx.snapshot_fn)();
    let chunks = chunk_nodes(&snap.nodes);
    let total_chunks = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
    send_host_frame(
        writer,
        peer_id,
        &HostFrame::Welcome {
            room: ctx.room.clone(),
            session_id: snap.session_id,
            title: snap.title,
            model: snap.model,
            can_write,
            total_chunks,
            head_node_id: snap.head_node_id.clone(),
        },
        &ctx.key,
    )
    .await?;

    for (idx, nodes) in chunks.into_iter().enumerate() {
        let index = u32::try_from(idx).unwrap_or(u32::MAX);
        send_host_frame(
            writer,
            peer_id,
            &HostFrame::SnapshotChunk {
                index,
                total: total_chunks,
                nodes,
            },
            &ctx.key,
        )
        .await?;
    }

    send_host_frame(
        writer,
        0,
        &HostFrame::State {
            busy: snap.busy,
            head_node_id: snap.head_node_id,
            guests: guests.len(),
        },
        &ctx.key,
    )
    .await?;

    let _ = ctx.inbound_tx.try_send(HostInbound::GuestJoined {
        peer_id,
        name,
        can_write,
    });
    Ok(())
}

async fn require_writable_guest<'a>(
    peer_id: u32,
    action: &str,
    guests: &'a HashMap<u32, GuestPeer>,
    writer: &mut WsWriter,
    ctx: &HostTaskCtx,
) -> Result<Option<&'a GuestPeer>> {
    let Some(guest) = guests.get(&peer_id) else {
        send_error(
            writer,
            peer_id,
            &format!("send hello before {action}"),
            &ctx.key,
        )
        .await?;
        return Ok(None);
    };
    if !guest.can_write {
        send_error(
            writer,
            peer_id,
            &format!("view-only link: {action} is not permitted"),
            &ctx.key,
        )
        .await?;
        return Ok(None);
    }
    Ok(Some(guest))
}

async fn handle_prompt(
    peer_id: u32,
    text: String,
    guests: &HashMap<u32, GuestPeer>,
    writer: &mut WsWriter,
    ctx: &HostTaskCtx,
) -> Result<()> {
    let Some(guest) = require_writable_guest(peer_id, "prompting", guests, writer, ctx).await?
    else {
        return Ok(());
    };
    if text.trim().is_empty() || text.len() > MAX_GUEST_PROMPT_BYTES {
        return send_error(
            writer,
            peer_id,
            "prompt must be non-empty and under 64 KiB",
            &ctx.key,
        )
        .await;
    }
    let badged_text = format_badged_prompt(&guest.name, &text);
    let _ = ctx.inbound_tx.try_send(HostInbound::Prompt {
        peer_id,
        guest_name: guest.name.clone(),
        badged_text,
        raw_text: text,
    });
    Ok(())
}

async fn handle_steer(
    peer_id: u32,
    text: String,
    guests: &HashMap<u32, GuestPeer>,
    writer: &mut WsWriter,
    ctx: &HostTaskCtx,
) -> Result<()> {
    let Some(guest) = require_writable_guest(peer_id, "steering", guests, writer, ctx).await?
    else {
        return Ok(());
    };
    if text.trim().is_empty() || text.len() > MAX_GUEST_PROMPT_BYTES {
        return send_error(
            writer,
            peer_id,
            "steer text must be non-empty and under 64 KiB",
            &ctx.key,
        )
        .await;
    }
    let _ = ctx.inbound_tx.try_send(HostInbound::Steer {
        peer_id,
        guest_name: guest.name.clone(),
        text,
    });
    Ok(())
}

async fn handle_abort(
    peer_id: u32,
    guests: &HashMap<u32, GuestPeer>,
    writer: &mut WsWriter,
    ctx: &HostTaskCtx,
) -> Result<()> {
    let Some(guest) = guests.get(&peer_id) else {
        return Ok(());
    };
    if !guest.can_write {
        return send_error(
            writer,
            peer_id,
            "view-only link: abort is not permitted",
            &ctx.key,
        )
        .await;
    }
    let _ = ctx.inbound_tx.try_send(HostInbound::Abort {
        peer_id,
        guest_name: guest.name.clone(),
    });
    Ok(())
}

async fn handle_approve(
    peer_id: u32,
    call_id: String,
    approved: bool,
    always: bool,
    guests: &HashMap<u32, GuestPeer>,
    writer: &mut WsWriter,
    ctx: &HostTaskCtx,
) -> Result<()> {
    let Some(guest) = require_writable_guest(peer_id, "approvals", guests, writer, ctx).await?
    else {
        return Ok(());
    };
    if call_id.trim().is_empty() {
        return send_error(
            writer,
            peer_id,
            "approval call_id must be non-empty",
            &ctx.key,
        )
        .await;
    }
    let _ = ctx.inbound_tx.try_send(HostInbound::Approve {
        peer_id,
        guest_name: guest.name.clone(),
        call_id,
        approved,
        always,
    });
    Ok(())
}

async fn handle_ask_answer(
    peer_id: u32,
    id: String,
    answers: Vec<String>,
    guests: &HashMap<u32, GuestPeer>,
    writer: &mut WsWriter,
    ctx: &HostTaskCtx,
) -> Result<()> {
    let Some(guest) =
        require_writable_guest(peer_id, "answering asks", guests, writer, ctx).await?
    else {
        return Ok(());
    };
    if id.trim().is_empty() {
        return send_error(writer, peer_id, "ask id must be non-empty", &ctx.key).await;
    }
    let _ = ctx.inbound_tx.try_send(HostInbound::AskAnswer {
        peer_id,
        guest_name: guest.name.clone(),
        id,
        answers,
    });
    Ok(())
}

async fn send_error(
    writer: &mut WsWriter,
    target: u32,
    message: &str,
    key: &[u8; KEY_LEN],
) -> Result<()> {
    send_host_frame(
        writer,
        target,
        &HostFrame::Error {
            message: message.to_owned(),
        },
        key,
    )
    .await
}

pub(super) async fn send_host_frame(
    writer: &mut WsWriter,
    target: u32,
    frame: &HostFrame,
    key: &[u8; KEY_LEN],
) -> Result<()> {
    let wire = encode_routed_frame(target, frame, key)?;
    writer.send_binary(&wire).await
}
