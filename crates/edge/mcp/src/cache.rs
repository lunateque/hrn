//! On-disk cache of each server's tool list, so a session can advertise MCP tools without
//! starting any server process. Entries are keyed by server name and invalidated when the
//! server's launch target or tool filters change.

use crate::config::{McpServerConfig, McpTransport};
use crate::protocol::McpToolSpec;
use hrn_kernel::{HrnError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A cached tool list stays valid this long; the next start after that reconnects once.
const TOOLS_TTL_SECS: u64 = 7 * 24 * 3600;
/// A recorded connect failure suppresses retries this long, so a dead server cannot slow
/// every startup.
const FAILURE_TTL_SECS: u64 = 10 * 60;

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    servers: BTreeMap<String, Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    fingerprint: String,
    at: u64,
    /// Present for a successful listing, absent for a recorded failure.
    #[serde(default)]
    tools: Option<Vec<McpToolSpec>>,
    #[serde(default)]
    error: Option<String>,
}

/// What the cache knows about one server.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    /// A fresh tool list: no process needs to start to advertise these.
    Tools(Vec<McpToolSpec>),
    /// A recent connect failure; do not retry yet.
    Failed {
        /// The recorded error text.
        error: String,
        /// Seconds until a retry is allowed.
        retry_in_secs: u64,
    },
    /// Nothing usable: connect to find out.
    Miss,
}

/// The cache file. Reads and writes are whole-file; writes are atomic renames.
#[derive(Clone, Debug)]
pub struct McpCache {
    path: PathBuf,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn fingerprint(cfg: &McpServerConfig) -> String {
    let mut h = DefaultHasher::new();
    match &cfg.transport {
        McpTransport::Stdio {
            command, args, cwd, ..
        } => (0u8, command, args, cwd).hash(&mut h),
        McpTransport::Http { url, .. } => (1u8, url).hash(&mut h),
    }
    (&cfg.allow_tools, &cfg.deny_tools).hash(&mut h);
    format!("{:016x}", h.finish())
}

impl McpCache {
    /// A cache stored at `path` (created on first write).
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn read(&self) -> Result<CacheFile> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| HrnError::Config(format!("MCP cache `{}`: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CacheFile::default()),
            Err(e) => Err(HrnError::Io(format!(
                "cannot read MCP cache `{}`: {e}",
                self.path.display()
            ))),
        }
    }

    /// Snapshot the file once so a whole startup is answered from one read.
    pub(crate) fn snapshot(&self) -> Result<Snapshot> {
        self.read().map(|file| Snapshot { file })
    }

    /// Record a fresh tool list for `cfg`.
    pub fn record_tools(&self, cfg: &McpServerConfig, tools: Vec<McpToolSpec>) -> Result {
        self.record(cfg, Some(tools), None)
    }

    /// Record that connecting to `cfg` failed with `error`.
    pub fn record_failure(&self, cfg: &McpServerConfig, error: String) -> Result {
        self.record(cfg, None, Some(error))
    }

    fn record(
        &self,
        cfg: &McpServerConfig,
        tools: Option<Vec<McpToolSpec>>,
        error: Option<String>,
    ) -> Result {
        // A corrupt file is replaced rather than blocking every future write.
        let mut file = self.read().unwrap_or_default();
        file.servers.insert(
            cfg.name.clone(),
            Entry {
                fingerprint: fingerprint(cfg),
                at: now_secs(),
                tools,
                error,
            },
        );
        let text = serde_json::to_string(&file)?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| HrnError::Io(format!("cannot create `{}`: {e}", dir.display())))?;
        }
        write_atomic(&self.path, &text)
    }
}

fn write_atomic(path: &Path, text: &str) -> Result {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| HrnError::Io(format!("cannot write MCP cache `{}`: {e}", path.display())))
}

/// One read of the cache file.
pub(crate) struct Snapshot {
    file: CacheFile,
}

impl Snapshot {
    pub(crate) fn lookup(&self, cfg: &McpServerConfig) -> Lookup {
        let Some(entry) = self.file.servers.get(&cfg.name) else {
            return Lookup::Miss;
        };
        if entry.fingerprint != fingerprint(cfg) {
            return Lookup::Miss;
        }
        let age = now_secs().saturating_sub(entry.at);
        match (&entry.tools, &entry.error) {
            (Some(tools), _) if age < TOOLS_TTL_SECS => Lookup::Tools(tools.clone()),
            (None, Some(error)) if age < FAILURE_TTL_SECS => Lookup::Failed {
                error: error.clone(),
                retry_in_secs: FAILURE_TTL_SECS - age,
            },
            _ => Lookup::Miss,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::default_object_schema;

    fn cfg(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_owned(),
            transport: McpTransport::Stdio {
                command: command.to_owned(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
            },
            enabled: true,
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
            timeout_secs: 5,
            auto_approve: true,
            read_only: false,
            source: "test".to_owned(),
        }
    }

    #[test]
    fn cached_tools_are_reused_until_the_launch_target_changes() {
        let dir = std::env::temp_dir().join(format!("hrn-mcp-cache-{}", std::process::id()));
        let cache = McpCache::new(dir.join("cache.json"));
        let tool = McpToolSpec {
            name: "t".to_owned(),
            description: None,
            input_schema: default_object_schema(),
        };
        assert_eq!(
            cache.snapshot().expect("empty").lookup(&cfg("a", "x")),
            Lookup::Miss
        );
        cache
            .record_tools(&cfg("a", "x"), vec![tool.clone()])
            .expect("record");
        cache
            .record_failure(&cfg("b", "y"), "boom".to_owned())
            .expect("record");
        let snap = cache.snapshot().expect("read");
        assert_eq!(snap.lookup(&cfg("a", "x")), Lookup::Tools(vec![tool]));
        assert_eq!(snap.lookup(&cfg("a", "other")), Lookup::Miss);
        assert!(matches!(snap.lookup(&cfg("b", "y")), Lookup::Failed { .. }));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
