//! Reads providers and models in omp's `models.yml` shape (`providers: <name>: baseUrl, api,
//! apiKey | auth, models: [id, name, contextWindow, maxTokens, reasoning, input, cost]`), so a
//! block copies between omp's file, hrn's `~/.harness/models.yml` and the built-in defaults.
//! omp keys hrn has no use for (`compat`, `contextPromotionTarget`, ...) are ignored.
//!
//! Credentials are never taken from the file's text: `apiKey` names an environment variable
//! (read from the process, then the dotenv file next to the models file) or a `!command` that
//! reads one of the files under `auth-gateway-tokens/` (the command is not run; the file is
//! read directly). `auth: none` is a keyless server; `auth: oauth` is a subscription login
//! (`hrn login claude|chatgpt`) and the provider exists only while that login does.

use crate::catalog::Catalog;
use crate::config::{AuthMode, ModelPreset, ProviderEntry, ProviderKind};
use crate::yaml::{self, Yaml};
use hrn_edge_oauth::Subscription;
use hrn_kernel::ModelPrice;
use std::path::Path;

/// Where a models file's credentials live.
pub(crate) struct Ctx<'a> {
    /// Directory of the models file: `.env` and `auth-gateway-tokens/` sit in it.
    pub(crate) dir: &'a Path,
    /// Directory of the `hrn login` credential files.
    pub(crate) auth_dir: &'a Path,
    /// When `true` (built-in `models.yml`), API-key providers only activate when a mapped env
    /// variable is non-empty (so bare `GROQ_API_KEY` unlocks `groq/*` without cluttering
    /// unconfigured installs).
    pub(crate) require_env_for_apikey: bool,
    /// Optional env lookup override (tests); `None` reads `std::env::var`.
    pub(crate) env_lookup: Option<crate::catalog::EnvLookup<'a>>,
}

impl Ctx<'_> {
    fn lookup_env(&self, name: &str) -> Option<String> {
        let from_env = match self.env_lookup {
            Some(f) => f(name),
            None => std::env::var(name).ok(),
        };
        if let Some(v) = from_env.filter(|v| !v.trim().is_empty()) {
            return Some(v);
        }
        let dotenv = self.dir.join(".env");
        let text = std::fs::read_to_string(dotenv).ok()?;
        crate::dotenv::lookup(&text, name).filter(|v| !v.trim().is_empty())
    }
}

/// The providers, models and roles of `text`; a provider this build cannot serve is left out.
pub(crate) fn load(text: &str, ctx: &Ctx<'_>) -> Catalog {
    let mut catalog = Catalog::default();
    let Some(doc) = yaml::parse(text) else {
        return catalog;
    };
    add_doc_roles(&mut catalog, &doc);
    let Some(providers) = doc.get("providers") else {
        return catalog;
    };
    let mut first_activated_key: Option<String> = None;
    for (name, node) in providers.entries() {
        let models = node.get("models").map_or(&[][..], Yaml::items);
        if models.is_empty() {
            // omp keeps per-model overrides under built-in providers; nothing to serve here.
            continue;
        }
        let Some(entry) = provider(name, node, ctx) else {
            continue;
        };
        for model in models {
            let Some(id) = model.get("id").and_then(Yaml::str) else {
                continue;
            };
            let price = (entry.auth_mode.subscription().is_none())
                .then(|| model.get("cost").and_then(price))
                .flatten();
            let compat = parse_compat(model.get("compat"));
            let vision = model.get("input").and_then(|node| {
                let items = node.items();
                (!items.is_empty()).then(|| {
                    items
                        .iter()
                        .filter_map(Yaml::str)
                        .any(|s| s.eq_ignore_ascii_case("image"))
                })
            });
            let key = Catalog::key(name, id);
            if first_activated_key.is_none() {
                first_activated_key = Some(key.clone());
            }
            catalog.presets.insert(
                key,
                ModelPreset {
                    provider: name.clone(),
                    id: id.to_owned(),
                    name: model.get("name").and_then(Yaml::str).map(str::to_owned),
                    reasoning: model.get("reasoning").and_then(Yaml::bool),
                    vision,
                    prompt_variant: model
                        .get("promptVariant")
                        .and_then(Yaml::str)
                        .map(str::to_owned),
                    context_window: model.get("contextWindow").and_then(Yaml::u64),
                    max_context_window: model.get("maxContextWindow").and_then(Yaml::u64),
                    context_promotion_target: model
                        .get("contextPromotionTarget")
                        .and_then(Yaml::str)
                        .map(str::to_owned),
                    max_tokens: model
                        .get("maxTokens")
                        .and_then(Yaml::u64)
                        .and_then(|v| u32::try_from(v).ok()),
                    thinking_budgets: thinking(model),
                    price,
                    replay_reasoning_content: compat.replay_reasoning_content,
                    reasoning_effort_map: compat.reasoning_effort_map,
                    thinking_format: compat.thinking_format,
                    prompt_cache_session_header: compat.prompt_cache_session_header,
                    stream_idle_timeout_ms: compat.stream_idle_timeout_ms,
                    ..ModelPreset::default()
                },
            );
        }
        catalog.providers.insert(name.clone(), entry);
    }
    if ctx.require_env_for_apikey
        && !catalog.roles.contains_key("default")
        && let Some(first) = first_activated_key
    {
        catalog.roles.insert("default".to_owned(), first);
    }
    catalog
}

#[derive(Default)]
struct CompatFields {
    replay_reasoning_content: bool,
    reasoning_effort_map: std::collections::BTreeMap<hrn_kernel::ThinkingLevel, String>,
    thinking_format: Option<String>,
    prompt_cache_session_header: Option<String>,
    stream_idle_timeout_ms: Option<u64>,
}

fn parse_compat(node: Option<&Yaml>) -> CompatFields {
    let Some(compat) = node else {
        return CompatFields::default();
    };
    let mut effort_map = std::collections::BTreeMap::new();
    if let Some(map) = compat.get("reasoningEffortMap") {
        for (raw_level, mapped) in map.entries() {
            if let (Some(level), Some(val)) =
                (hrn_kernel::ThinkingLevel::parse(raw_level), mapped.str())
            {
                effort_map.insert(level, val.to_owned());
            }
        }
    }
    CompatFields {
        replay_reasoning_content: compat
            .get("replayReasoningContent")
            .and_then(Yaml::bool)
            .unwrap_or(false),
        reasoning_effort_map: effort_map,
        thinking_format: compat
            .get("thinkingFormat")
            .and_then(Yaml::str)
            .map(str::to_owned),
        prompt_cache_session_header: compat
            .get("promptCacheSessionHeader")
            .and_then(Yaml::str)
            .map(str::to_owned),
        stream_idle_timeout_ms: compat
            .get("streamIdleTimeoutMs")
            .and_then(Yaml::u64)
            .filter(|&ms| ms > 0),
    }
}

/// The `thinking:` block of a models.yml model (omp `packages/catalog` `ThinkingConfig`): the
/// supported `efforts`, the `defaultLevel` `auto` resolves to, and the `effortBudgets` token
/// table budget-style providers send. `None` when the row has none or names no level we know.
fn thinking(model: &Yaml) -> Option<hrn_kernel::ThinkingBudgets> {
    let block = model.get("thinking")?;
    let efforts = block.get("efforts").map(|list| {
        list.items()
            .iter()
            .filter_map(Yaml::str)
            .filter_map(hrn_kernel::ThinkingLevel::parse)
            .collect::<Vec<_>>()
    });
    let default_level = block
        .get("defaultLevel")
        .and_then(Yaml::str)
        .and_then(hrn_kernel::ThinkingLevel::parse);
    let mut effort_budgets = std::collections::BTreeMap::new();
    if let Some(map) = block.get("effortBudgets") {
        for (level, tokens) in map.entries() {
            if let (Some(level), Some(tokens)) =
                (hrn_kernel::ThinkingLevel::parse(level), tokens.u64())
            {
                let budget = u32::try_from(tokens).unwrap_or(u32::MAX);
                effort_budgets.insert(level, budget);
            }
        }
    }
    let empty = efforts.as_ref().is_none_or(std::vec::Vec::is_empty);
    (default_level.is_some() || !effort_budgets.is_empty() || !empty).then(|| {
        hrn_kernel::ThinkingBudgets {
            efforts: (!empty).then_some(efforts.unwrap_or_default()),
            default_level,
            effort_budgets,
        }
    })
}

/// The provider as hrn runs it; `None` when its protocol or credentials are not supported.
fn provider(name: &str, node: &Yaml, ctx: &Ctx<'_>) -> Option<ProviderEntry> {
    let api = node.get("api").and_then(Yaml::str)?;
    let auth = node.get("auth").and_then(Yaml::str);
    let mut entry = ProviderEntry {
        base_url: node.get("baseUrl").and_then(Yaml::str).map(str::to_owned),
        ..ProviderEntry::default()
    };
    match (api, auth) {
        ("anthropic-messages" | "anthropic", Some("oauth")) => {
            subscription(&mut entry, Subscription::Claude, ctx)?;
        }
        ("openai-codex-responses" | "openai-codex", Some("oauth")) => {
            subscription(&mut entry, Subscription::Chatgpt, ctx)?;
        }
        ("kimi-code", Some("oauth")) => {
            subscription(&mut entry, Subscription::KimiCode, ctx)?;
        }
        (_, Some("oauth")) => return None,
        ("openai-completions" | "openai-chat" | "openai", _) => {
            entry.kind = ProviderKind::OpenaiChat;
        }
        ("openai-responses", _) => entry.kind = ProviderKind::OpenaiResponses,
        ("anthropic-messages" | "anthropic", _) => entry.kind = ProviderKind::Anthropic,
        ("google-generative-ai" | "google" | "gemini", _) => entry.kind = ProviderKind::Gemini,
        _ => return None,
    }
    entry.prompt_cache = entry.kind == ProviderKind::Anthropic;
    if auth == Some("oauth") || auth == Some("none") {
        return Some(entry);
    }
    match node.get("apiKey").and_then(Yaml::str) {
        None => {}
        Some(key) if key.starts_with('!') => {
            let file = key_file(key)?;
            entry.token_file = Some(
                ctx.dir
                    .join("auth-gateway-tokens")
                    .join(file)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        Some(raw) => {
            let key = env_var_name(raw)?;
            entry.token_env = Some(key.to_owned());
            let dotenv = ctx.dir.join(".env");
            if dotenv.is_file() {
                entry.env_file = Some(dotenv.to_string_lossy().into_owned());
            }
        }
    }
    if let Some(rule) = crate::provider_env::rule_for(name) {
        for env_name in rule.key_envs {
            if entry.token_env.as_deref() != Some(env_name.as_str())
                && !entry.token_envs.contains(&env_name)
            {
                entry.token_envs.push(env_name);
            }
        }
        if let Some((_, url)) =
            crate::provider_env::first_env_with(&rule.base_url_envs, &|k| ctx.lookup_env(k))
        {
            entry.base_url = Some(url);
        }
    }
    if ctx.require_env_for_apikey {
        let envs: Vec<String> = entry
            .token_env
            .iter()
            .chain(entry.token_envs.iter())
            .cloned()
            .collect();
        if !envs.is_empty()
            && crate::provider_env::first_env_with(&envs, &|k| ctx.lookup_env(k)).is_none()
        {
            return None;
        }
    }
    Some(entry)
}

fn subscription(entry: &mut ProviderEntry, kind: Subscription, ctx: &Ctx<'_>) -> Option<()> {
    let file = ctx.auth_dir.join(kind.file_name());
    hrn_edge_http::guard::check_token_path(&file.to_string_lossy()).ok()?;
    if !file.is_file() {
        return None;
    }
    (entry.kind, entry.auth_mode) = match kind {
        Subscription::Claude => (ProviderKind::Anthropic, AuthMode::Claude),
        Subscription::Chatgpt => (ProviderKind::OpenaiResponses, AuthMode::Chatgpt),
        Subscription::KimiCode => (ProviderKind::OpenaiChat, AuthMode::KimiCode),
    };
    entry.base_url = None;
    entry.token_file = Some(file.to_string_lossy().into_owned());
    Some(())
}

/// The name of the `auth-gateway-tokens/<name>` file a `!command` key reads.
fn key_file(command: &str) -> Option<&str> {
    let after = command.split_once("auth-gateway-tokens/")?.1;
    let end = after.find(['\'', '"', ' ', ')'])?;
    let name = after.get(..end)?;
    (!name.is_empty() && !name.contains(['/', '\\'])).then_some(name)
}

fn env_var_name(raw: &str) -> Option<&str> {
    let key = raw
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(raw);
    let ok = key.starts_with(|c: char| c.is_ascii_uppercase())
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    ok.then_some(key)
}

/// omp's `cost: { input, output, cacheRead, cacheWrite }` (USD per million tokens).
fn price(cost: &Yaml) -> Option<ModelPrice> {
    Some(ModelPrice {
        input: cost.get("input").and_then(Yaml::f64)?,
        output: cost.get("output").and_then(Yaml::f64)?,
        cache_read: cost.get("cacheRead").and_then(Yaml::f64),
        cache_write: cost.get("cacheWrite").and_then(Yaml::f64),
    })
}

/// Add the roles and cycle of omp's `config.yml` text to `catalog`.
pub(crate) fn add_roles(catalog: &mut Catalog, config_yml: &str) {
    if let Some(doc) = yaml::parse(config_yml) {
        add_doc_roles(catalog, &doc);
    }
}

/// Add the roles (`modelRoles`), `modelTags`, `ctrl+p` cycle (`cycleOrder`),
/// `retry.fallbackChains`, `modelPresets`, and `task.agentModelOverrides` of `doc` to `catalog`.
fn add_doc_roles(catalog: &mut Catalog, doc: &Yaml) {
    if let Some(roles) = doc.get("modelRoles") {
        for (role, target) in roles.entries() {
            let Some(target) = target.str().map(str::trim).filter(|t| !t.is_empty()) else {
                continue;
            };
            catalog.roles.insert(role.clone(), target.to_owned());
        }
    }
    add_doc_model_tags(catalog, doc);
    if let Some(order) = doc.get("cycleOrder") {
        catalog.cycle = order
            .items()
            .iter()
            .filter_map(Yaml::str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    let chains = doc
        .get("retry")
        .and_then(|r| r.get("fallbackChains"))
        .or_else(|| doc.get("fallbackChains"))
        .or_else(|| doc.get("fallback"));
    if let Some(chains) = chains {
        for (owner, targets) in chains.entries() {
            let list: Vec<String> = targets
                .items()
                .iter()
                .filter_map(Yaml::str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if !list.is_empty() {
                catalog.fallback_chains.insert(owner.clone(), list);
            }
        }
    }
    if let Some(presets) = doc.get("modelPresets") {
        for (name, block) in presets.entries() {
            let mut rp = crate::config::RolePreset::default();
            if let Some(roles) = block.get("modelRoles") {
                for (role, target) in roles.entries() {
                    if let Some(t) = target.str().map(str::trim).filter(|s| !s.is_empty()) {
                        rp.model_roles.insert(role.clone(), t.to_owned());
                    }
                }
            }
            rp.default_thinking_level = block
                .get("defaultThinkingLevel")
                .and_then(Yaml::str)
                .and_then(hrn_kernel::ThinkingLevel::parse);
            if !rp.model_roles.is_empty() || rp.default_thinking_level.is_some() {
                catalog.role_presets.insert(name.clone(), rp);
            }
        }
    }
    if let Some(overrides) = doc.get("task").and_then(|t| t.get("agentModelOverrides")) {
        for (agent, target) in overrides.entries() {
            if let Some(t) = target.str().map(str::trim).filter(|s| !s.is_empty()) {
                catalog.agent_models.insert(agent.clone(), t.to_owned());
            }
        }
    }
}

/// Parse omp's `modelTags:` map (`key: "label"` or `key: { name: "label", color: "cyan" }`).
fn add_doc_model_tags(catalog: &mut Catalog, doc: &Yaml) {
    let Some(tags) = doc.get("modelTags") else {
        return;
    };
    for (key, val) in tags.entries() {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let parsed = if let Some(name) = val.str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(crate::config::ModelTag {
                name: name.to_owned(),
                color: None,
            })
        } else {
            val.get("name")
                .and_then(Yaml::str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|name| crate::config::ModelTag {
                    name: name.to_owned(),
                    color: val
                        .get("color")
                        .and_then(Yaml::str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                })
        };
        if let Some(tag) = parsed {
            catalog.model_tags.insert(key.to_owned(), tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODELS: &str = "\
providers:
  anthropic:
    modelOverrides:
      claude-opus-5:
        compactionModel: x
  gw:
    baseUrl: https://gw.example/v1
    apiKey: '!powershell -Command \"(Get-Content -Raw ''C:/u/.omp/agent/auth-gateway-tokens/gw.token'').Trim()\"'
    api: openai-completions
    models:
      - id: a/b
        name: A B
        reasoning: false
        contextWindow: 262144
        cost: { input: 1, output: 2, cacheRead: 0.5, cacheWrite: 0 }
  keyed:
    baseUrl: https://k.example/v1
    apiKey: ${K_API_KEY}
    api: openai-chat
    models: [{ id: m }]
  local:
    baseUrl: http://localhost:1/v1
    auth: none
    api: openai-completions
    models:
      - id: m
  sub:
    api: anthropic-messages
    auth: oauth
    models:
      - id: claude-x
        contextWindow: 1000000
        cost: { input: 5, output: 25 }
";

    fn ctx<'a>(dir: &'a Path, auth: &'a Path) -> Ctx<'a> {
        Ctx {
            dir,
            auth_dir: auth,
            require_env_for_apikey: false,
            env_lookup: None,
        }
    }

    #[test]
    fn providers_models_and_credentials_map_onto_hrn_types() {
        let dir = Path::new("/omp");
        let cat = load(MODELS, &ctx(dir, Path::new("/nowhere")));
        assert_eq!(
            cat.providers.keys().map(String::as_str).collect::<Vec<_>>(),
            ["gw", "keyed", "local"],
            "overrides-only and logged-out subscription providers are left out"
        );
        let gw = &cat.providers["gw"];
        assert_eq!(gw.kind, ProviderKind::OpenaiChat);
        assert!(
            gw.token_file
                .as_deref()
                .is_some_and(|f| f.replace('\\', "/") == "/omp/auth-gateway-tokens/gw.token")
        );
        assert_eq!(
            cat.providers["keyed"].token_env.as_deref(),
            Some("K_API_KEY")
        );
        assert!(cat.providers["local"].token_env.is_none());
        let model = &cat.presets["gw/a/b"];
        assert_eq!(model.name.as_deref(), Some("A B"));
        assert_eq!(model.reasoning, Some(false));
        assert_eq!(model.context_window, Some(262_144));
        let price = model.price.expect("price");
        assert_eq!(
            (price.input, price.cache_read, price.cache_write),
            (1.0, Some(0.5), Some(0.0))
        );
    }

    #[test]
    fn a_subscription_provider_follows_its_login_and_carries_no_price() {
        let auth = std::env::temp_dir().join("hrn-models-yml-test-auth");
        std::fs::create_dir_all(&auth).expect("dir");
        std::fs::write(auth.join(Subscription::Claude.file_name()), "{}").expect("file");
        let cat = load(MODELS, &ctx(Path::new("/omp"), &auth));
        let sub = &cat.providers["sub"];
        assert_eq!(sub.auth_mode, AuthMode::Claude);
        assert!(sub.prompt_cache);
        let model = &cat.presets["sub/claude-x"];
        assert_eq!(model.context_window, Some(1_000_000));
        assert!(model.price.is_none());
        std::fs::remove_dir_all(&auth).ok();
    }

    #[test]
    fn roles_preserve_at_refs_level_suffixes_and_cycle_role_names() {
        let mut cat = Catalog::default();
        add_roles(
            &mut cat,
            "modelRoles:\n  fast: gw/a:high\n  ref: '@fast:low'\n  odd: example/model:120b\ncycleOrder:\n  - fast\n  - nope\n",
        );
        assert_eq!(cat.roles["fast"], "gw/a:high");
        assert_eq!(cat.roles["odd"], "example/model:120b");
        assert_eq!(cat.roles["ref"], "@fast:low");
        assert_eq!(cat.cycle, ["fast", "nope"]);
    }

    #[test]
    fn model_tags_parse_map_and_scalar_forms() {
        let mut cat = Catalog::default();
        add_roles(
            &mut cat,
            "\
modelTags:
  default:
    name: Daily
    color: cyan
  gw/a/b: Fast
  empty:
    name: ''
",
        );
        assert_eq!(
            cat.model_tags.get("default"),
            Some(&crate::config::ModelTag {
                name: "Daily".to_owned(),
                color: Some("cyan".to_owned()),
            })
        );
        assert_eq!(
            cat.model_tags.get("gw/a/b"),
            Some(&crate::config::ModelTag {
                name: "Fast".to_owned(),
                color: None,
            })
        );
        assert!(!cat.model_tags.contains_key("empty"));
    }
}
