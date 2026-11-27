//! `cfg://` internal URL scheme: inspect effective settings (`cfg://`, `cfg://<section>`,
//! `cfg://<key>`) and apply session-only (`write cfg://<key>`) or persisted global
//! (`write cfg://<key>/save`) updates behind [`ApprovalGate`].

use crate::approvals::GrantCache;
use hrn_kernel::{
    ApprovalDecision, ApprovalGate, ApprovalGrantScope, ApprovalRequest, CancelFlag, HrnError,
    Result, ToolContext, ToolTier, UrlBody, UrlScheme,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

/// Snapshot view of one setting exposed through `cfg://`.
#[derive(Clone, Debug)]
pub struct SettingView {
    /// Exact dotted setting key (e.g. `tools.premium_defer`).
    pub key: String,
    /// Effective resolved value (`None` when unset).
    pub value: Option<Value>,
    /// Schema default value (`None` when unset by default).
    pub default: Option<Value>,
    /// Schema type label (`string`, `bool`, `integer`, `float`, `array`, `table`).
    pub kind: String,
    /// Winning layer description (`default`, `global config`, `project config`, ...).
    pub source: String,
    /// Allowed enum values, if constrained.
    pub values: Vec<String>,
    /// Human-readable description.
    pub description: String,
    /// Whether this setting holds a credential and must render as `<redacted>`.
    pub secret: bool,
}

/// Host port supplied by the binary so `hrn-harness` can serve `cfg://` with zero `std::fs` or
/// TOML-writer dependencies.
pub trait SettingsPort: Send + Sync {
    /// List all schema-declared settings in order.
    fn list(&self) -> Vec<SettingView>;

    /// Look up one exact dotted setting key.
    fn get(&self, key: &str) -> Option<SettingView>;

    /// Fuzzy key suggestions for an unknown path.
    fn similar(&self, key: &str) -> Vec<String>;

    /// Apply a session-only runtime override for `key`.
    fn set_session(&self, key: &str, raw_value: &str) -> Result<String>;

    /// Persist `key` to the global config file.
    fn save_global(&self, key: &str, raw_value: &str) -> Result<String>;

    /// Drive an interactive approval prompt for a `cfg://` mutation.
    fn confirm(
        &self,
        gate: &Arc<dyn ApprovalGate>,
        request: ApprovalRequest,
        cancel: CancelFlag,
    ) -> Result<ApprovalDecision>;
}

/// `cfg://` [`UrlScheme`] backed by a [`SettingsPort`].
pub struct CfgScheme {
    port: Arc<dyn SettingsPort>,
    /// Session IDs that granted `"always for this session"` for `cfg://` writes.
    session_grants: Mutex<BTreeSet<String>>,
}

impl CfgScheme {
    /// Construct a `cfg://` scheme backed by `port`.
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        Self {
            port,
            session_grants: Mutex::new(BTreeSet::new()),
        }
    }

    /// Clear all `cfg://` session grants (`/forget-approvals`).
    pub fn clear_grants(&self) {
        self.session_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Bind this scheme to `grants` so [`GrantCache::clear`] also clears `cfg://` session grants.
    pub fn bind_grant_cache(self: &Arc<Self>, grants: &GrantCache) {
        let weak = Arc::downgrade(self);
        grants.register_on_clear(Arc::new(move || {
            if let Some(scheme) = weak.upgrade() {
                scheme.clear_grants();
            }
        }));
    }

    fn has_grant(&self, session_id: &str) -> bool {
        self.session_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id)
    }

    fn record_grant(&self, session_id: &str) {
        self.session_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_owned());
    }

    fn read_tree(&self, prefix: Option<&str>) -> Result<String> {
        let all = self.port.list();
        let filtered: Vec<&SettingView> = match prefix {
            None => all.iter().collect(),
            Some(pfx) => {
                let dotted = format!("{pfx}.");
                all.iter().filter(|s| s.key.starts_with(&dotted)).collect()
            }
        };
        if filtered.is_empty() {
            let target = prefix.unwrap_or("");
            return Err(unknown_error(self.port.as_ref(), target));
        }
        let mut out = String::new();
        for view in filtered {
            let val = format_setting_value(view);
            let mut comment_parts: Vec<String> = Vec::new();
            if !view.values.is_empty() {
                comment_parts.push(view.values.join("|"));
            }
            if let Some(def) = &view.default {
                let shown = if view.secret {
                    "<redacted>".to_owned()
                } else {
                    compact_json(def)
                };
                comment_parts.push(format!("default {shown}"));
            }
            let first_sentence = first_sentence_clipped(&view.description, 120);
            if !first_sentence.is_empty() {
                comment_parts.push(first_sentence);
            }
            let _ = writeln!(out, "{} = {val}  # {}", view.key, comment_parts.join(" · "));
        }
        Ok(out)
    }

    fn read_leaf(view: &SettingView) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "path: {}", view.key);
        let _ = writeln!(out, "value: {}", format_setting_value(view));
        let _ = writeln!(out, "type: {}", view.kind);
        let default_str = match (&view.default, view.secret) {
            (Some(_), true) => "<redacted>".to_owned(),
            (Some(v), false) => compact_json(v),
            (None, _) => "(unset)".to_owned(),
        };
        let _ = writeln!(out, "default: {default_str}");
        let _ = writeln!(out, "source: {}", view.source);
        if !view.values.is_empty() {
            let _ = writeln!(out, "values: {}", view.values.join(", "));
        }
        let _ = writeln!(out, "description: {}", view.description);
        out
    }
}

impl UrlScheme for CfgScheme {
    fn scheme(&self) -> &'static str {
        "cfg"
    }

    fn about(&self) -> &'static str {
        "effective settings: `cfg://` (full tree), `cfg://<section>`, `cfg://<key>`; \
         `write cfg://<key>` applies for this session and `write cfg://<key>/save` persists globally"
    }

    fn read(&self, _ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let key = rest.trim_matches('/');
        if key.is_empty() {
            return self.read_tree(None).map(UrlBody::Text);
        }
        if let Some(view) = self.port.get(key) {
            return Ok(UrlBody::Text(Self::read_leaf(&view)));
        }
        self.read_tree(Some(key)).map(UrlBody::Text)
    }

    fn write(&self, ctx: &ToolContext, rest: &str, content: &str) -> Result {
        if ctx.is_subagent {
            return Err(HrnError::Tool(
                "subagents cannot modify settings via `cfg://`".to_owned(),
            ));
        }
        let trimmed = rest.trim_matches('/');
        let (key, persist) = match trimmed.strip_suffix("/save") {
            Some(base) => (base.trim_matches('/'), true),
            None => (trimmed, false),
        };
        if key.is_empty() {
            return Err(HrnError::Tool(
                "`cfg://` write requires a setting key (`cfg://<key>` or `cfg://<key>/save`)"
                    .to_owned(),
            ));
        }
        let Some(view) = self.port.get(key) else {
            return Err(unknown_error(self.port.as_ref(), key));
        };
        let raw_value = normalize_written_value(key, content);
        let sensitive = is_security_sensitive_key(key);
        if sensitive || !self.has_grant(&ctx.session_id) {
            let Some(gate) = ctx.approval.as_ref() else {
                return Err(HrnError::Tool(
                    "`cfg://` write requires interactive user approval (no approval channel)"
                        .to_owned(),
                ));
            };
            let scope_label = if persist {
                "global config"
            } else {
                "this session"
            };
            let display_val = if view.secret {
                "<redacted>"
            } else {
                raw_value.as_str()
            };
            let choices = if sensitive {
                vec![ApprovalGrantScope::Once]
            } else {
                vec![ApprovalGrantScope::Once, ApprovalGrantScope::ToolSession]
            };
            let request = ApprovalRequest {
                id: format!("cfg-{}", ctx.call_id),
                session_id: ctx.session_id.clone(),
                tool_name: "cfg".to_owned(),
                summary: format!("set `{key}` = `{display_val}` ({scope_label})"),
                diff: None,
                reason: Some(if sensitive {
                    "Modify security-sensitive harness setting via cfg://".to_owned()
                } else {
                    "Modify harness settings via cfg://".to_owned()
                }),
                targets: vec![format!("cfg://{trimmed}")],
                choices,
                tier: Some(ToolTier::Write),
            };
            let decision = self.port.confirm(gate, request, ctx.cancel.clone())?;
            match decision {
                ApprovalDecision::Approve {
                    scope: ApprovalGrantScope::ToolSession | ApprovalGrantScope::Persistent,
                } if !sensitive => {
                    self.record_grant(&ctx.session_id);
                }
                ApprovalDecision::Approve { .. } => {}
                ApprovalDecision::Deny | ApprovalDecision::DenyAndSend { .. } => {
                    return Err(HrnError::Tool(format!(
                        "user denied `cfg://{trimmed}` write"
                    )));
                }
            }
        }
        let _summary = if persist {
            self.port.save_global(key, &raw_value)?
        } else {
            self.port.set_session(key, &raw_value)?
        };
        Ok(())
    }
}

fn is_security_sensitive_key(key: &str) -> bool {
    key == "tools"
        || key.starts_with("tools.")
        || key == "shell"
        || key.starts_with("shell.")
        || key == "llm.accounts"
        || key.starts_with("llm.accounts.")
        || key == "llm.retry"
        || key.starts_with("llm.retry.")
        || key == "llm.tier"
        || key.starts_with("llm.tier.")
        || key == "llm.extended_context"
        || key == "llm.anthropic"
        || key.starts_with("llm.anthropic.")
        || key == "eval"
        || key.starts_with("eval.")
        || key == "desktop.eval_prelude"
        || key == "edit"
        || key.starts_with("edit.")
        || key == "lsp"
        || key.starts_with("lsp.")
        || key == "llm.kimi"
        || key.starts_with("llm.kimi.")
        || key == "gc"
        || key.starts_with("gc.")
        || key == "share"
        || key.starts_with("share.")
        || key == "collab"
        || key.starts_with("collab.")
        || key == "commit"
        || key.starts_with("commit.")
        || key == "update"
        || key.starts_with("update.")
}

fn unknown_error(port: &dyn SettingsPort, key: &str) -> HrnError {
    let similar = port.similar(key);
    if similar.is_empty() {
        HrnError::Tool(format!("unknown setting `{key}`"))
    } else {
        HrnError::Tool(format!(
            "unknown setting `{key}`\nSimilar: {}",
            similar.join(", ")
        ))
    }
}

/// Accept either raw value (`false`) or `key = false` syntax inside the written content.
fn normalize_written_value(key: &str, content: &str) -> String {
    let trimmed = content.trim();
    if let Some((lhs, rhs)) = trimmed.split_once('=')
        && lhs.trim() == key
    {
        return rhs.trim().to_owned();
    }
    trimmed.to_owned()
}

fn format_setting_value(view: &SettingView) -> String {
    if view.secret {
        return if view.value.is_some() {
            "<redacted>".to_owned()
        } else {
            "(unset)".to_owned()
        };
    }
    view.value
        .as_ref()
        .map_or_else(|| "(unset)".to_owned(), compact_json)
}

fn compact_json(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn first_sentence_clipped(text: &str, max_chars: usize) -> String {
    let sentence = text
        .split_once(". ")
        .map_or(text.trim(), |(head, _)| head.trim());
    if sentence.chars().count() <= max_chars {
        sentence.to_owned()
    } else {
        let clipped: String = sentence.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{clipped}…")
    }
}

#[cfg(test)]
#[path = "cfg_scheme_tests.rs"]
mod tests;
