//! Outbound HTTP/HTTPS client (`hyper` + `rustls` + OS native certificate store),
//! streaming SSE frame parser, authenticated sealing/redaction, WebSocket client, and
//! unbypassable ToS/SSRF `guard`.

mod client;
pub mod guard;
pub mod redact;
pub mod seal;
pub mod sse;
pub mod ws;
mod ws_frame;

pub use client::{HttpClient, HttpResponse, SseStream, StreamReply};
pub use redact::{redact_json, redact_text};
pub use seal::{
    CollabLink, ct_eq, decode_key, encode_key, format_share_url, generate_key, generate_room_id,
    is_valid_id, parse_share_url, seal, unseal, unseal_with_limit,
};
pub use sse::{SseEvent, SseParser};
pub use ws::{
    WsEvent, WsReader, WsStream, WsWriter, connect_loopback_ws, connect_ws, connect_ws_timeout,
    loopback_http_get,
};
