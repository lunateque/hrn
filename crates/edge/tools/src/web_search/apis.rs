//! The JSON search APIs added after the first chain: Perplexity, Parallel and Firecrawl. Their
//! endpoints and credentials come from `backends.toml`.

use super::backends::{endpoint_of, header, iso_date_days_ago, key_of, post_json, set, text_of};
use super::{Found, Hit, Query, Recency, SearchBackend, WebSearchConfig};
use hrn_edge_http::HttpClient;
use hrn_kernel::{HrnError, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// Parallel's Search API is a beta: it answers only with this header.
const PARALLEL_BETA: &str = "search-extract-2025-10-10";
/// Longest excerpt Parallel returns per result; the list shows far less.
const PARALLEL_EXCERPT_CHARS: u32 = 1000;

fn results<'a>(root: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    root.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

// ---- Perplexity -----------------------------------------------------------------------------

pub(super) async fn perplexity(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let key = key_of(cfg, backend)?;
    let mut body = json!({ "query": q.text, "max_results": q.limit });
    if let Some(r) = q.recency {
        set(&mut body, "search_recency_filter", json!(r.word()));
    }
    let headers = vec![header("authorization", &format!("Bearer {key}"))];
    let resp = post_json(http, endpoint_of(cfg, backend)?, headers, &body, timeout).await?;
    parse_perplexity(&resp.body, q.limit)
}

fn parse_perplexity(body: &[u8], limit: usize) -> Result<Found> {
    let root: Value = serde_json::from_slice(body)?;
    let hits = results(&root, "results")
        .filter_map(|r| {
            let url = text_of(r, "url")?;
            Some(Hit {
                title: text_of(r, "title").unwrap_or_else(|| url.clone()),
                url,
                snippet: text_of(r, "snippet").unwrap_or_default(),
                date: text_of(r, "date").or_else(|| text_of(r, "last_updated")),
            })
        })
        .take(limit)
        .collect();
    Ok(Found::hits(hits))
}

// ---- Parallel -------------------------------------------------------------------------------

pub(super) async fn parallel(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let key = key_of(cfg, backend)?;
    let mut body = json!({
        "objective": q.text,
        "search_queries": [q.text],
        "mode": "fast",
        "excerpts": { "max_chars_per_result": PARALLEL_EXCERPT_CHARS },
    });
    if let Some(r) = q.recency {
        set(
            &mut body,
            "source_policy",
            json!({ "after_date": iso_date_days_ago(r.days()) }),
        );
    }
    let headers = vec![
        header("x-api-key", key),
        header("parallel-beta", PARALLEL_BETA),
    ];
    let resp = post_json(http, endpoint_of(cfg, backend)?, headers, &body, timeout).await?;
    parse_parallel(&resp.body, q.limit)
}

fn parse_parallel(body: &[u8], limit: usize) -> Result<Found> {
    let root: Value = serde_json::from_slice(body)?;
    let hits = results(&root, "results")
        .filter_map(|r| {
            let url = text_of(r, "url")?;
            let excerpts = results(r, "excerpts")
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            Some(Hit {
                title: text_of(r, "title").unwrap_or_else(|| url.clone()),
                url,
                snippet: excerpts.split_whitespace().collect::<Vec<_>>().join(" "),
                date: text_of(r, "publish_date"),
            })
        })
        .take(limit)
        .collect();
    Ok(Found::hits(hits))
}

// ---- Firecrawl ------------------------------------------------------------------------------

pub(super) async fn firecrawl(
    http: &HttpClient,
    cfg: &WebSearchConfig,
    backend: SearchBackend,
    q: &Query<'_>,
    timeout: Duration,
) -> Result<Found> {
    let key = key_of(cfg, backend)?;
    let mut body = json!({ "query": q.text, "limit": q.limit, "sources": [{ "type": "web" }] });
    if let Some(r) = q.recency {
        set(
            &mut body,
            "tbs",
            json!(match r {
                Recency::Day => "qdr:d",
                Recency::Week => "qdr:w",
                Recency::Month => "qdr:m",
                Recency::Year => "qdr:y",
            }),
        );
    }
    let headers = vec![header("authorization", &format!("Bearer {key}"))];
    let resp = post_json(http, endpoint_of(cfg, backend)?, headers, &body, timeout).await?;
    parse_firecrawl(&resp.body, q.limit)
}

fn parse_firecrawl(body: &[u8], limit: usize) -> Result<Found> {
    let root: Value = serde_json::from_slice(body)?;
    if root.get("success").and_then(Value::as_bool) == Some(false) {
        let why = text_of(&root, "error").unwrap_or_else(|| "request failed".to_owned());
        return Err(HrnError::Tool(format!("Firecrawl: {why}")));
    }
    // `data` is the result list itself or an object of lists by source; some deployments answer
    // with a top-level `results`.
    let list = match root.get("data") {
        Some(Value::Array(list)) => Some(list),
        Some(data) => data.get("web").and_then(Value::as_array),
        None => root.get("results").and_then(Value::as_array),
    };
    let hits = list
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let url = text_of(r, "url")?;
            Some(Hit {
                title: text_of(r, "title").unwrap_or_else(|| url.clone()),
                url,
                snippet: text_of(r, "description")
                    .or_else(|| text_of(r, "snippet"))
                    .unwrap_or_default(),
                date: None,
            })
        })
        .take(limit)
        .collect();
    Ok(Found::hits(hits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perplexity_results_carry_snippet_and_date() {
        let body = br#"{"results":[{"title":"T","url":"https://t.test","snippet":"s","date":"2025-02-03"},{"url":"https://u.test","last_updated":"2025-01-01"}]}"#;
        let found = parse_perplexity(body, 5).expect("parse");
        assert_eq!(found.hits.len(), 2);
        assert_eq!(found.hits[0].date.as_deref(), Some("2025-02-03"));
        assert_eq!(found.hits[1].title, "https://u.test");
        assert_eq!(found.hits[1].date.as_deref(), Some("2025-01-01"));
    }

    #[test]
    fn parallel_excerpts_join_into_one_snippet() {
        let body = br#"{"search_id":"x","results":[{"url":"https://p.test","title":"P","publish_date":"2025-05-06","excerpts":["one\n two","three"]}]}"#;
        let found = parse_parallel(body, 5).expect("parse");
        assert_eq!(found.hits[0].snippet, "one two three");
        assert_eq!(found.hits[0].date.as_deref(), Some("2025-05-06"));
    }

    #[test]
    fn firecrawl_reads_both_data_shapes_and_reports_failure() {
        let nested = br#"{"success":true,"data":{"web":[{"title":"A","url":"https://a.test","description":"d"}]}}"#;
        assert_eq!(parse_firecrawl(nested, 5).expect("nested").hits.len(), 1);
        let flat = br#"{"data":[{"url":"https://b.test","snippet":"s"}]}"#;
        assert_eq!(parse_firecrawl(flat, 5).expect("flat").hits[0].snippet, "s");
        let failed = br#"{"success":false,"error":"quota"}"#;
        assert!(
            parse_firecrawl(failed, 5)
                .expect_err("failure")
                .to_string()
                .contains("quota")
        );
    }
}
