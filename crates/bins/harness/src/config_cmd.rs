//! `hrn config [list|get|set|reset|path|check]` CLI subcommands.

use crate::cli::{ConfigCommand, ConfigScope};
use crate::config::Config;
use crate::config_layers::{LayerInput, LayerSource, MergedConfig, load_layers, lookup_dotted};
use crate::settings_schema::{format_value, parse_cli_value, schema, toml_to_json};
use crate::term::out;
use crate::toml_write;
use hrn_kernel::{HrnError, Result};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::Path;

/// Dispatch `hrn config [subcommand]`. Bare `hrn config` lists all settings.
pub(crate) fn run(
    cmd: Option<&ConfigCommand>,
    input: &LayerInput,
    global_path: &Path,
    agent_dir: &Path,
) -> Result {
    match cmd {
        None => run_list(input, false, None),
        Some(ConfigCommand::List { json, scope }) => run_list(input, *json, *scope),
        Some(ConfigCommand::Get { key, json }) => run_get(input, key, *json),
        Some(ConfigCommand::Set { key, value, json }) => {
            run_set(input, global_path, key, value, *json)
        }
        Some(ConfigCommand::Reset { key }) => run_reset(input, global_path, key),
        Some(ConfigCommand::Path) => {
            out(&format!(
                "dir: {}\nconfig: {}\n",
                agent_dir.display(),
                global_path.display()
            ));
            Ok(())
        }
        Some(ConfigCommand::Check) => {
            let merged = load_layers(input)?;
            let cfg = Config::from_merged(&merged, input.home.as_deref())?;
            crate::commands::config_check(global_path, &cfg)
        }
    }
}

fn run_list(input: &LayerInput, as_json: bool, scope: Option<ConfigScope>) -> Result {
    let merged = load_layers(input)?;
    if let Some(scope) = scope {
        render_scoped_list(&merged, scope, as_json);
        return Ok(());
    }
    if as_json {
        let rows: Vec<Value> = schema()
            .all()
            .iter()
            .map(|spec| {
                let val = merged.get(&spec.key);
                let src = merged.source_of(&spec.key);
                if spec.secret && val.is_some() {
                    json!({
                        "key": spec.key,
                        "tab": spec.tab,
                        "type": spec.kind.as_str(),
                        "source": src.label(),
                        "redacted": true,
                        "description": spec.description,
                    })
                } else {
                    json!({
                        "key": spec.key,
                        "tab": spec.tab,
                        "type": spec.kind.as_str(),
                        "value": val.map(toml_to_json),
                        "source": src.label(),
                        "description": spec.description,
                    })
                }
            })
            .collect();
        out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_owned())
        ));
        return Ok(());
    }
    let mut text = String::new();
    let mut current_tab = "";
    for spec in schema().all() {
        if spec.tab != current_tab {
            if !text.is_empty() {
                text.push('\n');
            }
            let _ = writeln!(text, "[{}]", spec.tab);
            current_tab = &spec.tab;
        }
        let val = merged.get(&spec.key).or(spec.default.as_ref());
        let shown = format_value(spec, val, true, "********");
        let src = merged.source_of(&spec.key).label();
        let _ = writeln!(
            text,
            "  {:<28} {:<16} {:<8} ({src})",
            spec.key,
            shown,
            spec.kind.as_str()
        );
    }
    out(&text);
    Ok(())
}

fn render_scoped_list(merged: &MergedConfig, scope: ConfigScope, as_json: bool) {
    let raw = match scope {
        ConfigScope::Global => merged.global_raw.as_ref(),
        ConfigScope::Project => merged.project_raw.as_ref(),
    };
    let mut rows = Vec::new();
    if let Some(table) = raw {
        for spec in schema().all() {
            if let Some(val) = lookup_dotted(table, &spec.key) {
                rows.push((spec, val));
            }
        }
    }
    if as_json {
        let json_rows: Vec<Value> = rows
            .iter()
            .map(|(spec, val)| {
                if spec.secret {
                    json!({
                        "key": spec.key,
                        "type": spec.kind.as_str(),
                        "redacted": true,
                    })
                } else {
                    json!({
                        "key": spec.key,
                        "type": spec.kind.as_str(),
                        "value": toml_to_json(val),
                    })
                }
            })
            .collect();
        out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&json_rows).unwrap_or_else(|_| "[]".to_owned())
        ));
        return;
    }
    if rows.is_empty() {
        out("(no settings in this scope)\n");
        return;
    }
    let mut text = String::new();
    for (spec, val) in rows {
        let shown = format_value(spec, Some(val), true, "********");
        let _ = writeln!(text, "{} = {shown}", spec.key);
    }
    out(&text);
}

fn unknown_key_err(key: &str) -> HrnError {
    let similar = schema().similar_keys(key);
    if similar.is_empty() {
        HrnError::Config(format!("unknown setting `{key}`"))
    } else {
        HrnError::Config(format!(
            "unknown setting `{key}` (similar: {})",
            similar.join(", ")
        ))
    }
}

fn run_get(input: &LayerInput, key: &str, as_json: bool) -> Result {
    let spec = schema().get(key).ok_or_else(|| unknown_key_err(key))?;
    let merged = load_layers(input)?;
    let val = merged.get(key).or(spec.default.as_ref());
    let src = merged.source_of(key);
    if as_json {
        let payload = json!({
            "key": spec.key,
            "value": val.map(toml_to_json),
            "type": spec.kind.as_str(),
            "description": spec.description,
            "source": src.label(),
        });
        out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        ));
        return Ok(());
    }
    // Single-key `get` prints credentials unmasked, matching omp's `omp config get`.
    let shown = format_value(spec, val, false, "");
    out(&format!("{shown}\n"));
    Ok(())
}

fn run_set(
    input: &LayerInput,
    global_path: &Path,
    key: &str,
    raw_value: &str,
    as_json: bool,
) -> Result {
    let spec = schema().get(key).ok_or_else(|| unknown_key_err(key))?;
    let parsed = parse_cli_value(spec, raw_value)?;
    toml_write::set_key(global_path, key, &parsed)?;
    let merged = load_layers(input)?;
    let overridden = merged.overridden_above_global(key);
    if as_json {
        let payload = json!({
            "key": spec.key,
            "written": toml_to_json(&parsed),
            "path": global_path.display().to_string(),
            "overridden_by": overridden.map(LayerSource::kind_tag),
        });
        out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        ));
        return Ok(());
    }
    let shown = format_value(spec, Some(&parsed), true, "********");
    out(&format!(
        "set {} = {shown} ({})\n",
        spec.key,
        global_path.display()
    ));
    if let Some(winner) = overridden {
        out(&format!(
            "note: overridden_by {} ({})\n",
            winner.kind_tag(),
            winner.label()
        ));
    }
    Ok(())
}

fn run_reset(input: &LayerInput, global_path: &Path, key: &str) -> Result {
    let spec = schema().get(key).ok_or_else(|| unknown_key_err(key))?;
    let _ = toml_write::delete_key(global_path, key)?;
    let merged = load_layers(input)?;
    let effective = merged.get(key).or(spec.default.as_ref());
    let shown = format_value(spec, effective, true, "********");
    let src = merged.source_of(key).label();
    out(&format!("reset {} -> {shown} ({src})\n", spec.key));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn set_get_and_reset_roundtrip_and_preserve_comments() {
        let dir = std::env::temp_dir().join(format!("hrn-cfg-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let global = dir.join("config.toml");
        std::fs::write(&global, "# keep me\nthinking = \"medium\"\n").expect("seed");

        let input = LayerInput {
            global_path: Some(global.clone()),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            ..LayerInput::default()
        };

        run_set(&input, &global, "advisor.enabled", "true", false).expect("set true");
        let merged = load_layers(&input).expect("load");
        assert_eq!(
            merged.get("advisor.enabled").and_then(toml::Value::as_bool),
            Some(true)
        );
        let text = std::fs::read_to_string(&global).expect("read");
        assert!(text.contains("# keep me"), "{text}");

        run_reset(&input, &global, "advisor.enabled").expect("reset");
        let merged = load_layers(&input).expect("load");
        assert_eq!(
            merged.get("advisor.enabled").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert!(run_get(&input, "definitely.unknown", false).is_err());

        let mut env = BTreeMap::new();
        env.insert("HRN_TUI_THEME".to_owned(), "env-theme".to_owned());
        let env_input = LayerInput {
            global_path: Some(global.clone()),
            cwd: PathBuf::from(&dir),
            env,
            ..LayerInput::default()
        };
        run_set(&env_input, &global, "tui.theme", "titanium", true).expect("set shadowed");
        let after = load_layers(&env_input).expect("load");
        assert_eq!(
            after
                .overridden_above_global("tui.theme")
                .map(LayerSource::kind_tag),
            Some("env")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
