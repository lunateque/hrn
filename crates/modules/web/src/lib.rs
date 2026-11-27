//! `web`: `web_fetch` (public-page fetch) and `web_search` (ordered provider chain).
//!
//! ```toml
//! [web]
//! chain = ["brave", "duckduckgo"]  # names from the embedded backends.toml, tried in this order
//! timeout_secs = 20
//! ddg_interval_ms = 1500
//! searxng_url = "https://searx.example"
//! ```
//!
//! API keys are read from the environment only, never from the file; which variable each backend
//! reads (`BRAVE_API_KEY`, `PERPLEXITY_API_KEY`, `PARALLEL_API_KEY`, `FIRECRAWL_API_KEY`, ...) is
//! listed in `crates/edge/tools/src/web_search/backends.toml`.

mod fetch;
mod search;

use hrn_edge_http::HttpClient;
use hrn_edge_tools::{PageFetcher, SearchBackend, WebSearchConfig};
use hrn_kernel::serde::Deserialize;
use hrn_kernel::{HrnError, Registry, Result};
use std::sync::Arc;
use std::time::Duration;

/// The `[web]` config table.
#[derive(Debug, Default, Deserialize)]
#[serde(crate = "hrn_kernel::serde", deny_unknown_fields)]
struct WebConfig {
    /// Backends in the order they are tried (default: the built-in chain).
    chain: Option<Vec<String>>,
    /// Per-backend request timeout in seconds (default 20).
    timeout_secs: Option<u64>,
    /// Minimum milliseconds between `DuckDuckGo` requests (default 1500).
    ddg_interval_ms: Option<u64>,
    /// `SearXNG` base URL (default: `SEARXNG_URL` / `SEARXNG_ENDPOINT`).
    searxng_url: Option<String>,
}

fn bad(msg: String) -> HrnError {
    HrnError::Config(msg)
}

impl WebConfig {
    fn lower(self) -> Result<WebSearchConfig> {
        let chain = match self.chain {
            None => SearchBackend::default_chain()?,
            Some(names) => {
                let mut chain = Vec::with_capacity(names.len());
                for name in names {
                    let backend = SearchBackend::parse(name.trim())?.ok_or_else(|| {
                        bad(format!(
                            "[web] chain: unknown backend `{name}` (expected one of {})",
                            SearchBackend::names()
                                .map_or_else(|e| e.to_string(), |names| names.join(", "))
                        ))
                    })?;
                    if chain.contains(&backend) {
                        return Err(bad(format!("[web] chain lists `{name}` twice")));
                    }
                    chain.push(backend);
                }
                chain
            }
        };
        let cfg = WebSearchConfig::new(
            chain,
            self.timeout_secs.unwrap_or(20).max(1),
            Duration::from_millis(self.ddg_interval_ms.unwrap_or(1500)),
        )?;
        match self.searxng_url.filter(|u| !u.trim().is_empty()) {
            Some(url) => {
                let searxng = SearchBackend::parse("searxng")?.ok_or_else(|| {
                    HrnError::Internal("backends.toml has no `searxng` backend".to_owned())
                })?;
                Ok(cfg.with_base_url(searxng, &url))
            }
            None => Ok(cfg),
        }
    }
}

/// Register `web_fetch` and `web_search`.
///
/// # Errors
///
/// When `[web]` is malformed, the HTTP client cannot start, or a tool name is taken.
pub fn register(registry: &mut Registry) -> Result {
    let cfg = registry.config::<WebConfig>()?.lower()?;
    let http = registry.shared().get_or_init(HttpClient::new)?;
    registry.tool(Arc::new(search::WebSearchTool::new(
        http.as_ref().clone(),
        cfg,
    )))?;
    let fetcher = PageFetcher::shared(&registry.shared_arc())?;
    registry.tool(Arc::new(fetch::WebFetchTool::new(fetcher)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::serde_json::json;

    fn lower(config: hrn_kernel::serde_json::Value) -> Result<WebSearchConfig> {
        hrn_kernel::serde_json::from_value::<WebConfig>(config)
            .map_err(|e| bad(e.to_string()))?
            .lower()
    }

    #[test]
    fn the_chain_is_typed_and_validated() {
        let default = lower(json!({})).expect("default chain");
        assert_eq!(
            default.chain,
            SearchBackend::default_chain().expect("descriptors")
        );

        let custom = lower(json!({"chain": ["duckduckgo", "firecrawl"], "ddg_interval_ms": 0}))
            .expect("custom chain");
        let names: Vec<&str> = custom.chain.iter().map(|b| b.name()).collect();
        assert_eq!(names, ["duckduckgo", "firecrawl"]);

        for bad_chain in [json!(["bing"]), json!(["exa", "exa"])] {
            assert!(lower(json!({"chain": bad_chain})).is_err(), "{bad_chain}");
        }
        assert!(
            lower(json!({"brave_api_key": "x"})).is_err(),
            "keys are env-only; unknown fields are rejected"
        );
    }
}
