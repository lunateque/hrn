//! Startup model discovery (`ollama`, `OpenRouter` `/api/v1/models`, generic `OpenAI` `{base}/models`)
//! with strict timeouts and offline-tolerant `<data>/catalog-cache/` persistence.

use crate::catalog::Catalog;
use crate::config::{AuthMode, LlmConfig, ModelPreset, ProviderEntry, ProviderKind};
use hrn_edge_http::HttpClient;
use hrn_kernel::ModelPrice;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

/// Default loopback discovery budget for local Ollama (omp: 150-250 ms).
pub const DEFAULT_OLLAMA_TIMEOUT: Duration = Duration::from_millis(250);
/// Default context window for Ollama models when `/api/show` omits `model_info`.
const OLLAMA_FALLBACK_CONTEXT: u64 = 128_000;
/// Default max output tokens for Ollama models.
const OLLAMA_DEFAULT_MAX_TOKENS: u32 = 8_192;
/// Maximum HTTP response payload accepted during discovery (4 MiB).
const MAX_DISCOVERY_BYTES: usize = 4 * 1024 * 1024;
/// Default timeout for remote `OpenRouter` / `OpenAI` `/models` discovery.
const REMOTE_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Freshness TTL for cached Antigravity (`agy_acp`) model catalogs so warm startups avoid
/// spawning an ACP discovery subprocess on every launch.
pub const AGY_CACHE_TTL: Duration = Duration::from_secs(3_600);

pub use crate::agy_acp::discover::{build_agy_catalog, default_agy_provider};

/// Discover models from a local Ollama server (`/api/tags` + `/api/show`).
///
/// Implicit keyless provider: returns `None` silently within `timeout` when Ollama is not running
/// or unreachable. Context length is extracted from `model_info` keys (`*.context_length`,
/// `*.num_ctx`, `*.context_window`), falling back to `128_000` with `max_tokens = 8_192`.
pub async fn discover_ollama(
    http: &HttpClient,
    root_override: Option<&str>,
    timeout: Duration,
) -> Option<Catalog> {
    let deadline = Instant::now() + timeout;
    let root = root_override.map_or_else(
        crate::provider_env::ollama_root_url,
        crate::provider_env::normalize_ollama_root,
    );
    let tags_url = format!("{root}/api/tags");
    let remaining = deadline.checked_duration_since(Instant::now())?;
    let resp = tokio::time::timeout(
        remaining,
        http.get(&tags_url, &[], remaining, MAX_DISCOVERY_BYTES),
    )
    .await
    .ok()?
    .ok()?;
    if !resp.status.is_success() {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&resp.body).ok()?;
    let models = parsed.get("models")?.as_array()?;
    if models.is_empty() {
        return None;
    }

    let mut catalog = Catalog::default();
    catalog.providers.insert(
        "ollama".to_owned(),
        ProviderEntry {
            kind: ProviderKind::OpenaiChat,
            base_url: Some(format!("{root}/v1")),
            auth_mode: AuthMode::Default,
            ..ProviderEntry::default()
        },
    );

    let show_url = format!("{root}/api/show");
    for item in models {
        let Some(name) = item
            .get("name")
            .or_else(|| item.get("model"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let (context_window, reasoning) = match deadline.checked_duration_since(Instant::now()) {
            Some(left) if !left.is_zero() => fetch_ollama_show(http, &show_url, name, left).await,
            _ => (OLLAMA_FALLBACK_CONTEXT, None),
        };
        catalog.presets.insert(
            Catalog::key("ollama", name),
            ModelPreset {
                provider: "ollama".to_owned(),
                id: name.to_owned(),
                name: Some(name.to_owned()),
                context_window: Some(context_window),
                max_tokens: Some(OLLAMA_DEFAULT_MAX_TOKENS),
                reasoning,
                ..ModelPreset::default()
            },
        );
    }
    (!catalog.presets.is_empty()).then_some(catalog)
}

async fn fetch_ollama_show(
    http: &HttpClient,
    show_url: &str,
    model: &str,
    budget: Duration,
) -> (u64, Option<bool>) {
    let body = match serde_json::to_vec(&serde_json::json!({ "name": model })) {
        Ok(b) => bytes::Bytes::from(b),
        Err(_) => return (OLLAMA_FALLBACK_CONTEXT, None),
    };
    let headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    let Ok(Ok(resp)) =
        tokio::time::timeout(budget, http.post_raw(show_url, &headers, body, budget)).await
    else {
        return (OLLAMA_FALLBACK_CONTEXT, None);
    };
    if !resp.status.is_success() {
        return (OLLAMA_FALLBACK_CONTEXT, None);
    }
    let Ok(val) = serde_json::from_slice::<Value>(&resp.body) else {
        return (OLLAMA_FALLBACK_CONTEXT, None);
    };
    let ctx = extract_ollama_context(&val).unwrap_or(OLLAMA_FALLBACK_CONTEXT);
    let reasoning = val
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|caps| {
            caps.iter()
                .filter_map(Value::as_str)
                .any(|c| c.eq_ignore_ascii_case("thinking"))
        });
    (ctx, reasoning)
}

fn extract_ollama_context(show: &Value) -> Option<u64> {
    let info = show.get("model_info")?.as_object()?;
    for (k, v) in info {
        let low = k.to_ascii_lowercase();
        if (low.ends_with(".context_length")
            || low.ends_with(".num_ctx")
            || low.ends_with(".context_window")
            || low == "context_length"
            || low == "num_ctx"
            || low == "context_window")
            && let Some(n) = v.as_u64().filter(|&n| n > 0)
        {
            return Some(n);
        }
    }
    None
}

/// Generic OpenAI-compatible `{base}/models` discovery.
pub async fn openai_models_list(
    http: &HttpClient,
    provider_name: &str,
    entry: &ProviderEntry,
    timeout: Duration,
) -> Option<Catalog> {
    let url = format!("{}/models", entry.effective_base_url());
    let mut headers = entry.resolve_env_headers();
    if let Ok(Some(token)) = entry.resolve_static_token()
        && !token.is_empty()
    {
        headers.push(("authorization".to_owned(), format!("Bearer {token}")));
    }
    let resp = tokio::time::timeout(
        timeout,
        http.get(&url, &headers, timeout, MAX_DISCOVERY_BYTES),
    )
    .await
    .ok()?
    .ok()?;
    if !resp.status.is_success() {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&resp.body).ok()?;
    let rows = parsed.get("data")?.as_array()?;
    let mut catalog = Catalog::default();
    catalog
        .providers
        .insert(provider_name.to_owned(), entry.clone());
    for row in rows {
        let Some(id) = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let context_window = row
            .get("context_window")
            .or_else(|| row.get("max_context_length"))
            .or_else(|| row.get("context_length"))
            .and_then(Value::as_u64)
            .filter(|&n| n > 0);
        catalog.presets.insert(
            Catalog::key(provider_name, id),
            ModelPreset {
                provider: provider_name.to_owned(),
                id: id.to_owned(),
                name: row.get("name").and_then(Value::as_str).map(str::to_owned),
                context_window,
                ..ModelPreset::default()
            },
        );
    }
    (!catalog.presets.is_empty()).then_some(catalog)
}

/// Credential-scoped `OpenRouter` `/api/v1/models` discovery, keeping only tool-capable models.
pub async fn discover_openrouter(
    http: &HttpClient,
    entry: &ProviderEntry,
    timeout: Duration,
) -> Option<Catalog> {
    let token = entry.resolve_static_token().ok()??;
    if token.is_empty() {
        return None;
    }
    let url = format!("{}/models", entry.effective_base_url());
    let mut headers = vec![
        ("authorization".to_owned(), format!("Bearer {token}")),
        ("X-OpenRouter-Title".to_owned(), "hrn".to_owned()),
    ];
    headers.extend(entry.resolve_env_headers());
    let resp = tokio::time::timeout(
        timeout,
        http.get(&url, &headers, timeout, MAX_DISCOVERY_BYTES),
    )
    .await
    .ok()?
    .ok()?;
    if !resp.status.is_success() {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&resp.body).ok()?;
    let rows = parsed.get("data")?.as_array()?;
    let mut catalog = Catalog::default();
    catalog
        .providers
        .insert("openrouter".to_owned(), entry.clone());
    for row in rows {
        let supports_tools = row
            .get("supported_parameters")
            .and_then(Value::as_array)
            .is_some_and(|params| {
                params
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|p| p == "tools")
            });
        if !supports_tools {
            continue;
        }
        let Some(id) = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let context_window = row
            .get("context_length")
            .and_then(Value::as_u64)
            .filter(|&n| n > 0);
        let max_tokens = row
            .get("top_provider")
            .and_then(|tp| tp.get("max_completion_tokens"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok());
        let price = parse_openrouter_price(row.get("pricing"));
        let reasoning = row
            .get("supported_parameters")
            .and_then(Value::as_array)
            .map(|params| {
                params
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|p| p == "reasoning" || p == "include_reasoning")
            });
        catalog.presets.insert(
            Catalog::key("openrouter", id),
            ModelPreset {
                provider: "openrouter".to_owned(),
                id: id.to_owned(),
                name: row.get("name").and_then(Value::as_str).map(str::to_owned),
                context_window,
                max_tokens,
                reasoning,
                price,
                thinking_format: Some("openrouter".to_owned()),
                ..ModelPreset::default()
            },
        );
    }
    (!catalog.presets.is_empty()).then_some(catalog)
}

fn parse_openrouter_price(pricing: Option<&Value>) -> Option<ModelPrice> {
    let obj = pricing?.as_object()?;
    let per_m = |key: &str| -> Option<f64> {
        let raw = obj.get(key)?;
        let per_tok = raw
            .as_f64()
            .or_else(|| raw.as_str().and_then(|s| s.parse::<f64>().ok()))?;
        (per_tok.is_finite() && per_tok >= 0.0).then_some(per_tok * 1_000_000.0)
    };
    Some(ModelPrice {
        input: per_m("prompt")?,
        output: per_m("completion")?,
        cache_read: per_m("input_cache_read"),
        cache_write: per_m("input_cache_write"),
    })
}

/// Discover models exposed by an Antigravity (`agy_acp`) provider via ACP `session/new`.
async fn discover_agy_catalog(
    provider_name: &str,
    entry: &ProviderEntry,
    timeout: Duration,
) -> Option<Catalog> {
    let binary = crate::agy_acp::discover_agy_binary(entry.agy.binary.as_deref())?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let discovered = crate::agy_acp::discover_agy_models(&binary, &cwd, timeout)
        .await
        .ok()?;
    build_agy_catalog(provider_name, entry, &discovered)
}

fn load_fresh_agy_cache(cache_dir: &Path, provider: &str, max_age: Duration) -> Option<Catalog> {
    let path = cache_dir.join(format!("{provider}.json"));
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    if modified.elapsed().ok()? > max_age {
        return None;
    }
    let cat = load_cache(cache_dir, provider)?;
    let complete = !cat.presets.is_empty()
        && cat.presets.values().all(|p| p.context_window.is_some())
        && cat.roles.contains_key(provider);
    complete.then_some(cat)
}

fn patch_cached_agy(mut cat: Catalog, provider_name: &str, entry: &ProviderEntry) -> Catalog {
    cat.providers
        .insert(provider_name.to_owned(), entry.clone());
    for preset in cat.presets.values_mut() {
        // Recompute default context window & reasoning on cache load so stale caches upgrade
        // immediately when model window heuristics improve (user overrides still win in absorb).
        preset.context_window = Some(crate::agy_acp::discover::default_agy_context_window(
            &preset.id,
        ));
        if preset.reasoning != Some(true)
            && crate::agy_acp::discover::default_agy_reasoning(&preset.id)
        {
            preset.reasoning = Some(true);
        }
    }
    if !cat.roles.contains_key(provider_name)
        && let Some(first) = cat.presets.keys().next().cloned()
    {
        cat.roles.insert(provider_name.to_owned(), first.clone());
        cat.roles.entry("default".to_owned()).or_insert(first);
    }
    cat
}

/// Run startup discovery (Ollama loopback + credential-scoped `OpenRouter` + explicit or
/// auto-detected `agy_acp`) concurrently under hard timeouts, reading/writing offline-tolerant
/// cache files in `cache_dir`.
pub async fn discover_startup(
    http: &HttpClient,
    cfg: &LlmConfig,
    cache_dir: Option<&Path>,
) -> Vec<Catalog> {
    Box::pin(discover_startup_inner(http, cfg, cache_dir)).await
}

async fn discover_startup_inner(
    http: &HttpClient,
    cfg: &LlmConfig,
    cache_dir: Option<&Path>,
) -> Vec<Catalog> {
    let disabled = |name: &str| {
        cfg.disabled_providers
            .iter()
            .any(|p| p.eq_ignore_ascii_case(name))
    };

    let run_ollama = !disabled("ollama");
    let openrouter_entry = (!disabled("openrouter"))
        .then(|| cfg.providers.get("openrouter"))
        .flatten()
        .filter(|entry| {
            entry
                .resolve_static_token()
                .ok()
                .flatten()
                .is_some_and(|t| !t.is_empty())
        })
        .cloned();

    let mut agy_targets: Vec<(String, ProviderEntry)> = cfg
        .providers
        .iter()
        .filter(|(name, entry)| entry.kind == ProviderKind::AgyAcp && !disabled(name))
        .map(|(name, entry)| (name.clone(), entry.clone()))
        .collect();
    let has_explicit_agy = cfg
        .providers
        .values()
        .any(|entry| entry.kind == ProviderKind::AgyAcp);
    if !has_explicit_agy && !disabled("agy") && crate::agy_acp::discover_agy_binary(None).is_some()
    {
        agy_targets.push(("agy".to_owned(), default_agy_provider()));
    }

    let ollama_fut = async {
        if !run_ollama {
            return None;
        }
        if let Some(cat) = discover_ollama(http, None, DEFAULT_OLLAMA_TIMEOUT).await {
            if let Some(dir) = cache_dir {
                save_cache(dir, "ollama", &cat);
            }
            Some(cat)
        } else {
            cache_dir.and_then(|dir| load_cache(dir, "ollama"))
        }
    };

    let openrouter_fut = async {
        let entry = openrouter_entry.as_ref()?;
        if let Some(cat) = discover_openrouter(http, entry, REMOTE_DISCOVERY_TIMEOUT).await {
            if let Some(dir) = cache_dir {
                save_cache(dir, "openrouter", &cat);
            }
            Some(cat)
        } else {
            cache_dir.and_then(|dir| load_cache(dir, "openrouter"))
        }
    };

    let agy_fut = async {
        let mut cats = Vec::new();
        for (name, entry) in agy_targets {
            if crate::agy_acp::discover_agy_binary(entry.agy.binary.as_deref()).is_none() {
                continue;
            }
            if let Some(cached) =
                cache_dir.and_then(|dir| load_fresh_agy_cache(dir, &name, AGY_CACHE_TTL))
            {
                cats.push(patch_cached_agy(cached, &name, &entry));
                continue;
            }
            if let Some(cat) =
                discover_agy_catalog(&name, &entry, crate::agy_acp::DEFAULT_DISCOVER_TIMEOUT).await
            {
                if let Some(dir) = cache_dir {
                    save_cache(dir, &name, &cat);
                }
                cats.push(cat);
            } else if let Some(cached) = cache_dir.and_then(|dir| load_cache(dir, &name)) {
                cats.push(patch_cached_agy(cached, &name, &entry));
            }
        }
        cats
    };

    let (ollama_cat, openrouter_cat, agy_cats) = tokio::join!(ollama_fut, openrouter_fut, agy_fut);
    let mut out = Vec::new();
    if let Some(cat) = ollama_cat {
        out.push(cat);
    }
    if let Some(cat) = openrouter_cat {
        out.push(cat);
    }
    out.extend(agy_cats);
    out
}

/// Best-effort single-provider catalog discovery (used after a successful `/login`), persisting
/// discovered models to `cache_dir` when available. Never fails the login if offline or unsupported.
pub async fn discover_provider(
    http: &HttpClient,
    provider_name: &str,
    entry: &ProviderEntry,
    cache_dir: Option<&Path>,
    timeout: Duration,
) -> Option<Catalog> {
    let base = entry.effective_base_url();
    let live = if entry.kind == ProviderKind::AgyAcp {
        discover_agy_catalog(provider_name, entry, timeout).await
    } else if provider_name.eq_ignore_ascii_case("ollama") {
        discover_ollama(http, Some(base), timeout).await
    } else if provider_name.eq_ignore_ascii_case("openrouter")
        || crate::openai_chat::is_openrouter_url(base)
    {
        discover_openrouter(http, entry, timeout).await
    } else if entry.auth_mode.subscription().is_none()
        && matches!(
            entry.kind,
            ProviderKind::OpenaiChat | ProviderKind::OpenaiResponses
        )
    {
        openai_models_list(http, provider_name, entry, timeout).await
    } else {
        None
    };
    if let (Some(cat), Some(dir)) = (&live, cache_dir) {
        save_cache(dir, provider_name, cat);
    }
    live
}

/// Persist a discovered provider catalog under `cache_dir/<provider>.json`.
pub fn save_cached_catalog(cache_dir: &Path, provider: &str, catalog: &Catalog) {
    save_cache(cache_dir, provider, catalog);
}

/// Load a previously cached provider catalog from `cache_dir/<provider>.json`.
#[must_use]
pub fn load_cached_catalog(cache_dir: &Path, provider: &str) -> Option<Catalog> {
    load_cache(cache_dir, provider)
}

pub(crate) fn save_cache(cache_dir: &Path, provider: &str, catalog: &Catalog) {
    let _ = std::fs::create_dir_all(cache_dir);
    let path = cache_dir.join(format!("{provider}.json"));
    if let Ok(json) = serde_json::to_string(catalog) {
        let _ = std::fs::write(path, json);
    }
}

pub(crate) fn load_cache(cache_dir: &Path, provider: &str) -> Option<Catalog> {
    let path = cache_dir.join(format!("{provider}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
#[path = "discover_tests.rs"]
mod tests;
