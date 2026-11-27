//! Live ACP model catalog discovery (`discover_agy_models`).

use super::binary::merged_no_proxy_env;
use super::client::{AcpClient, AcpSpawnSpec};
use super::config::AgyToolMode;
use super::wire::AcpModelOption;
use crate::catalog::Catalog;
use crate::config::{AuthMode, ModelPreset, ProviderEntry, ProviderKind};
use hrn_kernel::Result;
use std::path::Path;
use std::time::Duration;

/// Default timeout for ACP model discovery (`initialize` + `session/new` + `session/close`).
pub const DEFAULT_DISCOVER_TIMEOUT: Duration = Duration::from_secs(15);

/// Result returned by [`discover_agy_models`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredAgyModels {
    /// Models advertised in ACP `session/new` `configOptions` (`category == "model"`).
    pub models: Vec<AcpModelOption>,
    /// Default model id active on the server.
    pub current_model: Option<String>,
    /// Antigravity subscription tier label if reported (e.g. `"Google AI Ultra"`).
    pub tier_name: Option<String>,
    /// `agy` ACP server version reported by `initialize`.
    pub agent_version: String,
}

/// Spawn a short-lived `agy acp --stdio` process, run `initialize` + `session/new`, extract
/// available model options, close the temporary session, and terminate cleanly.
pub async fn discover_agy_models(
    binary: &Path,
    cwd: &Path,
    timeout: Duration,
) -> Result<DiscoveredAgyModels> {
    let spec = AcpSpawnSpec {
        binary: binary.to_path_buf(),
        args: vec!["acp".to_owned(), "--stdio".to_owned()],
        cwd: cwd.to_path_buf(),
        env: merged_no_proxy_env(),
    };
    discover_from_spawn_spec(spec, cwd, timeout).await
}

/// Deterministic helper accepting a custom [`AcpSpawnSpec`] (for mock-ACP tests).
pub async fn discover_from_spawn_spec(
    spec: AcpSpawnSpec,
    cwd: &Path,
    timeout: Duration,
) -> Result<DiscoveredAgyModels> {
    let client = AcpClient::spawn_and_initialize(spec, AgyToolMode::Native, timeout).await?;
    let agent_version = client.capabilities().agent_version.clone();
    let session = client.session_new(cwd, &[], timeout).await?;
    let _ = client
        .session_close(&session.session_id, Duration::from_secs(3))
        .await;
    Ok(DiscoveredAgyModels {
        models: session.available_models,
        current_model: session.current_model,
        tier_name: session.tier_name,
        agent_version,
    })
}

/// Default implicit provider entry used when `agy` CLI is installed and not configured explicitly.
#[must_use]
pub fn default_agy_provider() -> ProviderEntry {
    ProviderEntry {
        kind: ProviderKind::AgyAcp,
        auth_mode: AuthMode::Agy,
        ..ProviderEntry::default()
    }
}

pub(crate) fn default_agy_context_window(model_id: &str) -> u64 {
    let lower = model_id.to_ascii_lowercase();
    if lower.contains("gemini") {
        return 1_000_000;
    }
    if lower.contains("gpt-oss") {
        return 131_072;
    }
    if lower.contains("claude") {
        let is_legacy_200k = lower.contains("claude-3")
            || lower.contains("-4-0")
            || lower.contains("-4-1")
            || lower.contains("-4-5")
            || lower.contains("-4.0")
            || lower.contains("-4.1")
            || lower.contains("-4.5");
        return if is_legacy_200k { 200_000 } else { 1_000_000 };
    }
    200_000
}

pub(crate) fn default_agy_reasoning(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.contains("thinking")
        || lower.contains("high")
        || lower.contains("pro")
        || lower.contains("opus")
        || lower.contains("sonnet")
        || lower.contains("haiku")
        || lower.contains("fable")
        || lower.contains("mythos")
        || lower.contains("gemini-2.5")
        || lower.contains("gemini-3")
        || lower.contains("gemini-4")
        || lower.contains("gpt-oss")
}

/// Convert a live ACP discovery payload into an absorbable [`Catalog`] with context windows,
/// a `<provider>` role pointing at the server's active model, and a fallback `default` role.
#[must_use]
pub fn build_agy_catalog(
    provider_name: &str,
    entry: &ProviderEntry,
    discovered: &DiscoveredAgyModels,
) -> Option<Catalog> {
    let mut catalog = Catalog::default();
    catalog
        .providers
        .insert(provider_name.to_owned(), entry.clone());
    for model in &discovered.models {
        if model.value.trim().is_empty()
            || hrn_edge_http::guard::check_model_id(&model.value).is_err()
        {
            continue;
        }
        let reasoning = Some(default_agy_reasoning(&model.value));
        catalog.presets.insert(
            Catalog::key(provider_name, &model.value),
            ModelPreset {
                provider: provider_name.to_owned(),
                id: model.value.clone(),
                name: (!model.name.is_empty()).then(|| model.name.clone()),
                context_window: Some(default_agy_context_window(&model.value)),
                reasoning,
                ..ModelPreset::default()
            },
        );
    }
    if catalog.presets.is_empty() {
        return None;
    }
    let default_key = discovered
        .current_model
        .as_deref()
        .map(|id| Catalog::key(provider_name, id))
        .filter(|key| catalog.presets.contains_key(key))
        .or_else(|| catalog.presets.keys().next().cloned());
    if let Some(default_key) = default_key {
        catalog
            .roles
            .insert(provider_name.to_owned(), default_key.clone());
        catalog
            .roles
            .entry("default".to_owned())
            .or_insert(default_key);
    }
    Some(catalog)
}
