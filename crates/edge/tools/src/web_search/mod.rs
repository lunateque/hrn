//! `web_search` over an ordered chain of backends. The backends (Brave, Tavily, Perplexity,
//! Parallel, Firecrawl, Jina, a `SearXNG` instance, Exa and the keyless `DuckDuckGo` HTML
//! endpoint) are described in the embedded `backends.toml`. The first backend that answers wins;
//! a failure or an empty answer moves on to the next. Keys come from the environment only.

mod apis;
mod backends;
mod ddg;
mod descriptors;

use descriptors::{Descriptor, KeyMode};
use hrn_edge_http::HttpClient;
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

/// Results returned when the caller does not say.
pub const DEFAULT_RESULTS: usize = 10;
/// Most results one search may ask for.
pub const MAX_RESULTS: usize = 20;
const SNIPPET_CHARS: usize = 240;

/// One search backend: a row of `backends.toml`.
#[derive(Clone, Copy, Debug)]
pub struct SearchBackend(&'static Descriptor);

impl PartialEq for SearchBackend {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for SearchBackend {}

impl SearchBackend {
    /// The default chain: the file's order, keyed backends first (skipped without their key),
    /// keyless last.
    ///
    /// # Errors
    ///
    /// When the embedded descriptor file is malformed (a build defect).
    pub fn default_chain() -> Result<Vec<Self>> {
        Ok(descriptors::all()?.iter().map(Self).collect())
    }

    /// Config / display name.
    pub fn name(self) -> &'static str {
        &self.0.name
    }

    /// Look up a config name; `Ok(None)` when no backend has it.
    ///
    /// # Errors
    ///
    /// When the embedded descriptor file is malformed (a build defect).
    pub fn parse(name: &str) -> Result<Option<Self>> {
        Ok(descriptors::all()?
            .iter()
            .find(|d| d.name == name)
            .map(Self))
    }

    /// The names of every backend, in default-chain order.
    ///
    /// # Errors
    ///
    /// When the embedded descriptor file is malformed (a build defect).
    pub fn names() -> Result<Vec<&'static str>> {
        Ok(descriptors::all()?
            .iter()
            .map(|d| d.name.as_str())
            .collect())
    }
}

/// How `web_search` runs.
#[derive(Clone)]
pub struct WebSearchConfig {
    /// Backends in the order they are tried.
    pub chain: Vec<SearchBackend>,
    /// Per-backend request timeout in seconds.
    pub timeout_secs: u64,
    /// Minimum spacing between `DuckDuckGo` requests.
    pub ddg_interval: Duration,
    /// Credentials found in the environment, by backend name.
    keys: BTreeMap<&'static str, String>,
    /// Endpoints a base-URL variable or [`Self::with_base_url`] replaced, by backend name.
    endpoints: BTreeMap<&'static str, String>,
    ddg_gate: Arc<ddg::PoliteGate>,
}

impl std::fmt::Debug for WebSearchConfig {
    /// Key values never reach logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSearchConfig")
            .field("chain", &self.chain)
            .field("timeout_secs", &self.timeout_secs)
            .field("ddg_interval", &self.ddg_interval)
            .finish_non_exhaustive()
    }
}

fn env_nonempty(names: &[String]) -> Option<String> {
    names
        .iter()
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
}

fn join_url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

impl WebSearchConfig {
    /// A config with the credentials and base URLs found in the environment.
    ///
    /// # Errors
    ///
    /// When the embedded descriptor file is malformed (a build defect).
    pub fn new(
        chain: Vec<SearchBackend>,
        timeout_secs: u64,
        ddg_interval: Duration,
    ) -> Result<Self> {
        let mut keys = BTreeMap::new();
        let mut endpoints = BTreeMap::new();
        for d in descriptors::all()? {
            if let Some(key) = env_nonempty(&d.key_env) {
                keys.insert(d.name.as_str(), key);
            }
            if let Some(base) = env_nonempty(&d.endpoint_env) {
                endpoints.insert(d.name.as_str(), join_url(&base, &d.endpoint_path));
            }
        }
        Ok(Self {
            chain,
            timeout_secs,
            ddg_interval,
            keys,
            endpoints,
            ddg_gate: Arc::new(ddg::PoliteGate::new(ddg_interval)),
        })
    }

    /// Send `backend` to the service at `base` (with its `endpoint_path` appended) instead of
    /// the one in `backends.toml` or the environment.
    #[must_use]
    pub fn with_base_url(mut self, backend: SearchBackend, base: &str) -> Self {
        self.endpoints
            .insert(backend.name(), join_url(base, &backend.0.endpoint_path));
        self
    }

    /// The credential of `backend`, if the environment had one.
    fn key(&self, backend: SearchBackend) -> Option<&str> {
        self.keys.get(backend.name()).map(String::as_str)
    }

    /// Where `backend` sends its request: a configured base URL, else the descriptor's endpoint
    /// (the keyless one when `key` is optional and unset).
    fn endpoint(&self, backend: SearchBackend) -> Option<&str> {
        let d = backend.0;
        if let Some(url) = self.endpoints.get(backend.name()) {
            return Some(url);
        }
        match (d.key, self.key(backend), &d.keyless_endpoint) {
            (KeyMode::Optional, None, Some(url)) => Some(url),
            _ => d.endpoint.as_deref(),
        }
    }

    /// Whether `backend` can run: it has its credential (when it needs one) and an endpoint.
    fn usable(&self, backend: SearchBackend) -> bool {
        (backend.0.key != KeyMode::Required || self.key(backend).is_some())
            && self.endpoint(backend).is_some()
    }
}

/// Relative time filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "year" => Some(Self::Year),
            _ => None,
        }
    }

    const fn word(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }

    const fn days(self) -> u64 {
        match self {
            Self::Day => 1,
            Self::Week => 7,
            Self::Month => 30,
            Self::Year => 365,
        }
    }
}

struct Query<'a> {
    text: &'a str,
    limit: usize,
    recency: Option<Recency>,
}

#[derive(Debug, PartialEq, Eq)]
struct Hit {
    title: String,
    url: String,
    snippet: String,
    /// Publication date or age as the backend reports it.
    date: Option<String>,
}

/// What one backend answered.
#[derive(Debug)]
struct Found {
    answer: Option<String>,
    hits: Vec<Hit>,
}

impl Found {
    const fn hits(hits: Vec<Hit>) -> Self {
        Self { answer: None, hits }
    }
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", text.get(..cut).unwrap_or_default()),
        None => text.to_owned(),
    }
}

/// Keep the date part of an ISO timestamp; leave relative ages ("2 days ago") alone.
fn short_date(date: &str) -> &str {
    let bytes = date.as_bytes();
    if bytes.len() > 10 && bytes.get(4) == Some(&b'-') && bytes.get(7) == Some(&b'-') {
        date.get(..10).unwrap_or(date)
    } else {
        date
    }
}

/// The compact list: an optional answer, then `[n] title (date)` / url / snippet per source.
fn format_found(found: &Found) -> String {
    let mut out = String::new();
    if let Some(answer) = &found.answer {
        let _ = write!(out, "{answer}\n\n## Sources ({})\n", found.hits.len());
    }
    for (i, hit) in found.hits.iter().enumerate() {
        let _ = write!(out, "[{}] {}", i + 1, hit.title);
        if let Some(date) = &hit.date {
            let _ = write!(out, " ({})", short_date(date));
        }
        let _ = write!(out, "\n    {}\n", hit.url);
        if !hit.snippet.is_empty() {
            let _ = writeln!(out, "    {}", truncate(&hit.snippet, SNIPPET_CHARS));
        }
    }
    out
}

async fn run_chain(http: &HttpClient, cfg: &WebSearchConfig, q: &Query<'_>) -> Result<String> {
    let usable: Vec<SearchBackend> = cfg
        .chain
        .iter()
        .copied()
        .filter(|b| cfg.usable(*b))
        .collect();
    if usable.is_empty() {
        let names: Vec<&str> = cfg.chain.iter().map(|b| b.name()).collect();
        let vars: Vec<&str> = cfg
            .chain
            .iter()
            .flat_map(|b| {
                let d = b.0;
                if d.key == KeyMode::Required {
                    d.key_env.iter()
                } else {
                    d.endpoint_env.iter()
                }
            })
            .map(String::as_str)
            .collect();
        return Err(HrnError::Tool(format!(
            "web_search has no usable backend in the chain [{}]: set one of {}, or add a \
             keyless backend to [web].chain; no results were fetched",
            names.join(", "),
            vars.join(", ")
        )));
    }
    let mut failures: Vec<String> = Vec::new();
    for backend in usable {
        match backends::search(http, cfg, backend, q).await {
            Ok(found) if !found.hits.is_empty() || found.answer.is_some() => {
                let mut out = String::new();
                if !failures.is_empty() {
                    let _ = writeln!(
                        out,
                        "Note: answered by {} after failures: {}.\n",
                        backend.name(),
                        failures.join("; ")
                    );
                }
                out.push_str(&format_found(&found));
                return Ok(out);
            }
            Ok(_) => failures.push(format!("{}: no results", backend.name())),
            Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
            Err(e) => failures.push(format!("{}: {e}", backend.name())),
        }
    }
    if failures.iter().all(|f| f.ends_with("no results")) {
        return Ok(format!("No results for `{}`.", q.text));
    }
    Err(HrnError::Tool(format!(
        "All web search providers failed: {}",
        failures.join("; ")
    )))
}

/// Search the web through `cfg`'s backend chain: the first backend that answers wins, a failing
/// one falls through to the next. `recency` is `day`, `week`, `month` or `year`; `limit` is
/// clamped to `1..=MAX_RESULTS`.
///
/// # Errors
///
/// When no backend in the chain is usable or all of them fail.
pub async fn search_web(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    text: &str,
    limit: usize,
    recency: Option<&str>,
) -> Result<String> {
    let q = Query {
        text,
        limit: limit.clamp(1, MAX_RESULTS),
        recency: recency.and_then(Recency::parse),
    };
    run_chain(http, cfg, &q).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(title: &str, date: Option<&str>, snippet: &str) -> Hit {
        Hit {
            title: title.to_owned(),
            url: format!("https://{title}.test"),
            snippet: snippet.to_owned(),
            date: date.map(str::to_owned),
        }
    }

    #[test]
    fn formats_the_omp_list_shape() {
        let found = Found {
            answer: Some("42".to_owned()),
            hits: vec![
                hit("a", Some("2025-03-04T10:00:00Z"), "first"),
                hit("b", Some("2 days ago"), ""),
            ],
        };
        assert_eq!(
            format_found(&found),
            "42\n\n## Sources (2)\n[1] a (2025-03-04)\n    https://a.test\n    first\n[2] b (2 days ago)\n    https://b.test\n"
        );
    }

    #[test]
    fn chain_names_round_trip_and_default_chain_ends_keyless() {
        let chain = SearchBackend::default_chain().expect("descriptors parse");
        for b in &chain {
            assert_eq!(SearchBackend::parse(b.name()).expect("parse"), Some(*b));
        }
        assert_eq!(SearchBackend::parse("bing").expect("parse"), None);
        assert_eq!(chain.last().map(|b| b.name()), Some("duckduckgo"));
        for name in ["firecrawl", "perplexity", "parallel"] {
            assert!(SearchBackend::names().expect("names").contains(&name));
        }
    }

    #[tokio::test]
    async fn an_all_keyed_chain_without_keys_fails_loudly() {
        let chain = ["brave", "tavily", "firecrawl"]
            .map(|n| SearchBackend::parse(n).expect("parse").expect("known"))
            .to_vec();
        let mut cfg = WebSearchConfig::new(chain, 20, Duration::ZERO).expect("config");
        cfg.keys.clear();
        let http = HttpClient::new().expect("http");
        let q = Query {
            text: "x",
            limit: 3,
            recency: None,
        };
        let err = run_chain(&http, &cfg, &q).await.expect_err("no backend");
        let msg = err.to_string();
        assert!(msg.contains("no usable backend"), "{msg}");
        assert!(msg.contains("FIRECRAWL_API_KEY"), "{msg}");
    }
}
