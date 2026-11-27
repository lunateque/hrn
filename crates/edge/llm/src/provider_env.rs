//! Data-driven provider -> environment variable mapping (`data/provider-env.txt`).

const PROVIDER_ENV_DATA: &str = include_str!("../data/provider-env.txt");

/// Default local Ollama server root URL when neither `OLLAMA_BASE_URL` nor `OLLAMA_HOST` is set.
pub const DEFAULT_OLLAMA_ROOT: &str = "http://127.0.0.1:11434";

/// One auto-mapping row from `data/provider-env.txt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderEnvRule {
    /// Canonical provider name (`groq`, `xai`, `moonshot`, `openrouter`, `ollama`).
    pub provider: String,
    /// Ordered API-key environment variable names (empty for keyless providers like `ollama`).
    pub key_envs: Vec<String>,
    /// Ordered base-URL override environment variable names.
    pub base_url_envs: Vec<String>,
}

/// Parse all rows from `data/provider-env.txt`.
#[must_use]
pub fn provider_env_rules() -> Vec<ProviderEnvRule> {
    PROVIDER_ENV_DATA
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let provider = cols.next()?.to_owned();
            let raw_keys = cols.next()?;
            let key_envs = if raw_keys == "-" {
                Vec::new()
            } else {
                raw_keys
                    .split(':')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            };
            let base_url_envs = cols
                .next()
                .map(|raw| {
                    raw.split(':')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Some(ProviderEnvRule {
                provider,
                key_envs,
                base_url_envs,
            })
        })
        .collect()
}

/// Look up the auto-mapping rule for `provider`, if any.
#[must_use]
pub fn rule_for(provider: &str) -> Option<ProviderEnvRule> {
    provider_env_rules()
        .into_iter()
        .find(|r| r.provider.eq_ignore_ascii_case(provider))
}

/// Resolve the first non-empty value from `env_names` using `lookup` (process env / `.env`).
pub(crate) fn first_env_with(
    env_names: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<(String, String)> {
    for name in env_names {
        if let Some(val) = lookup(name) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some((name.clone(), trimmed.to_owned()));
            }
        }
    }
    None
}

/// Normalize an `OLLAMA_BASE_URL` / `OLLAMA_HOST` value into the root URL (`/api/tags` prefix).
#[must_use]
pub fn normalize_ollama_root(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return DEFAULT_OLLAMA_ROOT.to_owned();
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("http://{trimmed}")
    };
    with_scheme
        .strip_suffix("/v1")
        .unwrap_or(&with_scheme)
        .trim_end_matches('/')
        .to_owned()
}

/// Resolve the effective local Ollama root URL from `OLLAMA_BASE_URL` -> `OLLAMA_HOST` -> default.
#[must_use]
pub fn ollama_root_url() -> String {
    let envs = rule_for("ollama").map_or_else(
        || vec!["OLLAMA_BASE_URL".to_owned(), "OLLAMA_HOST".to_owned()],
        |r| r.base_url_envs,
    );
    first_env_with(&envs, &|k| std::env::var(k).ok()).map_or_else(
        || DEFAULT_OLLAMA_ROOT.to_owned(),
        |(_, v)| normalize_ollama_root(&v),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_env_table_and_moonshot_fallback() {
        let rules = provider_env_rules();
        assert!(
            rules
                .iter()
                .any(|r| r.provider == "groq" && r.key_envs == ["GROQ_API_KEY"])
        );
        let moonshot = rule_for("moonshot").expect("moonshot rule");
        assert_eq!(moonshot.key_envs, ["MOONSHOT_API_KEY", "KIMI_API_KEY"]);
        assert_eq!(moonshot.base_url_envs, ["MOONSHOT_BASE_URL"]);
        let ollama = rule_for("ollama").expect("ollama rule");
        assert!(ollama.key_envs.is_empty());
        assert_eq!(ollama.base_url_envs, ["OLLAMA_BASE_URL", "OLLAMA_HOST"]);
    }

    #[test]
    fn normalizes_ollama_host() {
        assert_eq!(
            normalize_ollama_root("127.0.0.1:11434"),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            normalize_ollama_root("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
    }
}
