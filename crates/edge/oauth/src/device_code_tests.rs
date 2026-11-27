use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve_sequence(
    replies: Vec<(&'static str, &'static str)>,
) -> (String, Arc<Mutex<Vec<(String, Instant)>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for (status, body) in replies {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some((head, body_part)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if body_part.len() >= len || n == 0 {
                        break;
                    }
                } else if n == 0 {
                    break;
                }
            }
            log.lock()
                .expect("lock")
                .push((String::from_utf8_lossy(&buf).into_owned(), Instant::now()));
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn test_cfg(base: &str) -> DeviceCodeConfig {
    DeviceCodeConfig {
        device_auth_url: format!("{base}/device"),
        token_url: format!("{base}/token"),
        client_id: "test-device-client".into(),
        scope: Some("coding".into()),
        extra_params: Vec::new(),
        extra_headers: vec![("X-Test-Header".into(), "ok".into())],
        json_body: false,
        poll_time_unit: Some(Duration::from_millis(12)),
        max_wait: None,
    }
}

#[tokio::test]
async fn device_code_poll_expiry() {
    let http = HttpClient::new().expect("http");

    // 1. authorization_pending -> slow_down (+5 interval) -> expired_token
    let (base, seen) = serve_sequence(vec![
        (
            "200 OK",
            r#"{"device_code":"secret-dc-1","user_code":"ABCD-EFGH","verification_uri":"https://auth.example.com/verify","verification_uri_complete":"https://auth.example.com/verify?code=ABCD-EFGH","expires_in":120,"interval":1}"#,
        ),
        ("400 Bad Request", r#"{"error":"authorization_pending"}"#),
        ("400 Bad Request", r#"{"error":"slow_down"}"#),
        ("400 Bad Request", r#"{"error":"expired_token"}"#),
    ])
    .await;
    let cfg = test_cfg(&base);
    let auth = start_device_flow(&http, &cfg).await.expect("start flow");
    assert_eq!(auth.user_code, "ABCD-EFGH");
    assert_eq!(
        auth.display_url(),
        "https://auth.example.com/verify?code=ABCD-EFGH"
    );

    let err = poll_device_token(&http, &cfg, &auth, &CancelFlag::new())
        .await
        .expect_err("must expire on expired_token");
    assert!(matches!(err, HrnError::Auth(_)), "{err}");
    assert!(err.to_string().contains("expired"), "{err}");
    assert!(
        !err.to_string().contains("secret-dc-1"),
        "device_code must never leak into errors: {err}"
    );

    let reqs = seen.lock().expect("seen").clone();
    assert_eq!(reqs.len(), 4, "1 device_auth + 3 token polls");
    // Gap after slow_down (req[3] - req[2], 6 units ~= 72ms) must exceed initial gap (req[2] - req[1], 1 unit ~= 12ms).
    let gap_after_slow_down = reqs[3].1.saturating_duration_since(reqs[2].1);
    assert!(
        gap_after_slow_down >= Duration::from_millis(50),
        "slow_down must increase interval by +5 units: {gap_after_slow_down:?}"
    );
    assert!(reqs[0].0.contains("x-test-header: ok") || reqs[0].0.contains("X-Test-Header: ok"));

    // 2. Wall-clock expiry when server stays pending past expires_in / max_wait.
    let (base2, _) = serve_sequence(vec![
        ("400 Bad Request", r#"{"error":"authorization_pending"}"#),
        ("400 Bad Request", r#"{"error":"authorization_pending"}"#),
        ("400 Bad Request", r#"{"error":"authorization_pending"}"#),
    ])
    .await;
    let mut short_cfg = test_cfg(&base2);
    short_cfg.max_wait = Some(Duration::from_millis(35));
    let short_auth = DeviceAuthResponse {
        device_code: "secret-dc-2".into(),
        user_code: "WXYZ-1234".into(),
        verification_uri: "https://auth.example.com/verify".into(),
        verification_uri_complete: None,
        expires_in: 100,
        interval: 2,
    };
    let err = poll_device_token(&http, &short_cfg, &short_auth, &CancelFlag::new())
        .await
        .expect_err("wall-clock expiry");
    assert!(matches!(err, HrnError::Auth(_)), "{err}");
    assert!(err.to_string().contains("expired"), "{err}");

    // 3. access_denied -> Auth error.
    let (base3, _) =
        serve_sequence(vec![("400 Bad Request", r#"{"error":"access_denied"}"#)]).await;
    let err = poll_device_token(&http, &test_cfg(&base3), &short_auth, &CancelFlag::new())
        .await
        .expect_err("denied");
    assert!(matches!(err, HrnError::Auth(_)), "{err}");
    assert!(err.to_string().contains("denied"), "{err}");

    // 4. CancelFlag cancellation -> HrnError::Cancelled.
    let cancel = CancelFlag::new();
    cancel.cancel();
    let err = poll_device_token(&http, &test_cfg(&base3), &short_auth, &cancel)
        .await
        .expect_err("cancelled");
    assert!(matches!(err, HrnError::Cancelled), "{err}");

    // 5. Success path after one pending poll.
    let (base4, _) = serve_sequence(vec![
        ("400 Bad Request", r#"{"error":"authorization_pending"}"#),
        (
            "200 OK",
            r#"{"access_token":"tok-ok","refresh_token":"ref-ok","expires_in":3600,"token_type":"Bearer"}"#,
        ),
    ])
    .await;
    let tok = poll_device_token(&http, &test_cfg(&base4), &short_auth, &CancelFlag::new())
        .await
        .expect("token success");
    assert_eq!(tok.access_token, "tok-ok");
    assert_eq!(tok.refresh_token.as_deref(), Some("ref-ok"));
}

#[test]
fn device_code_debug_redacts_secrets() {
    let resp = DeviceAuthResponse {
        device_code: "super-secret-device-code-999".into(),
        user_code: "ABCD-1234".into(),
        verification_uri: "https://auth.kimi.com/verify".into(),
        verification_uri_complete: Some("https://auth.kimi.com/verify?code=ABCD-1234".into()),
        expires_in: 600,
        interval: 5,
    };
    let shown = format!("{resp:?}");
    assert!(
        !shown.contains("super-secret-device-code-999"),
        "device_code leaked in Debug: {shown}"
    );
    assert!(shown.contains("[redacted]"), "{shown}");
    assert!(shown.contains("ABCD-1234"), "{shown}");

    let cfg = DeviceCodeConfig {
        device_auth_url: "https://auth.kimi.com/api/oauth/device_authorization".into(),
        token_url: "https://auth.kimi.com/api/oauth/token".into(),
        client_id: "client-1".into(),
        scope: None,
        extra_params: Vec::new(),
        extra_headers: vec![("Authorization".into(), "Bearer super-secret-hdr".into())],
        json_body: false,
        poll_time_unit: None,
        max_wait: None,
    };
    let cfg_shown = format!("{cfg:?}");
    assert!(
        !cfg_shown.contains("super-secret-hdr"),
        "header value leaked in Debug: {cfg_shown}"
    );
}

#[tokio::test]
async fn device_code_rejects_plaintext_non_loopback_verification_uri() {
    let http = HttpClient::new().expect("http");
    let (base1, _) = serve_sequence(vec![(
        "200 OK",
        r#"{"device_code":"dc-1","user_code":"ABCD-1234","verification_uri":"http://auth.example.com/verify","expires_in":60,"interval":1}"#,
    )])
    .await;
    let err = start_device_flow(&http, &test_cfg(&base1))
        .await
        .expect_err("non-loopback http verification_uri must fail");
    assert!(err.to_string().contains("https://"), "{err}");

    let (base2, _) = serve_sequence(vec![(
        "200 OK",
        r#"{"device_code":"dc-2","user_code":"ABCD-1234","verification_uri":"https://auth.example.com/verify","verification_uri_complete":"http://evil.example.com/phish","expires_in":60,"interval":1}"#,
    )])
    .await;
    let err = start_device_flow(&http, &test_cfg(&base2))
        .await
        .expect_err("non-loopback http verification_uri_complete must fail");
    assert!(err.to_string().contains("https://"), "{err}");
}
