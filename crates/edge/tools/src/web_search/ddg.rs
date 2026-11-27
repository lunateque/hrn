//! `DuckDuckGo` HTML frontend: result parsing, bot-challenge detection and the politeness gate.

use super::Hit;
use crate::html;
use hrn_kernel::{HrnError, Result};
use percent_encoding::percent_decode_str;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Spaces `DuckDuckGo` requests apart. The endpoint is unofficial and throttles bursts, so every
/// request waits until `interval` has passed since the previous one.
pub(super) struct PoliteGate {
    interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl PoliteGate {
    pub(super) fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    pub(super) async fn wait(&self) {
        let mut last = self.last.lock().await;
        if let Some(at) = *last
            && let Some(left) = self.interval.checked_sub(at.elapsed())
        {
            tokio::time::sleep(left).await;
        }
        *last = Some(Instant::now());
    }
}

/// `true` when `DuckDuckGo` answered with its bot challenge instead of results.
pub(super) fn is_challenge(status: u16, page: &str) -> bool {
    status == 202 || page.contains("anomaly-modal")
}

/// The `Err` a challenge page maps to, so the chain moves on with a clear cause.
pub(super) fn challenge_error() -> HrnError {
    HrnError::Tool("DuckDuckGo served a bot challenge (rate limited)".into())
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let head = tag.get(..tag.find('>')?)?;
    let after = head.get(head.find(name)? + name.len()..)?;
    let quote = after.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let inner = after.get(1..)?;
    Some(html::unescape(inner.get(..inner.find(quote)?)?))
}

fn decode_redirect(href: &str) -> String {
    if let Some((_, target)) = href.split_once("uddg=") {
        let raw = target.split('&').next().unwrap_or(target);
        percent_decode_str(raw).decode_utf8_lossy().into_owned()
    } else if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_owned()
    }
}

/// Parse a result page; an empty page with no result markup is an error, not "no results".
pub(super) fn parse(page: &str, limit: usize) -> Result<Vec<Hit>> {
    const MARK: &str = "class=\"result__a\"";
    let mut hits = Vec::new();
    let mut rest = page;
    while hits.len() < limit {
        let Some(pos) = rest.find(MARK) else { break };
        let chunk = rest.get(pos..).unwrap_or_default();
        rest = chunk.get(MARK.len()..).unwrap_or_default();
        let url = attribute(chunk, "href=")
            .map(|h| decode_redirect(&h))
            .unwrap_or_default();
        let title = chunk
            .split_once('>')
            .and_then(|(_, after)| after.split_once("</a>"))
            .map(|(inner, _)| html::fragment_text(inner))
            .unwrap_or_default();
        // The snippet belongs to this result only if it appears before the next result.
        let this_result = rest.find(MARK).and_then(|n| rest.get(..n)).unwrap_or(rest);
        let snippet = this_result
            .find("result__snippet")
            .and_then(|sp| this_result.get(sp..))
            .and_then(|s| s.split_once('>'))
            .and_then(|(_, after)| after.split("</a").next())
            .map(html::fragment_text)
            .unwrap_or_default();
        if !title.is_empty() && !url.is_empty() {
            hits.push(Hit {
                title,
                url,
                snippet,
                date: None,
            });
        }
    }
    if hits.is_empty() && !page.contains("no-results") && !page.contains("No results") {
        return Err(HrnError::Tool(
            "DuckDuckGo page had no recognizable results (layout changed or blocked)".into(),
        ));
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_results_and_keeps_snippets_with_their_result() {
        let page = r#"
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Fratatui&rut=x">Ratatui <b>Docs</b></a>
            <a class="result__snippet">Rust TUI &amp; widgets</a>
            <a class="result__a" href="https://example.org/two">Two</a>
        "#;
        let hits = parse(page, 5).expect("hits");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://docs.rs/ratatui");
        assert_eq!(hits[0].title, "Ratatui Docs");
        assert_eq!(hits[0].snippet, "Rust TUI & widgets");
        assert_eq!(
            hits[1].snippet, "",
            "snippet must not leak from a neighbour"
        );
    }

    #[test]
    fn challenge_and_layout_drift_are_errors_but_no_results_is_not() {
        assert!(is_challenge(200, "<div class=\"anomaly-modal\">"));
        assert!(is_challenge(202, ""));
        assert!(parse("<html>weird</html>", 5).is_err());
        assert!(
            parse("<div class=\"no-results\">No results.</div>", 5)
                .expect("empty")
                .is_empty()
        );
    }
}
