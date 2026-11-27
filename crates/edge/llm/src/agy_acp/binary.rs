//! `agy` CLI binary discovery, version verification (`>= 1.5.0`), and loopback `NO_PROXY` env helper.

use hrn_edge_proc::{RunSpec, resolve_on_path, run_streaming};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Minimum supported `agy` CLI release (`1.5.0`, verified on `1.5.18`).
pub const MIN_AGY_VERSION: (u64, u64, u64) = (1, 5, 0);
/// Default timeout for `agy --version` probes.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Discover the `agy` CLI executable.
///
/// Resolution order:
/// 1. Explicit `binary` path/name from provider config.
/// 2. `HRN_AGY_BIN` environment override.
/// 3. `PATH` lookup (`agy.exe` / `agy`).
/// 4. Well-known install locations (`%LOCALAPPDATA%\agy\bin\agy.exe` on Windows;
///    `~/.local/bin/agy`, `~/.agy/bin/agy`, `/opt/homebrew/bin/agy`, `/usr/local/bin/agy` on Unix).
pub fn discover_agy_binary(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(raw) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(found) = resolve_explicit_or_path(raw) {
            return Some(found);
        }
        return None;
    }

    if let Ok(env_bin) = std::env::var("HRN_AGY_BIN") {
        let trimmed = env_bin.trim();
        if !trimmed.is_empty()
            && let Some(found) = resolve_explicit_or_path(trimmed)
        {
            return Some(found);
        }
    }

    if let Some(on_path) = resolve_on_path("agy") {
        return Some(on_path);
    }

    platform_fallback_paths()
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn resolve_explicit_or_path(raw: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(raw);
    if direct.is_file() {
        return Some(direct);
    }
    if !raw.contains(['/', '\\']) {
        return resolve_on_path(raw);
    }
    None
}

fn platform_fallback_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(windows)]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA")
            && !local_app_data.trim().is_empty()
        {
            paths.push(
                PathBuf::from(local_app_data)
                    .join("agy")
                    .join("bin")
                    .join("agy.exe"),
            );
        }
        if let Ok(profile) = std::env::var("USERPROFILE")
            && !profile.trim().is_empty()
        {
            let home = PathBuf::from(profile);
            paths.push(home.join(".agy").join("bin").join("agy.exe"));
            paths.push(home.join(".local").join("bin").join("agy.exe"));
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(home_str) = std::env::var("HOME")
            && !home_str.trim().is_empty()
        {
            let home = PathBuf::from(home_str);
            paths.push(home.join(".local").join("bin").join("agy"));
            paths.push(home.join(".agy").join("bin").join("agy"));
        }
        paths.push(PathBuf::from("/opt/homebrew/bin/agy"));
        paths.push(PathBuf::from("/usr/local/bin/agy"));
    }
    paths
}

/// Probe `agy --version` and return the trimmed version line.
pub async fn probe_agy_version(binary: &Path) -> Result<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let spec = RunSpec {
        program: binary.to_string_lossy().into_owned(),
        args: vec!["--version".to_owned()],
        cwd,
        env: BTreeMap::new(),
        timeout: VERSION_PROBE_TIMEOUT,
        max_output_bytes: 8 * 1024,
    };
    let out = run_streaming(&spec, &CancelFlag::new(), |_| {}).await?;
    if out.timed_out {
        return Err(HrnError::Io("`agy --version` timed out".to_owned()));
    }
    if out.exit_code != Some(0) {
        let detail = out.stderr.trim();
        return Err(HrnError::Io(format!(
            "`agy --version` failed (exit {:?}): {}",
            out.exit_code,
            if detail.is_empty() {
                out.stdout.trim()
            } else {
                detail
            }
        )));
    }
    let text = out.stdout.trim();
    if text.is_empty() {
        return Err(HrnError::Io(
            "`agy --version` returned empty output".to_owned(),
        ));
    }
    Ok(text.to_owned())
}

/// Extract a `(major, minor, patch)` triple from a version string such as `"1.5.18"` or `"agy 1.5.18"`.
pub fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    for token in raw.split(|c: char| c.is_whitespace() || matches!(c, 'v' | 'V' | '(' | ')' | ','))
    {
        let core = token.split(['-', '+']).next().unwrap_or("");
        let mut parts = core.split('.');
        let (Some(a), Some(b), Some(c)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if parts.next().is_some() {
            continue;
        }
        if let (Ok(maj), Ok(min), Ok(pat)) = (a.parse::<u64>(), b.parse::<u64>(), c.parse::<u64>())
        {
            return Some((maj, min, pat));
        }
    }
    None
}

/// Whether `raw` represents an `agy` version meeting [`MIN_AGY_VERSION`] (`>= 1.5.0`).
pub fn is_supported_version(raw: &str) -> bool {
    parse_semver(raw).is_some_and(|v| v >= MIN_AGY_VERSION)
}

/// Merge `localhost,127.0.0.1,::1` into both `NO_PROXY` and `no_proxy` so corporate/system proxies
/// never intercept loopback MCP bridge calls.
pub fn merged_no_proxy_env() -> BTreeMap<String, String> {
    merged_no_proxy_from(
        std::env::var("NO_PROXY").ok().as_deref(),
        std::env::var("no_proxy").ok().as_deref(),
    )
}

/// Deterministic helper for testing `NO_PROXY` merging without mutating process env.
pub fn merged_no_proxy_from(
    existing_upper: Option<&str>,
    existing_lower: Option<&str>,
) -> BTreeMap<String, String> {
    let mut items: Vec<String> = Vec::new();
    for raw in existing_upper
        .into_iter()
        .chain(existing_lower)
        .flat_map(|v| v.split(','))
    {
        let trimmed = raw.trim();
        if !trimmed.is_empty() && !items.iter().any(|e| e.eq_ignore_ascii_case(trimmed)) {
            items.push(trimmed.to_owned());
        }
    }
    for required in ["localhost", "127.0.0.1", "::1"] {
        if !items.iter().any(|e| e.eq_ignore_ascii_case(required)) {
            items.push(required.to_owned());
        }
    }
    let joined = items.join(",");
    BTreeMap::from([
        ("NO_PROXY".to_owned(), joined.clone()),
        ("no_proxy".to_owned(), joined),
    ])
}
