//! `?key=value&...` query strings of `read` paths (`?q=`, `?limit=`, `?offset=`, `?order=`,
//! `?raw=`, `?compact=`).

use hrn_kernel::{HrnError, Result};
use percent_encoding::percent_decode_str;

/// Parsed query parameters.
#[derive(Default)]
pub(super) struct Params {
    pub(super) q: Option<String>,
    pub(super) limit: Option<usize>,
    pub(super) offset: usize,
    pub(super) order: Option<String>,
    pub(super) raw: bool,
    pub(super) compact: bool,
}

const KEYS: &[&str] = &["q", "query", "limit", "offset", "order", "raw", "compact"];

fn bad(key: &str, value: &str) -> HrnError {
    HrnError::Tool(format!(
        "query parameter `{key}` has invalid value `{value}`"
    ))
}

impl Params {
    /// Parse `query` (text after `?`). A `&` not followed by a known `key=` belongs to the
    /// previous value, so SQL / jq with `&&` survives.
    pub(super) fn parse(query: &str) -> Result<Self> {
        let mut pairs: Vec<(&str, String)> = Vec::new();
        for (i, segment) in query.split('&').enumerate() {
            let key_hit = segment
                .split_once('=')
                .filter(|(k, _)| KEYS.contains(k) || (i == 0 && !k.contains(' ')));
            match (key_hit, pairs.last_mut()) {
                (Some((k, v)), _) => pairs.push((k, v.to_owned())),
                (None, Some((_, v))) => {
                    v.push('&');
                    v.push_str(segment);
                }
                (None, None) => {
                    return Err(HrnError::Tool(format!("malformed query `?{query}`")));
                }
            }
        }
        let mut out = Self::default();
        for (key, value) in pairs {
            let value = percent_decode_str(&value).decode_utf8_lossy().into_owned();
            match key {
                "q" | "query" => out.q = Some(value),
                "limit" => out.limit = Some(value.parse().map_err(|_| bad(key, &value))?),
                "offset" => out.offset = value.parse().map_err(|_| bad(key, &value))?,
                "order" => out.order = Some(value),
                "raw" => out.raw = value == "true" || value == "1",
                "compact" => out.compact = value == "true" || value == "1",
                other => {
                    return Err(HrnError::Tool(format!(
                        "unknown query parameter `{other}` (known: q, limit, offset, order, \
                         raw, compact)"
                    )));
                }
            }
        }
        Ok(out)
    }
}
