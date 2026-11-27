//! Reusable CLI helpers for `hrn login agy` and `hrn doctor`.
//!
//! Strictly 100% ToS-compliant: delegates authentication exclusively to the official `agy` binary
//! via ACP `authenticate` and never reads, copies, or stores Google OAuth tokens in `hrn`.

use super::binary::{
    MIN_AGY_VERSION, discover_agy_binary, is_supported_version, merged_no_proxy_env,
    probe_agy_version,
};
use super::client::{AcpClient, AcpSpawnSpec};
use super::config::AgyToolMode;
use hrn_kernel::{HrnError, Result};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Authentication method passed to ACP `authenticate` during `hrn login agy`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgyAuthMethod {
    /// Standard Google account login (`oauth-personal`).
    #[default]
    OauthPersonal,
    /// Gemini Enterprise / workspace account login (`oauth-business`).
    OauthBusiness,
}

impl AgyAuthMethod {
    /// ACP wire `methodId`.
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::OauthPersonal => "oauth-personal",
            Self::OauthBusiness => "oauth-business",
        }
    }

    /// Parse a CLI `--method` value.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "oauth-personal" | "personal" | "google" => Some(Self::OauthPersonal),
            "oauth-business" | "business" | "enterprise" => Some(Self::OauthBusiness),
            _ => None,
        }
    }
}

/// Diagnostic report returned by [`doctor_agy`] for `hrn doctor` / `hrn auth status`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgyDoctorReport {
    /// Discovered `agy` binary path, if any.
    pub binary: Option<PathBuf>,
    /// Raw output of `agy --version`, if available.
    pub version: Option<String>,
    /// Whether `version` satisfies [`MIN_AGY_VERSION`] (`>= 1.5.0`).
    pub version_supported: bool,
    /// Whether ACP `initialize` succeeded.
    pub acp_ready: bool,
    /// Whether an ACP session could be opened (`Some(true)` = authenticated, `Some(false)` = needs login).
    pub authenticated: Option<bool>,
    /// Whether the server advertised native `session/fork`.
    pub supports_fork: bool,
    /// Subscription tier reported by `agy`, if authenticated.
    pub tier_name: Option<String>,
    /// Human-readable warnings or hints.
    pub notes: Vec<String>,
}

impl AgyDoctorReport {
    /// Render a concise multi-line diagnostic block for terminal output.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("agy_acp:\n");
        if let Some(bin) = &self.binary {
            let _ = writeln!(out, "  binary: {}", bin.display());
        } else {
            out.push_str("  binary: NOT FOUND (install Antigravity CLI `agy >= 1.5.0`)\n");
            return out;
        }
        match &self.version {
            Some(ver) if self.version_supported => {
                let _ = writeln!(out, "  version: {ver} (ok)");
            }
            Some(ver) => {
                let _ = writeln!(
                    out,
                    "  version: {ver} (OUTDATED; requires >= {}.{}.{})",
                    MIN_AGY_VERSION.0, MIN_AGY_VERSION.1, MIN_AGY_VERSION.2
                );
            }
            None => out.push_str("  version: unknown\n"),
        }
        let _ = writeln!(
            out,
            "  acp: {} (native_fork={})",
            if self.acp_ready { "ok" } else { "FAILED" },
            self.supports_fork
        );
        let auth_label = match self.authenticated {
            Some(true) => match &self.tier_name {
                Some(tier) => format!("authenticated ({tier})"),
                None => "authenticated".to_owned(),
            },
            Some(false) => "NOT AUTHENTICATED (run `agy` or `hrn login agy`)".to_owned(),
            None => "unknown".to_owned(),
        };
        let _ = writeln!(out, "  auth: {auth_label}");
        for note in &self.notes {
            let _ = writeln!(out, "  note: {note}");
        }
        out
    }
}

/// Trigger official `agy` authentication via ACP `initialize` + `authenticate`.
///
/// Stores zero credentials inside `hrn`; `agy` itself manages its own login flow and token storage.
pub async fn login_agy(
    binary: &Path,
    cwd: &Path,
    method: AgyAuthMethod,
    timeout: Duration,
) -> Result<String> {
    let spec = AcpSpawnSpec {
        binary: binary.to_path_buf(),
        args: vec!["acp".to_owned(), "--stdio".to_owned()],
        cwd: cwd.to_path_buf(),
        env: merged_no_proxy_env(),
    };
    login_from_spawn_spec(spec, method, timeout).await
}

/// Deterministic helper accepting a custom [`AcpSpawnSpec`] (for mock-ACP tests).
pub async fn login_from_spawn_spec(
    spec: AcpSpawnSpec,
    method: AgyAuthMethod,
    timeout: Duration,
) -> Result<String> {
    let client = AcpClient::spawn_and_initialize(spec, AgyToolMode::Native, timeout).await?;
    let method_id = method.as_wire_str();
    let advertised = client
        .capabilities()
        .auth_methods
        .iter()
        .any(|m| m.id == method_id);
    if !advertised && !client.capabilities().auth_methods.is_empty() {
        return Err(HrnError::Auth(format!(
            "`agy` does not advertise auth method `{method_id}`; run `agy` interactively to log in"
        )));
    }
    let _ = client.authenticate(method_id, timeout).await?;
    Ok(format!(
        "Authenticated via Antigravity CLI (`{method_id}`). No tokens were stored in hrn."
    ))
}

/// Probe `agy` binary discovery, version (`>= 1.5.0`), ACP handshake, and auth status.
pub async fn doctor_agy(
    explicit_binary: Option<&str>,
    cwd: &Path,
    timeout: Duration,
) -> AgyDoctorReport {
    let mut report = AgyDoctorReport::default();
    let Some(binary) = discover_agy_binary(explicit_binary) else {
        report
            .notes
            .push("`agy` binary not found on PATH or standard install locations".to_owned());
        return report;
    };
    report.binary = Some(binary.clone());

    match probe_agy_version(&binary).await {
        Ok(ver) => {
            report.version_supported = is_supported_version(&ver);
            report.version = Some(ver);
        }
        Err(err) => {
            report.notes.push(format!("version probe failed: {err}"));
        }
    }

    let spec = AcpSpawnSpec {
        binary,
        args: vec!["acp".to_owned(), "--stdio".to_owned()],
        cwd: cwd.to_path_buf(),
        env: merged_no_proxy_env(),
    };
    match AcpClient::spawn_and_initialize(spec, AgyToolMode::Native, timeout).await {
        Ok(client) => {
            report.acp_ready = true;
            report.supports_fork = client.capabilities().supports_fork;
            if report.version.is_none() && !client.capabilities().agent_version.is_empty() {
                let ver = client.capabilities().agent_version.clone();
                report.version_supported = is_supported_version(&ver);
                report.version = Some(ver);
            }
            match client.session_new(cwd, &[], timeout).await {
                Ok(session) => {
                    report.authenticated = Some(true);
                    report.tier_name = session.tier_name;
                    let _ = client
                        .session_close(&session.session_id, Duration::from_secs(3))
                        .await;
                }
                Err(HrnError::Auth(_)) => {
                    report.authenticated = Some(false);
                }
                Err(err) => {
                    report.notes.push(format!("session probe failed: {err}"));
                }
            }
        }
        Err(err) => {
            report.notes.push(format!("ACP initialize failed: {err}"));
        }
    }

    report
}
