//! Explicit read-only importer for OMP providers, models, and config (`hrn providers import omp`).
//!
//! Reads `<omp_dir>/models.yml`, `<omp_dir>/config.yml`, `<omp_dir>/.env`, and explicitly
//! referenced `<omp_dir>/auth-gateway-tokens/<file>` entries (refusing symlinks and forbidden
//! consumer CLI roots), then merges them into `<hrn_dir>/models.yml`, `<hrn_dir>/.env`,
//! `<hrn_dir>/auth-gateway-tokens/`, and `<config_path>` (`config.toml`).
//!
//! Security invariants:
//! - Never reads `agent.db*`, `models.db`, `history.db`, `transcripts.db`, `auth.json`, or
//!   `.credentials.json`.
//! - Never writes plaintext secrets into `models.yml` or `config.toml` (literal `apiKey` values
//!   are moved into `<hrn_dir>/.env` under `<PROVIDER>_API_KEY`).
//! - Blocks Google/Antigravity `OAuth` and unsupported `OAuth` protocols.

use crate::cli::{ProvidersImportArgs, ProvidersImportSource};
use crate::config_layers::insert_dotted;
use crate::omp_config::parse_omp_config_yaml;
use crate::toml_write::{atomic_write, load_doc};
use hrn_edge_llm::yaml::{Yaml, parse as parse_yaml};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_kernel::{HrnError, Result, ThinkingLevel, WorkspaceFs as _};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "omp_import_io.rs"]
mod io;

use io::{
    atomic_write_secret, canonicalize_omp_dir, collect_toml_leaves, derive_api_key_env_name,
    emit_yaml, extract_gateway_token_file, has_toml_path, map_entries, parse_dotenv_map,
    parse_env_var_name, read_confined_file, render_merged_dotenv, render_report, set_toml_path,
};

/// Summary of an OMP provider/config import (or `--dry-run` preview).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImportReport {
    pub(crate) source_dir: PathBuf,
    pub(crate) dry_run: bool,
    pub(crate) providers_imported: Vec<String>,
    pub(crate) providers_skipped: Vec<(String, String)>,
    pub(crate) oauth_providers: Vec<String>,
    pub(crate) tokens_imported: Vec<String>,
    pub(crate) tokens_skipped: Vec<String>,
    pub(crate) env_imported: Vec<String>,
    pub(crate) env_skipped: Vec<String>,
    pub(crate) tags_imported: Vec<String>,
    pub(crate) config_imported: Vec<String>,
    pub(crate) config_skipped: Vec<String>,
}

/// CLI entrypoint for `hrn providers import omp`.
pub(crate) fn run_cli(args: &ProvidersImportArgs, hrn_dir: &Path, config_path: &Path) -> Result {
    let omp_dir = match args.source {
        ProvidersImportSource::Omp => match args.dir.clone() {
            Some(dir) => dir,
            None => hrn_edge_store::default_omp_dir(StdWorkspaceFs.home_dir().as_deref())
                .ok_or_else(|| {
                    HrnError::Config(
                        "cannot locate home directory; pass `--dir <path>` to OMP agent dir"
                            .to_owned(),
                    )
                })?,
        },
    };
    let report = import_from_dir(&omp_dir, hrn_dir, config_path, args.dry_run, args.force)?;
    crate::term::out(&render_report(&report));
    Ok(())
}

/// Import providers, model tags, referenced secrets, and config keys from `omp_dir`.
pub(crate) fn import_from_dir(
    omp_dir: &Path,
    hrn_dir: &Path,
    config_path: &Path,
    dry_run: bool,
    force: bool,
) -> Result<ImportReport> {
    let omp_canon = canonicalize_omp_dir(omp_dir)?;
    let models_raw = read_confined_file(&omp_canon, Path::new("models.yml"))?;
    let config_raw = read_confined_file(&omp_canon, Path::new("config.yml"))?;
    if models_raw.is_none() && config_raw.is_none() {
        return Err(HrnError::Config(format!(
            "no `models.yml` or `config.yml` found in `{}`",
            omp_canon.display()
        )));
    }
    let src_env = read_confined_file(&omp_canon, Path::new(".env"))?
        .map(|s| parse_dotenv_map(&s))
        .unwrap_or_default();

    let mut report = ImportReport {
        source_dir: omp_canon.clone(),
        dry_run,
        ..ImportReport::default()
    };
    let mut queued_env = BTreeMap::new();
    let mut queued_tokens = BTreeMap::new();

    let config_doc = config_raw.as_deref().and_then(parse_yaml);
    let models_doc = models_raw.as_deref().and_then(parse_yaml);

    merge_models_yml(
        &omp_canon,
        hrn_dir,
        models_doc.as_ref(),
        config_doc.as_ref(),
        &src_env,
        force,
        dry_run,
        &mut queued_env,
        &mut queued_tokens,
        &mut report,
    )?;
    apply_token_copies(hrn_dir, &queued_tokens, force, dry_run, &mut report)?;
    apply_dotenv_merge(hrn_dir, &queued_env, force, dry_run, &mut report)?;
    if let Some(cfg_text) = config_raw.as_deref() {
        merge_config_toml(
            cfg_text,
            config_doc.as_ref(),
            config_path,
            force,
            dry_run,
            &mut report,
        )?;
    }
    Ok(report)
}

#[expect(
    clippy::too_many_arguments,
    reason = "single-pass provider/tag merge collecting queued secrets and report entries"
)]
fn merge_models_yml(
    omp_canon: &Path,
    hrn_dir: &Path,
    src_models: Option<&Yaml>,
    src_config: Option<&Yaml>,
    src_env: &BTreeMap<String, String>,
    force: bool,
    dry_run: bool,
    queued_env: &mut BTreeMap<String, String>,
    queued_tokens: &mut BTreeMap<String, String>,
    report: &mut ImportReport,
) -> Result {
    let dst_path = hrn_dir.join("models.yml");
    let dst_doc = if dst_path.is_file() {
        let raw = std::fs::read_to_string(&dst_path)
            .map_err(|e| HrnError::Config(format!("cannot read `{}`: {e}", dst_path.display())))?;
        parse_yaml(&raw)
    } else {
        None
    };

    let mut providers = map_entries(dst_doc.as_ref().and_then(|d| d.get("providers")));
    let mut tags = map_entries(dst_doc.as_ref().and_then(|d| d.get("modelTags")));
    let other_top: Vec<(String, Yaml)> = dst_doc
        .as_ref()
        .map_or(&[][..], Yaml::entries)
        .iter()
        .filter(|(k, _)| k != "providers" && k != "modelTags")
        .cloned()
        .collect();

    if let Some(src_providers) = src_models.and_then(|d| d.get("providers")) {
        for (name, node) in src_providers.entries() {
            match prepare_provider(
                omp_canon,
                name,
                node,
                src_env,
                queued_env,
                queued_tokens,
                report,
            )? {
                ProviderAction::Import(cleaned) => {
                    if let Some(pos) = providers.iter().position(|(k, _)| k == name) {
                        if !force {
                            report
                                .providers_skipped
                                .push((name.clone(), "already exists (use --force)".to_owned()));
                            continue;
                        }
                        if let Some(slot) = providers.get_mut(pos) {
                            slot.1 = cleaned;
                        }
                    } else {
                        providers.push((name.clone(), cleaned));
                    }
                    report.providers_imported.push(name.clone());
                }
                ProviderAction::Skip(reason) => {
                    report.providers_skipped.push((name.clone(), reason));
                }
            }
        }
    }

    for doc in [src_models, src_config].into_iter().flatten() {
        if let Some(src_tags) = doc.get("modelTags") {
            for (key, val) in src_tags.entries() {
                if let Some(pos) = tags.iter().position(|(k, _)| k == key) {
                    if !force {
                        continue;
                    }
                    if let Some(slot) = tags.get_mut(pos) {
                        slot.1 = val.clone();
                    }
                } else {
                    tags.push((key.clone(), val.clone()));
                }
                if !report.tags_imported.contains(key) {
                    report.tags_imported.push(key.clone());
                }
            }
        }
    }

    if !dry_run && (!report.providers_imported.is_empty() || !report.tags_imported.is_empty()) {
        let mut root = Vec::new();
        if !providers.is_empty() {
            root.push(("providers".to_owned(), Yaml::Map(providers)));
        }
        if !tags.is_empty() {
            root.push(("modelTags".to_owned(), Yaml::Map(tags)));
        }
        root.extend(other_top);
        let rendered = emit_yaml(&Yaml::Map(root), 0);
        atomic_write(&dst_path, &rendered)?;
    }
    Ok(())
}

enum ProviderAction {
    Import(Yaml),
    Skip(String),
}

fn prepare_provider(
    omp_canon: &Path,
    name: &str,
    node: &Yaml,
    src_env: &BTreeMap<String, String>,
    queued_env: &mut BTreeMap<String, String>,
    queued_tokens: &mut BTreeMap<String, String>,
    report: &mut ImportReport,
) -> Result<ProviderAction> {
    if node.get("models").map_or(&[][..], Yaml::items).is_empty() {
        return Ok(ProviderAction::Skip("override-only (no models)".to_owned()));
    }
    let Some(api) = node.get("api").and_then(Yaml::str) else {
        return Ok(ProviderAction::Skip("missing `api`".to_owned()));
    };
    let auth = node.get("auth").and_then(Yaml::str);
    let api_lower = api.to_ascii_lowercase();
    if api_lower.contains("antigravity")
        || api_lower.contains("gemini-cli")
        || api_lower.contains("cloudcode")
    {
        return Ok(ProviderAction::Skip(format!(
            "blocked provider api `{api}`"
        )));
    }
    let is_oauth = auth == Some("oauth");
    if is_oauth {
        if !matches!(
            api,
            "anthropic-messages"
                | "anthropic"
                | "openai-codex-responses"
                | "openai-codex"
                | "kimi-code"
        ) {
            return Ok(ProviderAction::Skip(format!(
                "unsupported or blocked OAuth api `{api}`"
            )));
        }
    } else if !matches!(
        api,
        "openai-completions"
            | "openai-chat"
            | "openai"
            | "openai-responses"
            | "anthropic-messages"
            | "anthropic"
            | "google-generative-ai"
            | "google"
            | "gemini"
    ) {
        return Ok(ProviderAction::Skip(format!("unsupported api `{api}`")));
    }

    let mut entries: Vec<(String, Yaml)> = Vec::new();
    for (k, v) in node.entries() {
        if k == "apiKey" {
            if is_oauth || auth == Some("none") {
                continue;
            }
            let Some(raw) = v.str() else {
                continue;
            };
            match normalize_api_key(omp_canon, name, raw, src_env, queued_env, queued_tokens)? {
                KeyAction::Rewrite(rewritten) => {
                    entries.push((k.clone(), Yaml::Scalar(rewritten)));
                }
                KeyAction::Skip(reason) => return Ok(ProviderAction::Skip(reason)),
            }
        } else {
            entries.push((k.clone(), v.clone()));
        }
    }

    if !is_oauth && auth != Some("none") {
        queue_provider_rule_envs(name, src_env, queued_env);
    }
    if is_oauth {
        report.oauth_providers.push(name.to_owned());
    }
    Ok(ProviderAction::Import(Yaml::Map(entries)))
}

enum KeyAction {
    Rewrite(String),
    Skip(String),
}

fn normalize_api_key(
    omp_canon: &Path,
    provider_name: &str,
    raw: &str,
    src_env: &BTreeMap<String, String>,
    queued_env: &mut BTreeMap<String, String>,
    queued_tokens: &mut BTreeMap<String, String>,
) -> Result<KeyAction> {
    if raw.starts_with('!') {
        let Some(file_name) = extract_gateway_token_file(raw) else {
            return Ok(KeyAction::Skip(
                "unsupported `!command` apiKey (only auth-gateway-tokens/* supported)".to_owned(),
            ));
        };
        let rel = Path::new("auth-gateway-tokens").join(&file_name);
        let Some(token_body) = read_confined_file(omp_canon, &rel)? else {
            return Ok(KeyAction::Skip(format!(
                "missing token file `auth-gateway-tokens/{file_name}`"
            )));
        };
        queued_tokens.insert(file_name.clone(), token_body);
        return Ok(KeyAction::Rewrite(format!(
            "!cat 'auth-gateway-tokens/{file_name}'"
        )));
    }
    if let Some(env_name) = parse_env_var_name(raw) {
        if let Some(val) = src_env.get(env_name) {
            queued_env.insert(env_name.to_owned(), val.clone());
        }
        return Ok(KeyAction::Rewrite(env_name.to_owned()));
    }
    let env_name = derive_api_key_env_name(provider_name);
    queued_env.insert(env_name.clone(), raw.to_owned());
    Ok(KeyAction::Rewrite(env_name))
}

fn queue_provider_rule_envs(
    name: &str,
    src_env: &BTreeMap<String, String>,
    queued_env: &mut BTreeMap<String, String>,
) {
    if let Some(rule) = hrn_edge_llm::rule_for(name) {
        for key in rule.key_envs.iter().chain(rule.base_url_envs.iter()) {
            if let Some(val) = src_env.get(key) {
                queued_env.entry(key.clone()).or_insert_with(|| val.clone());
            }
        }
    }
}

fn apply_token_copies(
    hrn_dir: &Path,
    queued_tokens: &BTreeMap<String, String>,
    force: bool,
    dry_run: bool,
    report: &mut ImportReport,
) -> Result {
    let dst_dir = hrn_dir.join("auth-gateway-tokens");
    for (file_name, content) in queued_tokens {
        let dst = dst_dir.join(file_name);
        if dst.exists() && !force {
            report.tokens_skipped.push(file_name.clone());
            continue;
        }
        if !dry_run {
            atomic_write_secret(&dst, content)?;
        }
        report.tokens_imported.push(file_name.clone());
    }
    Ok(())
}

fn apply_dotenv_merge(
    hrn_dir: &Path,
    queued_env: &BTreeMap<String, String>,
    force: bool,
    dry_run: bool,
    report: &mut ImportReport,
) -> Result {
    if queued_env.is_empty() {
        return Ok(());
    }
    let env_path = hrn_dir.join(".env");
    let existing_raw = if env_path.is_file() {
        std::fs::read_to_string(&env_path)
            .map_err(|e| HrnError::Io(format!("cannot read `{}`: {e}", env_path.display())))?
    } else {
        String::new()
    };
    let existing_map = parse_dotenv_map(&existing_raw);
    let mut updates = BTreeMap::new();
    for (key, val) in queued_env {
        if existing_map.contains_key(key) && !force {
            report.env_skipped.push(key.clone());
            continue;
        }
        updates.insert(key.clone(), val.clone());
        report.env_imported.push(key.clone());
    }
    if dry_run || updates.is_empty() {
        return Ok(());
    }
    let merged = render_merged_dotenv(&existing_raw, &updates);
    atomic_write_secret(&env_path, &merged)
}

fn merge_config_toml(
    cfg_text: &str,
    cfg_doc: Option<&Yaml>,
    config_path: &Path,
    force: bool,
    dry_run: bool,
    report: &mut ImportReport,
) -> Result {
    let mut tbl = parse_omp_config_yaml(cfg_text).unwrap_or_default();
    if let Some(doc) = cfg_doc {
        append_routing_config(doc, &mut tbl);
    }
    if tbl.is_empty() {
        return Ok(());
    }
    let mut leaves = Vec::new();
    collect_toml_leaves(&tbl, &mut Vec::new(), &mut leaves);
    let mut doc = load_doc(config_path)?;
    for (path, value) in leaves {
        let dotted = path.join(".");
        if has_toml_path(doc.as_table(), &path) && !force {
            report.config_skipped.push(dotted);
            continue;
        }
        set_toml_path(doc.as_table_mut(), &path, &value)?;
        report.config_imported.push(dotted);
    }
    if !dry_run && !report.config_imported.is_empty() {
        atomic_write(config_path, &doc.to_string())?;
    }
    Ok(())
}

fn append_routing_config(doc: &Yaml, root: &mut toml::Table) {
    if let Some(roles) = doc.get("modelRoles") {
        for (role, target) in roles.entries() {
            if let Some(t) = target.str().map(str::trim).filter(|s| !s.is_empty()) {
                insert_dotted(
                    root,
                    &format!("roles.{role}"),
                    toml::Value::String(t.to_owned()),
                );
            }
        }
    }
    if let Some(order) = doc.get("cycleOrder") {
        let items: Vec<toml::Value> = order
            .items()
            .iter()
            .filter_map(Yaml::str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| toml::Value::String(s.to_owned()))
            .collect();
        if !items.is_empty() {
            root.insert("cycle".to_owned(), toml::Value::Array(items));
        }
    }
    let chains = doc
        .get("retry")
        .and_then(|r| r.get("fallbackChains"))
        .or_else(|| doc.get("fallbackChains"))
        .or_else(|| doc.get("fallback"));
    if let Some(chains) = chains {
        for (owner, targets) in chains.entries() {
            let list: Vec<toml::Value> = targets
                .items()
                .iter()
                .filter_map(Yaml::str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| toml::Value::String(s.to_owned()))
                .collect();
            if !list.is_empty() {
                insert_dotted(root, &format!("fallback.{owner}"), toml::Value::Array(list));
            }
        }
    }
    if let Some(presets) = doc.get("modelPresets") {
        for (name, block) in presets.entries() {
            let mut preset_tbl = toml::Table::new();
            if let Some(roles) = block.get("modelRoles") {
                let mut role_tbl = toml::Table::new();
                for (role, target) in roles.entries() {
                    if let Some(t) = target.str().map(str::trim).filter(|s| !s.is_empty()) {
                        role_tbl.insert(role.clone(), toml::Value::String(t.to_owned()));
                    }
                }
                if !role_tbl.is_empty() {
                    preset_tbl.insert("modelRoles".to_owned(), toml::Value::Table(role_tbl));
                }
            }
            if let Some(level) = block
                .get("defaultThinkingLevel")
                .and_then(Yaml::str)
                .map(str::trim)
                .filter(|s| ThinkingLevel::parse(s).is_some())
            {
                preset_tbl.insert(
                    "defaultThinkingLevel".to_owned(),
                    toml::Value::String(level.to_ascii_lowercase()),
                );
            }
            if !preset_tbl.is_empty() {
                let entry = root
                    .entry("modelPresets")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if let Some(map) = entry.as_table_mut() {
                    map.insert(name.clone(), toml::Value::Table(preset_tbl));
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "omp_import_tests.rs"]
mod tests;
