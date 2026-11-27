//! Compatibility loader mapping omp's `~/.omp/agent/config.yml` into `hrn`'s layered TOML
//! schema as [`crate::config_layers::LayerSource::Omp`] (sits above embedded defaults and below
//! global `~/.harness/config.toml`).
//!
//! Unreadable, missing, or malformed files return `None` so an omp YAML issue never bricks `hrn`.
//! Module tables (`[shell]`, `[browser]`, `[edit]`) are emitted only when the corresponding
//! module is compiled into this binary so `xtask removability` never encounters unclaimed tables.

use crate::config_layers::insert_dotted;
use hrn_edge_llm::yaml::{Yaml, parse as parse_yaml};
use hrn_kernel::ThinkingLevel;
use std::path::Path;

/// Load `<home>/.omp/agent/config.yml` and translate supported settings into a `toml::Table`.
pub(crate) fn load_omp_config_table(home: Option<&Path>) -> Option<toml::Table> {
    let path = home?.join(".omp").join("agent").join("config.yml");
    let text = std::fs::read_to_string(&path).ok()?;
    parse_omp_config_yaml(&text)
}

/// Pure translation helper from omp `config.yml` text into `hrn`'s TOML config schema.
pub(crate) fn parse_omp_config_yaml(yaml_text: &str) -> Option<toml::Table> {
    let doc = parse_yaml(yaml_text)?;
    if doc.entries().is_empty() {
        return None;
    }
    let mut root = toml::Table::new();
    map_core(&doc, &mut root);
    map_compaction(&doc, &mut root);
    map_tui(&doc, &mut root);
    map_advisor_and_llm(&doc, &mut root);
    map_task(&doc, &mut root);
    map_modules(&doc, &mut root);
    (!root.is_empty()).then_some(root)
}

fn map_core(doc: &Yaml, root: &mut toml::Table) {
    if let Some(level) = doc.get("defaultThinkingLevel").and_then(parse_thinking_str) {
        insert_dotted(root, "thinking", toml::Value::String(level));
    }
    if let Some(raw) = doc.get("doubleEscapeAction").and_then(Yaml::str) {
        let mapped = match raw.trim().to_ascii_lowercase().as_str() {
            "tree" | "branch" => Some("tree"),
            "rewind" => Some("rewind"),
            "none" => Some("none"),
            _ => None,
        };
        if let Some(action) = mapped {
            insert_dotted(
                root,
                "interaction.double_escape_action",
                toml::Value::String(action.to_owned()),
            );
        }
    }
}

fn map_compaction(doc: &Yaml, root: &mut toml::Table) {
    let Some(comp) = doc.get("compaction") else {
        return;
    };
    insert_bool(comp, "enabled", root, "compaction.enabled");
    insert_i64(
        comp,
        "thresholdPercent",
        root,
        "compaction.threshold_percent",
    );
    insert_i64(comp, "thresholdTokens", root, "compaction.threshold_tokens");
    insert_nonneg_i64(comp, "reserveTokens", root, "compaction.reserve_tokens");
    insert_nonneg_i64(
        comp,
        "keepRecentTokens",
        root,
        "compaction.keep_recent_tokens",
    );
    insert_bool(comp, "autoContinue", root, "compaction.auto_continue");
    insert_bool(comp, "midTurnEnabled", root, "compaction.mid_turn_enabled");
}

fn map_tui(doc: &Yaml, root: &mut toml::Table) {
    if let Some(theme) = doc.get("theme") {
        match theme {
            Yaml::Scalar(raw) => {
                let name = raw.trim();
                if !name.is_empty() {
                    insert_dotted(root, "tui.theme", toml::Value::String(name.to_owned()));
                }
            }
            Yaml::Map(_) => {
                if let Some(dark) = theme.get("dark").and_then(nonempty_str) {
                    insert_dotted(root, "tui.theme.dark", toml::Value::String(dark));
                }
                if let Some(light) = theme.get("light").and_then(nonempty_str) {
                    insert_dotted(root, "tui.theme.light", toml::Value::String(light));
                }
                if let Some(cur) = theme.get("current").and_then(Yaml::str) {
                    let low = cur.trim().to_ascii_lowercase();
                    if matches!(low.as_str(), "auto" | "dark" | "light") {
                        insert_dotted(root, "tui.theme.current", toml::Value::String(low));
                    }
                }
            }
            Yaml::Seq(_) => {}
        }
    }
    insert_bool(doc, "colorBlindMode", root, "tui.color_blind_mode");
    insert_bool(doc, "hideThinkingBlock", root, "tui.hide_thinking");
}

fn map_advisor_and_llm(doc: &Yaml, root: &mut toml::Table) {
    if let Some(adv) = doc.get("advisor") {
        insert_bool(adv, "enabled", root, "advisor.enabled");
        if let Some(level) = adv.get("thinking").and_then(parse_thinking_str) {
            insert_dotted(root, "advisor.thinking", toml::Value::String(level));
        }
        if let Some(raw) = adv.get("maxTokens").and_then(Yaml::u64)
            && raw > 0
            && u32::try_from(raw).is_ok()
            && let Ok(val) = i64::try_from(raw)
        {
            insert_dotted(root, "advisor.max_tokens", toml::Value::Integer(val));
        }
    }
    if let Some(cp) = doc.get("contextPromotion") {
        insert_bool(cp, "enabled", root, "llm.context_promotion");
    }
    if let Some(anth) = doc.get("providers").and_then(|p| p.get("anthropic")) {
        insert_bool(anth, "slowMode", root, "llm.anthropic.slow_mode");
    }
}

fn map_task(doc: &Yaml, root: &mut toml::Table) {
    let Some(task) = doc.get("task") else {
        return;
    };
    insert_nonneg_i64(task, "softRequestBudget", root, "task.soft_request_budget");
    insert_nonneg_i64(task, "maxConcurrency", root, "task.max_concurrency");
    insert_nonneg_i64(task, "maxRecursionDepth", root, "task.max_recursion_depth");
    if let Some(Yaml::Seq(items)) = task.get("disabledAgents") {
        let names: Vec<toml::Value> = items
            .iter()
            .filter_map(nonempty_str)
            .map(toml::Value::String)
            .collect();
        insert_dotted(root, "task.disabled_agents", toml::Value::Array(names));
    }
}

fn map_modules(doc: &Yaml, root: &mut toml::Table) {
    if crate::modules::is_compiled("shell")
        && let Some(path) = doc.get("shellPath").and_then(nonempty_str)
    {
        insert_dotted(root, "shell.shell_path", toml::Value::String(path));
    }
    if crate::modules::is_compiled("browser")
        && let Some(browser) = doc.get("browser")
    {
        insert_bool(browser, "headless", root, "browser.headless");
        insert_bool(browser, "relay", root, "browser.relay");
    }
    if crate::modules::is_compiled("edit")
        && let Some(edit) = doc.get("edit")
    {
        insert_bool(
            edit,
            "blockAutoGenerated",
            root,
            "edit.block_auto_generated",
        );
    }
}

fn nonempty_str(node: &Yaml) -> Option<String> {
    let trimmed = node.str()?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn parse_thinking_str(node: &Yaml) -> Option<String> {
    let trimmed = node.str()?.trim();
    ThinkingLevel::parse(trimmed).map(|_| trimmed.to_ascii_lowercase())
}

fn insert_bool(parent: &Yaml, yaml_key: &str, root: &mut toml::Table, toml_key: &str) {
    if let Some(v) = parent.get(yaml_key).and_then(Yaml::bool) {
        insert_dotted(root, toml_key, toml::Value::Boolean(v));
    }
}

fn insert_i64(parent: &Yaml, yaml_key: &str, root: &mut toml::Table, toml_key: &str) {
    if let Some(v) = parent
        .get(yaml_key)
        .and_then(Yaml::str)
        .and_then(|s| s.trim().parse::<i64>().ok())
    {
        insert_dotted(root, toml_key, toml::Value::Integer(v));
    }
}

fn insert_nonneg_i64(parent: &Yaml, yaml_key: &str, root: &mut toml::Table, toml_key: &str) {
    if let Some(v) = parent
        .get(yaml_key)
        .and_then(Yaml::u64)
        .and_then(|n| i64::try_from(n).ok())
    {
        insert_dotted(root, toml_key, toml::Value::Integer(v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_layers::lookup_dotted;

    #[test]
    fn maps_core_tui_compaction_and_advisor_settings() {
        let yaml = "\
defaultThinkingLevel: xhigh
theme:
  dark: catppuccin-mocha
  light: catppuccin-latte
colorBlindMode: true
hideThinkingBlock: true
doubleEscapeAction: branch
compaction:
  enabled: false
  thresholdPercent: 80
  thresholdTokens: 120000
  reserveTokens: 16384
  keepRecentTokens: 24000
  autoContinue: false
  midTurnEnabled: false
advisor:
  enabled: true
  thinking: high
  maxTokens: 512
unknownFutureKey: ignored
";
        let tbl = parse_omp_config_yaml(yaml).expect("parsed omp config");
        assert_eq!(
            lookup_dotted(&tbl, "thinking").and_then(toml::Value::as_str),
            Some("xhigh")
        );
        assert_eq!(
            lookup_dotted(&tbl, "tui.theme.dark").and_then(toml::Value::as_str),
            Some("catppuccin-mocha")
        );
        assert_eq!(
            lookup_dotted(&tbl, "tui.theme.light").and_then(toml::Value::as_str),
            Some("catppuccin-latte")
        );
        assert_eq!(
            lookup_dotted(&tbl, "tui.color_blind_mode").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            lookup_dotted(&tbl, "tui.hide_thinking").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            lookup_dotted(&tbl, "interaction.double_escape_action").and_then(toml::Value::as_str),
            Some("tree")
        );
        assert_eq!(
            lookup_dotted(&tbl, "compaction.enabled").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            lookup_dotted(&tbl, "compaction.threshold_percent").and_then(toml::Value::as_integer),
            Some(80)
        );
        assert_eq!(
            lookup_dotted(&tbl, "compaction.threshold_tokens").and_then(toml::Value::as_integer),
            Some(120_000)
        );
        assert_eq!(
            lookup_dotted(&tbl, "compaction.reserve_tokens").and_then(toml::Value::as_integer),
            Some(16_384)
        );
        assert_eq!(
            lookup_dotted(&tbl, "compaction.keep_recent_tokens").and_then(toml::Value::as_integer),
            Some(24_000)
        );
        assert_eq!(
            lookup_dotted(&tbl, "compaction.auto_continue").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            lookup_dotted(&tbl, "compaction.mid_turn_enabled").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            lookup_dotted(&tbl, "advisor.enabled").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            lookup_dotted(&tbl, "advisor.thinking").and_then(toml::Value::as_str),
            Some("high")
        );
        assert_eq!(
            lookup_dotted(&tbl, "advisor.max_tokens").and_then(toml::Value::as_integer),
            Some(512)
        );
        assert!(tbl.get("unknownFutureKey").is_none());
    }

    #[test]
    fn maps_llm_task_and_guarded_module_settings() {
        let yaml = "\
shellPath: /usr/local/bin/bash
contextPromotion:
  enabled: true
providers:
  anthropic:
    slowMode: true
task:
  softRequestBudget: 150
  maxConcurrency: 8
  maxRecursionDepth: 3
  disabledAgents: [oracle, explore]
browser:
  headless: false
  relay: true
edit:
  blockAutoGenerated: false
";
        let tbl = parse_omp_config_yaml(yaml).expect("parsed omp config");
        assert_eq!(
            lookup_dotted(&tbl, "llm.context_promotion").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            lookup_dotted(&tbl, "llm.anthropic.slow_mode").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            lookup_dotted(&tbl, "task.soft_request_budget").and_then(toml::Value::as_integer),
            Some(150)
        );
        assert_eq!(
            lookup_dotted(&tbl, "task.max_concurrency").and_then(toml::Value::as_integer),
            Some(8)
        );
        assert_eq!(
            lookup_dotted(&tbl, "task.max_recursion_depth").and_then(toml::Value::as_integer),
            Some(3)
        );
        let disabled: Vec<&str> = lookup_dotted(&tbl, "task.disabled_agents")
            .and_then(toml::Value::as_array)
            .expect("disabled_agents")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(disabled, ["oracle", "explore"]);

        if crate::modules::is_compiled("shell") {
            assert_eq!(
                lookup_dotted(&tbl, "shell.shell_path").and_then(toml::Value::as_str),
                Some("/usr/local/bin/bash")
            );
        } else {
            assert!(tbl.get("shell").is_none());
        }
        if crate::modules::is_compiled("browser") {
            assert_eq!(
                lookup_dotted(&tbl, "browser.headless").and_then(toml::Value::as_bool),
                Some(false)
            );
            assert_eq!(
                lookup_dotted(&tbl, "browser.relay").and_then(toml::Value::as_bool),
                Some(true)
            );
        } else {
            assert!(tbl.get("browser").is_none());
        }
        if crate::modules::is_compiled("edit") {
            assert_eq!(
                lookup_dotted(&tbl, "edit.block_auto_generated").and_then(toml::Value::as_bool),
                Some(false)
            );
        } else {
            assert!(tbl.get("edit").is_none());
        }
    }

    #[test]
    fn scalar_theme_and_invalid_values_behave_safely() {
        let tbl = parse_omp_config_yaml(
            "theme: dracula\ndefaultThinkingLevel: bogus\ndoubleEscapeAction: invalid\n",
        )
        .expect("theme only");
        assert_eq!(
            lookup_dotted(&tbl, "tui.theme").and_then(toml::Value::as_str),
            Some("dracula")
        );
        assert!(lookup_dotted(&tbl, "thinking").is_none());
        assert!(lookup_dotted(&tbl, "interaction.double_escape_action").is_none());

        assert!(parse_omp_config_yaml("").is_none());
        assert!(parse_omp_config_yaml("# comment only\n").is_none());
        assert!(parse_omp_config_yaml("not a map scalar").is_none());
        assert!(load_omp_config_table(None).is_none());
    }
}
