//! The backend descriptors of the embedded `backends.toml`: names, endpoints and the environment
//! variables of the credentials.

use hrn_kernel::{HrnError, Result};
use std::sync::LazyLock;

const DATA: &str = include_str!("backends.toml");

/// Whether a backend needs a credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KeyMode {
    /// Skipped without one.
    Required,
    /// Used when set.
    Optional,
    /// Takes none.
    None,
}

/// One row of `backends.toml`.
#[derive(Debug)]
pub(super) struct Descriptor {
    pub(super) name: String,
    pub(super) key: KeyMode,
    pub(super) key_env: Vec<String>,
    pub(super) endpoint: Option<String>,
    pub(super) endpoint_env: Vec<String>,
    pub(super) endpoint_path: String,
    pub(super) keyless_endpoint: Option<String>,
}

static PARSED: LazyLock<std::result::Result<Vec<Descriptor>, String>> =
    LazyLock::new(|| parse(DATA));

/// Every backend, in default-chain order.
pub(super) fn all() -> Result<&'static [Descriptor]> {
    PARSED
        .as_ref()
        .map(Vec::as_slice)
        .map_err(|e| HrnError::Internal(format!("web_search backends.toml: {e}")))
}

fn parse(text: &str) -> std::result::Result<Vec<Descriptor>, String> {
    let doc: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let rows = doc
        .get("backend")
        .and_then(toml::Value::as_array)
        .ok_or("no [[backend]] rows")?;
    rows.iter().map(row).collect()
}

fn string(row: &toml::Value, key: &str) -> Option<String> {
    row.get(key)
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}

fn strings(row: &toml::Value, key: &str) -> Vec<String> {
    row.get(key)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn row(row: &toml::Value) -> std::result::Result<Descriptor, String> {
    let name = string(row, "name").ok_or("a [[backend]] row has no name")?;
    let key = match string(row, "key").as_deref() {
        Some("required") => KeyMode::Required,
        Some("optional") => KeyMode::Optional,
        Some("none") => KeyMode::None,
        other => return Err(format!("backend `{name}`: bad `key` {other:?}")),
    };
    Ok(Descriptor {
        key,
        key_env: strings(row, "key_env"),
        endpoint: string(row, "endpoint"),
        endpoint_env: strings(row, "endpoint_env"),
        endpoint_path: string(row, "endpoint_path").unwrap_or_default(),
        keyless_endpoint: string(row, "keyless_endpoint"),
        name,
    })
}
