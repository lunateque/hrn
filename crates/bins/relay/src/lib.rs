//! Optional local/LAN `hrn-relay` server library for sealed share blobs and live collab rooms.
//!
//! Re-exports [`hrn_edge_ipc::relay`] so both the standalone `hrn-relay` binary and embedded
//! in-process relay inside `hrn` share the exact same implementation.

pub use hrn_edge_ipc::relay::*;

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use hrn_edge_http::{HttpClient, connect_ws};
    use std::time::Duration;
    use tokio::net::TcpListener;

    #[test]
    fn ttl_parser_handles_units() {
        assert_eq!(parse_ttl("30d").expect("30d"), Duration::from_hours(720));
        assert_eq!(parse_ttl("12h").expect("12h"), Duration::from_hours(12));
        assert_eq!(parse_ttl("5m").expect("5m"), Duration::from_mins(5));
        assert_eq!(parse_ttl("45s").expect("45s"), Duration::from_secs(45));
        assert!(parse_ttl("0s").is_err());
        assert!(parse_ttl("999999999d").is_err());
        assert!(parse_ttl("bogus").is_err());
    }

    #[tokio::test]
    async fn relay_enforces_blob_cap_security_headers_and_csp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let cfg = RelayConfig {
            max_blob_bytes: 256,
            ..RelayConfig::default()
        };
        let handle = serve(listener, cfg).await.expect("serve");
        let http = HttpClient::new().expect("http");

        // Viewer serves strict hash-pinned CSP with zero external scripts or unsafe-inline.
        let home = http
            .get(&handle.base_url(), &[], Duration::from_secs(5), 64 * 1024)
            .await
            .expect("get /");
        assert_eq!(home.status, http::StatusCode::OK);
        let csp = home
            .headers
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .expect("csp header");
        assert!(csp.contains("default-src 'none'"), "{csp}");
        assert!(csp.contains("script-src 'sha256-"), "{csp}");
        assert!(!csp.contains("unsafe-inline"), "{csp}");
        assert_eq!(
            home.headers
                .get("x-content-type-options")
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            home.headers
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let html = home.text();
        assert!(
            !html.contains("innerHTML"),
            "viewer must never use innerHTML"
        );
        assert!(
            !html.contains("https://"),
            "viewer must have zero external URLs"
        );

        // Oversize POST /s is rejected with 413 Payload Too Large.
        let post_url = format!("{}/s", handle.base_url());
        let oversize = http
            .post_raw(
                &post_url,
                &[],
                Bytes::from(vec![0xAB; 512]),
                Duration::from_secs(5),
            )
            .await
            .expect("oversize response");
        assert_eq!(oversize.status, http::StatusCode::PAYLOAD_TOO_LARGE);

        // Valid POST /s roundtrips opaque bytes via GET /s/<id>/raw.
        let payload = vec![0x42u8; 64];
        let created = http
            .post_raw(
                &post_url,
                &[],
                Bytes::from(payload.clone()),
                Duration::from_secs(5),
            )
            .await
            .expect("post ok");
        assert_eq!(created.status, http::StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&created.body).expect("json");
        let id = json["id"].as_str().expect("id");
        let fetched = http
            .get(
                &format!("{}/s/{id}/raw", handle.base_url()),
                &[],
                Duration::from_secs(5),
                4096,
            )
            .await
            .expect("get raw");
        assert_eq!(fetched.status, http::StatusCode::OK);
        assert_eq!(fetched.body.as_ref(), payload.as_slice());

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn room_close_and_peer_eviction_send_ws_close_promptly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let handle = serve(listener, RelayConfig::default())
            .await
            .expect("serve");
        let ws_base = handle.base_url().replacen("http://", "ws://", 1);

        let mut host = connect_ws(&format!("{ws_base}/r/room_teardown_1?role=host"))
            .await
            .expect("host ws");
        let mut guest = connect_ws(&format!("{ws_base}/r/room_teardown_1?role=guest"))
            .await
            .expect("guest ws");

        // Verify live frame delivery works before teardown.
        let mut frame = vec![0u8; 4];
        frame.extend_from_slice(b"opaque_ciphertext");
        host.send_binary(&frame).await.expect("host send");
        let got = tokio::time::timeout(Duration::from_secs(2), guest.recv_binary())
            .await
            .expect("guest recv timeout")
            .expect("guest recv")
            .expect("binary");
        assert_eq!(&got[4..], b"opaque_ciphertext");

        // Host closes -> relay removes room, closes guest's out_rx, and guest sees clean Close/EOF
        // immediately (must not deadlock waiting on pong_rx).
        let _ = host.send_close().await;
        drop(host);
        let closed = tokio::time::timeout(Duration::from_secs(2), guest.recv_binary())
            .await
            .expect("guest must observe close promptly without deadlock");
        assert!(
            matches!(closed, Ok(None) | Err(_)),
            "expected closed WS stream after host disconnect"
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn relay_serves_mobile_pwa_on_room_url_and_manifest() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let handle = serve(listener, RelayConfig::default())
            .await
            .expect("serve");
        let http = HttpClient::new().expect("http");

        // Regular browser GET /r/<room> receives the single-file E2E Mobile PWA with hash CSP.
        let pwa = http
            .get(
                &format!("{}/r/room_mobile_1", handle.base_url()),
                &[],
                Duration::from_secs(5),
                256 * 1024,
            )
            .await
            .expect("get /r/<room>");
        assert_eq!(pwa.status, http::StatusCode::OK);
        let csp = pwa
            .headers
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .expect("pwa csp");
        assert!(csp.contains("default-src 'none'"), "{csp}");
        assert!(csp.contains("script-src 'sha256-"), "{csp}");
        assert!(csp.contains("style-src 'sha256-"), "{csp}");
        assert!(csp.contains("connect-src 'self' ws: wss:"), "{csp}");
        assert!(!csp.contains("unsafe-inline"), "{csp}");
        let html = pwa.text();
        assert!(html.contains("hrn remote"));
        assert!(!html.contains("innerHTML"));

        // GET /manifest.json returns the PWA manifest.
        let manifest = http
            .get(
                &format!("{}/manifest.json", handle.base_url()),
                &[],
                Duration::from_secs(5),
                16 * 1024,
            )
            .await
            .expect("get manifest");
        assert_eq!(manifest.status, http::StatusCode::OK);
        let mjson: serde_json::Value = serde_json::from_slice(&manifest.body).expect("json");
        assert_eq!(mjson["name"], "hrn remote");
        assert_eq!(mjson["display"], "standalone");

        // GET /r/<room>?role=guest without Upgrade still returns 400 expected WebSocket upgrade.
        let bad_ws = http
            .get(
                &format!("{}/r/room_mobile_1?role=guest", handle.base_url()),
                &[],
                Duration::from_secs(5),
                4096,
            )
            .await
            .expect("get ?role=guest");
        assert_eq!(bad_ws.status, http::StatusCode::BAD_REQUEST);

        handle.shutdown().await;
    }
}
