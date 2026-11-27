//! Interactive provider & API-key setup shared by `hrn login` (CLI) and `/login` (TUI).
//!
//! Writes API keys to `~/.harness/keys/<provider>.key` (never echoed) and merges provider +
//! model presets into `config.toml` without clobbering existing user configuration.

use crate::auth;
use crate::config::Config;
use crate::probe::ProbeTarget;
use hrn_edge_http::{HttpClient, guard};
use hrn_edge_llm::ProviderKind;
use hrn_edge_oauth::{Subscription, SubscriptionStatus, subscription_status, write_bytes_atomic};
use hrn_kernel::{HrnError, Result};
use hrn_tui::{LoginOutcome, LoginProviderChoice, LoginSubmission};
use std::path::{Path, PathBuf};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value as EditValue};

/// Blocked Google / Antigravity OAuth aliases that must always be rejected.
const BLOCKED_GOOGLE_OAUTH: &[&str] = &[
    "google",
    "antigravity",
    "google-antigravity",
    "gemini-cli",
    "gemini-oauth",
    "code-assist",
    "cloudcode",
];

/// Reject blocked Google/Antigravity OAuth names everywhere (`/login`, `hrn login`, `/setup`).
pub(crate) fn reject_google_oauth(name: &str) -> Result {
    let lower = name.trim().to_ascii_lowercase();
    if BLOCKED_GOOGLE_OAUTH.contains(&lower.as_str()) {
        return Err(HrnError::Config(
            "Google / Antigravity OAuth is blocked by ToS guard; use `gemini` with an official \
             Gemini API key"
                .into(),
        ));
    }
    Ok(())
}

#[path = "setup_templates.rs"]
mod templates;
use templates::{ProviderTemplate, TEMPLATES};

/// List all interactive `/login` provider options, marking which ones already have working auth.
pub(crate) fn provider_choices_in(
    cfg: Option<&Config>,
    auth_dir: Option<&Path>,
) -> Vec<LoginProviderChoice> {
    let mut out = Vec::new();
    for kind in Subscription::ALL {
        let configured = auth_dir.is_some_and(|dir| {
            matches!(
                subscription_status(kind, &dir.join(kind.file_name())),
                SubscriptionStatus::LoggedIn { .. }
            )
        });
        let hint = if kind.is_device_code() {
            "in-TUI RFC 8628 device-code login"
        } else {
            "in-TUI browser OAuth login"
        };
        out.push(LoginProviderChoice {
            id: kind.id().to_owned(),
            name: format!("{} (subscription)", kind.label()),
            hint: hint.to_owned(),
            configured,
            needs_custom_fields: false,
            default_base_url: String::new(),
            default_model: String::new(),
            key_optional: true,
            is_oauth: true,
        });
    }
    for t in TEMPLATES {
        let configured = cfg.is_some_and(|c| {
            c.llm
                .providers
                .get(t.id)
                .is_some_and(|entry| auth::provider_key_report(entry).is_ok())
        });
        out.push(LoginProviderChoice {
            id: t.id.to_owned(),
            name: t.name.to_owned(),
            hint: t.hint.to_owned(),
            configured,
            needs_custom_fields: t.needs_custom_fields,
            default_base_url: t.base_url.to_owned(),
            default_model: t
                .presets
                .first()
                .map_or_else(String::new, |p| p.model_id.to_owned()),
            key_optional: t.key_optional,
            is_oauth: false,
        });
    }
    if let Some(cfg) = cfg {
        for name in cfg.oauth_logins.keys() {
            if out.iter().any(|p| p.id == *name) {
                continue;
            }
            let configured = cfg
                .llm
                .providers
                .get(name)
                .is_some_and(|entry| auth::provider_key_report(entry).is_ok());
            out.push(LoginProviderChoice {
                id: name.clone(),
                name: format!("{name} (OAuth)"),
                hint: "in-TUI browser OAuth2 PKCE login".to_owned(),
                configured,
                needs_custom_fields: false,
                default_base_url: String::new(),
                default_model: String::new(),
                key_optional: true,
                is_oauth: true,
            });
        }
    }
    out
}

/// Format a clean non-secret auth status report for `/auth` in the TUI.
pub(crate) fn format_auth_status(cfg: &Config) -> String {
    if cfg.llm.providers.is_empty() {
        return "no providers configured · run /login to add one".to_owned();
    }
    let mut lines = Vec::with_capacity(cfg.llm.providers.len() + 1);
    lines.push("providers:".to_owned());
    for (name, entry) in &cfg.llm.providers {
        let kind = match entry.kind {
            ProviderKind::OpenaiChat => "openai_chat",
            ProviderKind::OpenaiResponses => "openai_responses",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
            ProviderKind::AgyAcp => "agy_acp",
        };
        let state = match auth::provider_key_report(entry) {
            Ok(ok) => format!("ok ({ok})"),
            Err(err) => format!("missing ({err})"),
        };
        lines.push(format!("  {name} [{kind}] · {state}"));
    }
    lines.join("\n")
}

fn template_kind(raw: &str) -> ProviderKind {
    match raw {
        "anthropic" => ProviderKind::Anthropic,
        "openai_responses" => ProviderKind::OpenaiResponses,
        "gemini" => ProviderKind::Gemini,
        _ => ProviderKind::OpenaiChat,
    }
}

/// Build the validation [`ProbeTarget`] for `sub` before touching disk.
pub(crate) fn probe_target_for(
    sub: &LoginSubmission,
    allow_plaintext_http: bool,
) -> Result<ProbeTarget> {
    reject_google_oauth(&sub.provider_id)?;
    let template = TEMPLATES
        .iter()
        .find(|t| t.id.eq_ignore_ascii_case(&sub.provider_id))
        .ok_or_else(|| HrnError::Config(format!("unknown provider `{}`", sub.provider_id)))?;
    let api_key = sub.api_key.trim();
    if api_key.is_empty() && !template.key_optional {
        return Err(HrnError::Config(format!(
            "API key is required for {}",
            template.name
        )));
    }
    let base_url = sub
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(template.base_url);
    Ok(ProbeTarget {
        provider_id: template.id.to_owned(),
        kind: template_kind(template.kind),
        base_url: base_url.to_owned(),
        api_key: api_key.to_owned(),
        allow_plaintext_http,
    })
}

/// Read `[llm] allow_plaintext_http` directly from a global `config.toml` even before `[roles]`
/// exists on a fresh machine.
pub(crate) fn read_allow_plaintext_http(config_path: &Path) -> bool {
    std::fs::read_to_string(config_path)
        .ok()
        .and_then(|raw| toml::from_str::<toml::Table>(&raw).ok())
        .and_then(|tbl| {
            tbl.get("llm")
                .and_then(toml::Value::as_table)
                .and_then(|llm| llm.get("allow_plaintext_http"))
                .and_then(toml::Value::as_bool)
        })
        .unwrap_or(false)
}

/// Validate `sub` against the upstream endpoint first; write key + config ONLY when the probe
/// succeeds. On probe failure, NOTHING is written to disk.
pub(crate) async fn probe_and_apply_login(
    http: &HttpClient,
    config_path: &Path,
    home: Option<&Path>,
    sub: &LoginSubmission,
    allow_plaintext_http: bool,
) -> Result<(Config, LoginOutcome)> {
    let target = probe_target_for(sub, allow_plaintext_http)?;
    crate::probe::probe_api_key(http, &target).await?;
    apply_login(config_path, home, sub)
}

/// Persist credentials and merge provider + presets into `config_path`, validating before saving.
pub(crate) fn apply_login(
    config_path: &Path,
    home: Option<&Path>,
    sub: &LoginSubmission,
) -> Result<(Config, LoginOutcome)> {
    reject_google_oauth(&sub.provider_id)?;
    let template = TEMPLATES
        .iter()
        .find(|t| t.id.eq_ignore_ascii_case(&sub.provider_id))
        .ok_or_else(|| HrnError::Config(format!("unknown provider `{}`", sub.provider_id)))?;

    let provider_name = if template.needs_custom_fields {
        sanitize_name(sub.custom_name.as_deref().unwrap_or("custom"))
    } else {
        template.id.to_owned()
    };
    let api_key = sub.api_key.trim();
    if api_key.is_empty() && !template.key_optional {
        return Err(HrnError::Config(format!(
            "API key is required for {}",
            template.name
        )));
    }
    let base_url = sub
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(template.base_url);

    let key_file_path =
        (!api_key.is_empty()).then(|| key_file_location(config_path, home, &provider_name));
    if let Some(path) = &key_file_path {
        guard::check_token_path(&path.to_string_lossy())?;
    }

    let mut doc = crate::toml_write::load_doc(config_path)?;
    upsert_provider(
        &mut doc,
        &provider_name,
        template.kind,
        base_url,
        template.strip_temperature,
        key_file_path.as_deref(),
    );

    let aliases = upsert_models(&mut doc, template, &provider_name, sub.model_id.as_deref());
    ensure_roles_and_cycle(&mut doc, &aliases);

    let serialized = doc.to_string();
    let parsed =
        Config::parse_with_catalogs(&serialized, home, crate::config::model_catalogs(home))?;
    if let Some(path) = &key_file_path {
        write_secret_file(path, api_key)?;
    }
    crate::toml_write::atomic_write(config_path, &serialized)?;

    let summary = format!(
        "configured `{provider_name}` ({}) · models: {} · switch with /model or ctrl+p",
        config_path.display(),
        aliases.join(", ")
    );
    Ok((
        parsed,
        LoginOutcome {
            provider: provider_name,
            aliases,
            summary,
        },
    ))
}

fn sanitize_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "custom".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn key_file_location(config_path: &Path, home: Option<&Path>, provider: &str) -> PathBuf {
    let base = config_path
        .parent()
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".harness")))
        .unwrap_or_else(|| PathBuf::from(".harness"));
    base.join("keys").join(format!("{provider}.key"))
}

fn write_secret_file(path: &Path, secret: &str) -> Result {
    guard::check_token_path(&path.to_string_lossy())?;
    write_bytes_atomic(path, format!("{secret}\n").as_bytes())
}

fn upsert_provider(
    doc: &mut DocumentMut,
    name: &str,
    kind: &str,
    base_url: &str,
    strip_temperature: bool,
    key_file: Option<&Path>,
) {
    let entry = build_provider_table(name, kind, base_url, strip_temperature, key_file);
    let root = doc.as_table_mut();
    if !root.get("providers").is_some_and(Item::is_array_of_tables) {
        root.insert("providers", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let Some(arr) = root
        .get_mut("providers")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return;
    };
    if let Some(existing) = arr
        .iter_mut()
        .find(|t| t.get("name").and_then(Item::as_str) == Some(name))
    {
        *existing = entry;
    } else {
        arr.push(entry);
    }
}

fn build_provider_table(
    name: &str,
    kind: &str,
    base_url: &str,
    strip_temperature: bool,
    key_file: Option<&Path>,
) -> Table {
    let mut t = Table::new();
    t.insert("name", Item::Value(EditValue::from(name)));
    t.insert("kind", Item::Value(EditValue::from(kind)));
    t.insert("base_url", Item::Value(EditValue::from(base_url)));
    if let Some(path) = key_file {
        let normalized = path.to_string_lossy().replace('\\', "/");
        t.insert("api_key_file", Item::Value(EditValue::from(normalized)));
    }
    if strip_temperature {
        t.insert("strip_temperature", Item::Value(EditValue::from(true)));
    }
    t
}

fn upsert_models(
    doc: &mut DocumentMut,
    template: &ProviderTemplate,
    provider_name: &str,
    model_override: Option<&str>,
) -> Vec<String> {
    let root = doc.as_table_mut();
    if !root.get("models").is_some_and(Item::is_table) {
        let mut tbl = Table::new();
        tbl.set_implicit(true);
        root.insert("models", Item::Table(tbl));
    }
    let Some(models) = root.get_mut("models").and_then(Item::as_table_mut) else {
        return Vec::new();
    };
    let mut added = Vec::new();
    let custom_model = model_override.map(str::trim).filter(|s| !s.is_empty());
    if template.needs_custom_fields {
        let model_id = custom_model.unwrap_or("qwen2.5-coder:latest");
        let alias = provider_name.to_owned();
        models.insert(
            &alias,
            Item::Table(model_table(provider_name, model_id, 128_000)),
        );
        added.push(alias);
        return added;
    }
    for (idx, preset) in template.presets.iter().enumerate() {
        let model_id = if idx == 0 {
            custom_model.unwrap_or(preset.model_id)
        } else {
            preset.model_id
        };
        models.insert(
            preset.alias,
            Item::Table(model_table(provider_name, model_id, preset.context_window)),
        );
        added.push(preset.alias.to_owned());
    }
    added
}

fn model_table(provider: &str, model_id: &str, context_window: i64) -> Table {
    let mut t = Table::new();
    t.insert("provider", Item::Value(EditValue::from(provider)));
    t.insert("id", Item::Value(EditValue::from(model_id)));
    t.insert(
        "context_window",
        Item::Value(EditValue::from(context_window)),
    );
    t
}

fn ensure_roles_and_cycle(doc: &mut DocumentMut, aliases: &[String]) {
    let Some(first) = aliases.first() else {
        return;
    };
    let root = doc.as_table_mut();
    if !root.get("roles").is_some_and(Item::is_table) {
        root.insert("roles", Item::Table(Table::new()));
    }
    if let Some(roles) = root.get_mut("roles").and_then(Item::as_table_mut)
        && !roles.contains_key("default")
    {
        roles.insert("default", Item::Value(EditValue::from(first.as_str())));
    }
    if let Some(cycle) = root.get_mut("cycle").and_then(Item::as_array_mut) {
        for alias in aliases {
            if !cycle.iter().any(|v| v.as_str() == Some(alias.as_str())) {
                cycle.push(alias.as_str());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_merges_config_with_key_file() {
        let dir = std::env::temp_dir().join(format!("hrn_setup_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "# keep this user comment\nthinking = \"high\"\n").expect("seed");

        let (cfg1, out1) = apply_login(
            &cfg_path,
            Some(&dir),
            &LoginSubmission {
                provider_id: "anthropic".into(),
                custom_name: None,
                api_key: "sk-ant-test-123".into(),
                base_url: None,
                model_id: None,
            },
        )
        .expect("anthropic setup");
        assert_eq!(out1.provider, "anthropic");
        assert!(cfg1.llm.presets.contains_key("sonnet"));
        assert!(cfg1.llm.presets.contains_key("opus"));
        assert_eq!(
            cfg1.llm.roles.get("default").map(String::as_str),
            Some("sonnet")
        );

        let (cfg2, out2) = apply_login(
            &cfg_path,
            Some(&dir),
            &LoginSubmission {
                provider_id: "custom".into(),
                custom_name: Some("ollama".into()),
                api_key: String::new(),
                base_url: Some("http://localhost:11434/v1".into()),
                model_id: Some("llama3.3".into()),
            },
        )
        .expect("ollama setup");
        assert_eq!(out2.provider, "ollama");
        assert!(cfg2.llm.presets.contains_key("sonnet"));
        assert!(cfg2.llm.presets.contains_key("ollama"));
        assert_eq!(
            cfg2.llm.roles.get("default").map(String::as_str),
            Some("sonnet")
        );
        let saved = std::fs::read_to_string(&cfg_path).expect("read saved");
        assert!(saved.contains("# keep this user comment"), "{saved}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
