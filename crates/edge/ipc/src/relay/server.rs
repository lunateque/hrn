//! HTTP/1.1 + WebSocket upgrade router for `hrn-relay`.

use super::RelayConfig;
use super::blobs::BlobStore;
use super::rooms::{JoinError, PeerRole, RoomHub};
use super::ws_server::{RelayWsEvent, split_upgraded};
use crate::{MOBILE_MANIFEST_JSON, MOBILE_PWA_HTML, sha256, ws_accept_key};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use hrn_edge_http::seal::{NONCE_LEN, TAG_LEN, is_valid_id};
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::TcpStream;

const VIEWER_HTML: &str = include_str!("../../data/share_viewer.html");
/// Maximum time allowed for an HTTP/1.1 client to finish sending request headers.
const HTTP_HEADER_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum time allowed for a client to stream a `POST /s` blob body.
const POST_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared server state passed to every connection handler.
pub(crate) struct RelayState {
    pub(crate) cfg: RelayConfig,
    pub(crate) blobs: BlobStore,
    pub(crate) rooms: RoomHub,
}

impl RelayState {
    pub(crate) fn new(cfg: RelayConfig) -> Self {
        let blobs = BlobStore::new(
            cfg.max_blob_bytes,
            cfg.max_total_blob_bytes,
            cfg.max_blobs,
            cfg.blob_ttl,
        );
        let rooms = RoomHub::new(
            cfg.max_rooms,
            cfg.max_guests_per_room,
            cfg.send_queue_cap,
            cfg.room_idle_timeout,
            cfg.on_frame.clone(),
        );
        Self { cfg, blobs, rooms }
    }
}

/// Serve one accepted TCP connection with HTTP/1.1 + WebSocket upgrade support.
pub(crate) async fn handle_connection(stream: TcpStream, state: Arc<RelayState>) {
    let io = TokioIo::new(stream);
    let svc = service_fn(move |req| {
        let st = Arc::clone(&state);
        async move { Ok::<_, std::convert::Infallible>(route_request(req, st).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HTTP_HEADER_TIMEOUT)
        .serve_connection(io, svc)
        .with_upgrades()
        .await;
}

async fn route_request(req: Request<Incoming>, state: Arc<RelayState>) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().map(str::to_owned);

    match (method, path.as_str()) {
        (Method::GET, "/healthz") => healthz(&state),
        (Method::GET, "/") => viewer_response(),
        (Method::GET, "/manifest.json") => manifest_response(),
        (Method::POST, "/s") => handle_post_blob(req, &state).await,
        (Method::GET, p) if p.starts_with("/s/") => handle_get_share(p, &state),
        (Method::GET, p) if p.starts_with("/r/") => handle_ws_room(req, p, query.as_deref(), state),
        _ => plain_error(StatusCode::NOT_FOUND, "not found"),
    }
}

fn healthz(state: &RelayState) -> Response<Full<Bytes>> {
    let body = json!({
        "status": "ok",
        "rooms": state.rooms.room_count(),
        "blobs": state.blobs.count_and_reap(),
    })
    .to_string();
    json_response(StatusCode::OK, body)
}

fn handle_get_share(path: &str, state: &RelayState) -> Response<Full<Bytes>> {
    let rest = path.strip_prefix("/s/").unwrap_or_default();
    if let Some(id) = rest.strip_suffix("/raw") {
        if !is_valid_id(id) {
            return plain_error(StatusCode::BAD_REQUEST, "invalid blob id");
        }
        return match state.blobs.get(id) {
            Some(bytes) => binary_response(StatusCode::OK, bytes),
            None => plain_error(StatusCode::NOT_FOUND, "blob not found or expired"),
        };
    }
    let id = rest.trim_end_matches('/');
    if !is_valid_id(id) {
        return plain_error(StatusCode::BAD_REQUEST, "invalid blob id");
    }
    viewer_response()
}

async fn handle_post_blob(req: Request<Incoming>, state: &RelayState) -> Response<Full<Bytes>> {
    let max = state.cfg.max_blob_bytes;
    if let Some(cl) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        && cl > max
    {
        return plain_error(StatusCode::PAYLOAD_TOO_LARGE, "blob exceeds 1 MiB limit");
    }

    let mut body = req.into_body();
    let mut buf = Vec::new();
    let read_res = tokio::time::timeout(POST_BODY_TIMEOUT, async {
        while let Some(frame_res) = body.frame().await {
            let Ok(frame) = frame_res else {
                return Err((StatusCode::BAD_REQUEST, "failed reading request body"));
            };
            if let Some(data) = frame.data_ref() {
                if buf.len().saturating_add(data.len()) > max {
                    return Err((StatusCode::PAYLOAD_TOO_LARGE, "blob exceeds 1 MiB limit"));
                }
                buf.extend_from_slice(data);
            }
        }
        Ok(())
    })
    .await;
    match read_res {
        Ok(Ok(())) => {}
        Ok(Err((status, msg))) => return plain_error(status, msg),
        Err(_) => return plain_error(StatusCode::REQUEST_TIMEOUT, "request body read timed out"),
    }
    if buf.len() < NONCE_LEN + TAG_LEN {
        return plain_error(StatusCode::BAD_REQUEST, "sealed blob too short");
    }
    match state.blobs.insert(Bytes::from(buf)) {
        Ok(id) => json_response(StatusCode::OK, json!({ "id": id }).to_string()),
        Err(_) => plain_error(StatusCode::INSUFFICIENT_STORAGE, "blob store full"),
    }
}

fn handle_ws_room(
    req: Request<Incoming>,
    path: &str,
    query: Option<&str>,
    state: Arc<RelayState>,
) -> Response<Full<Bytes>> {
    let room_id = path
        .strip_prefix("/r/")
        .unwrap_or_default()
        .trim_end_matches('/');
    if !is_valid_id(room_id) {
        return plain_error(StatusCode::BAD_REQUEST, "invalid room id");
    }
    let upgrade_ok = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let has_role_param = query.is_some_and(|q| {
        q.split('&')
            .any(|pair| pair == "role" || pair.starts_with("role="))
    });
    // Regular browser navigation to `/r/<room>#<key>[.<write_token>]` receives the E2E Mobile PWA;
    // the fragment stays strictly client-side and the PWA upgrades `/r/<room>?role=guest`.
    if !upgrade_ok && !has_role_param {
        return mobile_pwa_response();
    }
    let Some(role) = parse_role(query) else {
        return plain_error(
            StatusCode::BAD_REQUEST,
            "missing or invalid role=host|guest",
        );
    };
    let ws_key = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let Some(key) = ws_key.filter(|_| upgrade_ok) else {
        return plain_error(StatusCode::BAD_REQUEST, "expected WebSocket upgrade");
    };
    if let Err(err) = state.rooms.precheck_join(room_id, role) {
        return join_error_response(err);
    }

    let accept = ws_accept_key(&key);
    let room_owned = room_id.to_owned();
    let max_frame = state.cfg.max_ws_frame_bytes;
    let on_upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
        let Ok(upgraded) = on_upgrade.await else {
            return;
        };
        run_ws_peer(upgraded, room_owned, role, max_frame, state).await;
    });

    let mut resp = Response::new(Full::new(Bytes::new()));
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = resp.headers_mut();
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Ok(val) = HeaderValue::from_str(&accept) {
        headers.insert("sec-websocket-accept", val);
    }
    resp
}

async fn run_ws_peer(
    upgraded: hyper::upgrade::Upgraded,
    room_id: String,
    role: PeerRole,
    max_frame_bytes: usize,
    state: Arc<RelayState>,
) {
    let (mut reader, mut writer) = split_upgraded(upgraded, max_frame_bytes);
    let Ok((peer_id, conn_token, mut out_rx)) = state.rooms.join(&room_id, role) else {
        let _ = writer.send_close().await;
        return;
    };

    let (pong_tx, mut pong_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
    let mut write_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = out_rx.recv() => {
                    let Some(frame) = frame else {
                        break;
                    };
                    if writer.send_binary(&frame).await.is_err() {
                        break;
                    }
                }
                ping = pong_rx.recv() => {
                    let Some(ping_data) = ping else {
                        break;
                    };
                    if writer.send_pong(&ping_data).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = writer.send_close().await;
        let _ = done_tx.send(());
    });

    loop {
        tokio::select! {
            biased;
            _ = &mut done_rx => break,
            ev_res = reader.next_event() => {
                match ev_res {
                    Ok(RelayWsEvent::Binary(data)) => {
                        state
                            .rooms
                            .route_frame(&room_id, role, peer_id, conn_token, &data);
                    }
                    Ok(RelayWsEvent::Ping(payload)) => {
                        let _ = pong_tx.try_send(payload);
                    }
                    Ok(RelayWsEvent::Closed) | Err(_) => break,
                }
            }
        }
    }

    state.rooms.leave(&room_id, role, peer_id, conn_token);
    drop(pong_tx);
    tokio::select! {
        _ = &mut write_task => {}
        () = tokio::time::sleep(Duration::from_millis(250)) => {
            write_task.abort();
        }
    }
}

fn parse_role(query: Option<&str>) -> Option<PeerRole> {
    let q = query?;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == "role"
        {
            return match v {
                "host" => Some(PeerRole::Host),
                "guest" => Some(PeerRole::Guest),
                _ => None,
            };
        }
    }
    None
}

fn join_error_response(err: JoinError) -> Response<Full<Bytes>> {
    match err {
        JoinError::InvalidRoom => plain_error(StatusCode::BAD_REQUEST, "invalid room id"),
        JoinError::HostAlreadyPresent => {
            plain_error(StatusCode::CONFLICT, "room already has an active host")
        }
        JoinError::MaxRoomsReached => {
            plain_error(StatusCode::SERVICE_UNAVAILABLE, "max rooms reached")
        }
        JoinError::MaxGuestsReached => {
            plain_error(StatusCode::SERVICE_UNAVAILABLE, "room guest limit reached")
        }
    }
}

fn viewer_csp() -> &'static str {
    static CSP: OnceLock<String> = OnceLock::new();
    CSP.get_or_init(|| {
        let style_hash = extract_tag_sha256(VIEWER_HTML, "<style>", "</style>");
        let script_hash = extract_tag_sha256(VIEWER_HTML, "<script>", "</script>");
        format!(
            "default-src 'none'; script-src 'sha256-{script_hash}'; \
             style-src 'sha256-{style_hash}'; connect-src 'self'; img-src 'none'; \
             base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
        )
    })
}

fn mobile_pwa_csp() -> &'static str {
    static CSP: OnceLock<String> = OnceLock::new();
    CSP.get_or_init(|| {
        let style_hash = extract_tag_sha256(MOBILE_PWA_HTML, "<style>", "</style>");
        let script_hash = extract_tag_sha256(MOBILE_PWA_HTML, "<script>", "</script>");
        format!(
            "default-src 'none'; script-src 'sha256-{script_hash}'; \
             style-src 'sha256-{style_hash}'; connect-src 'self' ws: wss:; \
             img-src 'self' data:; manifest-src 'self'; \
             base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
        )
    })
}

fn extract_tag_sha256(html: &str, open: &str, close: &str) -> String {
    let body = html
        .split_once(open)
        .and_then(|(_, rest)| rest.split_once(close))
        .map_or("", |(inner, _)| inner);
    // Normalize line endings to LF per HTML5 §13.2.3.5 so CSP hashes stay valid on CRLF checkouts.
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    B64.encode(sha256(normalized.as_bytes()))
}

fn add_security_headers(resp: &mut Response<Full<Bytes>>) {
    let h = resp.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
}

fn viewer_response() -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from_static(VIEWER_HTML.as_bytes())));
    *resp.status_mut() = StatusCode::OK;
    add_security_headers(&mut resp);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    if let Ok(val) = HeaderValue::from_str(viewer_csp()) {
        resp.headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, val);
    }
    resp
}

fn mobile_pwa_response() -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from_static(MOBILE_PWA_HTML.as_bytes())));
    *resp.status_mut() = StatusCode::OK;
    add_security_headers(&mut resp);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    if let Ok(val) = HeaderValue::from_str(mobile_pwa_csp()) {
        resp.headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, val);
    }
    resp
}

fn manifest_response() -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from_static(
        MOBILE_MANIFEST_JSON.as_bytes(),
    )));
    *resp.status_mut() = StatusCode::OK;
    add_security_headers(&mut resp);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/manifest+json"),
    );
    resp
}

fn binary_response(status: StatusCode, bytes: Bytes) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(bytes));
    *resp.status_mut() = status;
    add_security_headers(&mut resp);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    resp
}

fn json_response(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(body)));
    *resp.status_mut() = status;
    add_security_headers(&mut resp);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

fn plain_error(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(msg.to_owned())));
    *resp.status_mut() = status;
    add_security_headers(&mut resp);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_hash_is_crlf_resilient() {
        let lf = "<script>\nconst x = 1;\n</script>";
        let crlf = "<script>\r\nconst x = 1;\r\n</script>";
        assert_eq!(
            extract_tag_sha256(lf, "<script>", "</script>"),
            extract_tag_sha256(crlf, "<script>", "</script>")
        );
    }
}
