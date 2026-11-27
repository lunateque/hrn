use super::*;
use crate::update_archive::{extract_release_binaries, hrn_ast_exe_name, hrn_exe_name};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

fn temp_dir(tag: &str) -> PathBuf {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("hrn-update-test-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn update_check_parses_release() {
    let stable_json = r#"{
        "tag_name": "v9.2.1",
        "prerelease": false,
        "draft": false,
        "html_url": "https://github.com/lunateque/harness/releases/tag/v9.2.1",
        "assets": [
            {
                "name": "hrn-v9.2.1-x86_64-pc-windows-msvc.zip",
                "browser_download_url": "https://github.com/lunateque/harness/releases/download/v9.2.1/hrn-v9.2.1-x86_64-pc-windows-msvc.zip",
                "size": 1024
            },
            {
                "name": "SHA256SUMS",
                "browser_download_url": "https://github.com/lunateque/harness/releases/download/v9.2.1/SHA256SUMS",
                "size": 128
            }
        ]
    }"#;
    let rel = parse_release_response(stable_json, UpdateChannel::Stable).expect("parse stable");
    assert_eq!(rel.tag_name, "v9.2.1");
    assert!(!rel.prerelease);
    assert!(is_newer_version("0.1.0", &rel.tag_name));
    assert!(!is_newer_version("9.2.1", &rel.tag_name));
    assert!(is_newer_version("1.0.0-canary.1", "1.0.0"));
    assert!(!is_newer_version("1.0.0", "1.0.0-canary.1"));

    let (bin, sums) = select_release_assets(&rel, "windows", "x86_64").expect("select assets");
    assert_eq!(bin.name, "hrn-v9.2.1-x86_64-pc-windows-msvc.zip");
    assert_eq!(sums.name, "SHA256SUMS");

    // Canary list picks first non-draft prerelease.
    let list_json = r#"[
        {"tag_name": "v9.3.0-draft", "prerelease": true, "draft": true, "assets": []},
        {"tag_name": "v9.3.0-canary.1", "prerelease": true, "draft": false, "assets": []},
        {"tag_name": "v9.2.1", "prerelease": false, "draft": false, "assets": []}
    ]"#;
    let canary = parse_release_response(list_json, UpdateChannel::Canary).expect("parse canary");
    assert_eq!(canary.tag_name, "v9.3.0-canary.1");
    let stable_from_list =
        parse_release_response(list_json, UpdateChannel::Stable).expect("stable skips prerelease");
    assert_eq!(stable_from_list.tag_name, "v9.2.1");
}

#[test]
fn update_replace_rename_dance() {
    let dir = temp_dir("rename");
    let target = dir.join(hrn_exe_name());
    fs::write(&target, b"OLD_BINARY_V1").expect("write v1");

    // Verify SHA-256 mismatch refuses update before touching disk.
    let new_bytes = b"NEW_BINARY_V2";
    let bad_hex = "0".repeat(64);
    assert!(verify_sha256(new_bytes, &bad_hex).is_err());
    assert_eq!(fs::read(&target).expect("v1 intact"), b"OLD_BINARY_V1");

    // Verify matching SHA-256 passes and rename dance moves v1 -> .old and stages v2 -> target.
    let good_hex = sha256_hex(new_bytes);
    let sums_body = format!("{good_hex}  hrn-release.zip\n");
    let parsed_hex = parse_sha256_for_asset(&sums_body, "hrn-release.zip").expect("parse sums");
    verify_sha256(new_bytes, &parsed_hex).expect("sha256 match");

    replace_binary_atomic(&target, new_bytes).expect("atomic replace");
    assert_eq!(fs::read(&target).expect("read v2"), b"NEW_BINARY_V2");
    let old = old_backup_path(&target);
    assert_eq!(fs::read(&old).expect("read .old"), b"OLD_BINARY_V1");

    // Startup cleanup deletes stale .old and sibling hrn-ast.old safely.
    let ast_old = old_backup_path(&dir.join(hrn_ast_exe_name()));
    fs::write(&ast_old, b"OLD_AST").expect("write ast old");
    cleanup_stale_old_binaries_for(&target);
    assert!(!old.exists(), "stale .old should be cleaned on startup");
    assert!(
        !ast_old.exists(),
        "stale sibling .old should be cleaned on startup"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn update_rejects_non_https_and_untrusted_hosts() {
    for bad in [
        "http://api.github.com/repos/lunateque/harness/releases/latest",
        "https://evil.example.com/hrn.zip",
        "https://api.github.com@evil.example.com/hrn.zip",
        "https://github.com.evil.com/hrn.zip",
        "https://github.com:8443/lunateque/harness/releases/download/v1/hrn.zip",
        "file:///etc/passwd",
        "https://github.com/foo\r\nbar",
    ] {
        assert!(
            validate_update_url(bad).is_err(),
            "expected URL `{bad}` to be rejected"
        );
    }
    for ok in [
        "https://api.github.com/repos/lunateque/harness/releases/latest",
        "https://github.com/lunateque/harness/releases/download/v1.0.0/hrn.zip",
        "https://objects.githubusercontent.com/github-production-release-asset-123/abc",
        "https://release-assets.githubusercontent.com/foo/bar",
    ] {
        assert!(
            validate_update_url(ok).is_ok(),
            "expected valid GitHub URL `{ok}`"
        );
    }
}

#[test]
fn update_rejects_invalid_repo_slugs_and_missing_checksums() {
    for bad in [
        "",
        "noslash",
        "a/b/c",
        "../evil",
        "owner/..",
        "owner/repo?foo=bar",
        "owner/repo#frag",
        "https://evil.com/repo",
    ] {
        assert!(
            validate_repo_slug(bad).is_err(),
            "expected repo slug `{bad}` to be rejected"
        );
    }
    assert!(validate_repo_slug("lunateque/harness").is_ok());

    let no_sum_rel = ReleaseInfo {
        tag_name: "v2.0.0".to_owned(),
        prerelease: false,
        draft: false,
        html_url: String::new(),
        assets: vec![ReleaseAsset {
            name: "hrn-v2.0.0-x86_64-pc-windows-msvc.zip".to_owned(),
            browser_download_url:
                "https://github.com/lunateque/harness/releases/download/v2.0.0/a.zip".to_owned(),
            size: 100,
        }],
    };
    let err = select_release_assets(&no_sum_rel, "windows", "x86_64")
        .expect_err("must refuse release without SHA-256 asset");
    assert!(err.to_string().contains("SHA-256"), "{err}");
}

fn build_tar_gz(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    for (name, typeflag, data) in entries {
        let mut hdr = [0u8; 512];
        let nb = name.as_bytes();
        hdr[..nb.len()].copy_from_slice(nb);
        let size_oct = format!("{:011o}\0", data.len());
        hdr[124..136].copy_from_slice(size_oct.as_bytes());
        hdr[156] = *typeflag;
        enc.write_all(&hdr).expect("write tar hdr");
        enc.write_all(data).expect("write tar data");
        let pad = (512 - (data.len() % 512)) % 512;
        if pad > 0 {
            enc.write_all(&vec![0u8; pad]).expect("pad");
        }
    }
    enc.write_all(&[0u8; 1024]).expect("eof");
    enc.finish().expect("finish gz")
}

#[test]
fn update_archive_rejects_zip_slip_and_symlinks() {
    let traversal = build_tar_gz(&[("../evil.exe", b'0', b"pwn")]);
    assert!(
        extract_release_binaries("hrn.tar.gz", &traversal, MAX_ASSET_BYTES).is_err(),
        "must reject .. traversal"
    );

    let abs_path = build_tar_gz(&[("/usr/bin/hrn", b'0', b"pwn")]);
    assert!(
        extract_release_binaries("hrn.tar.gz", &abs_path, MAX_ASSET_BYTES).is_err(),
        "must reject absolute path"
    );

    let symlink = build_tar_gz(&[(hrn_exe_name(), b'2', b"")]);
    assert!(
        extract_release_binaries("hrn.tar.gz", &symlink, MAX_ASSET_BYTES).is_err(),
        "must reject symlink entry"
    );

    // Valid tar.gz with hrn + hrn-ast succeeds.
    let valid = build_tar_gz(&[
        (&format!("bundle/{}", hrn_exe_name()), b'0', b"HRN_BIN"),
        (&format!("bundle/{}", hrn_ast_exe_name()), b'0', b"AST_BIN"),
        ("bundle/README.md", b'0', b"ignored"),
    ]);
    let extracted =
        extract_release_binaries("hrn.tar.gz", &valid, MAX_ASSET_BYTES).expect("valid tar.gz");
    assert_eq!(extracted.hrn, b"HRN_BIN");
    assert_eq!(extracted.hrn_ast.as_deref(), Some(b"AST_BIN".as_slice()));
}

#[test]
fn update_archive_rejects_tar_overflow_and_oversized_entries() {
    // Base-256 size field set to u64::MAX must be rejected cleanly (no overflow panic).
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    let mut hdr = [0u8; 512];
    let name = b"README.md";
    hdr[..name.len()].copy_from_slice(name);
    // Base-256 positive value = u64::MAX (0x80, 0, 0, 0, 0xff..0xff)
    hdr[124] = 0x80;
    hdr[128..136].fill(0xff);
    hdr[156] = b'0';
    enc.write_all(&hdr).expect("write hdr");
    enc.write_all(&[0u8; 1024]).expect("write eof");
    let overflow_gz = enc.finish().expect("finish gz");
    let err = extract_release_binaries("hrn.tar.gz", &overflow_gz, MAX_ASSET_BYTES)
        .expect_err("must reject u64::MAX tar size");
    assert!(
        err.to_string().contains("maximum size") || err.to_string().contains("overflow"),
        "{err}"
    );

    // Non-target entry larger than max_bytes must also be rejected before skipping.
    let oversized = build_tar_gz(&[("ignored.bin", b'0', &[b'x'; 2048])]);
    let err = extract_release_binaries("hrn.tar.gz", &oversized, 1024)
        .expect_err("must reject oversized non-target tar entry");
    assert!(err.to_string().contains("maximum size"), "{err}");
}
