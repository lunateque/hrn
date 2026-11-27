//! Model catalogs merged under the user's own `config.toml`: the files [`standard_catalogs`]
//! reads, all in omp's `models.yml` shape. Models are keyed `<provider>/<id>`, which is also
//! how every surface displays them.

use crate::config::{LlmConfig, ModelPreset, ProviderEntry, validate_preset, validate_provider};
use crate::models_yml;
#[cfg(test)]
use hrn_edge_oauth::Subscription;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Providers, models and roles from one source; [`LlmConfig::absorb`] merges it in.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Catalog {
    /// Providers by name.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
    /// Models by `<provider>/<id>`.
    #[serde(default)]
    pub presets: BTreeMap<String, ModelPreset>,
    /// Role name to model key.
    #[serde(default)]
    pub roles: BTreeMap<String, String>,
    /// Model keys `ctrl+p` cycles through.
    #[serde(default)]
    pub cycle: Vec<String>,
    /// Per-model, per-role, or `<provider>/*` ordered fallback chains (omp `retry.fallbackChains`).
    #[serde(default)]
    pub fallback_chains: BTreeMap<String, Vec<String>>,
    /// Display badges keyed by role or model key (omp `modelTags`).
    #[serde(default)]
    pub model_tags: BTreeMap<String, crate::config::ModelTag>,
    /// Named role bundles (omp `modelPresets`).
    #[serde(default)]
    pub role_presets: BTreeMap<String, crate::config::RolePreset>,
    /// Per-agent model selector overrides (omp `task.agentModelOverrides`).
    #[serde(default)]
    pub agent_models: BTreeMap<String, String>,
}

impl Catalog {
    /// The key a model is known by.
    pub fn key(provider: &str, id: &str) -> String {
        format!("{provider}/{id}")
    }
}

impl LlmConfig {
    /// Add what `catalog` offers and the configuration does not already define: the user's own
    /// providers, models, roles, fallback chains, and cycle order always win. Imports are best
    /// effort: entries that fail guardrails or reference unknown providers/models are dropped
    /// rather than failing the configuration.
    pub fn absorb(&mut self, catalog: Catalog) {
        self.absorb_all(vec![catalog]);
    }

    /// Merge multiple `catalogs` (highest priority first) in two passes: all providers & presets
    /// first, then roles, fallback chains, role presets, and cycle order so references from an
    /// upper catalog (e.g. `~/.omp/agent/config.yml`) to models in a lower catalog (e.g. built-in
    /// `models.yml`) resolve cleanly.
    pub fn absorb_all(&mut self, catalogs: Vec<Catalog>) {
        let mut deferred = Vec::with_capacity(catalogs.len());
        for catalog in catalogs {
            for (name, provider) in catalog.providers {
                if validate_provider(&name, &provider).is_ok() {
                    self.providers.entry(name).or_insert(provider);
                }
            }
            for (key, preset) in catalog.presets {
                if let Some(provider) = self.providers.get(&preset.provider)
                    && validate_preset(&key, &preset, provider).is_ok()
                {
                    self.presets.entry(key).or_insert(preset);
                }
            }
            for (key, tag) in catalog.model_tags {
                self.model_tags.entry(key).or_insert(tag);
            }
            deferred.push((
                catalog.roles,
                catalog.fallback_chains,
                catalog.role_presets,
                catalog.cycle,
            ));
        }
        // Absorb roles first across all catalogs so role-keyed fallback chains and role targets
        // resolve even when defined in another catalog layer.
        for (roles, _, _, _) in &mut deferred {
            for (role, selector) in std::mem::take(roles) {
                let valid_target = selector.split(',').any(|part| {
                    let (base, _) = hrn_kernel::split_level_suffix(part.trim());
                    let base = base.trim();
                    base == "*"
                        || base.starts_with('@')
                        || self.presets.contains_key(base)
                        || crate::candidates::role_fallbacks(base).is_some()
                });
                if valid_target {
                    self.roles.entry(role).or_insert(selector);
                }
            }
        }
        for (_, fallback_chains, role_presets, cycle) in deferred {
            for (owner, chain) in fallback_chains {
                if !crate::candidates::is_valid_fallback_owner(self, &owner)
                    || self.fallback_chains.contains_key(&owner)
                {
                    continue;
                }
                let valid: Vec<String> = chain
                    .into_iter()
                    .filter(|alias| crate::candidates::is_valid_fallback_target(self, alias))
                    .collect();
                if !valid.is_empty() {
                    self.fallback_chains.insert(owner, valid);
                }
            }
            for (name, preset) in role_presets {
                self.role_presets.entry(name).or_insert(preset);
            }
            if self.cycle_order.is_empty() {
                self.cycle_order = cycle
                    .into_iter()
                    .filter(|entry| {
                        self.presets.contains_key(entry)
                            || self.roles.contains_key(entry)
                            || crate::candidates::role_fallbacks(entry).is_some()
                    })
                    .collect();
            }
        }
    }
}

/// The built-in models and role names, in omp's `models.yml` shape: omp's own subscription
/// catalog (`anthropic/claude-*`, `openai-codex/*`) with the windows and flags omp ships.
const DEFAULTS: &str = include_str!("../models.yml");

/// Every model source below `config.toml`, the highest priority first: the user's own
/// `~/.harness/models.yml`, omp's `~/.omp/agent/models.yml` (with the roles and cycle order of
/// its `config.yml`), then the built-in defaults (subscription providers when logged in, and
/// hosted API-key providers whose env var in `data/provider-env.txt` is set). Files that are
/// absent are skipped.
pub fn standard_catalogs(home: &Path, auth_dir: &Path) -> Vec<Catalog> {
    standard_catalogs_with_env(home, auth_dir, None)
}

/// Environment lookup callback used for deterministic tests of env-activated providers.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Same as [`standard_catalogs`] with an explicit env-lookup hook (for deterministic tests).
pub fn standard_catalogs_with_env(
    home: &Path,
    auth_dir: &Path,
    env_lookup: Option<EnvLookup<'_>>,
) -> Vec<Catalog> {
    let own_dir = home.join(".harness");
    let omp_dir = home.join(".omp").join("agent");
    let read = |dir: &Path, name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let load = |dir: &Path, text: &str, require_env: bool| {
        models_yml::load(
            text,
            &models_yml::Ctx {
                dir,
                auth_dir,
                require_env_for_apikey: require_env,
                env_lookup,
            },
        )
    };
    let mut catalogs = Vec::new();
    if let Some(text) = read(&own_dir, "models.yml") {
        catalogs.push(load(&own_dir, &text, false));
    }
    if let Some(text) = read(&omp_dir, "models.yml") {
        let mut catalog = load(&omp_dir, &text, false);
        if let Some(config) = read(&omp_dir, "config.yml") {
            models_yml::add_roles(&mut catalog, &config);
        }
        catalogs.push(catalog);
    } else if let Some(config) = read(&omp_dir, "config.yml") {
        let mut catalog = Catalog::default();
        models_yml::add_roles(&mut catalog, &config);
        catalogs.push(catalog);
    }
    catalogs.push(load(&own_dir, DEFAULTS, true));
    catalogs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(provider: &str) -> ModelPreset {
        ModelPreset {
            provider: provider.to_owned(),
            id: "m".to_owned(),
            ..ModelPreset::default()
        }
    }

    #[test]
    fn the_users_own_entries_win_and_dangling_ones_are_dropped() {
        let mut cfg = LlmConfig::default();
        cfg.providers.insert("own".into(), ProviderEntry::default());
        cfg.presets.insert("own/m".into(), preset("own"));
        cfg.roles.insert("default".into(), "own/m".into());

        let mut catalog = Catalog::default();
        catalog
            .providers
            .insert("own".into(), ProviderEntry::default());
        catalog
            .providers
            .insert("omp".into(), ProviderEntry::default());
        let mut theirs = preset("own");
        theirs.context_window = Some(1);
        catalog.presets.insert("own/m".into(), theirs);
        catalog.presets.insert("omp/m".into(), preset("omp"));
        catalog.presets.insert("ghost/m".into(), preset("ghost"));
        catalog.roles.insert("default".into(), "omp/m".into());
        catalog.roles.insert("fast".into(), "omp/m".into());
        catalog.roles.insert("lost".into(), "ghost/m".into());
        catalog.cycle = vec!["omp/m".into(), "ghost/m".into()];
        cfg.absorb(catalog);

        assert_eq!(cfg.presets.keys().collect::<Vec<_>>(), ["omp/m", "own/m"]);
        assert_eq!(cfg.presets["own/m"].context_window, None);
        assert_eq!(cfg.roles["default"], "own/m");
        assert_eq!(cfg.roles["fast"], "omp/m");
        assert!(!cfg.roles.contains_key("lost"));
        assert_eq!(cfg.cycle_order, ["omp/m"]);
    }

    #[test]
    fn the_built_in_defaults_follow_the_logins_present() {
        let auth = std::env::temp_dir().join("hrn-catalog-test-auth");
        let home = std::env::temp_dir().join("hrn-catalog-test-no-home");
        std::fs::create_dir_all(&auth).expect("dir");
        std::fs::remove_file(auth.join(Subscription::Claude.file_name())).ok();
        std::fs::remove_file(auth.join(Subscription::Chatgpt.file_name())).ok();
        std::fs::remove_file(auth.join(Subscription::KimiCode.file_name())).ok();
        let no_env = |_: &str| None;
        let catalogs = standard_catalogs_with_env(&home, &auth, Some(&no_env));
        assert_eq!(catalogs.len(), 1);
        assert!(
            catalogs
                .iter()
                .all(|c| c.providers.is_empty() && c.presets.is_empty())
        );

        std::fs::write(auth.join(Subscription::Claude.file_name()), "{}").expect("claude");
        std::fs::write(auth.join(Subscription::Chatgpt.file_name()), "{}").expect("chatgpt");
        let catalogs = standard_catalogs_with_env(&home, &auth, Some(&no_env));
        let Some(all) = catalogs.first() else {
            unreachable!("defaults");
        };
        let opus = &all.presets["anthropic/claude-opus-5-5"];
        assert_eq!(opus.context_window, Some(1_000_000));
        assert_eq!(opus.name.as_deref(), Some("Claude Opus 5.5"));
        assert!(opus.price.is_none());
        assert!(
            all.presets
                .keys()
                .any(|k| k.starts_with("openai-codex/gpt-"))
        );
        std::fs::remove_dir_all(&auth).ok();
    }

    #[test]
    fn shipped_models_yml_validates() {
        let auth = std::env::temp_dir().join(format!("hrn-shipped-yml-{}", std::process::id()));
        let home = auth.join("home");
        std::fs::create_dir_all(&auth).expect("dir");
        std::fs::write(auth.join(Subscription::Claude.file_name()), "{}").expect("claude");
        std::fs::write(auth.join(Subscription::Chatgpt.file_name()), "{}").expect("chatgpt");
        std::fs::write(auth.join(Subscription::KimiCode.file_name()), "{}").expect("kimi-code");
        let all_keys = |k: &str| {
            matches!(
                k,
                "GROQ_API_KEY" | "XAI_API_KEY" | "MOONSHOT_API_KEY" | "OPENROUTER_API_KEY"
            )
            .then(|| "test-key".to_owned())
        };
        let catalogs = standard_catalogs_with_env(&home, &auth, Some(&all_keys));
        let mut llm = LlmConfig::default();
        for cat in catalogs {
            llm.absorb(cat);
        }
        crate::validate_config(&llm).expect("shipped models.yml must validate");
        assert!(llm.presets.contains_key("kimi-code/kimi-for-coding"));
        assert!(llm.presets.contains_key("groq/openai/gpt-oss-120b"));
        assert!(llm.presets.contains_key("xai/grok-4.6"));
        assert!(llm.presets.contains_key("moonshot/kimi-k2.7-code"));
        assert!(llm.presets.contains_key("openrouter/openai/gpt-5.5"));
        assert!(llm.presets["moonshot/kimi-k2.7-code"].replay_reasoning_content);
        assert_eq!(
            llm.presets["xai/grok-4.6"]
                .prompt_cache_session_header
                .as_deref(),
            Some("x-grok-conv-id")
        );
        assert_eq!(
            llm.providers["moonshot"].token_envs,
            vec!["KIMI_API_KEY".to_owned()]
        );
        std::fs::remove_dir_all(&auth).ok();
    }

    #[test]
    fn omp_agent_layout_loads_gemini_timeout_fallbacks_presets_and_agent_overrides() {
        let root = std::env::temp_dir().join(format!("hrn-omp-compat-{}", std::process::id()));
        let omp = root.join(".omp").join("agent");
        let auth = root.join("auth");
        std::fs::create_dir_all(&omp).expect("omp dir");
        std::fs::create_dir_all(&auth).expect("auth dir");
        std::fs::write(auth.join(Subscription::Claude.file_name()), "{}").expect("claude");
        std::fs::write(
            omp.join("models.yml"),
            "\
providers:
  local-gw:
    baseUrl: http://127.0.0.1:4000/v1
    auth: none
    api: openai-completions
    models:
      - id: example-flash
        name: Example Flash
        reasoning: true
        contextWindow: 1000000
      - id: example-mini
        name: Example Mini
        reasoning: true
        contextWindow: 262144
        compat:
          streamIdleTimeoutMs: 600000
  google-direct:
    baseUrl: https://generativelanguage.googleapis.com
    apiKey: GEMINI_API_KEY
    api: google-generative-ai
    models:
      - id: gemini-2.5-flash
        contextWindow: 1000000
",
        )
        .expect("models.yml");
        std::fs::write(
            omp.join("config.yml"),
            "\
modelRoles:
  default: local-gw/example-flash
  mini: local-gw/example-mini:medium
  opus: anthropic/claude-opus-5-5
retry:
  fallbackChains:
    local-gw/example-flash:
      - local-gw/example-mini
      - anthropic/claude-opus-5-5
      - missing/nonexistent
modelPresets:
  fast-local:
    modelRoles:
      default: local-gw/example-flash
    defaultThinkingLevel: medium
task:
  agentModelOverrides:
    scout: '@mini'
    reviewer: local-gw/example-flash:low
",
        )
        .expect("config.yml");

        let no_env = |_: &str| None;
        let catalogs = standard_catalogs_with_env(&root, &auth, Some(&no_env));
        let omp_cat = catalogs.first().expect("omp catalog");
        assert_eq!(
            omp_cat.agent_models.get("scout").map(String::as_str),
            Some("@mini")
        );
        assert_eq!(
            omp_cat.agent_models.get("reviewer").map(String::as_str),
            Some("local-gw/example-flash:low")
        );

        let mut llm = LlmConfig::default();
        llm.absorb_all(catalogs);
        crate::validate_config(&llm).expect("omp compat config validates");
        assert_eq!(
            llm.providers["google-direct"].kind,
            crate::config::ProviderKind::Gemini
        );
        assert_eq!(
            llm.presets["local-gw/example-mini"].stream_idle_timeout_ms,
            Some(600_000)
        );
        assert_eq!(llm.roles["opus"], "anthropic/claude-opus-5-5");
        assert_eq!(
            llm.fallback_chains["local-gw/example-flash"],
            vec![
                "local-gw/example-mini".to_owned(),
                "anthropic/claude-opus-5-5".to_owned(),
            ],
            "cross-catalog fallback targets resolve and missing targets are dropped"
        );
        assert_eq!(
            llm.role_presets["fast-local"].default_thinking_level,
            Some(hrn_kernel::ThinkingLevel::Medium)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn absorb_preserves_model_tags_and_role_and_wildcard_fallback_chains() {
        let mut cfg = LlmConfig::default();
        cfg.model_tags.insert(
            "default".into(),
            crate::config::ModelTag {
                name: "Pinned".into(),
                color: Some("green".into()),
            },
        );

        let mut cat = Catalog::default();
        cat.providers
            .insert("anth".into(), ProviderEntry::default());
        cat.providers
            .insert("local".into(), ProviderEntry::default());
        cat.presets.insert("anth/opus".into(), preset("anth"));
        cat.presets.insert("local/fast".into(), preset("local"));
        cat.roles.insert("default".into(), "anth/opus".into());
        cat.roles.insert("judge".into(), "anth/opus".into());
        cat.model_tags.insert(
            "default".into(),
            crate::config::ModelTag {
                name: "Lower".into(),
                color: None,
            },
        );
        cat.model_tags.insert(
            "local/fast".into(),
            crate::config::ModelTag {
                name: "Local".into(),
                color: Some("blue".into()),
            },
        );
        cat.fallback_chains.insert(
            "default".into(),
            vec!["local/fast:low".into(), "nope".into()],
        );
        cat.fallback_chains
            .insert("judge".into(), vec!["@default".into()]);
        cat.fallback_chains
            .insert("anth/*".into(), vec!["local/fast".into()]);
        cat.fallback_chains
            .insert("ghost/*".into(), vec!["local/fast".into()]);

        cfg.absorb(cat);
        crate::validate_config(&cfg).expect("role and wildcard chains validate");
        assert_eq!(cfg.model_tags["default"].name, "Pinned");
        assert_eq!(cfg.model_tags["local/fast"].name, "Local");
        assert_eq!(cfg.fallback_chains["default"], vec!["local/fast:low"]);
        assert_eq!(cfg.fallback_chains["judge"], vec!["@default"]);
        assert_eq!(cfg.fallback_chains["anth/*"], vec!["local/fast"]);
        assert!(!cfg.fallback_chains.contains_key("ghost/*"));
    }
}
