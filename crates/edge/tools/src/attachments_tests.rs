use super::*;
use crate::testutil::TempDir;

#[test]
fn attachment_scheme_prevents_path_traversal() {
    let dir = TempDir::new("attachment-sec");
    let store = DiskAttachmentStore::new(dir.path().to_path_buf());
    // Write a secret file outside the session attachments directory.
    let secret = dir.path().join("secret.txt");
    fs::write(&secret, "TOP_SECRET").expect("write secret");
    let session_parent_secret = dir.path().join("sessions").join("s1").join("secret.txt");
    fs::create_dir_all(session_parent_secret.parent().expect("parent")).expect("mkdir");
    fs::write(&session_parent_secret, "SESSION_SECRET").expect("write session secret");

    let key = store
        .store_bytes("s1", "../../evil\\report (1).txt", b"safe body")
        .expect("store");
    assert_eq!(key, "1-report__1_.txt");

    let resolved = store.resolve("s1", &key).expect("resolve stored");
    assert_eq!(fs::read(&resolved).expect("read"), b"safe body");

    for traversal in [
        "../secret.txt",
        "..\\secret.txt",
        "../../secret.txt",
        "sub/1-report__1_.txt",
        "/etc/passwd",
        "C:\\Windows\\win.ini",
        "1-report\0.txt",
        "1-report\n.txt",
        ".",
        "..",
        ".hidden",
        "CON",
        "NUL.txt",
    ] {
        assert!(
            store.resolve("s1", traversal).is_err(),
            "must reject traversal key: {traversal:?}"
        );
    }

    for bad_session in ["../escape", "..\\escape", "a/b", "CON", ""] {
        assert!(
            store.resolve(bad_session, &key).is_err(),
            "must reject bad session id: {bad_session:?}"
        );
        assert!(
            store.list(bad_session).is_err(),
            "must reject bad session id in list: {bad_session:?}"
        );
    }

    // If symlink creation is permitted on this host, verify an escaped symlink is refused.
    let link = store.attachments_dir("s1").expect("dir").join("2-link.txt");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&secret, &link).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&secret, &link).is_ok();
    if linked {
        let err = store
            .resolve("s1", "2-link.txt")
            .expect_err("escaped symlink must be refused")
            .to_string();
        assert!(err.contains("escapes"), "{err}");
        let listed = store.list("s1").expect("list");
        assert!(listed.iter().all(|(n, _)| n != "2-link.txt"));
    }
}

#[test]
fn stores_decodes_and_lists_images_in_numeric_order() {
    let dir = TempDir::new("attachment-img");
    let store = DiskAttachmentStore::new(dir.path().to_path_buf());
    let img = ImageAttachment {
        media_type: "image/png".to_owned(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(b"PNGDATA"),
        label: Some("capture.png".to_owned()),
        width: Some(10),
        height: Some(10),
    };
    let k1 = store.store_image("root-1", &img).expect("store image");
    let k2 = store
        .store_bytes("root-1", "notes.json", br#"{"ok":true}"#)
        .expect("store json");
    assert_eq!(k1, "1-capture.png");
    assert_eq!(k2, "2-notes.json");
    let list = store.list("root-1").expect("list");
    assert_eq!(list, vec![(k1, 7), (k2, 11)]);
}

#[test]
fn enforces_pre_decode_base64_cap_quotas_and_dot_boundary_truncation() {
    let dir = TempDir::new("attachment-quotas");
    let store = DiskAttachmentStore::with_quotas(dir.path().to_path_buf(), 2, 100);

    // Pre-decode base64 size check rejects oversized strings without allocating decoded output.
    let huge = ImageAttachment {
        media_type: "image/png".to_owned(),
        data_base64: "A".repeat(MAX_BASE64_LEN + 8),
        label: Some("huge.png".to_owned()),
        width: None,
        height: None,
    };
    let err = store
        .store_image("s-q", &huge)
        .expect_err("oversized base64 must fail pre-decode")
        .to_string();
    assert!(err.contains("encoded limit"), "{err}");

    // Filename whose `.` falls right on the truncation boundary must never produce `..`.
    let dotted = format!("{}.tail.png", "a".repeat(115));
    let k1 = store
        .store_bytes("s-q", &dotted, &[1u8; 40])
        .expect("store 1");
    assert!(!k1.contains(".."), "{k1}");
    assert!(store.resolve("s-q", &k1).is_ok());

    // Byte quota enforced before writing a 2nd oversized attachment.
    let byte_err = store
        .store_bytes("s-q", "too-big.bin", &[2u8; 70])
        .expect_err("byte quota")
        .to_string();
    assert!(byte_err.contains("byte quota"), "{byte_err}");

    // File count quota enforced after reaching max_session_files.
    store
        .store_bytes("s-q", "ok.bin", &[3u8; 20])
        .expect("store 2");
    let count_err = store
        .store_bytes("s-q", "third.bin", &[4u8; 1])
        .expect_err("file count quota")
        .to_string();
    assert!(count_err.contains("limit reached"), "{count_err}");
}
