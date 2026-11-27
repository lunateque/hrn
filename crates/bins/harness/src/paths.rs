//! Where the config file, profile directories, and the session database live.

use hrn_kernel::{HrnError, Result};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

static ACTIVE_PROFILE: RwLock<Option<String>> = RwLock::new(None);

/// Record the process-wide config profile (`--profile` beats `HRN_PROFILE`).
pub(crate) fn init_active_profile(cli_flag: Option<&str>, env_val: Option<&str>) -> Result {
    let raw = cli_flag.or(env_val);
    let normalized = normalize_profile(raw)?;
    *ACTIVE_PROFILE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = normalized;
    Ok(())
}

/// Active named profile (`None` for the default profile).
pub(crate) fn active_profile() -> Option<String> {
    ACTIVE_PROFILE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Normalize a config profile name (`--profile` / `HRN_PROFILE`).
/// Blank or `"default"` selects the default profile (`Ok(None)`).
pub(crate) fn normalize_profile(raw: Option<&str>) -> Result<Option<String>> {
    let Some(name) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if name.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !valid {
        return Err(HrnError::Config(format!(
            "invalid profile name `{name}`: use ASCII letters, digits, `-` or `_`"
        )));
    }
    Ok(Some(name.to_owned()))
}

/// Active harness configuration directory: `<home>/.harness` or `<home>/.harness/profiles/<name>`.
pub(crate) fn agent_dir(home: Option<&Path>, profile: Option<&str>) -> Result<PathBuf> {
    let base = home.map(|h| h.join(".harness")).ok_or_else(|| {
        HrnError::Config("cannot locate the home directory; pass --config or set HRN_CONFIG".into())
    })?;
    match normalize_profile(profile)? {
        Some(name) => Ok(base.join("profiles").join(name)),
        None => Ok(base),
    }
}

/// Global config file location: `HRN_CONFIG` beats `<agent_dir>/config.toml`.
pub(crate) fn global_config_path(
    env_value: Option<String>,
    home: Option<&Path>,
    profile: Option<&str>,
) -> Result<PathBuf> {
    if let Some(path) = env_value.filter(|v| !v.trim().is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(agent_dir(home, profile)?.join("config.toml"))
}

/// Project-local config overlay (`<cwd>/.harness/config.toml`).
pub(crate) fn project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(".harness").join("config.toml")
}

/// Config file location: `--config` beats `HRN_CONFIG` beats `<home>/.harness/config.toml`.
#[cfg(test)]
pub(crate) fn config_path(
    flag: Option<&Path>,
    env_value: Option<String>,
    home: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path.to_path_buf());
    }
    global_config_path(env_value, home, None)
}

/// Default `OAuth2` token file for `provider`: `<home>/.harness/tokens/<provider>.json`.
pub(crate) fn default_token_file(provider: &str, home: Option<&Path>) -> Result<PathBuf> {
    let clean: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    home.map(|h| h.join(".harness").join("tokens").join(format!("{clean}.json")))
        .ok_or_else(|| {
            HrnError::Config(format!(
                "provider `{provider}`: cannot resolve home directory for default token_file; set `oauth.token_file` explicitly"
            ))
        })
}

/// Per-user data directory: `%LOCALAPPDATA%\harness`, else `$XDG_DATA_HOME/harness`, else
/// `<home>/.local/share/harness` (relocated under `profiles/<name>` when a named profile is set).
pub(crate) fn data_dir_for_profile(
    local_app_data: Option<String>,
    xdg_data_home: Option<String>,
    home: Option<&Path>,
    profile: Option<&str>,
) -> Result<PathBuf> {
    let base = data_dir(local_app_data, xdg_data_home, home)?;
    match normalize_profile(profile)? {
        Some(name) => Ok(base.join("profiles").join(name)),
        None => Ok(base),
    }
}

/// Ordered `keybindings.toml` remap layers (`default_profile < active_profile`) merged over the
/// embedded defaults. For the default profile this is a single path; for a named profile it
/// returns `[<default_data_dir>/keybindings.toml, <active_data_dir>/keybindings.toml]`.
pub(crate) fn keybindings_paths(
    local_app_data: Option<String>,
    xdg_data_home: Option<String>,
    home: Option<&Path>,
    profile: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let default_dir = data_dir(local_app_data.clone(), xdg_data_home.clone(), home)?;
    let active_dir = data_dir_for_profile(local_app_data, xdg_data_home, home, profile)?;
    let mut paths = vec![default_dir.join("keybindings.toml")];
    if active_dir != default_dir {
        paths.push(active_dir.join("keybindings.toml"));
    }
    Ok(paths)
}

/// Per-user data directory: `%LOCALAPPDATA%\harness`, else `$XDG_DATA_HOME/harness`, else
/// `<home>/.local/share/harness`.
pub(crate) fn data_dir(
    local_app_data: Option<String>,
    xdg_data_home: Option<String>,
    home: Option<&Path>,
) -> Result<PathBuf> {
    let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty()).map(PathBuf::from);
    if let Some(base) = nonempty(local_app_data).or_else(|| nonempty(xdg_data_home)) {
        return Ok(base.join("harness"));
    }
    home.map(|h| h.join(".local").join("share").join("harness"))
        .ok_or_else(|| HrnError::Config("cannot locate a per-user data directory".into()))
}

/// Subscription logins (`hrn login claude|chatgpt`): `<data dir>/auth`.
pub(crate) fn auth_dir(home: Option<&Path>) -> Result<PathBuf> {
    let env = |key: &str| std::env::var(key).ok();
    Ok(data_dir(env("LOCALAPPDATA"), env("XDG_DATA_HOME"), home)?.join("auth"))
}

/// Persistent daemon host-key identity (`<home>/.harness/daemon-host-key.json`).
pub(crate) fn host_key_path(home: Option<&Path>) -> Result<PathBuf> {
    home.map(|h| h.join(".harness").join("daemon-host-key.json"))
        .ok_or_else(|| HrnError::Config("cannot locate home directory for daemon host key".into()))
}

/// Pinned remote daemon fingerprints (`<home>/.harness/remote-hosts.json`).
pub(crate) fn remote_hosts_path(home: Option<&Path>) -> Result<PathBuf> {
    home.map(|h| h.join(".harness").join("remote-hosts.json"))
        .ok_or_else(|| HrnError::Config("cannot locate home directory for remote hosts".into()))
}

/// MCP tool-list cache (`<home>/.harness/mcp-cache.json`), `None` without a home directory.
pub(crate) fn mcp_cache_path(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|h| h.join(".harness").join("mcp-cache.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_precedence() {
        let home = Path::new("/h");
        let flag = Path::new("/f.toml");
        assert_eq!(
            config_path(Some(flag), Some("/e.toml".into()), Some(home)).expect("flag"),
            flag
        );
        assert_eq!(
            config_path(None, Some("/e.toml".into()), Some(home)).expect("env"),
            Path::new("/e.toml")
        );
        assert_eq!(
            config_path(None, Some("  ".into()), Some(home)).expect("home"),
            home.join(".harness").join("config.toml")
        );
        assert!(config_path(None, None, None).is_err());
    }

    #[test]
    fn profile_relocates_agent_and_data_dirs() {
        let home = Path::new("/h");
        assert_eq!(
            agent_dir(Some(home), Some("work")).expect("work"),
            home.join(".harness").join("profiles").join("work")
        );
        assert_eq!(
            agent_dir(Some(home), Some("default")).expect("default"),
            home.join(".harness")
        );
        assert!(agent_dir(Some(home), Some("../bad")).is_err());
        assert_eq!(
            data_dir_for_profile(Some("/lad".into()), None, Some(home), Some("work"))
                .expect("work data"),
            Path::new("/lad")
                .join("harness")
                .join("profiles")
                .join("work")
        );
        assert_eq!(
            keybindings_paths(Some("/lad".into()), None, Some(home), None).expect("default"),
            vec![Path::new("/lad").join("harness").join("keybindings.toml")]
        );
        assert_eq!(
            keybindings_paths(Some("/lad".into()), None, Some(home), Some("work"))
                .expect("work keybindings"),
            vec![
                Path::new("/lad").join("harness").join("keybindings.toml"),
                Path::new("/lad")
                    .join("harness")
                    .join("profiles")
                    .join("work")
                    .join("keybindings.toml"),
            ]
        );
    }

    #[test]
    fn data_dir_prefers_local_app_data() {
        let home = Path::new("/h");
        assert_eq!(
            data_dir(Some("/lad".into()), Some("/xdg".into()), Some(home)).expect("lad"),
            Path::new("/lad").join("harness")
        );
        assert_eq!(
            data_dir(None, Some("/xdg".into()), Some(home)).expect("xdg"),
            Path::new("/xdg").join("harness")
        );
        assert_eq!(
            data_dir(None, None, Some(home)).expect("home"),
            home.join(".local").join("share").join("harness")
        );
        assert!(data_dir(None, None, None).is_err());
    }
}
