//! `hrn setup`: 3-step CLI first-run onboarding wizard sharing state, choices, and TOML writes
//! with the TUI `/setup` wizard (1. Login/Provider -> 2. Default Role -> 3. Theme).

use crate::auth;
use crate::cli::LoginArgs;
use crate::config::Config;
use crate::term::out;
use crate::toml_write;
use hrn_kernel::{HrnError, Result};
use hrn_tui::SetupWizardInfo;
use std::io::{self, Write};
use std::path::Path;

/// Collect current onboarding state shared by TUI `/setup` and CLI `hrn setup`.
pub(crate) fn collect_setup_info(
    cfg: Option<&Config>,
    auth_dir: &Path,
    themes_dir: Option<&Path>,
) -> SetupWizardInfo {
    let mut configured = Vec::new();
    for choice in crate::setup::provider_choices_in(cfg, Some(auth_dir)) {
        if choice.configured {
            configured.push(choice.id);
        }
    }
    let model_aliases = cfg.map_or_else(Vec::new, |c| {
        hrn_edge_llm::enabled_presets(&c.llm)
            .into_iter()
            .map(|(alias, _)| alias.to_owned())
            .collect()
    });
    let current_default = cfg.and_then(|c| c.llm.roles.get("default").cloned());
    let current_theme = cfg.map_or_else(
        || "titanium".to_owned(),
        |c| {
            let tone = match c.appearance.theme.current {
                hrn_tui::ThemeMode::Light => hrn_tui::TerminalTone::Light,
                hrn_tui::ThemeMode::Dark | hrn_tui::ThemeMode::Auto => hrn_tui::TerminalTone::Dark,
            };
            c.appearance.theme.resolve_name(tone).to_owned()
        },
    );
    SetupWizardInfo {
        configured_providers: configured,
        model_aliases,
        current_default,
        themes: hrn_tui::available_theme_names_in(themes_dir),
        current_theme,
    }
}

/// Persist `[tui] theme = <theme>` (or update `[tui.theme]` when already structured) via
/// comment-preserving `toml_write`, validating against built-ins, `themes_dir`, and file paths.
pub(crate) fn persist_theme_in(
    config_path: &Path,
    theme: &str,
    themes_dir: Option<&Path>,
) -> Result<String> {
    let trimmed = theme.trim();
    if trimmed.is_empty() {
        return Err(HrnError::Config("theme name cannot be empty".into()));
    }
    if !hrn_tui::is_known_theme(trimmed, themes_dir) {
        let available = hrn_tui::available_theme_names_in(themes_dir).join(", ");
        return Err(HrnError::Config(format!(
            "unknown theme `{trimmed}` (available: {available})"
        )));
    }
    let existing_doc = toml_write::load_doc(config_path)?;
    let structured_slot = existing_doc
        .get("tui")
        .and_then(toml_edit::Item::as_table)
        .and_then(|tui| tui.get("theme"))
        .and_then(toml_edit::Item::as_table)
        .map(|tbl| {
            let cur = tbl.get("current").and_then(toml_edit::Item::as_str);
            if cur.is_some_and(|c| c.eq_ignore_ascii_case("light")) {
                "tui.theme.light"
            } else {
                "tui.theme.dark"
            }
        });
    let key = structured_slot.unwrap_or("tui.theme");
    toml_write::set_key(config_path, key, &toml::Value::String(trimmed.to_owned()))?;
    Ok(format!(
        "Saved theme `{trimmed}` ({key}) to {}",
        config_path.display()
    ))
}

/// Persist `[tui] theme = <theme>` via comment-preserving `toml_write`.
pub(crate) fn persist_theme(config_path: &Path, theme: &str) -> Result<String> {
    let themes_dir = crate::app_data_dir().ok().map(|d| d.join("themes"));
    persist_theme_in(config_path, theme, themes_dir.as_deref())
}

/// Persist `[roles] default = <alias>` via comment-preserving `toml_write`.
pub(crate) fn persist_default_role(config_path: &Path, alias: &str) -> Result<String> {
    let trimmed = alias.trim();
    if trimmed.is_empty() {
        return Err(HrnError::Config(
            "default role alias cannot be empty".into(),
        ));
    }
    toml_write::set_key(
        config_path,
        "roles.default",
        &toml::Value::String(trimmed.to_owned()),
    )?;
    Ok(format!(
        "Saved [roles] default = `{trimmed}` to {}",
        config_path.display()
    ))
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

/// Execute `hrn setup`: 3-step interactive onboarding wizard.
pub(crate) async fn run_cli(config_path: &Path, auth_dir: &Path) -> Result {
    out("hrn first-run setup (3 steps: login -> default role -> theme)\n\n");

    let themes_dir = crate::app_data_dir().ok().map(|d| d.join("themes"));

    // Step 1: Login / Provider
    let existing = Config::load(config_path).ok();
    let info1 = collect_setup_info(existing.as_ref(), auth_dir, themes_dir.as_deref());
    out("Step 1/3 · Provider login\n");
    if !info1.configured_providers.is_empty() {
        out(&format!(
            "  Currently configured: {}\n",
            info1.configured_providers.join(", ")
        ));
    }
    let ans = prompt_line("Configure or add a provider now? [y/N]: ")?;
    if ans.eq_ignore_ascii_case("y") || ans.eq_ignore_ascii_case("yes") {
        let login_args = LoginArgs {
            provider: None,
            manual: false,
            no_browser: false,
            method: None,
        };
        auth::login(config_path, auth_dir, &login_args).await?;
    }

    // Step 2: Default Role
    let refreshed = Config::load(config_path).ok();
    let info2 = collect_setup_info(refreshed.as_ref(), auth_dir, themes_dir.as_deref());
    out("\nStep 2/3 · Default model role\n");
    if info2.model_aliases.is_empty() {
        out("  No models configured yet; skipping default role.\n");
    } else {
        for (idx, alias) in info2.model_aliases.iter().enumerate() {
            let mark = if info2.current_default.as_deref() == Some(alias.as_str()) {
                " (current)"
            } else {
                ""
            };
            out(&format!("  {}) {alias}{mark}\n", idx + 1));
        }
        let raw = prompt_line(&format!(
            "Select default model [1-{}] (Enter to keep current): ",
            info2.model_aliases.len()
        ))?;
        if let Some(chosen) = pick_choice(&raw, &info2.model_aliases) {
            let msg = persist_default_role(config_path, chosen)?;
            out(&format!("  {msg}\n"));
        }
    }

    // Step 3: Theme
    out("\nStep 3/3 · Appearance theme\n");
    for (idx, name) in info2.themes.iter().enumerate() {
        let mark = if *name == info2.current_theme {
            " (current)"
        } else {
            ""
        };
        out(&format!("  {}) {name}{mark}\n", idx + 1));
    }
    let raw = prompt_line(&format!(
        "Select theme [1-{}] (Enter to keep `{}`): ",
        info2.themes.len(),
        info2.current_theme
    ))?;
    if let Some(chosen) = pick_choice(&raw, &info2.themes) {
        let msg = persist_theme(config_path, chosen)?;
        out(&format!("  {msg}\n"));
    }

    out("\nSetup complete! Launch `hrn` to start chatting.\n");
    Ok(())
}

fn pick_choice<'a>(raw: &str, options: &'a [String]) -> Option<&'a str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(n) = trimmed.parse::<usize>()
        && let Some(val) = n.checked_sub(1).and_then(|i| options.get(i))
    {
        return Some(val.as_str());
    }
    options
        .iter()
        .find(|o| o.eq_ignore_ascii_case(trimmed))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_persists_role_and_theme_preserving_comments() {
        let dir = std::env::temp_dir().join(format!("hrn-setup-wiz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "# keep user note\nthinking = \"medium\"\n").expect("seed");

        persist_default_role(&cfg_path, "sonnet").expect("role");
        persist_theme(&cfg_path, "titanium-vivid").expect("theme");
        let saved = std::fs::read_to_string(&cfg_path).expect("read");
        assert!(saved.contains("# keep user note"), "{saved}");
        assert!(saved.contains("default = \"sonnet\""), "{saved}");
        assert!(saved.contains("theme = \"titanium-vivid\""), "{saved}");
        assert!(persist_theme(&cfg_path, "nonexistent-theme").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
