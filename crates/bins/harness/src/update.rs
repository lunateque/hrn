//! `hrn update [--check] [--canary|--stable]`: verified self-updater for `hrn` and sibling
//! `hrn-ast` binaries.
//!
//! Security invariants:
//! - `[update]` configuration is loaded strictly from operator layers (`project_path = None`),
//!   so an untrusted repository `.harness/config.toml` can NEVER redirect binary downloads.
//! - `[update] repo` is strictly validated (`^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$`, no `..`).
//! - All network requests require `https://` and a strict GitHub host allowlist (`api.github.com`,
//!   `github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`),
//!   re-checked on every redirect hop (max 5 redirects).
//! - SHA-256 verification against the release checksum asset is mandatory (`hrn_edge_ipc::sha256_hex`
//!   + `ct_eq`) BEFORE extracting or touching any installed binary.
//! - Archive extraction rejects path traversal (`..`), absolute paths, drive prefixes, and symlinks.
//! - Binary replacement stages atomically in the target binary's directory and uses a rollback-safe
//!   `.old` rename dance (safe on Windows locked executables), paired with symlink-safe startup
//!   cleanup of stale `*.old` files.

#[path = "update_install.rs"]
mod install;

pub(crate) use install::{cleanup_stale_old_binaries, replace_binary_atomic};
#[cfg(test)]
pub(crate) use install::{cleanup_stale_old_binaries_for, old_backup_path};

use crate::cli::UpdateArgs;
use crate::config::UpdateConfig;
use crate::config_layers::{LayerInput, load_layers};
use crate::term::{out, sanitize_cli_inline};
use crate::update_archive::{extract_release_binaries, hrn_ast_exe_name, hrn_exe_name};
use hrn_edge_http::HttpClient;
use hrn_edge_ipc::{ct_eq, sha256_hex};
use hrn_kernel::{HrnError, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Maximum response size for GitHub Release JSON and SHA-256 checksum files (1 MiB).
pub(crate) const MAX_META_BYTES: usize = 1024 * 1024;
/// Maximum response and extraction size for a release binary or archive (128 MiB).
pub(crate) const MAX_ASSET_BYTES: usize = 128 * 1024 * 1024;
/// Maximum HTTP redirect hops allowed while downloading release metadata or assets.
const MAX_REDIRECTS: usize = 5;
/// Allowed GitHub hostnames for production update requests (HTTPS only).
const ALLOWED_UPDATE_HOSTS: &[&str] = &[
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Release channel for `hrn update`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpdateChannel {
    Stable,
    Canary,
}

impl UpdateChannel {
    pub(crate) fn from_config(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "stable" | "" => Ok(Self::Stable),
            "canary" => Ok(Self::Canary),
            other => Err(HrnError::Config(format!(
                "invalid [update] channel `{other}` (expected `stable` or `canary`)"
            ))),
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Canary => "canary",
        }
    }
}

/// One downloadable asset inside a GitHub release.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct ReleaseAsset {
    pub(crate) name: String,
    pub(crate) browser_download_url: String,
    #[serde(default)]
    pub(crate) size: u64,
}

/// Parsed release metadata returned by the GitHub Releases API.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct ReleaseInfo {
    pub(crate) tag_name: String,
    #[serde(default)]
    pub(crate) prerelease: bool,
    #[serde(default)]
    pub(crate) draft: bool,
    #[serde(default)]
    pub(crate) html_url: String,
    #[serde(default)]
    pub(crate) assets: Vec<ReleaseAsset>,
}

/// Load `[update]` strictly from operator layers (`Default -> Global -> Overlays -> Env`),
/// ignoring untrusted project `.harness/config.toml`.
pub(crate) fn load_update_config(
    global_path: Option<PathBuf>,
    overlays: &[PathBuf],
    cwd: &Path,
    home: Option<PathBuf>,
    env: BTreeMap<String, String>,
) -> Result<UpdateConfig> {
    if let Some(ref gp) = global_path {
        match fs::symlink_metadata(gp) {
            Ok(_) if !gp.is_file() => {
                return Err(HrnError::Config(format!(
                    "global config `{}` is not a readable file",
                    gp.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(HrnError::Config(format!(
                    "cannot stat global config `{}`: {e}",
                    gp.display()
                )));
            }
        }
    }
    let input = LayerInput {
        global_path,
        project_path: None,
        overlays: overlays.to_vec(),
        cwd: cwd.to_path_buf(),
        home,
        env,
        runtime: toml::Table::new(),
    };
    let merged = load_layers(&input)?;
    let cfg = match merged.root.get("update") {
        Some(val) => val
            .clone()
            .try_into::<UpdateConfig>()
            .map_err(|e| HrnError::Config(format!("invalid [update] configuration: {e}")))?,
        None => UpdateConfig::default(),
    };
    validate_repo_slug(&cfg.repo)?;
    let _ = UpdateChannel::from_config(&cfg.channel)?;
    Ok(cfg)
}

/// Strictly validate `[update] repo` (`owner/name`, safe ASCII subset, no `..` or URL injection).
pub(crate) fn validate_repo_slug(repo: &str) -> Result {
    let mut parts = repo.split('/');
    let Some(owner) = parts.next() else {
        return Err(HrnError::Config("update.repo must not be empty".to_owned()));
    };
    let Some(name) = parts.next() else {
        return Err(HrnError::Config(format!(
            "invalid update.repo `{repo}`: expected `owner/name`"
        )));
    };
    if parts.next().is_some() {
        return Err(HrnError::Config(format!(
            "invalid update.repo `{repo}`: must contain exactly one `/`"
        )));
    }
    for seg in [owner, name] {
        if seg.is_empty()
            || seg == "."
            || seg == ".."
            || seg.contains("..")
            || !seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(HrnError::Config(format!(
                "invalid update.repo `{repo}`: disallowed characters or traversal segment"
            )));
        }
    }
    Ok(())
}

/// Enforce HTTPS-only + strict GitHub hostname allowlist on every update request and redirect hop.
pub(crate) fn validate_update_url(url: &str) -> Result {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(HrnError::Config(format!(
            "refused non-HTTPS update URL `{url}`"
        )));
    };
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(HrnError::Config(format!(
            "refused malformed update URL `{url}`"
        )));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(HrnError::Config(format!(
            "refused update URL with empty host or userinfo `{url}`"
        )));
    }
    let host = match authority.split_once(':') {
        Some((h, "443")) => h,
        Some(_) => {
            return Err(HrnError::Config(format!(
                "refused update URL with non-443 port `{url}`"
            )));
        }
        None => authority,
    };
    let lower = host.to_ascii_lowercase();
    if !ALLOWED_UPDATE_HOSTS.contains(&lower.as_str()) {
        return Err(HrnError::Config(format!(
            "refused untrusted update host `{host}`"
        )));
    }
    Ok(())
}

/// Parse a GitHub releases API response (`latest` object for stable, or array for canary).
pub(crate) fn parse_release_response(body: &str, channel: UpdateChannel) -> Result<ReleaseInfo> {
    let trimmed = body.trim();
    if trimmed.starts_with('[') {
        let releases: Vec<ReleaseInfo> = serde_json::from_str(trimmed)
            .map_err(|e| HrnError::Tool(format!("invalid GitHub releases list JSON: {e}")))?;
        releases
            .into_iter()
            .find(|r| !r.draft && (channel == UpdateChannel::Canary || !r.prerelease))
            .ok_or_else(|| {
                HrnError::Tool(format!("no published `{}` release found", channel.label()))
            })
    } else {
        let release: ReleaseInfo = serde_json::from_str(trimmed)
            .map_err(|e| HrnError::Tool(format!("invalid GitHub release JSON: {e}")))?;
        if release.draft {
            return Err(HrnError::Tool("release is still a draft".to_owned()));
        }
        Ok(release)
    }
}

/// Compare two version strings (leading `v` stripped). Returns `true` when `remote` is newer.
pub(crate) fn is_newer_version(current: &str, remote: &str) -> bool {
    let cur = parse_semver_tuple(current);
    let rem = parse_semver_tuple(remote);
    match (cur, rem) {
        (Some((c_maj, c_min, c_pat, c_pre)), Some((r_maj, r_min, r_pat, r_pre))) => {
            if (r_maj, r_min, r_pat) != (c_maj, c_min, c_pat) {
                return (r_maj, r_min, r_pat) > (c_maj, c_min, c_pat);
            }
            match (c_pre.is_empty(), r_pre.is_empty()) {
                (false, true) => true,
                (true, false) => false,
                _ => r_pre > c_pre,
            }
        }
        _ => current.trim_start_matches('v') != remote.trim_start_matches('v'),
    }
}

fn parse_semver_tuple(raw: &str) -> Option<(u64, u64, u64, String)> {
    let cleaned = raw.trim().trim_start_matches('v');
    let (core, pre) = match cleaned.split_once('-') {
        Some((c, p)) => (c, p.split('+').next().unwrap_or(p).to_owned()),
        None => (cleaned.split('+').next().unwrap_or(cleaned), String::new()),
    };
    let mut nums = core.split('.');
    let maj = nums.next()?.parse::<u64>().ok()?;
    let min = nums.next()?.parse::<u64>().ok()?;
    let pat = nums.next()?.parse::<u64>().ok()?;
    Some((maj, min, pat, pre))
}

/// Target platform keywords used to select the matching release asset.
pub(crate) fn current_target_triple_hints() -> (&'static str, &'static str) {
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    (os, arch)
}

/// Select the platform release asset and its mandatory SHA-256 checksum asset.
pub(crate) fn select_release_assets<'a>(
    release: &'a ReleaseInfo,
    os_hint: &str,
    arch_hint: &str,
) -> Result<(&'a ReleaseAsset, &'a ReleaseAsset)> {
    let arch_alts: &[&str] = if arch_hint == "aarch64" {
        &["aarch64", "arm64"]
    } else {
        &["x86_64", "amd64"]
    };
    let os_alts: &[&str] = if os_hint == "darwin" {
        &["darwin", "macos", "apple"]
    } else {
        &[os_hint]
    };

    let binary = release
        .assets
        .iter()
        .find(|a| {
            let n = a.name.to_ascii_lowercase();
            let is_sig = Path::new(&a.name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("sig") || e.eq_ignore_ascii_case("asc"));
            !is_checksum_name(&n)
                && !is_sig
                && os_alts.iter().any(|o| n.contains(o))
                && arch_alts.iter().any(|ar| n.contains(ar))
        })
        .ok_or_else(|| {
            HrnError::Tool(format!(
                "release `{}` has no asset matching `{os_hint}`/`{arch_hint}`",
                release.tag_name
            ))
        })?;

    let exact_sha = format!("{}.sha256", binary.name).to_ascii_lowercase();
    let exact_sha_sum = format!("{}.sha256sum", binary.name).to_ascii_lowercase();
    let checksum = release
        .assets
        .iter()
        .find(|a| {
            let n = a.name.to_ascii_lowercase();
            n == exact_sha || n == exact_sha_sum
        })
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|a| is_sums_manifest(&a.name.to_ascii_lowercase()))
        })
        .ok_or_else(|| {
            HrnError::Tool(format!(
                "refusing update: release `{}` is missing a SHA-256 checksum asset for `{}`",
                release.tag_name, binary.name
            ))
        })?;

    Ok((binary, checksum))
}

fn is_checksum_name(lower: &str) -> bool {
    lower.ends_with(".sha256")
        || lower.ends_with(".sha256sum")
        || lower.ends_with(".sha256.txt")
        || is_sums_manifest(lower)
}

fn is_sums_manifest(lower: &str) -> bool {
    matches!(
        lower,
        "sha256sums" | "sha256sums.txt" | "checksums.txt" | "sha256.txt"
    )
}

/// Extract the exact 64-hex-character SHA-256 digest for `asset_name` from a checksum file.
pub(crate) fn parse_sha256_for_asset(body: &str, asset_name: &str) -> Result<String> {
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if let [single] = lines.as_slice()
        && is_valid_sha256_hex(single)
    {
        return Ok(single.to_ascii_lowercase());
    }
    for line in lines {
        let mut parts = line.split_whitespace();
        let Some(digest) = parts.next() else {
            continue;
        };
        let Some(raw_name) = parts.next() else {
            continue;
        };
        let cleaned_name = raw_name.trim_start_matches('*').trim_start_matches("./");
        if cleaned_name == asset_name && is_valid_sha256_hex(digest) {
            return Ok(digest.to_ascii_lowercase());
        }
    }
    Err(HrnError::Tool(format!(
        "no valid 64-hex SHA-256 entry found for `{asset_name}`"
    )))
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Verify `bytes` against `expected_hex` using constant-time equality (`hrn_edge_ipc`).
pub(crate) fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result {
    let expected = expected_hex.trim().to_ascii_lowercase();
    if !is_valid_sha256_hex(&expected) {
        return Err(HrnError::Tool(
            "invalid expected SHA-256 digest format".to_owned(),
        ));
    }
    let actual = sha256_hex(bytes);
    if !ct_eq(actual.as_bytes(), expected.as_bytes()) {
        return Err(HrnError::Tool(format!(
            "SHA-256 verification failed: expected `{expected}`, got `{actual}`"
        )));
    }
    Ok(())
}

async fn http_get_verified(
    client: &HttpClient,
    initial_url: &str,
    max_bytes: usize,
    test_loopback_prefix: Option<&str>,
) -> Result<Vec<u8>> {
    let mut url = initial_url.to_owned();
    let headers = vec![
        (
            "User-Agent".to_owned(),
            format!("hrn/{}", env!("CARGO_PKG_VERSION")),
        ),
        (
            "Accept".to_owned(),
            "application/vnd.github+json".to_owned(),
        ),
    ];
    for hop in 0..=MAX_REDIRECTS {
        validate_hop_url(&url, test_loopback_prefix)?;
        let resp = client
            .get(&url, &headers, Duration::from_secs(30), max_bytes)
            .await?;
        let code = resp.status.as_u16();
        if matches!(code, 301 | 302 | 303 | 307 | 308) {
            if hop == MAX_REDIRECTS {
                return Err(HrnError::Tool(
                    "too many redirects during update".to_owned(),
                ));
            }
            let location = resp
                .headers
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| HrnError::Tool("redirect missing Location header".to_owned()))?;
            location.clone_into(&mut url);
            continue;
        }
        if !resp.status.is_success() {
            return Err(resp.status_error());
        }
        if resp.truncated {
            return Err(HrnError::Tool(format!(
                "update download exceeded maximum size ({max_bytes} bytes)"
            )));
        }
        return Ok(resp.body.to_vec());
    }
    Err(HrnError::Tool(
        "too many redirects during update".to_owned(),
    ))
}

fn validate_hop_url(url: &str, test_loopback_prefix: Option<&str>) -> Result {
    #[cfg(test)]
    if let Some(prefix) = test_loopback_prefix
        && url.starts_with(prefix)
    {
        return Ok(());
    }
    let _ = test_loopback_prefix;
    validate_update_url(url)
}

/// Entry point for `hrn update [--check] [--canary|--stable]`.
pub(crate) async fn run_cli(cfg: &UpdateConfig, args: &UpdateArgs) -> Result<ExitCode> {
    validate_repo_slug(&cfg.repo)?;
    let channel = if args.canary {
        UpdateChannel::Canary
    } else if args.stable {
        UpdateChannel::Stable
    } else {
        UpdateChannel::from_config(&cfg.channel)?
    };
    let current = env!("CARGO_PKG_VERSION");
    let client = HttpClient::new()?;
    let api_url = match channel {
        UpdateChannel::Stable => {
            format!("https://api.github.com/repos/{}/releases/latest", cfg.repo)
        }
        UpdateChannel::Canary => format!(
            "https://api.github.com/repos/{}/releases?per_page=10",
            cfg.repo
        ),
    };
    let body = http_get_verified(&client, &api_url, MAX_META_BYTES, None).await?;
    let body_str = String::from_utf8_lossy(&body);
    let release = parse_release_response(&body_str, channel)?;

    let safe_tag = sanitize_cli_inline(&release.tag_name);
    let newer = is_newer_version(current, &release.tag_name);
    if args.check || !newer {
        if newer {
            out(&format!(
                "Update available ({}): v{current} -> {safe_tag}\nRun `hrn update` to install.\n",
                channel.label(),
            ));
        } else {
            out(&format!(
                "hrn v{current} is up to date ({}, latest: {safe_tag}).\n",
                channel.label(),
            ));
        }
        return Ok(ExitCode::SUCCESS);
    }

    let (os_hint, arch_hint) = current_target_triple_hints();
    let (bin_asset, sum_asset) = select_release_assets(&release, os_hint, arch_hint)?;
    let sum_bytes = http_get_verified(
        &client,
        &sum_asset.browser_download_url,
        MAX_META_BYTES,
        None,
    )
    .await?;
    let expected_sha =
        parse_sha256_for_asset(&String::from_utf8_lossy(&sum_bytes), &bin_asset.name)?;
    let asset_bytes = http_get_verified(
        &client,
        &bin_asset.browser_download_url,
        MAX_ASSET_BYTES,
        None,
    )
    .await?;
    verify_sha256(&asset_bytes, &expected_sha)?;

    let extracted = extract_release_binaries(&bin_asset.name, &asset_bytes, MAX_ASSET_BYTES)?;
    let target_exe = std::env::current_exe()?;
    replace_binary_atomic(&target_exe, &extracted.hrn)?;
    if let Some(ast_bytes) = extracted.hrn_ast
        && let Some(parent) = target_exe.parent()
    {
        let ast_target = parent.join(hrn_ast_exe_name());
        replace_binary_atomic(&ast_target, &ast_bytes)?;
    }

    out(&format!(
        "Updated {} to {safe_tag} (SHA-256 verified).\n",
        hrn_exe_name(),
    ));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
