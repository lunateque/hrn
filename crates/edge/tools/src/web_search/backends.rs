//! One request builder and response parser per search backend.

use super::{Found, Hit, Query, Recency, SearchBackend, WebSearchConfig, apis, ddg};
use crate::html;
use bytes::Bytes;
use hrn_edge_http::{HttpClient, HttpResponse};
use hrn_kernel::{HrnError, Result};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_GET_BYTES: usize = 1024 * 1024;
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub(super) type Headers = Vec<(String, String)>;

pub(super) fn header(name: &str, value: &str) -> (String, String) {
    (name.to_owned(), value.to_owned())
}

pub(super) fn enc(text: &str) -> String {
    utf8_percent_encode(text, NON_ALPHANUMERIC).to_string()
}

pub(super) fn text_of(item: &Value, key: &str) -> Option<String> {
    item.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// `YYYY-MM-DD` of the day `days` ago (UTC).
pub(super) fn iso_date_days_ago(days: u64) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let today = i64::try_from(secs / 86_400).unwrap_or(0);
    civil_date(today.saturating_sub(i64::try_from(days).unwrap_or(0)))
}

/// Proleptic Gregorian date of `days_since_epoch` (Howard Hinnant's `civil_from_days`).
fn civil_date(days_since_epoch: i64) -> String {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Check the status, mapping 401/403/429/5xx to their typed errors.
fn require_ok(resp: HttpResponse) -> Result<HttpResponse> {
    if resp.status.is_success() {
        Ok(resp)
    } else {
        Err(resp.status_error())
    }
}

async fn get(
    http: &HttpClient,
    url: &str,
    headers: &Headers,
    timeout: Duration,
) -> Result<HttpResponse> {
    require_ok(http.get(url, headers, timeout, MAX_GET_BYTES).await?)
}

async fn post(
    http: &HttpClient,
    url: &str,
    mut headers: Headers,
    content_type: &str,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<HttpResponse> {
    headers.push(header("content-type", content_type));
    require_ok(
        http.post_raw(url, &headers, Bytes::from(body), timeout)
            .await?,
    )
}

pub(super) async fn post_json(
    http: &HttpClient,
    url: &str,
    headers: Headers,
    body: &Value,
    timeout: Duration,
) -> Result<HttpResponse> {
    post(
        http,
        url,
        headers,
        "application/json",
        serde_json::to_vec(body)?,
        timeout,
    )
    .await
}

/// Add `key` to a JSON object body.
pub(super) fn set(body: &mut Value, key: &str, value: Value) {
    if let Some(map) = body.as_object_mut() {
        map.insert(key.to_owned(), value);
    }
}

/// The credential of `backend`, or the error naming the variables that would supply it.
pub(super) fn key_of(cfg: &WebSearchConfig, backend: SearchBackend) -> Result<&str> {
    cfg.key(backend)
        .ok_or_else(|| HrnError::Config(format!("{} is not set", backend.0.key_env.join(" or "))))
}

/// The endpoint of `backend`, or the error naming the variables that would supply it.
pub(super) fn endpoint_of(cfg: &WebSearchConfig, backend: SearchBackend) -> Result<&str> {
    cfg.endpoint(backend).ok_or_else(|| {
        HrnError::Config(format!(
            "{} is not set",
            backend.0.endpoint_env.join(" or ")
        ))
    })
}

/// Run `q` against `backend`.
pub(super) async fn search(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
) -> Result<Found> {
    let timeout = Duration::from_secs(cfg.timeout_secs.max(5));
    match backend.name() {
        "brave" => brave(http, cfg, backend, q, timeout).await,
        "searxng" => searxng(http, cfg, backend, q, timeout).await,
        "duckduckgo" => duckduckgo(http, cfg, backend, q, timeout).await,
        "exa" => exa(http, cfg, backend, q, timeout).await,
        "tavily" => tavily(http, cfg, backend, q, timeout).await,
        "jina" => jina(http, cfg, backend, q, timeout).await,
        "perplexity" => apis::perplexity(http, cfg, backend, q, timeout).await,
        "parallel" => apis::parallel(http, cfg, backend, q, timeout).await,
        "firecrawl" => apis::firecrawl(http, cfg, backend, q, timeout).await,
        other => Err(HrnError::Internal(format!(
            "web_search backend `{other}` is in backends.toml but has no implementation"
        ))),
    }
}

// ---- Brave ----------------------------------------------------------------------------------

async fn brave(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let key = key_of(cfg, backend)?;
    let mut url = format!(
        "{}?q={}&count={}",
        endpoint_of(cfg, backend)?,
        enc(q.text),
        q.limit
    );
    if let Some(r) = q.recency {
        url.push_str(match r {
            Recency::Day => "&freshness=pd",
            Recency::Week => "&freshness=pw",
            Recency::Month => "&freshness=pm",
            Recency::Year => "&freshness=py",
        });
    }
    let headers = vec![
        header("accept", "application/json"),
        header("x-subscription-token", key),
    ];
    let resp = get(http, &url, &headers, timeout).await?;
    parse_brave(&resp.body, q.limit)
}

fn parse_brave(body: &[u8], limit: usize) -> Result<Found> {
    let root: Value = serde_json::from_slice(body)?;
    let hits = root
        .pointer("/web/results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Hit {
                title: html::fragment_text(&text_of(r, "title")?),
                url: text_of(r, "url")?,
                snippet: html::fragment_text(&text_of(r, "description").unwrap_or_default()),
                date: text_of(r, "age").or_else(|| text_of(r, "page_age")),
            })
        })
        .take(limit)
        .collect();
    Ok(Found::hits(hits))
}

// ---- SearXNG --------------------------------------------------------------------------------

async fn searxng(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let mut url = format!(
        "{}?q={}&format=json",
        endpoint_of(cfg, backend)?,
        enc(q.text)
    );
    if let Some(r) = q.recency {
        // SearXNG has no week bucket; the nearest wider one keeps results fresh enough.
        url.push_str(match r {
            Recency::Day => "&time_range=day",
            Recency::Week | Recency::Month => "&time_range=month",
            Recency::Year => "&time_range=year",
        });
    }
    let mut headers = vec![header("accept", "application/json")];
    if let Some(token) = cfg.key(backend) {
        headers.push(header("authorization", &format!("Bearer {token}")));
    }
    let resp = get(http, &url, &headers, timeout).await?;
    parse_searxng(&resp.body, q.limit)
}

fn parse_searxng(body: &[u8], limit: usize) -> Result<Found> {
    let root: Value = serde_json::from_slice(body)?;
    let hits: Vec<Hit> = root
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Hit {
                title: text_of(r, "title")?,
                url: text_of(r, "url")?,
                snippet: text_of(r, "content").unwrap_or_default(),
                date: text_of(r, "publishedDate"),
            })
        })
        .take(limit)
        .collect();
    let broken = root
        .get("unresponsive_engines")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if hits.is_empty() && broken {
        return Err(HrnError::Tool(
            "SearXNG returned no results and reported unresponsive engines".into(),
        ));
    }
    let answer = root
        .get("answers")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|a| {
            a.as_str()
                .map(str::to_owned)
                .or_else(|| text_of(a, "answer"))
        });
    Ok(Found { answer, hits })
}

// ---- DuckDuckGo -----------------------------------------------------------------------------

async fn duckduckgo(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let mut form = format!("q={}&kl=us-en", enc(q.text));
    if let Some(r) = q.recency {
        form.push_str(match r {
            Recency::Day => "&df=d",
            Recency::Week => "&df=w",
            Recency::Month => "&df=m",
            Recency::Year => "&df=y",
        });
    }
    cfg.ddg_gate.wait().await;
    let headers = vec![
        header("accept", "text/html"),
        header("user-agent", BROWSER_UA),
        header("referer", "https://html.duckduckgo.com/"),
    ];
    let resp = post(
        http,
        endpoint_of(cfg, backend)?,
        headers,
        "application/x-www-form-urlencoded",
        form.into_bytes(),
        timeout,
    )
    .await?;
    let page = resp.text();
    if ddg::is_challenge(resp.status.as_u16(), &page) {
        return Err(ddg::challenge_error());
    }
    Ok(Found::hits(ddg::parse(&page, q.limit)?))
}

// ---- Tavily ---------------------------------------------------------------------------------

async fn tavily(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let key = key_of(cfg, backend)?;
    let mut body = json!({ "query": q.text, "max_results": q.limit, "include_answer": true });
    if let Some(r) = q.recency {
        set(&mut body, "time_range", json!(r.word()));
    }
    let headers = vec![header("authorization", &format!("Bearer {key}"))];
    let resp = post_json(http, endpoint_of(cfg, backend)?, headers, &body, timeout).await?;
    parse_tavily(&resp.body, q.limit)
}

fn parse_tavily(body: &[u8], limit: usize) -> Result<Found> {
    let root: Value = serde_json::from_slice(body)?;
    let hits = root
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Hit {
                title: text_of(r, "title")?,
                url: text_of(r, "url")?,
                snippet: text_of(r, "content").unwrap_or_default(),
                date: text_of(r, "published_date"),
            })
        })
        .take(limit)
        .collect();
    Ok(Found {
        answer: text_of(&root, "answer"),
        hits,
    })
}

// ---- Jina -----------------------------------------------------------------------------------

async fn jina(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let key = key_of(cfg, backend)?;
    let url = format!(
        "{}{}?count={}",
        endpoint_of(cfg, backend)?,
        enc(q.text),
        q.limit
    );
    let headers = vec![
        header("authorization", &format!("Bearer {key}")),
        header("accept", "application/json"),
        header("x-respond-with", "no-content"),
    ];
    let resp = get(http, &url, &headers, timeout).await?;
    let root: Value = serde_json::from_slice(&resp.body)?;
    let hits = root
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Hit {
                title: text_of(r, "title")?,
                url: text_of(r, "url")?,
                snippet: text_of(r, "description").unwrap_or_default(),
                date: None,
            })
        })
        .take(q.limit)
        .collect();
    Ok(Found::hits(hits))
}

// ---- Exa ------------------------------------------------------------------------------------

async fn exa(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let url = endpoint_of(cfg, backend)?;
    match cfg.key(backend) {
        Some(key) => exa_api(http, url, key, q, timeout).await,
        None => exa_mcp(http, url, q, timeout).await,
    }
}

async fn exa_api(
    http: &HttpClient,
    url: &str,
    key: &str,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let mut body = json!({
        "query": q.text,
        "numResults": q.limit,
        "type": "auto",
        "contents": { "text": { "maxCharacters": 500 } }
    });
    if let Some(r) = q.recency {
        set(
            &mut body,
            "startPublishedDate",
            json!(iso_date_days_ago(r.days())),
        );
    }
    let headers = vec![header("x-api-key", key)];
    let resp = post_json(http, url, headers, &body, timeout).await?;
    let root: Value = serde_json::from_slice(&resp.body)?;
    let hits = root
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let snippet = text_of(r, "text")
                .or_else(|| text_of(r, "summary"))
                .or_else(|| {
                    r.get("highlights")
                        .and_then(Value::as_array)
                        .and_then(|h| h.first())
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            Some(Hit {
                title: text_of(r, "title").unwrap_or_else(|| text_of(r, "url").unwrap_or_default()),
                url: text_of(r, "url")?,
                snippet,
                date: text_of(r, "publishedDate"),
            })
        })
        .take(q.limit)
        .collect();
    Ok(Found::hits(hits))
}

/// The public keyless Exa MCP endpoint: JSON-RPC `tools/call`, answered as one SSE event.
async fn exa_mcp(http: &HttpClient, url: &str, q: &Query<'_>, timeout: Duration) -> Result<Found> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "web_search_exa", "arguments": { "query": q.text, "numResults": q.limit } }
    });
    let headers = vec![header("accept", "application/json, text/event-stream")];
    let resp = post_json(http, url, headers, &body, timeout).await?;
    parse_exa_mcp(&resp.text(), q.limit)
}

fn parse_exa_mcp(reply: &str, limit: usize) -> Result<Found> {
    // Either a bare JSON body or SSE frames (`data: {...}`).
    let payload = reply
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .unwrap_or(reply)
        .trim();
    let root: Value = serde_json::from_str(payload)?;
    if let Some(msg) = root.pointer("/error/message").and_then(Value::as_str) {
        return Err(HrnError::Tool(format!("Exa MCP error: {msg}")));
    }
    let text = root
        .pointer("/result/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Found::hits(parse_exa_blocks(&text, limit)))
}

/// Blocks of `Title:` / `URL:` / `Published:` / `Author:` / `Highlights:` lines.
fn parse_exa_blocks(text: &str, limit: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    for block in text
        .split("\nTitle: ")
        .map(|b| b.strip_prefix("Title: ").unwrap_or(b))
    {
        let mut lines = block.lines();
        let Some(title) = lines.next().map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        let (mut url, mut date, mut snippet) = (String::new(), None, String::new());
        let mut in_highlights = false;
        for line in lines {
            if in_highlights {
                snippet.push_str(line);
                snippet.push(' ');
            } else if let Some(u) = line.strip_prefix("URL:") {
                u.trim().clone_into(&mut url);
            } else if let Some(d) = line.strip_prefix("Published:") {
                date = Some(d.trim().to_owned()).filter(|d| d != "N/A" && !d.is_empty());
            } else if line.starts_with("Highlights:") {
                in_highlights = true;
            }
        }
        if url.is_empty() {
            continue;
        }
        hits.push(Hit {
            title: title.to_owned(),
            url,
            snippet: snippet.split_whitespace().collect::<Vec<_>>().join(" "),
            date,
        });
        if hits.len() == limit {
            break;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exa_mcp_blocks_from_an_sse_frame() {
        let text = "Title: Ratatui\\nURL: https://ratatui.rs/\\nPublished: N/A\\nAuthor: N/A\\nHighlights:\\nCook up\\n\\nTUIs\\n\\nTitle: Docs\\nURL: https://docs.rs/ratatui\\nPublished: 2025-01-02\\nHighlights:\\nAPI";
        let frame = format!(
            "event: message\ndata: {{\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n"
        );
        let found = parse_exa_mcp(&frame, 5).expect("parse");
        assert_eq!(found.hits.len(), 2);
        assert_eq!(found.hits[0].url, "https://ratatui.rs/");
        assert_eq!(found.hits[0].snippet, "Cook up TUIs");
        assert_eq!(found.hits[0].date, None);
        assert_eq!(found.hits[1].date.as_deref(), Some("2025-01-02"));
    }

    #[test]
    fn parses_brave_and_tavily_json() {
        let brave = br#"{"web":{"results":[{"title":"A <strong>b</strong>","url":"https://a.test","description":"d","age":"2 days ago"}]}}"#;
        let found = parse_brave(brave, 5).expect("brave");
        assert_eq!(found.hits[0].title, "A b");
        assert_eq!(found.hits[0].date.as_deref(), Some("2 days ago"));
        let tavily = br#"{"answer":"yes","results":[{"title":"T","url":"https://t.test","content":"c","published_date":"2025-03-04"}]}"#;
        let found = parse_tavily(tavily, 5).expect("tavily");
        assert_eq!(found.answer.as_deref(), Some("yes"));
        assert_eq!(found.hits[0].snippet, "c");
    }

    #[test]
    fn searxng_with_dead_engines_and_no_hits_is_an_error() {
        let body = br#"{"results":[],"unresponsive_engines":[["google","timeout"]]}"#;
        assert!(parse_searxng(body, 5).is_err());
    }

    #[test]
    fn civil_date_matches_known_days() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(11_017), "2000-03-01");
        assert_eq!(civil_date(11_016), "2000-02-29");
        assert_eq!(civil_date(20_000), "2024-10-04");
    }
}
