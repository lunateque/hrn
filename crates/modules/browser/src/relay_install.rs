//! `hrn browser-relay install [--uninstall]`: extracts the unpacked MV3 extension, writes the
//! Chrome `NativeMessagingHosts` manifest + launcher wrapper, and registers/removes the Windows
//! HKCU registry key via `reg.exe` (zero `unsafe`).

use hrn_edge_ipc::{base64_decode, sha256};
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{HrnError, Result};
use std::path::{Path, PathBuf};

/// Embedded MV3 extension `manifest.json`.
pub(crate) const EXTENSION_MANIFEST_JSON: &str = include_str!("../assets/extension/manifest.json");
/// Embedded MV3 extension `background.js`.
pub(crate) const EXTENSION_BACKGROUND_JS: &str = include_str!("../assets/extension/background.js");
/// Chrome Native Messaging host identifier.
pub const NATIVE_HOST_NAME: &str = "hrn.relay";
/// Default Windows HKCU registry key where Chrome reads `NativeMessagingHosts` manifests.
pub const DEFAULT_HKCU_KEY: &str = r"HKCU\Software\Google\Chrome\NativeMessagingHosts\hrn.relay";

/// Result of installing the browser relay extension and native host manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallReport {
    /// Directory containing the unpacked MV3 extension to load in `chrome://extensions`.
    pub extension_dir: PathBuf,
    /// Path to the written `hrn.relay.json` `NativeMessagingHosts` manifest.
    pub manifest_path: PathBuf,
    /// Path to the wrapper script Chrome launches for `hrn browser-relay serve --stdio`.
    pub wrapper_path: PathBuf,
    /// Deterministic Chrome extension ID derived from the manifest's pinned `"key"`.
    pub extension_id: String,
    /// Registry key (Windows) or target location registered for Chrome discovery.
    pub registry_key: String,
}

/// Derive Chrome's deterministic 32-character (`a`..`p`) extension ID from a base64 SPKI `"key"`.
///
/// # Errors
///
/// When `spki_b64` is not valid base64.
pub fn derive_extension_id(spki_b64: &str) -> Result<String> {
    let der = base64_decode(spki_b64)?;
    let digest = sha256(&der);
    let mut id = String::with_capacity(32);
    for &byte in digest.get(..16).unwrap_or_default() {
        let hi = b'a' + (byte >> 4);
        let lo = b'a' + (byte & 0x0f);
        id.push(char::from(hi));
        id.push(char::from(lo));
    }
    Ok(id)
}

/// Read the pinned `"key"` out of the bundled `manifest.json` and compute its Chrome extension ID.
///
/// # Errors
///
/// When the embedded manifest is missing `"key"` or cannot be decoded.
pub fn bundled_extension_id() -> Result<String> {
    let val: Value = serde_json::from_str(EXTENSION_MANIFEST_JSON)
        .map_err(|e| HrnError::Internal(format!("invalid bundled extension manifest: {e}")))?;
    let key = val
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| HrnError::Internal("bundled extension manifest missing `key`".to_owned()))?;
    derive_extension_id(key)
}

/// Install the unpacked Chrome extension, native messaging host wrapper + manifest, and register
/// `registry_key` (on Windows via `reg.exe`).
///
/// # Errors
///
/// When writing files or invoking `reg.exe` fails.
pub fn install_relay(
    data_dir: &Path,
    exe_path: &Path,
    registry_key: &str,
) -> Result<InstallReport> {
    let extension_id = bundled_extension_id()?;
    let root = data_dir.join("browser-relay");
    let extension_dir = root.join("extension");
    std::fs::create_dir_all(&extension_dir).map_err(|e| {
        HrnError::Io(format!(
            "create extension dir `{}`: {e}",
            extension_dir.display()
        ))
    })?;

    std::fs::write(extension_dir.join("manifest.json"), EXTENSION_MANIFEST_JSON)?;
    std::fs::write(extension_dir.join("background.js"), EXTENSION_BACKGROUND_JS)?;

    let wrapper_path = write_host_wrapper(&root, exe_path)?;
    let manifest_path = root.join(format!("{NATIVE_HOST_NAME}.json"));
    let manifest = json!({
        "name": NATIVE_HOST_NAME,
        "description": "hrn native browser relay host",
        "path": wrapper_path.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{extension_id}/")]
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| HrnError::Internal(format!("encode native host manifest: {e}")))?;
    std::fs::write(&manifest_path, manifest_bytes)?;

    register_native_host(registry_key, &manifest_path)?;

    Ok(InstallReport {
        extension_dir,
        manifest_path,
        wrapper_path,
        extension_id,
        registry_key: registry_key.to_owned(),
    })
}

/// Remove the native host registry entry and extracted relay files under `data_dir`.
///
/// # Errors
///
/// When removing files or unregistering the host fails unexpectedly.
pub fn uninstall_relay(data_dir: &Path, registry_key: &str) -> Result<()> {
    unregister_native_host(registry_key)?;
    let root = data_dir.join("browser-relay");
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(|e| {
            HrnError::Io(format!(
                "remove browser-relay dir `{}`: {e}",
                root.display()
            ))
        })?;
    }
    let state_file = data_dir.join("browser-relay.json");
    if state_file.exists() {
        let _ = std::fs::remove_file(&state_file);
    }
    Ok(())
}

fn write_host_wrapper(root: &Path, exe_path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let wrapper = root.join("hrn-relay-host.cmd");
        let exe = exe_path.to_string_lossy();
        let script = format!("@echo off\r\n\"{exe}\" browser-relay serve --stdio\r\n");
        std::fs::write(&wrapper, script)?;
        Ok(wrapper)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let wrapper = root.join("hrn-relay-host.sh");
        let exe = exe_path.to_string_lossy().replace('\'', "'\\''");
        let script = format!("#!/bin/sh\nexec '{exe}' browser-relay serve --stdio\n");
        std::fs::write(&wrapper, script)?;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
        Ok(wrapper)
    }
}

#[cfg(windows)]
fn register_native_host(registry_key: &str, manifest_path: &Path) -> Result<()> {
    let manifest_str = manifest_path.to_string_lossy().into_owned();
    let out = std::process::Command::new("reg.exe")
        .args([
            "add",
            registry_key,
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            &manifest_str,
            "/f",
        ])
        .output()
        .map_err(|e| HrnError::Io(format!("failed to execute reg.exe: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(HrnError::Tool(format!(
            "reg.exe add `{registry_key}` failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn register_native_host(registry_key: &str, manifest_path: &Path) -> Result<()> {
    let target = Path::new(registry_key);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(manifest_path, target)?;
    Ok(())
}

#[cfg(windows)]
fn unregister_native_host(registry_key: &str) -> Result<()> {
    let out = std::process::Command::new("reg.exe")
        .args(["delete", registry_key, "/f"])
        .output()
        .map_err(|e| HrnError::Io(format!("failed to execute reg.exe: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lower = stderr.to_ascii_lowercase();
        // Missing key on uninstall is idempotent.
        if !lower.contains("unable to find") && !lower.contains("cannot find") {
            return Err(HrnError::Tool(format!(
                "reg.exe delete `{registry_key}` failed: {}",
                stderr.trim()
            )));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn unregister_native_host(registry_key: &str) -> Result<()> {
    let target = Path::new(registry_key);
    if target.exists() {
        std::fs::remove_file(target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_id_matches_manifest_key() {
        let id = bundled_extension_id().expect("bundled id");
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| (b'a'..=b'p').contains(&b)));
        assert_eq!(id, "jgdhhmbajenkelgekoielmpnbiipmbna");
    }

    #[cfg(windows)]
    #[test]
    fn windows_registry_install_and_uninstall_roundtrip() {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("hrn-relay-reg-test-{pid}"));
        let _ = std::fs::remove_dir_all(&dir);
        let test_key = format!(r"HKCU\Software\hrn-test\NativeMessagingHosts\hrn.relay.{pid}");
        let fake_exe = dir.join("hrn.exe");

        let report = install_relay(&dir, &fake_exe, &test_key).expect("install");
        assert!(report.extension_dir.join("manifest.json").is_file());
        assert!(report.extension_dir.join("background.js").is_file());
        assert!(report.manifest_path.is_file());
        assert!(report.wrapper_path.is_file());

        let manifest_raw = std::fs::read_to_string(&report.manifest_path).expect("read manifest");
        let manifest_val: Value = serde_json::from_str(&manifest_raw).expect("parse manifest");
        assert_eq!(
            manifest_val.get("name").and_then(Value::as_str),
            Some(NATIVE_HOST_NAME)
        );
        let expected_origin = format!("chrome-extension://{}/", report.extension_id);
        assert_eq!(
            manifest_val
                .pointer("/allowed_origins/0")
                .and_then(Value::as_str),
            Some(expected_origin.as_str())
        );

        let query = std::process::Command::new("reg.exe")
            .args(["query", &test_key, "/ve"])
            .output()
            .expect("reg query");
        assert!(query.status.success(), "reg query failed");
        let stdout = String::from_utf8_lossy(&query.stdout);
        assert!(
            stdout.contains(&report.manifest_path.to_string_lossy().to_string()),
            "{stdout}"
        );

        uninstall_relay(&dir, &test_key).expect("uninstall");
        assert!(!report.manifest_path.exists());
        let after = std::process::Command::new("reg.exe")
            .args(["query", &test_key, "/ve"])
            .output()
            .expect("reg query after");
        assert!(!after.status.success(), "registry key should be deleted");

        // Clean up the test parent keys best-effort.
        let _ = std::process::Command::new("reg.exe")
            .args(["delete", r"HKCU\Software\hrn-test", "/f"])
            .output();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
