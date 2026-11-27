//! Host-authoritative encrypted live collaboration (`protocol`, `host`, `guest`).

pub mod guest;
pub mod host;
mod host_handlers;
pub mod protocol;

pub use guest::{CollabGuest, GuestEvent, ReplicaConfig};
pub use host::{CollabHost, HostInbound, HostSnapshot};
pub use protocol::{
    GuestFrame, HostFrame, MAX_COLLAB_FRAME_BYTES, MAX_GUEST_FRAME_UNSEALED_BYTES,
    MAX_GUEST_PROMPT_BYTES, collab_branch_tag, decode_routed_frame, decode_routed_frame_with_limit,
    encode_routed_frame, format_badged_prompt, replica_node_id, replica_session_id,
    sanitize_guest_name,
};
