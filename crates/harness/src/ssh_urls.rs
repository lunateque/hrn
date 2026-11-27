//! `ssh://host/<path>`: read-only internal URL scheme for reading UTF-8 files and directory
//! listings over configured, host-key-pinned SSH remotes via an injected [`SshTransport`] port.
//!
//! Zero I/O lives in `hrn-harness`; the concrete SSH transport lives in `hrn-edge-ipc::remote`
//! and is wired by the binary.

use hrn_kernel::{HrnError, Result, ToolContext, UrlBody, UrlScheme};
use std::sync::Arc;

/// Hard maximum number of bytes returned from a single `ssh://` file read (1 MiB).
pub const MAX_SSH_READ_BYTES: usize = 1_048_576;

/// UTF-8 content returned by [`SshTransport::read_file`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshFileContent {
    /// File text (or directory listing when the remote target is a directory).
    pub content: String,
    /// `true` when the remote file exceeded the byte limit and was truncated.
    pub truncated: bool,
    /// Optional total byte length on the remote host.
    pub total_bytes: Option<u64>,
}

/// Host port backing `ssh://` so `hrn-harness` remains free of process and network I/O.
pub trait SshTransport: Send + Sync {
    /// Return configured trusted SSH host identifiers (from `remote-hosts.json`).
    fn list_hosts(&self) -> Result<Vec<String>>;

    /// Read a remote UTF-8 file (capped at `max_bytes`, never exceeding [`MAX_SSH_READ_BYTES`]).
    fn read_file(&self, host: &str, path: &str, max_bytes: usize) -> Result<SshFileContent>;

    /// List entries of a remote directory.
    fn list_dir(&self, host: &str, path: &str) -> Result<Vec<String>>;
}

/// Parsed `ssh://` URL after stripping the `ssh://` prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshUrl {
    /// Bare `ssh://`: list configured trusted hosts.
    ListHosts,
    /// `ssh://host/`, `ssh://host/~`, `ssh://host/~/`, or any trailing-slash path.
    ListDir {
        /// Validated `[user@]host[:port]` authority.
        host: String,
        /// Remote path (`/` or `~` or `~/sub` or `/abs/dir`).
        path: String,
    },
    /// `ssh://host/path` or `ssh://host/~/path`.
    ReadFile {
        /// Validated `[user@]host[:port]` authority.
        host: String,
        /// Remote path (`~/...` or `/...`).
        path: String,
    },
}

/// Wrap `arg` in POSIX single quotes (`'` -> `'\''`) so shell metacharacters stay literal.
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

/// Split a remote path into `(home_relative, remainder)` for remote `$HOME` expansion.
///
/// - `"~"` or `"~/"` -> `(true, "")`
/// - `"~/notes.md"` -> `(true, "notes.md")`
/// - `"/etc/hosts"` -> `(false, "/etc/hosts")`
#[must_use]
pub fn split_remote_home(path: &str) -> (bool, &str) {
    if path == "~" {
        return (true, "");
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return (true, rest);
    }
    (false, path)
}

fn reject_control_chars(raw: &str, label: &str) -> Result {
    for ch in raw.chars() {
        if ch == '\0' || ch == '\r' || ch == '\n' || ch.is_control() {
            return Err(HrnError::Tool(format!(
                "invalid `ssh://` {label}: control characters, NUL bytes, and newlines are forbidden"
            )));
        }
    }
    Ok(())
}

fn validate_authority(authority: &str) -> Result<&str> {
    if authority.is_empty() {
        return Err(HrnError::Tool("`ssh://` URL requires a host name".into()));
    }
    reject_control_chars(authority, "host")?;
    if authority.chars().any(char::is_whitespace) {
        return Err(HrnError::Tool(format!(
            "invalid `ssh://` host `{authority}`: whitespace is not allowed"
        )));
    }
    let (user, host_port) = match authority.split_once('@') {
        Some((u, hp)) => {
            if u.is_empty()
                || u.starts_with('-')
                || hp.contains('@')
                || !u
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                return Err(HrnError::Tool(format!(
                    "invalid `ssh://` username in `{authority}`"
                )));
            }
            (Some(u), hp)
        }
        None => (None, authority),
    };
    let _ = user;
    if host_port.is_empty() {
        return Err(HrnError::Tool(format!(
            "missing hostname in `ssh://{authority}`"
        )));
    }
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']').ok_or_else(|| {
            HrnError::Tool(format!("unclosed IPv6 host bracket in `ssh://{authority}`"))
        })?;
        if addr.is_empty()
            || !addr
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'))
        {
            return Err(HrnError::Tool(format!(
                "invalid IPv6 host in `ssh://{authority}`"
            )));
        }
        let port = match tail {
            "" => None,
            t => Some(t.strip_prefix(':').ok_or_else(|| {
                HrnError::Tool(format!("invalid port suffix in `ssh://{authority}`"))
            })?),
        };
        (addr, port)
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        (h, Some(p))
    } else {
        (host_port, None)
    };
    if host.is_empty()
        || host.starts_with('-')
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        return Err(HrnError::Tool(format!(
            "invalid `ssh://` hostname `{host}`"
        )));
    }
    if let Some(p) = port {
        let parsed: u16 = p
            .parse()
            .map_err(|_| HrnError::Tool(format!("invalid `ssh://` port `{p}`")))?;
        if parsed == 0 {
            return Err(HrnError::Tool("`ssh://` port must be >= 1".into()));
        }
    }
    Ok(authority)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode_path(raw: &str) -> Result<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else { break };
        if b == b'%' {
            let (Some(&h1), Some(&h2)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                return Err(HrnError::Tool(format!(
                    "malformed percent-encoding in `ssh://` path `{raw}`"
                )));
            };
            let (Some(hi), Some(lo)) = (hex_val(h1), hex_val(h2)) else {
                return Err(HrnError::Tool(format!(
                    "malformed percent-encoding in `ssh://` path `{raw}`"
                )));
            };
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out)
        .map_err(|_| HrnError::Tool("`ssh://` path is not valid UTF-8".into()))?;
    reject_control_chars(&decoded, "path")?;
    Ok(decoded)
}

/// Parse the remainder of an `ssh://` URL (after stripping `ssh://`).
pub fn parse_ssh_url(rest: &str) -> Result<SshUrl> {
    reject_control_chars(rest, "URL")?;
    if rest.is_empty() || rest == "/" {
        return Ok(SshUrl::ListHosts);
    }
    if rest.starts_with('/') {
        return Err(HrnError::Tool(format!(
            "`ssh://{rest}` is missing a host name (expected `ssh://<host>/<path>`)"
        )));
    }
    let (authority, raw_path) = match rest.split_once('/') {
        Some((auth, path)) => (auth, Some(path)),
        None => (rest, None),
    };
    let host = validate_authority(authority)?.to_owned();
    let Some(raw_path) = raw_path else {
        return Ok(SshUrl::ListDir {
            host,
            path: "/".to_owned(),
        });
    };
    if raw_path.is_empty() {
        return Ok(SshUrl::ListDir {
            host,
            path: "/".to_owned(),
        });
    }
    let decoded = percent_decode_path(raw_path)?;
    if decoded == "~" || decoded.trim_end_matches('/') == "~" {
        return Ok(SshUrl::ListDir {
            host,
            path: "~".to_owned(),
        });
    }
    if decoded.starts_with("~/") {
        if decoded.ends_with('/') {
            let trimmed = decoded.trim_end_matches('/');
            return Ok(SshUrl::ListDir {
                host,
                path: trimmed.to_owned(),
            });
        }
        return Ok(SshUrl::ReadFile {
            host,
            path: decoded,
        });
    }
    let normalized = if decoded.starts_with('/') {
        decoded
    } else {
        format!("/{decoded}")
    };
    if normalized.ends_with('/') {
        Ok(SshUrl::ListDir {
            host,
            path: normalized,
        })
    } else {
        Ok(SshUrl::ReadFile {
            host,
            path: normalized,
        })
    }
}

/// `ssh://host/<path>` read-only internal URL scheme backed by [`SshTransport`].
pub struct SshScheme {
    transport: Arc<dyn SshTransport>,
}

impl SshScheme {
    /// Construct an `ssh://` scheme backed by `transport`.
    #[must_use]
    pub fn new(transport: Arc<dyn SshTransport>) -> Self {
        Self { transport }
    }
}

impl UrlScheme for SshScheme {
    fn scheme(&self) -> &'static str {
        "ssh"
    }

    fn about(&self) -> &'static str {
        "remote UTF-8 files/dirs on pinned SSH hosts (read-only, <=1 MiB); bare `ssh://` lists \
         hosts, `ssh://host/path` or `ssh://host/~/path` reads a file, trailing `/` lists a dir"
    }

    fn read(&self, _ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        match parse_ssh_url(rest)? {
            SshUrl::ListHosts => {
                let hosts = self.transport.list_hosts()?;
                if hosts.is_empty() {
                    return Ok(UrlBody::Text(
                        "(no trusted SSH hosts configured; run `hrn remote setup <host>` or \
                         `hrn remote trust <host> SHA256:...`)"
                            .to_owned(),
                    ));
                }
                let lines: Vec<String> = hosts.into_iter().map(|h| format!("ssh://{h}/")).collect();
                Ok(UrlBody::Text(lines.join("\n")))
            }
            SshUrl::ListDir { host, path } => {
                let entries = self.transport.list_dir(&host, &path)?;
                if entries.is_empty() {
                    return Ok(UrlBody::Text("(empty directory)".to_owned()));
                }
                Ok(UrlBody::Text(entries.join("\n")))
            }
            SshUrl::ReadFile { host, path } => {
                let file = self.transport.read_file(&host, &path, MAX_SSH_READ_BYTES)?;
                let mut text = file.content;
                let overflowed = text.len() > MAX_SSH_READ_BYTES;
                if overflowed {
                    let cut = text.floor_char_boundary(MAX_SSH_READ_BYTES);
                    text.truncate(cut);
                }
                if overflowed || file.truncated {
                    let note = match file.total_bytes {
                        Some(total) => format!(
                            "\n... [truncated at {MAX_SSH_READ_BYTES} bytes ({total} bytes total)]"
                        ),
                        None => format!("\n... [truncated at {MAX_SSH_READ_BYTES} bytes]"),
                    };
                    text.push_str(&note);
                }
                Ok(UrlBody::Text(text))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::testing::tool_context;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct StubSsh {
        hosts: Vec<String>,
        calls: Mutex<Vec<(String, String, String)>>,
        payload: String,
    }

    impl StubSsh {
        fn new(hosts: &[&str], payload: impl Into<String>) -> Self {
            Self {
                hosts: hosts.iter().map(|s| (*s).to_owned()).collect(),
                calls: Mutex::new(Vec::new()),
                payload: payload.into(),
            }
        }
    }

    impl SshTransport for StubSsh {
        fn list_hosts(&self) -> Result<Vec<String>> {
            Ok(self.hosts.clone())
        }

        fn read_file(&self, host: &str, path: &str, max_bytes: usize) -> Result<SshFileContent> {
            if !self.hosts.iter().any(|h| h == host) {
                return Err(HrnError::Auth(format!("untrusted host `{host}`")));
            }
            self.calls.lock().expect("lock").push((
                "read".to_owned(),
                host.to_owned(),
                path.to_owned(),
            ));
            let truncated = self.payload.len() > max_bytes;
            Ok(SshFileContent {
                content: self.payload.clone(),
                truncated,
                total_bytes: Some(self.payload.len() as u64),
            })
        }

        fn list_dir(&self, host: &str, path: &str) -> Result<Vec<String>> {
            if !self.hosts.iter().any(|h| h == host) {
                return Err(HrnError::Auth(format!("untrusted host `{host}`")));
            }
            self.calls.lock().expect("lock").push((
                "list".to_owned(),
                host.to_owned(),
                path.to_owned(),
            ));
            Ok(vec!["notes.md".to_owned(), "src/".to_owned()])
        }
    }

    #[test]
    fn ssh_scheme_caps_and_expands_home() {
        let oversized = "x".repeat(MAX_SSH_READ_BYTES + 2048);
        let stub = Arc::new(StubSsh::new(&["prod", "dev:2222"], oversized));
        let scheme = SshScheme::new(Arc::clone(&stub) as Arc<dyn SshTransport>);
        let ctx = tool_context(PathBuf::from("."));

        // Bare `ssh://` lists trusted hosts.
        let UrlBody::Text(hosts) = scheme.read(&ctx, "").expect("list hosts") else {
            panic!("expected text");
        };
        assert!(hosts.contains("ssh://prod/"), "{hosts}");
        assert!(hosts.contains("ssh://dev:2222/"), "{hosts}");

        // `ssh://prod/~/notes.md` preserves remote `~/` expansion and caps >1 MiB with note.
        let UrlBody::Text(body) = scheme.read(&ctx, "prod/~/notes.md").expect("read home") else {
            panic!("expected text");
        };
        assert!(body.contains("truncated at 1048576 bytes"), "{body}");
        let prefix = body.split("\n... [truncated").next().expect("prefix");
        assert_eq!(prefix.len(), MAX_SSH_READ_BYTES);
        assert_eq!(
            split_remote_home("~/notes.md"),
            (true, "notes.md"),
            "remote home expansion helper"
        );
        assert_eq!(split_remote_home("~"), (true, ""));
        assert_eq!(split_remote_home("/etc/hosts"), (false, "/etc/hosts"));

        // `ssh://prod/~/` lists remote home directory.
        let UrlBody::Text(dir) = scheme.read(&ctx, "prod/~/").expect("list home") else {
            panic!("expected text");
        };
        assert!(dir.contains("notes.md"), "{dir}");

        let calls = stub.calls.lock().expect("lock").clone();
        assert_eq!(
            calls,
            vec![
                (
                    "read".to_owned(),
                    "prod".to_owned(),
                    "~/notes.md".to_owned()
                ),
                ("list".to_owned(), "prod".to_owned(), "~".to_owned()),
            ]
        );

        // Read-only: write always errors.
        let err = scheme
            .write(&ctx, "prod/~/notes.md", "nope")
            .expect_err("read-only")
            .to_string();
        assert!(err.contains("read-only"), "{err}");

        // POSIX single-quoting neutralizes quotes and metacharacters.
        assert_eq!(
            posix_single_quote("a'b $(id) `uname`; rm -rf /"),
            "'a'\\''b $(id) `uname`; rm -rf /'"
        );
    }

    #[test]
    fn ssh_scheme_rejects_untrusted_host_and_shell_injection() {
        let stub = Arc::new(StubSsh::new(&["trusted"], "ok"));
        let scheme = SshScheme::new(stub);
        let ctx = tool_context(PathBuf::from("."));

        // Untrusted host refused.
        let err = scheme
            .read(&ctx, "evil.example/etc/passwd")
            .expect_err("untrusted host")
            .to_string();
        assert!(err.contains("untrusted"), "{err}");

        // SSH option injection (`-oProxyCommand=...`), NUL, newlines, control chars rejected.
        for bad in [
            "-oProxyCommand=id/etc/passwd",
            "-user@trusted/etc/passwd",
            "trusted/foo\0bar",
            "trusted/foo%00bar",
            "trusted/foo\nid",
            "trusted/foo%0aid",
            "trusted/foo\rid",
            "trusted/foo\x1b[31mbar",
            "trusted/foo%1bbar",
            "trusted:0/etc/hosts",
            "/etc/hosts",
            "bad host/etc/hosts",
        ] {
            assert!(
                scheme.read(&ctx, bad).is_err(),
                "must reject dangerous ssh URL `{bad}`"
            );
        }
    }
}
