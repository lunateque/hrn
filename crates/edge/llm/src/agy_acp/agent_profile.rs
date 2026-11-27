//! Ephemeral custom agent profile (`~/.gemini/config/agents/hrn-tools-<pid>-<uuid>/AGENT.md`)
//! with dead-PID orphan cleanup and strict tool-isolation invariants.

use super::schema::wire_tool_name;
use hrn_kernel::{HrnError, Result};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Directory prefix shared by all ephemeral `hrn` agent profiles.
pub const AGENT_DIR_PREFIX: &str = "hrn-tools-";

/// On-disk ephemeral custom agent profile removed automatically on [`Drop`].
#[derive(Debug)]
pub struct EphemeralAgentProfile {
    name: String,
    dir: PathBuf,
}

impl EphemeralAgentProfile {
    /// Default global Antigravity custom agents directory (`~/.gemini/config/agents`).
    pub fn default_agents_root() -> Result<PathBuf> {
        let home = home_dir().ok_or_else(|| {
            HrnError::Config(
                "cannot determine user home directory for `~/.gemini/config/agents`".to_owned(),
            )
        })?;
        Ok(home.join(".gemini").join("config").join("agents"))
    }

    /// Create an ephemeral `hrn-tools-<pid>-<hex>` profile inside `agents_root`.
    ///
    /// Built-in `agy` tools are explicitly disabled (`tools: []`), customizations and workspace
    /// MCP configs are not inherited (`inheritCustomizations: false`, `inheritMcp: false`), and
    /// `hrn-bridge` is declared as an eager Streamable HTTP MCP server.
    pub fn create_in(
        agents_root: &Path,
        bridge_url: &str,
        bridge_token: &str,
        eager_tool_names: &[String],
    ) -> Result<Self> {
        Self::create_with_tools(agents_root, bridge_url, bridge_token, eager_tool_names, &[])
    }

    /// Create an ephemeral profile with an explicit built-in tool list (validated against the
    /// `view_file` + `read_file` duplicate-alias bug in `agy >= 1.5.0`).
    pub fn create_with_tools(
        agents_root: &Path,
        bridge_url: &str,
        bridge_token: &str,
        eager_tool_names: &[String],
        builtin_tools: &[String],
    ) -> Result<Self> {
        validate_builtin_tools(builtin_tools)?;
        let suffix = random_hex(8)?;
        let name = format!("{AGENT_DIR_PREFIX}{}-{suffix}", std::process::id());
        let dir = agents_root.join(&name);

        std::fs::create_dir_all(&dir)?;
        set_dir_permissions_0700(&dir);

        let content = render_agent_markdown(
            &name,
            bridge_url,
            bridge_token,
            eager_tool_names,
            builtin_tools,
        );
        let file_path = dir.join("AGENT.md");
        std::fs::write(&file_path, content)?;
        set_file_permissions_0600(&file_path);

        Ok(Self { name, dir })
    }

    /// Custom agent name to pass to `agy acp --agent <name>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Filesystem directory containing `AGENT.md`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for EphemeralAgentProfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Enforce the `agy >= 1.5.0` invariant: never declare both `view_file` and `read_file`
/// simultaneously (they alias to the same tool and fail executor startup).
pub fn validate_builtin_tools(tools: &[String]) -> Result {
    let has_view = tools.iter().any(|t| t.eq_ignore_ascii_case("view_file"));
    let has_read = tools.iter().any(|t| t.eq_ignore_ascii_case("read_file"));
    if has_view && has_read {
        return Err(HrnError::Config(
            "ephemeral agy agent must never declare both `view_file` and `read_file`".to_owned(),
        ));
    }
    Ok(())
}

/// Render the `AGENT.md` YAML frontmatter + minimal system instructions.
pub fn render_agent_markdown(
    name: &str,
    bridge_url: &str,
    bridge_token: &str,
    eager_tool_names: &[String],
    builtin_tools: &[String],
) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    let _ = writeln!(out, "name: {name}");
    out.push_str("description: Ephemeral HRN tool bridge agent\n");
    out.push_str("mainAgent: true\n");
    out.push_str("subagent: false\n");
    out.push_str("inheritCustomizations: false\n");
    out.push_str("inheritMcp: false\n");
    if builtin_tools.is_empty() {
        out.push_str("tools: []\n");
    } else {
        out.push_str("tools:\n");
        for tool in builtin_tools {
            let _ = writeln!(out, "  - {}", yaml_quote(tool));
        }
    }
    out.push_str("mcpServers:\n");
    out.push_str("  - name: hrn-bridge\n");
    let _ = writeln!(out, "    url: {}", yaml_quote(bridge_url));
    let _ = writeln!(out, "    serverUrl: {}", yaml_quote(bridge_url));
    out.push_str("    headers:\n");
    let _ = writeln!(
        out,
        "      x-hrn-bridge-token: {}",
        yaml_quote(bridge_token)
    );
    if !eager_tool_names.is_empty() {
        out.push_str("    tools:\n");
        for raw_name in eager_tool_names {
            let prefixed = wire_tool_name(raw_name);
            let _ = writeln!(out, "      {}:", yaml_quote(&prefixed));
            out.push_str("        eager: true\n");
        }
    }
    out.push_str("---\n\n");
    out.push_str(
        "Use only the tools exposed by the connected `hrn-bridge` MCP server (`hrn__*`).\n",
    );
    out
}

/// Remove orphaned `hrn-tools-<dead_pid>-*` directories under `agents_root` whose owning PID is no
/// longer alive. Returns the number of removed directories.
pub fn cleanup_stale_agent_dirs(agents_root: &Path) -> usize {
    cleanup_stale_with_checker(agents_root, is_pid_alive)
}

/// Deterministic variant of [`cleanup_stale_agent_dirs`] accepting a custom PID liveness predicate.
pub fn cleanup_stale_with_checker(agents_root: &Path, pid_alive: impl Fn(u32) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(agents_root) else {
        return 0;
    };
    let my_pid = std::process::id();
    let mut removed = 0;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(pid) = parse_agent_dir_pid(name) else {
            continue;
        };
        if pid == my_pid || pid_alive(pid) {
            continue;
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Parse `<pid>` from `hrn-tools-<pid>-<suffix>`.
pub fn parse_agent_dir_pid(dir_name: &str) -> Option<u32> {
    let rest = dir_name.strip_prefix(AGENT_DIR_PREFIX)?;
    let (pid_str, suffix) = rest.split_once('-')?;
    if suffix.is_empty() {
        return None;
    }
    pid_str.parse::<u32>().ok()
}

fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let filter = format!("PID eq {pid}");
        let Ok(out) = std::process::Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        else {
            return true;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let needle = format!("\"{pid}\"");
        text.contains(&needle)
    }
}

fn yaml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn random_hex(num_bytes: usize) -> Result<String> {
    let mut bytes = vec![0u8; num_bytes];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| HrnError::Internal(format!("getrandom failed: {e}")))?;
    let mut out = String::with_capacity(num_bytes * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    Ok(out)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
}

#[cfg(unix)]
fn set_dir_permissions_0700(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_dir_permissions_0700(_path: &Path) {}

#[cfg(unix)]
fn set_file_permissions_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_file_permissions_0600(_path: &Path) {}
