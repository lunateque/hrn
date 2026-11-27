//! Kimi Code subscription wire constants, persisted 128-bit device ID (`<auth_dir>/kimi-device-id`),
//! and `KimiCLI` fingerprint headers shared across OAuth device login, token refresh, LLM transport,
//! and `/coding/v1/usages`.

use crate::device_code::DeviceCodeConfig;
use crate::lock::FileLock;
use crate::manager::write_bytes_atomic;
use crate::pkce::random_bytes;
use crate::sha256::to_hex;
use hrn_edge_http::guard;
use hrn_kernel::{HrnError, Result};
use std::path::Path;

/// Official Kimi Code RFC 8628 device authorization endpoint.
pub const KIMI_DEVICE_AUTH_URL: &str = "https://auth.kimi.com/api/oauth/device_authorization";
/// Official Kimi Code OAuth token & refresh endpoint.
pub const KIMI_TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";
/// Public `KimiCLI` OAuth client identifier.
pub const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
/// `KimiCLI` version advertised in fingerprint headers.
pub const KIMI_CLI_VERSION: &str = "1.22.0";
/// File name of the persisted 128-bit device id inside `auth_dir`.
pub const KIMI_DEVICE_ID_FILE: &str = "kimi-device-id";

/// Load the persisted 128-bit Kimi device id from `<auth_dir>/kimi-device-id`, creating it
/// atomically with mode `0600` under a cross-process file lock when absent. Validates the path
/// against `guard::check_token_path` before any filesystem or lock access.
pub async fn load_or_create_kimi_device_id(auth_dir: &Path) -> Result<String> {
    let path = auth_dir.join(KIMI_DEVICE_ID_FILE);
    guard::check_token_path(&path.to_string_lossy())?;
    let _lock = FileLock::acquire(&path).await?;
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if is_valid_device_id(trimmed) {
                return Ok(trimmed.to_owned());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(HrnError::Io(format!(
                "read Kimi device id `{}`: {err}",
                path.display()
            )));
        }
    }
    let id = to_hex(&random_bytes(16)?);
    write_bytes_atomic(&path, format!("{id}\n").as_bytes())?;
    Ok(id)
}

fn is_valid_device_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Build the `KimiCLI` fingerprint headers required by Kimi Code auth, chat, and usage endpoints.
#[must_use]
pub fn kimi_fingerprint_headers(device_id: &str) -> Vec<(String, String)> {
    let host = device_name();
    let model = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    let os_ver = format!("{} {}", std::env::consts::OS, std::env::consts::FAMILY);
    vec![
        (
            "User-Agent".to_owned(),
            format!("KimiCLI/{KIMI_CLI_VERSION}"),
        ),
        ("X-Msh-Platform".to_owned(), "kimi_cli".to_owned()),
        ("X-Msh-Version".to_owned(), KIMI_CLI_VERSION.to_owned()),
        ("X-Msh-Device-Name".to_owned(), host),
        ("X-Msh-Device-Model".to_owned(), model),
        ("X-Msh-Os-Version".to_owned(), os_ver),
        ("X-Msh-Device-Id".to_owned(), device_id.to_owned()),
    ]
}

fn device_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "hrn-host".to_owned());
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "hrn-host".to_owned()
    } else {
        cleaned
    }
}

/// Default RFC 8628 [`DeviceCodeConfig`] for Kimi Code using the official `auth.kimi.com` endpoints.
#[must_use]
pub fn kimi_device_code_config(device_id: &str) -> DeviceCodeConfig {
    kimi_device_code_config_with_urls(device_id, KIMI_DEVICE_AUTH_URL, KIMI_TOKEN_URL)
}

/// RFC 8628 [`DeviceCodeConfig`] for Kimi Code with explicit endpoints (for local stub tests).
#[must_use]
pub fn kimi_device_code_config_with_urls(
    device_id: &str,
    device_auth_url: &str,
    token_url: &str,
) -> DeviceCodeConfig {
    DeviceCodeConfig {
        device_auth_url: device_auth_url.to_owned(),
        token_url: token_url.to_owned(),
        client_id: KIMI_CLIENT_ID.to_owned(),
        scope: None,
        extra_params: Vec::new(),
        extra_headers: kimi_fingerprint_headers(device_id),
        json_body: false,
        poll_time_unit: None,
        max_wait: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn device_id_persists_across_calls_and_blocks_consumer_cli_paths() {
        let dir = std::env::temp_dir().join(format!("hrn-kimi-dev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (r1, r2) = tokio::join!(
            load_or_create_kimi_device_id(&dir),
            load_or_create_kimi_device_id(&dir)
        );
        let id1 = r1.expect("create 1");
        let id2 = r2.expect("create 2");
        let id3 = load_or_create_kimi_device_id(&dir).await.expect("reload");
        assert_eq!(
            id1, id2,
            "concurrent first-run calls must agree on one device id"
        );
        assert_eq!(id1, id3);
        assert_eq!(id1.len(), 32);
        assert!(id1.bytes().all(|b| b.is_ascii_hexdigit()));

        let hdrs = kimi_fingerprint_headers(&id1);
        let has = |k: &str| hdrs.iter().any(|(hk, _)| hk == k);
        for key in [
            "User-Agent",
            "X-Msh-Platform",
            "X-Msh-Version",
            "X-Msh-Device-Name",
            "X-Msh-Device-Model",
            "X-Msh-Os-Version",
            "X-Msh-Device-Id",
        ] {
            assert!(has(key), "missing {key}");
        }
        assert!(
            hdrs.iter()
                .any(|(k, v)| k == "User-Agent" && v.starts_with("KimiCLI/"))
        );

        let blocked = dir.join(".claude");
        assert!(load_or_create_kimi_device_id(&blocked).await.is_err());
        assert!(
            !blocked.exists(),
            "guard must block before touching fs or creating lock"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
