//! `hrn models`: every model hrn can talk to, named `<provider>/<id>` as omp names them.

use crate::config::Config;
use crate::term::out;
use hrn_edge_llm::{ModelPreset, resolve_alias};
use std::fmt::Write as _;

/// `1M`, `272k`, `8192`.
fn tokens(n: u64) -> String {
    if n >= 1_000_000 && n.is_multiple_of(100_000) {
        let tenths = n / 100_000;
        if tenths.is_multiple_of(10) {
            format!("{}M", tenths / 10)
        } else {
            format!("{}.{}M", tenths / 10, tenths % 10)
        }
    } else if n >= 1_000 && n.is_multiple_of(1_000) {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// `$in/$out` per million tokens, `-` when no price is known.
fn price(preset: &ModelPreset) -> String {
    preset.price.map_or_else(
        || "-".to_owned(),
        |p| format!("${}/${}", trim(p.input), trim(p.output)),
    )
}

fn trim(value: f64) -> String {
    let text = format!("{value:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The model table, then the roles that point into it.
pub(crate) fn render(cfg: &Config) -> String {
    let mut rows = vec![["MODEL", "NAME", "CONTEXT", "THINKS", "PRICE/M"].map(str::to_owned)];
    for (key, preset) in hrn_edge_llm::enabled_presets(&cfg.llm) {
        rows.push([
            key.to_owned(),
            preset.name.clone().unwrap_or_else(|| preset.id.clone()),
            tokens(preset.context_window.unwrap_or(200_000)),
            match preset.reasoning {
                Some(true) => "yes",
                Some(false) => "no",
                None => "-",
            }
            .to_owned(),
            price(preset),
        ]);
    }
    let widths: Vec<usize> = (0..5)
        .map(|col| {
            rows.iter()
                .filter_map(|row| row.get(col))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut text = String::new();
    for row in &rows {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(&widths) {
            let _ = write!(line, "{cell:<width$}  ");
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    let mut roles = String::new();
    for role in cfg.llm.roles.keys() {
        if let Ok(key) = resolve_alias(&cfg.llm, Some(role)) {
            let _ = writeln!(roles, "{role} -> {key}");
        }
    }
    if !roles.is_empty() {
        let _ = write!(text, "\nroles (accepted wherever a model is):\n{roles}");
    }
    text
}

/// Print the model table.
pub(crate) fn run(cfg: &Config) {
    out(&render(cfg));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_are_compact() {
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(1_500_000), "1.5M");
        assert_eq!(tokens(272_000), "272k");
        assert_eq!(tokens(8_192), "8192");
    }

    #[test]
    fn the_table_lists_models_by_provider_slash_id_with_roles() {
        let cfg = Config::parse(
            r#"
[[providers]]
name = "local"
kind = "openai_chat"

[models."local/m"]
provider = "local"
id = "m"
context_window = 262144
name = "Model M"
reasoning = false
price = { input = 0.5, output = 2 }

[roles]
default = "local/m"
"#,
        )
        .expect("valid");
        let text = render(&cfg);
        let row = text.lines().nth(1).expect("row");
        assert!(row.starts_with("local/m  Model M"), "{text}");
        assert!(
            row.contains("262144") && row.contains("no") && row.ends_with("$0.5/$2"),
            "{text}"
        );
        assert!(text.contains("default -> local/m"), "{text}");
    }

    #[test]
    fn empty_config_with_groq_env_lists_gpt_oss_120b() {
        let dir = std::env::temp_dir().join(format!("hrn-models-groq-{}", std::process::id()));
        let auth = dir.join("auth");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&auth).expect("mkdir");
        let env_fn = |k: &str| (k == "GROQ_API_KEY").then(|| "gsk_test".to_owned());
        let catalogs = hrn_edge_llm::standard_catalogs_with_env(&dir, &auth, Some(&env_fn));
        let mut llm = hrn_edge_llm::LlmConfig::default();
        for cat in catalogs {
            llm.absorb(cat);
        }
        hrn_edge_llm::validate_config(&llm).expect("valid");
        let mut cfg = Config::parse(
            r#"
[[providers]]
name = "dummy"
kind = "openai_chat"
[models."dummy/m"]
provider = "dummy"
id = "m"
context_window = 8192
[roles]
default = "dummy/m"
"#,
        )
        .expect("base");
        cfg.llm = llm;
        let rendered = render(&cfg);
        assert!(rendered.contains("groq/openai/gpt-oss-120b"), "{rendered}");
        assert!(
            rendered.contains("default -> groq/openai/gpt-oss-120b"),
            "{rendered}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
