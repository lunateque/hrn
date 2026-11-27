//! Hardened read-only SSH file and directory access over pinned hosts in [`HostTrustStore`].
//!
//! Security model:
//! - Only hosts explicitly pinned in `remote-hosts.json` ([`HostTrustStore`]) are permitted.
//! - Rejects NUL bytes, ASCII control characters, newlines, and `-`-prefixed hosts/users
//!   (preventing SSH option injection).
//! - Remote `sh -c` scripts are compile-time constants: untrusted paths are NEVER interpolated
//!   into shell source; they are passed strictly as POSIX single-quoted positional arguments.
//! - Leading `~/` expands against the remote `$HOME` inside double quotes.
//! - Hard 1 MiB response cap ([`MAX_SSH_READ_BYTES`]) and strict command timeout.

use super::{HostTrustStore, RemoteTarget};
use hrn_edge_proc::{RunSpec, run_streaming};
use hrn_kernel::{CancelFlag, HrnError, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Hard response cap for `ssh://` file reads (1 MiB).
pub const MAX_SSH_READ_BYTES: usize = 1_048_576;
/// Hard response cap for `ssh://` directory listings (256 KiB).
const MAX_SSH_DIR_BYTES: usize = 256 * 1024;
/// Strict wall-clock timeout for one remote file or directory read.
const SSH_FS_TIMEOUT: Duration = Duration::from_secs(20);
/// Marker emitted by [`REMOTE_READ_SCRIPT`] when the requested path is a directory.
const DIR_MARKER: &str = "__HRN_SSH_DIR__\n";

/// Constant POSIX `sh` script reading a file (bounded by `dd` blocks) or listing a directory.
/// Arguments: `$1` = `"1"` if `~/` home-relative else `"0"`, `$2` = path/suffix, `$3` = 4 KiB blocks.
const REMOTE_READ_SCRIPT: &str = "\
if [ \"$1\" = \"1\" ]; then \
if [ -z \"$2\" ]; then p=\"$HOME\"; else p=\"$HOME/$2\"; fi; \
else p=\"$2\"; fi; \
if [ -d \"$p\" ]; then printf \"__HRN_SSH_DIR__\\n\"; LC_ALL=C ls -la -- \"$p\"; \
elif [ ! -e \"$p\" ]; then printf \"remote path not found\\n\" >&2; exit 2; \
elif [ ! -r \"$p\" ]; then printf \"remote path is not readable\\n\" >&2; exit 3; \
else LC_ALL=C dd bs=4096 count=\"$3\" < \"$p\" 2>/dev/null; fi";

/// Constant POSIX `sh` script listing a remote directory.
/// Arguments: `$1` = `"1"` if `~/` home-relative else `"0"`, `$2` = path/suffix.
const REMOTE_LIST_SCRIPT: &str = "\
if [ \"$1\" = \"1\" ]; then \
if [ -z \"$2\" ]; then p=\"$HOME\"; else p=\"$HOME/$2\"; fi; \
else p=\"$2\"; fi; \
if [ ! -d \"$p\" ]; then printf \"remote path is not a directory\\n\" >&2; exit 2; fi; \
LC_ALL=C ls -la -- \"$p\"";

/// Result of [`read_file`] over SSH.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFileRead {
    /// UTF-8 file text (or directory listing), capped at [`MAX_SSH_READ_BYTES`].
    pub content: String,
    /// Whether the remote payload exceeded the configured byte cap.
    pub truncated: bool,
}

/// Strictly POSIX single-quote `arg` (`'` -> `'\''`) so every shell metacharacter stays literal.
#[must_use]
pub fn posix_single_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Reject NUL bytes, newlines, and ASCII control characters in a remote path.
pub fn validate_remote_path(path: &str) -> Result {
    if path.is_empty() {
        return Err(HrnError::Tool("remote SSH path must not be empty".into()));
    }
    for ch in path.chars() {
        if ch == '\0' || ch == '\r' || ch == '\n' || ch.is_control() {
            return Err(HrnError::Tool(
                "remote SSH path must not contain NUL bytes, newlines, or control characters"
                    .into(),
            ));
        }
    }
    Ok(())
}

/// Validate `target` against SSH option injection (`-`-prefixed host/user) and control characters.
pub fn validate_remote_target(target: &RemoteTarget) -> Result {
    if let Some(user) = &target.user
        && (user.is_empty()
            || user.starts_with('-')
            || !user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(HrnError::Auth(format!("invalid SSH username `{user}`")));
    }
    let bare_host = target
        .host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(&target.host);
    if bare_host.is_empty()
        || bare_host.starts_with('-')
        || !bare_host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        return Err(HrnError::Auth(format!(
            "invalid SSH hostname `{}`",
            target.host
        )));
    }
    if target.ssh_port == Some(0) {
        return Err(HrnError::Auth("SSH port must be >= 1".into()));
    }
    Ok(())
}

/// Return all canonical trusted host identifiers pinned in `trust_store`, sorted.
#[must_use]
pub fn list_hosts(trust_store: &HostTrustStore) -> Vec<String> {
    trust_store
        .list()
        .into_iter()
        .map(|entry| entry.host.clone())
        .collect()
}

fn ensure_trusted(target: &RemoteTarget, trust_store: &HostTrustStore) -> Result {
    validate_remote_target(target)?;
    let host_id = target.host_id();
    if trust_store.get(&host_id).is_none() {
        return Err(HrnError::Auth(format!(
            "remote host `{host_id}` is not trusted; pin it first with \
             `hrn remote setup {}` or `hrn remote trust {host_id} SHA256:...`",
            target.ssh_dest()
        )));
    }
    Ok(())
}

fn split_home_flag(path: &str) -> (&'static str, &str) {
    if path == "~" || path == "~/" {
        ("1", "")
    } else if let Some(rest) = path.strip_prefix("~/") {
        ("1", rest)
    } else {
        ("0", path)
    }
}

/// Build the POSIX-quoted remote `sh -c` command for reading `path` (bounded at `max_bytes`).
pub fn build_read_command(path: &str, max_bytes: usize) -> Result<String> {
    validate_remote_path(path)?;
    let capped = max_bytes.clamp(1, MAX_SSH_READ_BYTES);
    // Read one extra 4 KiB block past `capped` so exact overflow detection works cleanly.
    let blocks = capped.div_ceil(4096).saturating_add(1);
    let (home_flag, arg) = split_home_flag(path);
    Ok(format!(
        "sh -c {} _ {} {} {blocks}",
        posix_single_quote(REMOTE_READ_SCRIPT),
        posix_single_quote(home_flag),
        posix_single_quote(arg),
    ))
}

/// Build the POSIX-quoted remote `sh -c` command for listing directory `path`.
pub fn build_list_command(path: &str) -> Result<String> {
    validate_remote_path(path)?;
    let (home_flag, arg) = split_home_flag(path);
    Ok(format!(
        "sh -c {} _ {} {}",
        posix_single_quote(REMOTE_LIST_SCRIPT),
        posix_single_quote(home_flag),
        posix_single_quote(arg),
    ))
}

async fn run_ssh_fs_command(
    target: &RemoteTarget,
    remote_cmd: String,
    max_output_bytes: usize,
) -> Result<(String, bool)> {
    let mut args = target.base_ssh_args();
    args.extend([
        "-o".to_owned(),
        "ConnectTimeout=10".to_owned(),
        "-T".to_owned(),
        "--".to_owned(),
        target.ssh_dest(),
        remote_cmd,
    ]);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let spec = RunSpec {
        program: "ssh".to_owned(),
        args,
        cwd,
        env: BTreeMap::new(),
        timeout: SSH_FS_TIMEOUT,
        max_output_bytes,
    };
    let out = run_streaming(&spec, &CancelFlag::new(), |_| {}).await?;
    if out.timed_out {
        return Err(HrnError::Io(format!(
            "ssh read from `{}` timed out after {}s",
            target.host_id(),
            SSH_FS_TIMEOUT.as_secs()
        )));
    }
    if out.exit_code != Some(0) {
        let err = out.stderr.trim();
        let detail = if err.is_empty() {
            out.stdout.trim()
        } else {
            err
        };
        return Err(HrnError::Tool(format!(
            "ssh `{}` failed (exit {:?}): {detail}",
            target.host_id(),
            out.exit_code
        )));
    }
    Ok((out.stdout, out.truncated))
}

/// Capture budget passed to `run_streaming` so its 3/4 stdout split never truncates before `limit`.
const fn read_capture_budget(limit: usize) -> usize {
    let doubled = limit.saturating_mul(2);
    let padded = limit.saturating_add(32_768);
    if doubled > padded { doubled } else { padded }
}

/// Read a remote UTF-8 file from a pinned host in `trust_store`, enforcing [`MAX_SSH_READ_BYTES`].
pub async fn read_file(
    target: &RemoteTarget,
    trust_store: &HostTrustStore,
    path: &str,
    max_bytes: usize,
) -> Result<RemoteFileRead> {
    ensure_trusted(target, trust_store)?;
    let limit = max_bytes.clamp(1, MAX_SSH_READ_BYTES);
    let cmd = build_read_command(path, limit)?;
    let (mut stdout, stream_truncated) =
        run_ssh_fs_command(target, cmd, read_capture_budget(limit)).await?;
    if let Some(dir_listing) = stdout.strip_prefix(DIR_MARKER) {
        return Ok(RemoteFileRead {
            content: dir_listing.to_owned(),
            truncated: stream_truncated,
        });
    }
    let truncated = stream_truncated || stdout.len() > limit;
    if stdout.len() > limit {
        let cut = stdout.floor_char_boundary(limit);
        stdout.truncate(cut);
    }
    Ok(RemoteFileRead {
        content: stdout,
        truncated,
    })
}

/// List a remote directory on a pinned host in `trust_store`.
pub async fn list_dir(
    target: &RemoteTarget,
    trust_store: &HostTrustStore,
    path: &str,
) -> Result<Vec<String>> {
    ensure_trusted(target, trust_store)?;
    let cmd = build_list_command(path)?;
    let (stdout, _truncated) = run_ssh_fs_command(target, cmd, MAX_SSH_DIR_BYTES).await?;
    Ok(stdout.lines().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::parse_host_spec;

    #[tokio::test]
    async fn ssh_scheme_rejects_untrusted_host_and_shell_injection() {
        let store = HostTrustStore::default();
        let target = parse_host_spec("untrusted.example").expect("parse");
        let err = read_file(&target, &store, "/etc/hosts", MAX_SSH_READ_BYTES)
            .await
            .expect_err("must reject untrusted host");
        assert!(matches!(err, HrnError::Auth(_)), "expected Auth, got {err}");

        let err = list_dir(&target, &store, "/")
            .await
            .expect_err("must reject untrusted host for list_dir");
        assert!(matches!(err, HrnError::Auth(_)), "expected Auth, got {err}");

        // Option injection in host/user is rejected even if somehow passed directly.
        let opt_host = RemoteTarget {
            user: None,
            host: "-oProxyCommand=id".to_owned(),
            ssh_port: None,
            session_id: None,
        };
        assert!(validate_remote_target(&opt_host).is_err());
        let opt_user = RemoteTarget {
            user: Some("-evil".to_owned()),
            host: "box".to_owned(),
            ssh_port: None,
            session_id: None,
        };
        assert!(validate_remote_target(&opt_user).is_err());

        // Control chars, NUL bytes, and newlines in path are rejected.
        for bad in ["", "/tmp/a\0b", "/tmp/a\nb", "/tmp/a\rb", "/tmp/\x1b[31m"] {
            assert!(
                build_read_command(bad, MAX_SSH_READ_BYTES).is_err(),
                "must reject path {bad:?}"
            );
            assert!(
                build_list_command(bad).is_err(),
                "must reject list path {bad:?}"
            );
        }

        // Shell metacharacters are safely single-quoted as positional argument $2, never interpolated.
        let tricky = "~/dir/$(whoami); `id` | 'quoted'";
        let cmd = build_read_command(tricky, MAX_SSH_READ_BYTES).expect("build cmd");
        assert!(cmd.starts_with("sh -c '"), "{cmd}");
        assert!(
            cmd.contains("'1' 'dir/$(whoami); `id` | '\\''quoted'\\'''"),
            "tricky path must be strictly POSIX single-quoted with home flag 1: {cmd}"
        );

        let abs = build_list_command("/var/log/app's").expect("list cmd");
        assert!(
            abs.ends_with("'0' '/var/log/app'\\''s'"),
            "abs path must use home flag 0 and single quotes: {abs}"
        );
    }

    #[test]
    fn list_hosts_returns_sorted_pinned_hosts() {
        let fp = format!("SHA256:{}", "b".repeat(64));
        let mut store = HostTrustStore::default();
        store.trust("zeta:2222", &fp).expect("trust zeta");
        store.trust("alpha", &fp).expect("trust alpha");
        assert_eq!(list_hosts(&store), vec!["alpha", "zeta:2222"]);
    }

    #[test]
    fn ipv6_target_validates_and_capture_budget_covers_full_1mib_stdout_split() {
        let target = parse_host_spec("ops@[::1]:2222").expect("ipv6");
        validate_remote_target(&target).expect("valid ipv6 target");
        assert_eq!(target.ssh_dest(), "ops@::1");
        let budget = read_capture_budget(MAX_SSH_READ_BYTES);
        // `run_streaming` allocates 3/4 of `max_output_bytes` to stdout; must comfortably hold
        // 1 MiB + one extra 4 KiB overflow block from `dd`.
        assert!(budget * 3 / 4 >= MAX_SSH_READ_BYTES + 4096, "{budget}");
    }
}
