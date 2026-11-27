//! The public-page fetch engine behind `web_fetch` and `read https://...`: SSRF-guarded GET,
//! manual redirect handling (every hop is re-validated by `HttpClient::get_public`), a response
//! size cap and a small TTL cache.

use crate::html;
use hrn_edge_http::{HttpClient, HttpResponse};
use hrn_kernel::{HrnError, Result, Shared};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_mins(10);
const CACHE_CAPACITY: usize = 32;
const FETCH_TIMEOUT: Duration = Duration::from_secs(25);
/// Response bytes kept per fetch; the remainder is discarded and reported.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

type Cache = Mutex<HashMap<(String, bool), (Instant, String)>>;

/// Fetches public web pages as readable text.
pub struct PageFetcher {
    shared: Arc<Shared>,
    cache: Cache,
}

impl PageFetcher {
    /// The process-wide fetcher (one HTTP client, one cache) shared by every tool that reads
    /// the web. The HTTP client is built on the first fetch, so registering a tool that can read
    /// URLs costs nothing at startup.
    pub fn shared(shared: &Arc<Shared>) -> Result<Arc<Self>> {
        shared.get_or_init(|| {
            Ok(Self {
                shared: Arc::clone(shared),
                cache: Mutex::new(HashMap::new()),
            })
        })
    }

    fn http(&self) -> Result<HttpClient> {
        self.shared
            .get_or_init(HttpClient::new)
            .map(|c| c.as_ref().clone())
    }

    /// The text of `url`: HTML converted to readable text, or with `raw` the body as sent.
    ///
    /// # Errors
    ///
    /// A refused (private) host, a transport failure, an HTTP error status, too many
    /// redirects, or binary content.
    pub async fn fetch(&self, url: &str, raw: bool) -> Result<String> {
        let key = (url.to_owned(), raw);
        if let Some(text) = cache_get(&self.cache, &key) {
            return Ok(text);
        }
        let http = self.http()?;
        let (resp, final_url) = follow_redirects(&http, url).await?;
        if !resp.status.is_success() {
            return Err(HrnError::Tool(format!(
                "fetch: HTTP {} for `{final_url}`",
                resp.status.as_u16()
            )));
        }
        let text = render(&resp, &final_url, raw)?;
        cache_put(&self.cache, key, text.clone());
        Ok(text)
    }
}

async fn follow_redirects(http: &HttpClient, url: &str) -> Result<(HttpResponse, String)> {
    let headers = [
        (
            "accept".to_owned(),
            "text/html, text/plain, application/json;q=0.9, */*;q=0.1".to_owned(),
        ),
        ("user-agent".to_owned(), "hrn/0.1".to_owned()),
    ];
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let resp = http
            .get_public(&current, &headers, FETCH_TIMEOUT, MAX_BODY_BYTES)
            .await?;
        if !resp.status.is_redirection() {
            return Ok((resp, current));
        }
        let location = resp
            .headers
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                HrnError::Tool(format!(
                    "fetch: HTTP {} from `{current}` without a Location header",
                    resp.status.as_u16()
                ))
            })?;
        current = resolve_location(&current, location);
    }
    Err(HrnError::Tool(format!(
        "fetch: more than {MAX_REDIRECTS} redirects starting at `{url}`"
    )))
}

/// Resolve a `Location` header against the URL that returned it.
fn resolve_location(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_owned();
    }
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    let after_scheme = base.get(scheme_end..).unwrap_or_default();
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let origin = base.get(..scheme_end + authority_end).unwrap_or(base);
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{}{rest}", base.get(..scheme_end).unwrap_or_default());
    }
    if location.starts_with('/') {
        return format!("{origin}{location}");
    }
    let path = after_scheme
        .get(authority_end..)
        .unwrap_or_default()
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    if location.starts_with('?') {
        return format!("{origin}{path}{location}");
    }
    let dir = path.rfind('/').and_then(|i| path.get(..=i)).unwrap_or("/");
    format!("{origin}{dir}{location}")
}

fn render(resp: &HttpResponse, url: &str, raw: bool) -> Result<String> {
    let content_type = resp
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let binary_type = ["image/", "audio/", "video/", "font/"]
        .iter()
        .any(|p| content_type.starts_with(p))
        || [
            "application/octet-stream",
            "application/pdf",
            "application/zip",
            "application/gzip",
        ]
        .iter()
        .any(|t| content_type.starts_with(t));
    let probe = resp
        .body
        .get(..resp.body.len().min(8192))
        .unwrap_or_default();
    if binary_type || probe.contains(&0) {
        return Err(HrnError::Tool(format!(
            "fetch: `{url}` is binary content ({}, {} bytes); only text is supported",
            if content_type.is_empty() {
                "unknown type"
            } else {
                &content_type
            },
            resp.body.len()
        )));
    }

    let body = resp.text();
    let head: String = body
        .chars()
        .take(2048)
        .collect::<String>()
        .to_ascii_lowercase();
    let is_html = content_type.contains("html")
        || (content_type.is_empty() && (head.contains("<html") || head.contains("<!doctype html")));
    let mut text = if is_html && !raw {
        html::to_text(&body)
    } else {
        body
    };
    if resp.truncated {
        let _ = write!(
            text,
            "\n\n[response body cut at {} MiB]",
            MAX_BODY_BYTES / (1024 * 1024)
        );
    }
    Ok(text)
}

fn cache_get(cache: &Cache, key: &(String, bool)) -> Option<String> {
    let guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .get(key)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, text)| text.clone())
}

fn cache_put(cache: &Cache, key: (String, bool), text: String) {
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
    if guard.len() >= CACHE_CAPACITY
        && let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(key, _)| key.clone())
    {
        guard.remove(&oldest);
    }
    guard.insert(key, (Instant::now(), text));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_every_location_form() {
        let base = "https://example.com/a/b/page.html?q=1#frag";
        let cases = [
            ("https://other.org/x", "https://other.org/x"),
            ("//cdn.example.net/y", "https://cdn.example.net/y"),
            ("/root", "https://example.com/root"),
            ("next.html", "https://example.com/a/b/next.html"),
            ("?q=2", "https://example.com/a/b/page.html?q=2"),
        ];
        for (location, want) in cases {
            assert_eq!(resolve_location(base, location), want, "{location}");
        }
        assert_eq!(
            resolve_location("https://example.com", "rel"),
            "https://example.com/rel"
        );
    }
}
