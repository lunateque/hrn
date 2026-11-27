use super::*;

fn token(access: &str, extra: Option<(&str, &str)>) -> StoredToken {
    let mut map = Map::new();
    if let Some((k, v)) = extra {
        map.insert(k.to_owned(), json!(v));
    }
    StoredToken {
        access_token: access.to_owned(),
        refresh_token: Some("r1".to_owned()),
        expires_at: 4_000_000_000,
        token_type: Some("Bearer".to_owned()),
        extra: map,
    }
}

#[test]
fn refresh_keeps_rotated_or_previous_refresh_token_and_extra_fields() {
    let prev = token("old", Some(("account_id", "acct-9")));
    // Provider that does not rotate the refresh token and returns a new id_token.
    let body = br#"{"access_token":"new","expires_in":60,"id_token":"jwt"}"#;
    let got = parse_token_response(body, Some(&prev)).expect("parse");
    assert_eq!(got.access_token, "new");
    assert_eq!(got.refresh_token.as_deref(), Some("r1"));
    assert_eq!(got.extra.get("account_id"), Some(&json!("acct-9")));
    assert_eq!(got.extra.get("id_token"), Some(&json!("jwt")));
    // Provider that rotates it.
    let body = br#"{"access_token":"new2","refresh_token":"r2","expires_in":60}"#;
    let got = parse_token_response(body, Some(&prev)).expect("parse");
    assert_eq!(got.refresh_token.as_deref(), Some("r2"));
}

#[test]
fn atomic_write_replaces_existing_file_and_round_trips_unknown_fields() {
    let dir = std::env::temp_dir().join(format!("hrn-oauth-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("token.json");
    write_json_atomic(&path, &token("first", Some(("account_id", "a")))).expect("write 1");
    write_json_atomic(&path, &token("second", Some(("account_id", "b")))).expect("write 2");
    let back = read_token(&path).expect("read");
    assert_eq!(back.access_token, "second");
    assert_eq!(back.extra.get("account_id"), Some(&json!("b")));
    let leftovers = std::fs::read_dir(&dir).expect("ls").count();
    assert_eq!(leftovers, 1, "temp files must not be left behind");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn legacy_aliases_are_read_and_expiry_in_millis_is_normalized() {
    let dir = std::env::temp_dir().join(format!("hrn-oauth-alias-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("token.json");
    std::fs::write(
        &path,
        r#"{"access":"a","refresh":"r","expires":4000000000000}"#,
    )
    .expect("seed");
    let tok = read_token(&path).expect("read");
    assert_eq!(tok.refresh_token.as_deref(), Some("r"));
    assert_eq!(tok.expires_at, 4_000_000_000);
    assert!(tok.extra.is_empty());
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn token_urls_must_be_https_except_loopback() {
    assert!(require_secure_url("https://auth.example.com/token", "token_url").is_ok());
    assert!(require_secure_url("http://127.0.0.1:9/token", "token_url").is_ok());
    assert!(require_secure_url("http://auth.example.com/token", "token_url").is_err());
    assert!(require_secure_url("ftp://x", "token_url").is_err());
}

#[test]
fn transient_refresh_statuses_are_retryable_but_invalid_grant_is_auth() {
    assert!(matches!(
        token_endpoint_error("refresh", 503, "x", Some(3)),
        HrnError::Provider {
            retryable: true,
            retry_after_secs: Some(3),
            ..
        }
    ));
    assert!(matches!(
        token_endpoint_error("refresh", 400, "invalid_grant", None),
        HrnError::Auth(_)
    ));
}

#[test]
fn stored_token_debug_redacts_secrets() {
    let shown = format!(
        "{:?}",
        token("secret-access-token", Some(("id_token", "secret-jwt")))
    );
    assert!(
        !shown.contains("secret-access-token")
            && !shown.contains("r1")
            && !shown.contains("secret-jwt")
            && shown.contains("[redacted]"),
        "{shown}"
    );
}
