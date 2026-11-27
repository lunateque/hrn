//! MCP server configuration types and `.mcp.json` / `.harness/mcp.json` / omp `mcp.json`
//! auto-discovery.

use hrn_kernel::{HrnError, Result, WorkspaceFs};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Default per-call MCP request timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Transport configuration for one MCP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransport {
    /// Local child process speaking newline-delimited JSON-RPC over `stdin`/`stdout`.
    Stdio {
        /// Executable name or path (resolved via `PATHEXT` / `.cmd` on Windows).
        command: String,
        /// Command-line arguments.
        args: Vec<String>,
        /// Optional working directory for the child process.
        cwd: Option<PathBuf>,
        /// Extra environment variables.
        env: BTreeMap<String, String>,
    },
    /// Remote or local Streamable HTTP MCP endpoint.
    Http {
        /// Full `http://` or `https://` endpoint URL.
        url: String,
        /// Static HTTP headers (`Header-Name` -> value).
        headers: BTreeMap<String, String>,
        /// HTTP headers loaded from environment variables at request time.
        header_envs: BTreeMap<String, String>,
    },
}

impl McpTransport {
    /// Short display kind label (`stdio` or `http`).
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
        }
    }

    /// Short human-readable target summary without leaking header secrets.
    pub fn target_summary(&self) -> String {
        match self {
            Self::Stdio { command, args, .. } => {
                if args.is_empty() {
                    command.clone()
                } else {
                    format!("{command} {}", args.join(" "))
                }
            }
            Self::Http { url, .. } => url.clone(),
        }
    }
}

/// Validated configuration for a single MCP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConfig {
    /// Unique server name (used in `mcp__<name>__<tool>`).
    pub name: String,
    /// Stdio or Streamable HTTP transport parameters.
    pub transport: McpTransport,
    /// Whether the server should be connected automatically.
    pub enabled: bool,
    /// Optional allowlist of remote tool names (empty = allow all).
    pub allow_tools: Vec<String>,
    /// Denylist of remote tool names (applied after `allow_tools`).
    pub deny_tools: Vec<String>,
    /// Per-call timeout in seconds.
    pub timeout_secs: u64,
    /// Whether tools from this server run without confirmation prompts (default `true`).
    pub auto_approve: bool,
    /// Mark all tools from this server as `ReadOnly` so they can execute in parallel batches.
    pub read_only: bool,
    /// Origin label (e.g. `"config.toml"`, `".mcp.json"`, `".harness/mcp.json"`).
    pub source: String,
}

impl McpServerConfig {
    /// Check whether a remote tool name passes this server's `allow_tools` / `deny_tools` filter.
    pub fn allows_tool(&self, remote_name: &str) -> bool {
        if self.deny_tools.iter().any(|d| d == remote_name) {
            return false;
        }
        if self.allow_tools.is_empty() {
            return true;
        }
        self.allow_tools.iter().any(|a| a == remote_name)
    }
}

#[derive(Debug, Deserialize)]
struct RawMcpJson {
    #[serde(rename = "mcpServers", alias = "mcp_servers", default)]
    mcp_servers: BTreeMap<String, RawJsonServer>,
}

#[derive(Debug, Deserialize)]
struct RawJsonServer {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(rename = "headerEnvs", alias = "header_envs", default)]
    header_envs: BTreeMap<String, String>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(rename = "allowTools", alias = "allow_tools", default)]
    allow_tools: Vec<String>,
    #[serde(rename = "denyTools", alias = "deny_tools", default)]
    deny_tools: Vec<String>,
    #[serde(rename = "timeoutSecs", alias = "timeout_secs", default)]
    timeout_secs: Option<u64>,
    /// omp spelling: milliseconds.
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(rename = "autoApprove", alias = "auto_approve", default)]
    auto_approve: Option<bool>,
    #[serde(rename = "readOnly", alias = "read_only", default)]
    read_only: Option<bool>,
}

/// Expand `${VAR}` references using a caller-provided lookup function.
fn expand_env_str(raw: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start + 2..).unwrap_or_default();
        if let Some(end) = after.find('}') {
            let spec = after.get(..end).unwrap_or_default().trim();
            let (var_name, default) = spec.split_once(":-").unwrap_or((spec, ""));
            if let Some(val) = lookup(var_name.trim()).filter(|v| !v.is_empty()) {
                out.push_str(&val);
            } else {
                out.push_str(default);
            }
            rest = after.get(end + 1..).unwrap_or_default();
        } else {
            out.push_str(rest.get(start..).unwrap_or_default());
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// How a secret-bearing value (`env`, `headers`) is resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// `${VAR}` expansion only.
    Hrn,
    /// Also omp's rule: a value that is exactly the name of a set, non-empty environment variable
    /// is replaced by that variable's value (`"TOKEN": "TOKEN"` copies it from the shell), so the
    /// file itself holds no secret.
    Omp,
}

fn expand_value(raw: &str, dialect: Dialect, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let expanded = expand_env_str(raw, lookup);
    if dialect == Dialect::Omp
        && let Some(val) = lookup(expanded.trim()).filter(|v| !v.is_empty())
    {
        return val;
    }
    expanded
}

fn expand_map(
    map: BTreeMap<String, String>,
    dialect: Dialect,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    map.into_iter()
        .map(|(k, v)| {
            let v = expand_value(&v, dialect, lookup);
            (k, v)
        })
        .collect()
}

/// Parse a standard `.mcp.json` / `.harness/mcp.json` document into [`McpServerConfig`] entries.
pub fn parse_mcp_json(text: &str, source: &str) -> Result<Vec<McpServerConfig>> {
    parse_mcp_json_with_env(text, source, Dialect::Hrn, &|k| std::env::var(k).ok())
}

fn parse_mcp_json_with_env(
    text: &str,
    source: &str,
    dialect: Dialect,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<McpServerConfig>> {
    let raw: RawMcpJson = serde_json::from_str(text)
        .map_err(|e| HrnError::Config(format!("invalid MCP JSON `{source}`: {e}")))?;
    let mut out = Vec::with_capacity(raw.mcp_servers.len());
    for (name, entry) in raw.mcp_servers {
        let name = name.trim().to_owned();
        if name.is_empty() {
            continue;
        }
        let transport = match (
            entry.command.filter(|c| !c.trim().is_empty()),
            entry.url.filter(|u| !u.trim().is_empty()),
        ) {
            (Some(command), None) => McpTransport::Stdio {
                command: expand_env_str(&command, lookup),
                args: entry
                    .args
                    .into_iter()
                    .map(|a| expand_env_str(&a, lookup))
                    .collect(),
                cwd: entry
                    .cwd
                    .filter(|c| !c.trim().is_empty())
                    .map(|c| PathBuf::from(expand_env_str(&c, lookup))),
                env: expand_map(entry.env, dialect, lookup),
            },
            (None, Some(url)) => McpTransport::Http {
                url: expand_env_str(&url, lookup),
                headers: expand_map(entry.headers, dialect, lookup),
                header_envs: entry.header_envs,
            },
            (Some(_), Some(_)) => {
                return Err(HrnError::Config(format!(
                    "MCP server `{name}` in `{source}`: set only one of `command` or `url`"
                )));
            }
            (None, None) => {
                return Err(HrnError::Config(format!(
                    "MCP server `{name}` in `{source}`: missing `command` or `url`"
                )));
            }
        };
        let enabled = match (entry.enabled, entry.disabled) {
            (Some(en), _) => en,
            (None, Some(dis)) => !dis,
            (None, None) => true,
        };
        out.push(McpServerConfig {
            name,
            transport,
            enabled,
            allow_tools: entry.allow_tools,
            deny_tools: entry.deny_tools,
            timeout_secs: entry
                .timeout_secs
                .or_else(|| entry.timeout.map(|ms| ms.div_ceil(1000)))
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .max(1),
            auto_approve: entry.auto_approve.unwrap_or(true),
            read_only: entry.read_only.unwrap_or(false),
            source: source.to_owned(),
        });
    }
    Ok(out)
}

/// Merge MCP servers from all sources in precedence order (lowest -> highest):
///
/// 1. `~/.omp/agent/mcp.json` (oh-my-pi's file, a fallback so its servers work unchanged)
/// 2. `~/.harness/mcp.json`
/// 3. `[[mcp]]` entries from `config.toml`
/// 4. `<cwd>/.mcp.json`
/// 5. `<cwd>/.harness/mcp.json`
///
/// Parse warnings for optional JSON files are returned alongside the merged server list instead
/// of aborting startup.
pub fn discover_mcp_configs(
    fs: &dyn WorkspaceFs,
    cwd: &Path,
    from_config: &[McpServerConfig],
) -> (Vec<McpServerConfig>, Vec<String>) {
    let mut merged: BTreeMap<String, McpServerConfig> = BTreeMap::new();
    let mut warnings = Vec::new();

    if let Some(home) = fs.home_dir() {
        let omp_json = home.join(".omp").join("agent").join("mcp.json");
        load_optional_json(
            fs,
            &omp_json,
            "~/.omp/agent/mcp.json",
            Dialect::Omp,
            &mut merged,
            &mut warnings,
        );
        let global_json = home.join(".harness").join("mcp.json");
        load_optional_json(
            fs,
            &global_json,
            "~/.harness/mcp.json",
            Dialect::Hrn,
            &mut merged,
            &mut warnings,
        );
    }

    for entry in from_config {
        merged.insert(entry.name.clone(), entry.clone());
    }

    let project_mcp = cwd.join(".mcp.json");
    load_optional_json(
        fs,
        &project_mcp,
        ".mcp.json",
        Dialect::Hrn,
        &mut merged,
        &mut warnings,
    );

    let project_hrn = cwd.join(".harness").join("mcp.json");
    load_optional_json(
        fs,
        &project_hrn,
        ".harness/mcp.json",
        Dialect::Hrn,
        &mut merged,
        &mut warnings,
    );

    (merged.into_values().collect(), warnings)
}

fn load_optional_json(
    fs: &dyn WorkspaceFs,
    path: &Path,
    source_label: &str,
    dialect: Dialect,
    merged: &mut BTreeMap<String, McpServerConfig>,
    warnings: &mut Vec<String>,
) {
    match fs.read_to_string(path) {
        Ok(Some(text)) => {
            match parse_mcp_json_with_env(&text, source_label, dialect, &|k| std::env::var(k).ok())
            {
                Ok(servers) => {
                    for s in servers {
                        merged.insert(s.name.clone(), s);
                    }
                }
                Err(err) => warnings.push(err.to_string()),
            }
        }
        Ok(None) => {}
        Err(err) => warnings.push(format!("cannot read `{source_label}`: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stdio_and_http_servers_with_env_expansion() {
        let json = r#"{
            "mcpServers": {
                "local": {
                    "command": "node",
                    "args": ["server.js", "${EXTRA_ARG}"],
                    "env": { "TOKEN": "${MY_TOK}" },
                    "readOnly": true
                },
                "remote": {
                    "url": "https://mcp.example.com/rpc",
                    "headers": { "Authorization": "Bearer ${API_KEY}" },
                    "disabled": true,
                    "autoApprove": false,
                    "allowTools": ["search"],
                    "denyTools": ["delete"]
                }
            }
        }"#;
        let lookup = |k: &str| match k {
            "EXTRA_ARG" => Some("--fast".to_owned()),
            "MY_TOK" => Some("s3cr3t".to_owned()),
            "API_KEY" => Some("k123".to_owned()),
            _ => None,
        };
        let list =
            parse_mcp_json_with_env(json, ".mcp.json", Dialect::Hrn, &lookup).expect("parse");
        assert_eq!(list.len(), 2);

        let local = list.iter().find(|s| s.name == "local").expect("local");
        assert!(local.enabled);
        assert!(local.auto_approve);
        assert!(local.read_only);
        assert_eq!(
            local.transport,
            McpTransport::Stdio {
                command: "node".to_owned(),
                args: vec!["server.js".to_owned(), "--fast".to_owned()],
                cwd: None,
                env: BTreeMap::from([("TOKEN".to_owned(), "s3cr3t".to_owned())]),
            }
        );

        let remote = list.iter().find(|s| s.name == "remote").expect("remote");
        assert!(!remote.enabled);
        assert!(!remote.auto_approve);
        assert!(remote.allows_tool("search"));
        assert!(!remote.allows_tool("delete"));
        assert!(!remote.allows_tool("other"));
    }

    #[test]
    fn omp_file_resolves_env_names_and_millisecond_timeouts() {
        let json = r#"{
            "mcpServers": {
                "tg": {
                    "type": "stdio",
                    "command": "npx",
                    "env": { "API_ID": "API_ID", "UNSET": "UNSET", "LIT": "x", "D": "${NOPE:-dflt}" },
                    "timeout": 120000
                }
            }
        }"#;
        let lookup = |k: &str| (k == "API_ID").then(|| "42".to_owned());
        let list = parse_mcp_json_with_env(json, "omp", Dialect::Omp, &lookup).expect("parse");
        let tg = list.first().expect("tg");
        assert_eq!(tg.timeout_secs, 120);
        let McpTransport::Stdio { env, .. } = &tg.transport else {
            unreachable!("omp stdio entry parsed as stdio");
        };
        assert_eq!(env.get("API_ID").map(String::as_str), Some("42"));
        assert_eq!(env.get("UNSET").map(String::as_str), Some("UNSET"));
        assert_eq!(env.get("LIT").map(String::as_str), Some("x"));
        assert_eq!(env.get("D").map(String::as_str), Some("dflt"));
    }

    #[test]
    fn rejects_server_with_both_command_and_url() {
        let bad = r#"{"mcpServers":{"x":{"command":"a","url":"https://x"}}}"#;
        assert!(parse_mcp_json(bad, ".mcp.json").is_err());
    }
}
