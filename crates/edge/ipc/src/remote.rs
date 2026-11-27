//! `tern`-style SSH-tunneled remoting, host-key trust pinning (`~/.harness/remote-hosts.json`),
//! and remote doctor checks.
//!
//! Security model:
//! - Reuses the user's `ssh` binary (`~/.ssh/config`, keys, `ProxyJump`).
//! - Daemon bearer tokens are NEVER passed on `argv`; they are read over SSH stdout from the
//!   remote daemon's `run.json`.
//! - Every remote connection verifies the remote daemon's pinned `SHA256:<hex>` host-key
//!   fingerprint before opening the tunnel or transmitting credentials.

use crate::auth::{RunFile, ct_eq, now_unix_ms};
use crate::client::IpcClient;
use hrn_edge_proc::{RunSpec, build_wrapped_command, resolve_on_path, run_streaming};
use hrn_kernel::{CancelFlag, HrnError, Result};
use process_wrap::tokio::ChildWrapper;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[path = "remote_fs.rs"]
mod fs_ops;

pub use fs_ops::{
    MAX_SSH_READ_BYTES, RemoteFileRead, build_list_command, build_read_command, list_dir,
    list_hosts, posix_single_quote, read_file, validate_remote_path, validate_remote_target,
};

const SSH_CMD_TIMEOUT: Duration = Duration::from_secs(20);
const TUNNEL_WAIT_TIMEOUT: Duration = Duration::from_secs(8);

/// One pinned remote daemon host-key entry in `~/.harness/remote-hosts.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedHost {
    /// Canonical host key (`host` or `host:ssh_port`).
    pub host: String,
    /// Pinned daemon fingerprint (`SHA256:<64 hex chars>`).
    pub host_key: String,
    /// Timestamp when the host was trusted (Unix ms).
    pub added_at_ms: u64,
}

/// Persistent trust store for pinned remote daemon host-key fingerprints.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostTrustStore {
    /// Trusted remote hosts indexed by canonical address (`host` or `host:port`).
    pub hosts: BTreeMap<String, TrustedHost>,
}

impl HostTrustStore {
    /// Load `~/.harness/remote-hosts.json`, returning an empty store if the file does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        serde_json::from_str(&raw).map_err(|e| {
            HrnError::Config(format!(
                "invalid remote trust store `{}`: {e}",
                path.display()
            ))
        })
    }

    /// Persist the trust store atomically to `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, data)?;
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Pin `host_key` for `host`.
    pub fn trust(&mut self, host: &str, host_key: &str) -> Result<TrustedHost> {
        let key = normalize_host_id(host)?;
        let fp = validate_fingerprint(host_key)?;
        let entry = TrustedHost {
            host: key.clone(),
            host_key: fp,
            added_at_ms: now_unix_ms(),
        };
        self.hosts.insert(key, entry.clone());
        Ok(entry)
    }

    /// Remove `host` from the trust store. Returns `true` if an entry was removed.
    pub fn forget(&mut self, host: &str) -> Result<bool> {
        let key = normalize_host_id(host)?;
        Ok(self.hosts.remove(&key).is_some())
    }

    /// Look up the pinned fingerprint for `host`.
    pub fn get(&self, host: &str) -> Option<&TrustedHost> {
        let key = normalize_host_id(host).ok()?;
        self.hosts.get(&key)
    }

    /// Return all trusted hosts in sorted order.
    pub fn list(&self) -> Vec<&TrustedHost> {
        self.hosts.values().collect()
    }
}

/// Parsed SSH remote target (`[user@]host[:ssh_port]` or `ssh://[user@]host[:port][/session_id]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTarget {
    /// Optional SSH username.
    pub user: Option<String>,
    /// Remote hostname or IP.
    pub host: String,
    /// Optional SSH port override.
    pub ssh_port: Option<u16>,
    /// Optional session ID parsed from `ssh://host/session_id`.
    pub session_id: Option<String>,
}

impl RemoteTarget {
    /// Canonical host identifier used as the key in [`HostTrustStore`].
    pub fn host_id(&self) -> String {
        match self.ssh_port {
            Some(port) => format!("{}:{port}", self.host.to_ascii_lowercase()),
            None => self.host.to_ascii_lowercase(),
        }
    }

    /// SSH destination argument (`user@host` or `host`), stripping any IPv6 brackets.
    pub fn ssh_dest(&self) -> String {
        let host = self
            .host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(&self.host);
        match &self.user {
            Some(u) => format!("{u}@{host}"),
            None => host.to_owned(),
        }
    }

    fn base_ssh_args(&self) -> Vec<String> {
        let mut args = vec![
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
        ];
        if let Some(port) = self.ssh_port {
            args.push("-p".to_owned());
            args.push(port.to_string());
        }
        args
    }
}

/// Diagnostic report produced by [`doctor_remote`].
#[derive(Clone, Debug)]
pub struct DoctorReport {
    /// Whether `ssh` was found on `PATH`.
    pub ssh_found: bool,
    /// Remote `hrn --version` output when reachable.
    pub remote_version: Option<String>,
    /// Remote daemon host-key fingerprint (`SHA256:...`), if queried successfully.
    pub remote_host_key: Option<String>,
    /// Whether the remote host-key matches the local pinned entry.
    pub host_key_trusted: bool,
    /// Whether the remote daemon currently has a readable `run.json`.
    pub daemon_running: bool,
    /// Human-readable warnings or failure details.
    pub notes: Vec<String>,
}

/// Active SSH local port-forward tunnel connected to a remote loopback `hrn daemon`.
///
/// Dropping the tunnel kills the `ssh` subprocess cleanly via the OS process-tree guard.
pub struct SshTunnel {
    local_addr: SocketAddr,
    client: IpcClient,
    child: Box<dyn ChildWrapper>,
}

impl SshTunnel {
    /// Open an authenticated, host-key-verified SSH tunnel to `target`.
    pub async fn connect(target: &RemoteTarget, trust_store: &HostTrustStore) -> Result<Self> {
        let host_id = target.host_id();
        let trusted = trust_store.get(&host_id).ok_or_else(|| {
            HrnError::Auth(format!(
                "remote host `{host_id}` is not trusted yet; run `hrn remote setup {}` or `hrn remote trust {host_id} SHA256:...`",
                target.ssh_dest()
            ))
        })?;

        let remote_run = fetch_remote_run_file(target).await?;
        if !ct_eq(remote_run.host_key.as_bytes(), trusted.host_key.as_bytes()) {
            return Err(HrnError::Auth(format!(
                "REMOTE HOST KEY MISMATCH for `{host_id}`! Pinned `{}`, remote presented `{}`. Refusing to connect.",
                trusted.host_key, remote_run.host_key
            )));
        }

        let remote_addr = remote_run.loopback_addr()?;
        let local_port = reserve_ephemeral_port()?;
        let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port);

        let mut args = target.base_ssh_args();
        args.extend([
            "-N".to_owned(),
            "-T".to_owned(),
            "-o".to_owned(),
            "ExitOnForwardFailure=yes".to_owned(),
            "-L".to_owned(),
            format!("127.0.0.1:{local_port}:127.0.0.1:{}", remote_addr.port()),
            target.ssh_dest(),
        ]);

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut wrap = build_wrapped_command("ssh", &args, &cwd, &BTreeMap::new());
        let mut child = wrap
            .spawn()
            .map_err(|e| HrnError::Io(format!("spawn ssh tunnel: {e}")))?;

        let client = IpcClient::new(
            local_addr,
            remote_run.token.clone(),
            Some(trusted.host_key.clone()),
        )?;

        let deadline = tokio::time::Instant::now() + TUNNEL_WAIT_TIMEOUT;
        loop {
            if let Ok(Some(status)) = child.inner_mut().try_wait() {
                return Err(HrnError::Io(format!(
                    "ssh tunnel to `{host_id}` exited prematurely ({status})"
                )));
            }
            match client.health().await {
                Ok(_) => {
                    return Ok(Self {
                        local_addr,
                        client,
                        child,
                    });
                }
                Err(HrnError::Auth(msg)) => {
                    let _ = child.start_kill();
                    return Err(HrnError::Auth(msg));
                }
                Err(last_err) => {
                    if tokio::time::Instant::now() >= deadline {
                        let _ = child.start_kill();
                        return Err(HrnError::Io(format!(
                            "ssh tunnel to `{host_id}` did not become healthy in time: {last_err}"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(120)).await;
                }
            }
        }
    }

    /// Bound local forwarded loopback address (`127.0.0.1:<port>`).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Authenticated [`IpcClient`] talking through this tunnel.
    pub fn client(&self) -> &IpcClient {
        &self.client
    }

    /// Explicitly tear down the SSH tunnel process.
    pub async fn close(mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.inner_mut().wait()).await;
    }
}

/// Validate that `raw` is a canonical `SHA256:<64 hex chars>` host-key fingerprint.
pub fn validate_fingerprint(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let hex = trimmed.strip_prefix("SHA256:").ok_or_else(|| {
        HrnError::Config(format!(
            "invalid host-key fingerprint `{raw}` (expected `SHA256:<64 hex chars>`)"
        ))
    })?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HrnError::Config(format!(
            "invalid host-key fingerprint `{raw}` (expected 64 hex characters after `SHA256:`)"
        )));
    }
    Ok(format!("SHA256:{}", hex.to_ascii_lowercase()))
}

/// Parse `[user@]host[:ssh_port]` (used by `hrn remote setup | doctor | trust`).
pub fn parse_host_spec(raw: &str) -> Result<RemoteTarget> {
    let trimmed = raw.trim();
    if trimmed.starts_with("ssh://") {
        return parse_remote_target(trimmed);
    }
    let (user, rest) = match trimmed.split_once('@') {
        Some((u, r)) if !u.is_empty() => (Some(u.to_owned()), r),
        _ => (None, trimmed),
    };
    let (host, ssh_port) = split_host_port(rest)?;
    Ok(RemoteTarget {
        user,
        host,
        ssh_port,
        session_id: None,
    })
}

/// Parse `ssh://[user@]host[:ssh_port][/session_id]` (used by `hrn attach`).
pub fn parse_remote_target(raw: &str) -> Result<RemoteTarget> {
    let body = raw
        .trim()
        .strip_prefix("ssh://")
        .ok_or_else(|| HrnError::Config(format!("expected `ssh://...` URL, got `{raw}`")))?;
    let (authority, session_id) = match body.split_once('/') {
        Some((a, s)) => (a, (!s.trim().is_empty()).then(|| s.trim().to_owned())),
        None => (body, None),
    };
    let mut target = parse_host_spec(authority)?;
    target.session_id = session_id;
    Ok(target)
}

/// Query `hrn remote host-key` on `target` over SSH.
pub async fn fetch_remote_host_key(target: &RemoteTarget) -> Result<String> {
    let out = run_ssh_command(target, &["hrn", "remote", "host-key"]).await?;
    validate_fingerprint(out.trim())
}

/// Read the remote daemon's `run.json` over SSH stdout (token is NEVER passed on argv).
pub async fn fetch_remote_run_file(target: &RemoteTarget) -> Result<RunFile> {
    let out = run_ssh_command(target, &["hrn", "remote", "print-run-file"]).await?;
    serde_json::from_str(out.trim()).map_err(|e| {
        HrnError::Config(format!(
            "remote `hrn remote print-run-file` returned invalid JSON: {e}"
        ))
    })
}

/// Run diagnostics against a remote host (`ssh`, remote `hrn --version`, host-key, daemon status).
pub async fn doctor_remote(target: &RemoteTarget, trust_store: &HostTrustStore) -> DoctorReport {
    let mut report = DoctorReport {
        ssh_found: resolve_on_path("ssh").is_some(),
        remote_version: None,
        remote_host_key: None,
        host_key_trusted: false,
        daemon_running: false,
        notes: Vec::new(),
    };
    if !report.ssh_found {
        report
            .notes
            .push("`ssh` binary not found on PATH".to_owned());
        return report;
    }
    match run_ssh_command(target, &["hrn", "--version"]).await {
        Ok(ver) => report.remote_version = Some(ver.trim().to_owned()),
        Err(err) => {
            report
                .notes
                .push(format!("remote `hrn --version` failed: {err}"));
            return report;
        }
    }
    match fetch_remote_host_key(target).await {
        Ok(fp) => {
            if let Some(pinned) = trust_store.get(&target.host_id()) {
                report.host_key_trusted = ct_eq(pinned.host_key.as_bytes(), fp.as_bytes());
                if !report.host_key_trusted {
                    report.notes.push(format!(
                        "pinned host-key `{}` does not match remote `{fp}`",
                        pinned.host_key
                    ));
                }
            } else {
                report.notes.push(format!(
                    "host not pinned yet; run: hrn remote trust {} {fp}",
                    target.host_id()
                ));
            }
            report.remote_host_key = Some(fp);
        }
        Err(err) => report
            .notes
            .push(format!("remote `hrn remote host-key` failed: {err}")),
    }
    match fetch_remote_run_file(target).await {
        Ok(_) => report.daemon_running = true,
        Err(err) => report
            .notes
            .push(format!("remote daemon not active: {err}")),
    }
    report
}

async fn run_ssh_command(target: &RemoteTarget, remote_argv: &[&str]) -> Result<String> {
    let mut args = target.base_ssh_args();
    args.push("-T".to_owned());
    args.push(target.ssh_dest());
    for part in remote_argv {
        args.push((*part).to_owned());
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let spec = RunSpec {
        program: "ssh".to_owned(),
        args,
        cwd,
        env: BTreeMap::new(),
        timeout: SSH_CMD_TIMEOUT,
        max_output_bytes: 64 * 1024,
    };
    let out = run_streaming(&spec, &CancelFlag::new(), |_| {}).await?;
    if out.timed_out {
        return Err(HrnError::Io(format!(
            "ssh command to `{}` timed out",
            target.host_id()
        )));
    }
    if out.exit_code != Some(0) {
        let err = out.stderr.trim();
        let detail = if err.is_empty() {
            out.stdout.trim()
        } else {
            err
        };
        return Err(HrnError::Io(format!(
            "ssh `{}` failed (exit {:?}): {detail}",
            target.host_id(),
            out.exit_code
        )));
    }
    Ok(out.stdout)
}

fn reserve_ephemeral_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn normalize_host_id(raw: &str) -> Result<String> {
    let target = parse_host_spec(raw)?;
    Ok(target.host_id())
}

fn split_host_port(raw: &str) -> Result<(String, Option<u16>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(HrnError::Config("empty remote host".to_owned()));
    }
    if let Some(after_open) = trimmed.strip_prefix('[') {
        let (addr, rest) = after_open
            .split_once(']')
            .ok_or_else(|| HrnError::Config(format!("unclosed IPv6 bracket in `{raw}`")))?;
        let addr = addr.trim();
        if addr.is_empty() {
            return Err(HrnError::Config(format!("missing host in `{raw}`")));
        }
        let host = format!("[{addr}]");
        if rest.is_empty() {
            return Ok((host, None));
        }
        let port_str = rest.strip_prefix(':').ok_or_else(|| {
            HrnError::Config(format!(
                "invalid suffix `{rest}` after IPv6 host in `{raw}`"
            ))
        })?;
        let port: u16 = port_str
            .parse()
            .map_err(|_| HrnError::Config(format!("invalid SSH port `{port_str}` in `{raw}`")))?;
        return Ok((host, Some(port)));
    }
    if let Some((h, p)) = trimmed.rsplit_once(':') {
        let port: u16 = p
            .parse()
            .map_err(|_| HrnError::Config(format!("invalid SSH port `{p}` in `{raw}`")))?;
        if h.trim().is_empty() {
            return Err(HrnError::Config(format!("missing host in `{raw}`")));
        }
        Ok((h.trim().to_owned(), Some(port)))
    } else {
        Ok((trimmed.to_owned(), None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_specs_and_ssh_urls() {
        let t1 = parse_host_spec("dev@box.internal:2222").expect("host spec");
        assert_eq!(t1.user.as_deref(), Some("dev"));
        assert_eq!(t1.host, "box.internal");
        assert_eq!(t1.ssh_port, Some(2222));
        assert_eq!(t1.host_id(), "box.internal:2222");

        let t2 = parse_remote_target("ssh://alice@prod/sess_42").expect("url");
        assert_eq!(t2.user.as_deref(), Some("alice"));
        assert_eq!(t2.host, "prod");
        assert_eq!(t2.session_id.as_deref(), Some("sess_42"));

        let v6_bare = parse_host_spec("[::1]").expect("ipv6 bare");
        assert_eq!(v6_bare.host, "[::1]");
        assert_eq!(v6_bare.ssh_port, None);
        assert_eq!(v6_bare.host_id(), "[::1]");
        assert_eq!(v6_bare.ssh_dest(), "::1");

        let v6_port = parse_host_spec("dev@[2001:db8::1]:2222").expect("ipv6 port");
        assert_eq!(v6_port.user.as_deref(), Some("dev"));
        assert_eq!(v6_port.host, "[2001:db8::1]");
        assert_eq!(v6_port.ssh_port, Some(2222));
        assert_eq!(v6_port.host_id(), "[2001:db8::1]:2222");
        assert_eq!(v6_port.ssh_dest(), "dev@2001:db8::1");
    }

    #[test]
    fn trust_store_roundtrip_and_fingerprint_validation() {
        let fp = format!("SHA256:{}", "a".repeat(64));
        assert!(validate_fingerprint(&fp).is_ok());
        assert!(validate_fingerprint("SHA256:short").is_err());
        assert!(validate_fingerprint("MD5:1234").is_err());

        let dir = std::env::temp_dir().join(format!("hrn_trust_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("remote-hosts.json");
        let mut store = HostTrustStore::default();
        store.trust("WorkBox:2222", &fp).expect("trust");
        store.save(&path).expect("save");

        let reloaded = HostTrustStore::load(&path).expect("reload");
        assert_eq!(
            reloaded.get("workbox:2222").map(|h| h.host_key.as_str()),
            Some(fp.as_str())
        );
        let mut store2 = reloaded;
        assert!(store2.forget("workbox:2222").expect("forget"));
        assert!(store2.get("workbox:2222").is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
