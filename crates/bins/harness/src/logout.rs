//! Credential discovery and guarded deletion shared by `/logout` (TUI) and `hrn logout` (CLI).
//!
//! Strictly confines deletion to harness-owned credential roots (`auth_dir`, `<config_dir>/keys`,
//! `<config_dir>/tokens`, `<config_dir>/oauth`), rejects `..` traversal and symlinks, passes
//! `guard::check_token_path`, and refuses external CLI stores (`~/.claude`, `~/.codex`,
//! `~/.gemini`, `~/.omp`).

use crate::config::Config;
use crate::term::out;
use hrn_edge_http::guard;
use hrn_edge_llm::{AuthMode, ProviderKind};
use hrn_edge_oauth::{Subscription, SubscriptionStatus, subscription_accounts_status};
use hrn_kernel::{HrnError, Result};
use hrn_tui::LogoutProviderChoice;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

/// External consumer CLI directories that `/logout` must NEVER delete from (even `.omp` paths
/// permitted for read-only key access).
const EXTERNAL_STORES: &[&str] = &["/.claude/", "/.codex/", "/.gemini/", "/.omp/"];

/// Harness-owned credential directories inside which `/logout` and `hrn logout` may delete files.
pub(crate) fn harness_credential_roots(config_path: &Path, auth_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![auth_dir.to_path_buf()];
    if let Some(config_dir) = config_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        roots.push(config_dir.join("keys"));
        roots.push(config_dir.join("tokens"));
        roots.push(config_dir.join("oauth"));
    }
    roots
}

/// Validate that `path` is safe to delete:
/// 1. Passes `guard::check_token_path` and never targets external CLI stores.
/// 2. Contains no `..` (`ParentDir`) components.
/// 3. Is not a symlink or directory, and both its canonical parent directory and canonical target
///    file reside strictly under one of `allowed_roots`.
pub(crate) fn check_deletable_credential_path(path: &Path, allowed_roots: &[PathBuf]) -> Result {
    let raw = path.to_string_lossy();
    guard::check_token_path(&raw)?;
    let normalized = format!("/{}/", raw.replace('\\', "/").to_ascii_lowercase());
    for seg in EXTERNAL_STORES {
        if normalized.contains(seg) {
            return Err(HrnError::Config(format!(
                "refusing to delete credential outside harness storage (`{seg}`): {}",
                path.display()
            )));
        }
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(HrnError::Config(format!(
            "refusing to delete credential path with `..` traversal: {}",
            path.display()
        )));
    }
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Err(HrnError::Config(format!(
            "refusing to delete credential path without parent directory: {}",
            path.display()
        )));
    };
    if path.file_name().is_none() {
        return Err(HrnError::Config(format!(
            "refusing to delete non-file credential path: {}",
            path.display()
        )));
    }

    let canon_roots: Vec<PathBuf> = allowed_roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();

    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(HrnError::Config(format!(
                    "refusing to delete symlink credential path: {}",
                    path.display()
                )));
            }
            if !meta.is_file() {
                return Err(HrnError::Config(format!(
                    "refusing to delete non-file credential path: {}",
                    path.display()
                )));
            }
            let canon_parent = std::fs::canonicalize(parent).map_err(|e| {
                HrnError::Config(format!(
                    "cannot canonicalize credential parent `{}`: {e}",
                    parent.display()
                ))
            })?;
            let canon_file = std::fs::canonicalize(path).map_err(|e| {
                HrnError::Config(format!(
                    "cannot canonicalize credential path `{}`: {e}",
                    path.display()
                ))
            })?;
            let confined = canon_roots
                .iter()
                .any(|root| canon_parent.starts_with(root) && canon_file.starts_with(root));
            if !confined {
                return Err(HrnError::Config(format!(
                    "refusing to delete credential outside harness-owned directories: {}",
                    path.display()
                )));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let confined = match std::fs::canonicalize(parent) {
                Ok(canon_parent) => canon_roots
                    .iter()
                    .any(|root| canon_parent.starts_with(root)),
                Err(_) => allowed_roots.iter().any(|root| parent.starts_with(root)),
            };
            if !confined {
                return Err(HrnError::Config(format!(
                    "refusing to delete credential outside harness-owned directories: {}",
                    path.display()
                )));
            }
        }
        Err(e) => {
            return Err(HrnError::Io(format!(
                "inspect credential `{}`: {e}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// List every provider that currently holds an on-disk credential file inside harness-owned roots.
pub(crate) fn list_logout_choices(
    cfg: Option<&Config>,
    config_path: &Path,
    auth_dir: &Path,
) -> Vec<LogoutProviderChoice> {
    let roots = harness_credential_roots(config_path, auth_dir);
    let mut out = Vec::new();
    for kind in Subscription::ALL {
        let file = auth_dir.join(kind.file_name());
        if check_deletable_credential_path(&file, &roots).is_err() || !file.is_file() {
            continue;
        }
        let detail = describe_subscription_accounts(kind, &file);
        out.push(LogoutProviderChoice {
            id: kind.id().to_owned(),
            name: kind.label().to_owned(),
            kind: "subscription".to_owned(),
            detail,
        });
    }
    if let Some(cfg) = cfg {
        for (name, entry) in &cfg.llm.providers {
            if entry.auth_mode.subscription().is_some()
                || entry.kind == ProviderKind::AgyAcp
                || entry.auth_mode == AuthMode::Agy
            {
                continue;
            }
            if out.iter().any(|c| c.id.eq_ignore_ascii_case(name)) {
                continue;
            }
            let (kind_tag, file_path) = if entry.auth_mode == AuthMode::OauthJson {
                (
                    "oauth",
                    entry.oauth.as_ref().map(|o| PathBuf::from(&o.token_file)),
                )
            } else {
                ("api_key", entry.token_file.as_ref().map(PathBuf::from))
            };
            if let Some(path) = file_path
                && check_deletable_credential_path(&path, &roots).is_ok()
                && path.is_file()
            {
                out.push(LogoutProviderChoice {
                    id: name.clone(),
                    name: name.clone(),
                    kind: kind_tag.to_owned(),
                    detail: path.display().to_string(),
                });
            }
        }
    }
    out
}

fn describe_subscription_accounts(kind: Subscription, file: &Path) -> String {
    let statuses = subscription_accounts_status(kind, file);
    let labels: Vec<String> = statuses
        .into_iter()
        .filter_map(|s| match s {
            SubscriptionStatus::LoggedIn { account, .. } => {
                Some(account.unwrap_or_else(|| "default".to_owned()))
            }
            SubscriptionStatus::Missing | SubscriptionStatus::Invalid(_) => None,
        })
        .collect();
    if labels.is_empty() {
        file.display().to_string()
    } else {
        labels.join(", ")
    }
}

/// Delete the stored credential file for `provider` after passing path confinement checks.
pub(crate) fn remove_provider_credentials(
    cfg: Option<&Config>,
    config_path: &Path,
    auth_dir: &Path,
    provider: &str,
) -> Result<String> {
    let roots = harness_credential_roots(config_path, auth_dir);
    let target = provider.trim();
    if target.eq_ignore_ascii_case("agy") {
        return Ok(
            "Antigravity CLI (`agy`) manages its own credentials; `hrn` stores no tokens. \
             Run `agy /logout` to sign out."
                .to_owned(),
        );
    }
    if let Some(kind) = Subscription::from_id(target) {
        let file = auth_dir.join(kind.file_name());
        return delete_guarded_file(&file, &roots, kind.label());
    }
    if let Some(cfg) = cfg
        && let Some((name, entry)) = cfg
            .llm
            .providers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(target))
    {
        if entry.kind == ProviderKind::AgyAcp || entry.auth_mode == AuthMode::Agy {
            return Ok(format!(
                "provider `{name}` delegates authentication to the `agy` CLI (`hrn` stores no \
                 tokens); run `agy /logout` to sign out."
            ));
        }
        if let Some(kind) = entry.auth_mode.subscription() {
            let file = entry
                .token_file
                .as_deref()
                .map_or_else(|| auth_dir.join(kind.file_name()), PathBuf::from);
            return delete_guarded_file(&file, &roots, kind.label());
        }
        let path = if entry.auth_mode == AuthMode::OauthJson {
            entry.oauth.as_ref().map(|o| PathBuf::from(&o.token_file))
        } else {
            entry.token_file.as_ref().map(PathBuf::from)
        };
        let Some(file) = path else {
            return Err(HrnError::Config(format!(
                "provider `{name}` has no removable on-disk credential file"
            )));
        };
        return delete_guarded_file(&file, &roots, name);
    }
    Err(HrnError::Config(format!(
        "no stored credentials found for `{target}`"
    )))
}

/// Guard-checked file deletion (never touches the filesystem if `check_deletable_credential_path`
/// rejects `file`).
pub(crate) fn delete_guarded_file(
    file: &Path,
    allowed_roots: &[PathBuf],
    label: &str,
) -> Result<String> {
    check_deletable_credential_path(file, allowed_roots)?;
    match std::fs::remove_file(file) {
        Ok(()) => Ok(format!(
            "Logged out of {label} (removed {})",
            file.display()
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(format!("Not logged in to {label}")),
        Err(e) => Err(HrnError::Io(format!("remove `{}`: {e}", file.display()))),
    }
}

/// Execute CLI `hrn logout [provider]` with interactive picker & confirmation when omitted.
pub(crate) fn run_cli(config_path: &Path, auth_dir: &Path, provider: Option<&str>) -> Result {
    let cfg = config_path
        .is_file()
        .then(|| Config::load(config_path).ok())
        .flatten();
    if let Some(target) = provider {
        let msg = remove_provider_credentials(cfg.as_ref(), config_path, auth_dir, target)?;
        out(&format!("{msg}\n"));
        return Ok(());
    }
    let choices = list_logout_choices(cfg.as_ref(), config_path, auth_dir);
    if choices.is_empty() {
        out("No stored credentials found.\n");
        return Ok(());
    }
    out("Stored credentials:\n\n");
    for (idx, c) in choices.iter().enumerate() {
        out(&format!(
            "  {}) {:<18} [{}]  {}\n",
            idx + 1,
            c.name,
            c.kind,
            c.detail
        ));
    }
    let raw = prompt_line(&format!(
        "\nSelect provider to log out [1-{}] (or name): ",
        choices.len()
    ))?;
    if raw.is_empty() {
        return Err(HrnError::Cancelled);
    }
    let picked = raw
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| choices.get(i))
        .or_else(|| {
            choices
                .iter()
                .find(|c| c.id.eq_ignore_ascii_case(&raw) || c.name.eq_ignore_ascii_case(&raw))
        })
        .ok_or_else(|| HrnError::Config(format!("invalid selection `{raw}`")))?;
    let confirm = prompt_line(&format!("Remove credentials for {}? [y/N]: ", picked.name))?;
    if !confirm.eq_ignore_ascii_case("y") && !confirm.eq_ignore_ascii_case("yes") {
        return Err(HrnError::Cancelled);
    }
    let msg = remove_provider_credentials(cfg.as_ref(), config_path, auth_dir, &picked.id)?;
    out(&format!("{msg}\n"));
    Ok(())
}

fn prompt_line(label: &str) -> Result<String> {
    out(label);
    let _ = io::stdout().flush();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| HrnError::Io(format!("read stdin: {e}")))?;
    Ok(line.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logout_respects_token_path_guard() {
        let root = std::env::temp_dir().join(format!("hrn-logout-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let auth_dir = root.join(".harness").join("auth");
        let cfg_path = root.join(".harness").join("config.toml");
        std::fs::create_dir_all(&auth_dir).expect("auth_dir");
        let roots = harness_credential_roots(&cfg_path, &auth_dir);
        for rel in [
            ".claude/credentials.json",
            ".codex/auth.json",
            ".gemini/oauth_creds.json",
            ".omp/agent/auth-gateway-tokens/tok.json",
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, "keep-me").expect("seed");
            assert!(check_deletable_credential_path(&p, &roots).is_err());
            assert!(delete_guarded_file(&p, &roots, "external").is_err());
            assert!(
                p.is_file(),
                "external file must stay untouched: {}",
                p.display()
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn logout_confines_deletion_to_harness_roots() {
        let root = std::env::temp_dir().join(format!("hrn-logout-confine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let harness_dir = root.join("home").join(".harness");
        let auth_dir = harness_dir.join("auth");
        let keys_dir = harness_dir.join("keys");
        let cfg_path = harness_dir.join("config.toml");
        let ssh_key = root.join("home").join(".ssh").join("fake_id_rsa");
        let workspace_file = root.join("workspace").join("secret.txt");
        for d in [
            &auth_dir,
            &keys_dir,
            ssh_key.parent().unwrap(),
            workspace_file.parent().unwrap(),
        ] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        std::fs::write(&ssh_key, "private-key").expect("ssh");
        std::fs::write(&workspace_file, "important").expect("ws");
        let auth_file = auth_dir.join("claude.json");
        let key_file = keys_dir.join("openai.key");
        std::fs::write(&auth_file, "{}").expect("auth");
        std::fs::write(&key_file, "sk-test\n").expect("key");

        let roots = harness_credential_roots(&cfg_path, &auth_dir);

        // Out-of-root files and `..` traversal escaping `auth_dir` MUST be refused.
        assert!(delete_guarded_file(&ssh_key, &roots, "ssh").is_err());
        assert!(delete_guarded_file(&workspace_file, &roots, "ws").is_err());
        let traversal = auth_dir
            .join("..")
            .join("..")
            .join(".ssh")
            .join("fake_id_rsa");
        assert!(delete_guarded_file(&traversal, &roots, "trav").is_err());
        assert!(ssh_key.is_file() && workspace_file.is_file());

        // Provider configured with an out-of-root `api_key_file` is neither listed nor deleted.
        let ssh_norm = ssh_key.to_string_lossy().replace('\\', "/");
        let key_norm = key_file.to_string_lossy().replace('\\', "/");
        let cfg = Config::parse_with_home(
            &format!(
                "[[providers]]\nname = \"evil\"\nkind = \"openai_chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key_file = \"{ssh_norm}\"\n\
                 [[providers]]\nname = \"good\"\nkind = \"openai_chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key_file = \"{key_norm}\"\n\
                 [models.m]\nprovider = \"good\"\nid = \"m\"\ncontext_window = 8192\n[roles]\ndefault = \"m\"\n"
            ),
            Some(&root.join("home")),
        )
        .expect("cfg");
        let listed: Vec<String> = list_logout_choices(Some(&cfg), &cfg_path, &auth_dir)
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert!(
            !listed.contains(&"evil".to_owned()),
            "out-of-root must not be listed: {listed:?}"
        );
        assert!(listed.contains(&"good".to_owned()) && listed.contains(&"claude".to_owned()));
        assert!(remove_provider_credentials(Some(&cfg), &cfg_path, &auth_dir, "evil").is_err());
        assert!(ssh_key.is_file(), "out-of-root key must remain untouched");

        // Legitimate files inside `<config_dir>/keys/` and `auth_dir` delete cleanly.
        remove_provider_credentials(Some(&cfg), &cfg_path, &auth_dir, "good").expect("delete key");
        remove_provider_credentials(Some(&cfg), &cfg_path, &auth_dir, "claude")
            .expect("delete auth");
        assert!(!key_file.exists() && !auth_file.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
