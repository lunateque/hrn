//! Claude Pro/Max subscription request shaping, mirrored from oh-my-pi: the Claude Code headers
//! and beta list, the billing and identity system blocks with the `cch` body checksum,
//! `metadata.user_id`, the `_` tool-name prefix and one-hour cache breakpoints (at most 4).

use crate::anthropic::ANTHROPIC_VERSION;
use crate::anthropic_wire::encode_request;
use crate::common::fatal;
use crate::xxhash::xxh64;
use hrn_edge_oauth::{SubscriptionCredential, sha256, to_hex};
use hrn_kernel::{LlmRequest, Result, Role, Tier};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

/// Claude Code version presented when nothing newer was required.
pub(crate) const DEFAULT_VERSION: &str = "2.1.326";
/// Environment override of the presented Claude Code version.
pub(crate) const VERSION_ENV: &str = "HRN_CLAUDE_CODE_VERSION";
const SDK_VERSION: &str = "0.128.0";
const RUNTIME_VERSION: &str = "v26.12.1";
const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// Anthropic server tools keep their names; every other tool goes out as `_<name>`.
const BUILTIN_TOOLS: [&str; 4] = ["web_search", "code_execution", "text_editor", "computer"];
const BILLING_SALT: &str = "59cf53e54c78";
const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;
const CCH_ANCHOR: &[u8] = br#""system":[{"type":"text","text":"x-anthropic-billing-header:"#;
const CCH_PLACEHOLDER: &[u8] = b"cch=00000;";
/// The `cch` placeholder must sit this close behind the anchor.
const CCH_WINDOW: usize = 150;
/// System text from the first of these on changes between turns, so it follows the breakpoint.
const VOLATILE_MARKERS: [&str; 2] = ["<memories>", "<project-context>"];

#[derive(Clone, Copy, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
    ttl: &'static str,
}

const ONE_HOUR: CacheControl = CacheControl {
    kind: "ephemeral",
    ttl: "1h",
};

/// One `system` block; field order is wire order (the `cch` anchor depends on it).
#[derive(Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl SystemBlock {
    fn text(text: impl Into<String>) -> Self {
        Self {
            kind: "text",
            text: text.into(),
            cache_control: None,
        }
    }
}

#[derive(Serialize)]
struct Metadata {
    user_id: String,
}

#[derive(Serialize)]
struct UserId<'a> {
    device_id: String,
    session_id: &'a str,
    account_uuid: &'a str,
}

/// The request body in Claude Code's key order.
#[derive(Serialize)]
struct Body {
    model: Value,
    messages: Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<SystemBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Value>,
    metadata: Metadata,
    max_tokens: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'static str>,
    stream: bool,
}

/// A shaped request: the final body bytes and its `anthropic-beta` header.
pub(crate) struct ClaudeRequest {
    pub(crate) body: Vec<u8>,
    pub(crate) betas: String,
}

/// Wire name of a tool on the subscription path.
pub(crate) fn wire_tool_name(name: &str) -> String {
    if BUILTIN_TOOLS.contains(&name) {
        name.to_owned()
    } else {
        format!("_{name}")
    }
}

/// Shape `req` the way Claude Code sends it for the logged-in account.
pub(crate) fn encode(
    model_id: &str,
    req: &LlmRequest,
    cred: &SubscriptionCredential,
    session: &str,
    version: &str,
    strip_temp: bool,
) -> Result<ClaudeRequest> {
    let mut wire = encode_request(model_id, req, false, strip_temp)?;
    let Some(obj) = wire.as_object_mut() else {
        return Err(fatal("request body is not an object"));
    };
    let mut messages = obj.remove("messages").unwrap_or_else(|| json!([]));
    let mut tools = obj.remove("tools");
    let thinking = obj.remove("thinking");

    let mut system = Vec::new();
    if !is_haiku_3_5(model_id) {
        system.push(SystemBlock::text(billing_block(
            first_user_text(&messages),
            version,
        )));
        system.push(SystemBlock::text(IDENTITY));
    }
    if let Some(stable) = push_system_parts(&mut system, req).and_then(|i| system.get_mut(i)) {
        stable.cache_control = Some(ONE_HOUR);
    }
    if let Some(list) = tools.as_mut().and_then(Value::as_array_mut) {
        for tool in list.iter_mut() {
            prefix_name(tool);
        }
        if let Some(last) = list.last_mut() {
            mark_cached(last);
        }
    }
    shape_messages(&mut messages);

    let betas = betas(tools.is_some() || thinking.is_some(), thinking.is_some());
    let body = Body {
        model: obj.remove("model").unwrap_or_else(|| json!(model_id)),
        messages,
        system,
        tools,
        metadata: Metadata {
            user_id: user_id(cred, session)?,
        },
        max_tokens: obj.remove("max_tokens").unwrap_or_else(|| json!(16_384)),
        thinking,
        temperature: obj.remove("temperature"),
        service_tier: matches!(req.service_tier, Some(Tier::Priority | Tier::Ultrafast))
            .then_some("priority"),
        stream: true,
    };
    let mut bytes = serde_json::to_vec(&body)?;
    apply_cch(&mut bytes);
    Ok(ClaudeRequest { body: bytes, betas })
}

/// Claude Code's beta list: the agent set for tool or thinking turns, the utility set otherwise.
fn betas(agent: bool, thinking: bool) -> String {
    let mut list = Vec::with_capacity(9);
    if agent {
        list.push("claude-code-20250219");
    }
    list.extend([
        "oauth-2025-04-20",
        "interleaved-thinking-2025-05-14",
        "thinking-token-count-2026-05-13",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
    ]);
    if agent {
        if thinking {
            list.push("effort-2025-11-24");
        }
        list.push("fallback-credit-2026-06-01");
    } else {
        list.push("structured-outputs-2025-12-15");
    }
    list.join(",")
}

/// Claude Code skips its fingerprint blocks for Haiku 3.5.
fn is_haiku_3_5(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    ["3-5-haiku", "haiku-3-5", "3.5-haiku", "haiku-3.5"]
        .iter()
        .any(|m| id.contains(m))
}

/// Text of the first `text` block of the first user message on the wire.
fn first_user_text(messages: &Value) -> &str {
    let first_user = messages
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"));
    match first_user.and_then(|m| m.get("content")) {
        Some(Value::String(text)) => text,
        Some(Value::Array(blocks)) => blocks
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
        _ => "",
    }
}

/// `x-anthropic-billing-header` text; `cch=00000` is replaced once the body is serialized.
fn billing_block(first_user_text: &str, version: &str) -> String {
    format!(
        "x-anthropic-billing-header: cc_version={version}.{}; cc_entrypoint=cli; cch=00000;",
        billing_suffix(first_user_text, version)
    )
}

/// Three hex digits of `SHA-256(salt + text[4] + text[7] + text[20] + version)`, indexing UTF-16
/// code units as JavaScript does (a lone surrogate encodes as U+FFFD, a missing one as `0`).
fn billing_suffix(text: &str, version: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().take(21).collect();
    let mut seed = String::with_capacity(BILLING_SALT.len() + 12 + version.len());
    seed.push_str(BILLING_SALT);
    for index in [4, 7, 20] {
        seed.push(units.get(index).map_or('0', |&unit| {
            char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER)
        }));
    }
    seed.push_str(version);
    let hex = to_hex(&sha256(seed.as_bytes()));
    hex.get(..3).unwrap_or("000").to_owned()
}

/// Replace the billing block's `cch=00000` with `XXH64(body) & 0xfffff` as five hex digits.
fn apply_cch(body: &mut [u8]) {
    let Some(anchor) = find(body, CCH_ANCHOR) else {
        return;
    };
    let start = anchor + CCH_ANCHOR.len();
    let end = start.saturating_add(CCH_WINDOW).min(body.len());
    let Some(offset) = body.get(start..end).and_then(|w| find(w, CCH_PLACEHOLDER)) else {
        return;
    };
    let digits = format!("{:05x}", xxh64(body, CCH_SEED) & 0xf_ffff);
    let at = start + offset + "cch=".len();
    if let Some(slot) = body.get_mut(at..at + 5) {
        slot.copy_from_slice(digits.as_bytes());
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Append the request's system messages as their own blocks; text from the first volatile
/// marker on starts the uncached suffix. Returns the index of the last stable block.
fn push_system_parts(system: &mut Vec<SystemBlock>, req: &LlmRequest) -> Option<usize> {
    let mut last_stable = system.len().checked_sub(1);
    let mut volatile = false;
    for msg in req.messages.iter().filter(|m| m.role == Role::System) {
        let text = msg.content.as_str();
        if text.trim().is_empty() {
            continue;
        }
        let split = if volatile {
            Some(0)
        } else {
            VOLATILE_MARKERS.iter().filter_map(|m| text.find(m)).min()
        };
        if let Some((head, tail)) = split.and_then(|at| text.split_at_checked(at)) {
            if !head.trim().is_empty() {
                system.push(SystemBlock::text(head));
                last_stable = Some(system.len() - 1);
            }
            system.push(SystemBlock::text(tail));
            volatile = true;
        } else {
            system.push(SystemBlock::text(text));
            last_stable = Some(system.len() - 1);
        }
    }
    last_stable
}

fn prefix_name(item: &mut Value) {
    if let Some(Value::String(name)) = item.get_mut("name") {
        *name = wire_tool_name(name);
    }
}

fn mark_cached(block: &mut Value) {
    if let Some(obj) = block.as_object_mut() {
        obj.insert("cache_control".into(), json!(ONE_HOUR));
    }
}

fn content_blocks(msg: &mut Value) -> Option<&mut Vec<Value>> {
    msg.get_mut("content").and_then(Value::as_array_mut)
}

/// Prefix history `tool_use` names and put the two rolling breakpoints on the last block of
/// each of the last two messages (thinking blocks cannot carry one).
fn shape_messages(messages: &mut Value) {
    let Some(list) = messages.as_array_mut() else {
        return;
    };
    for blocks in list.iter_mut().filter_map(content_blocks) {
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                prefix_name(block);
            }
        }
    }
    for blocks in list.iter_mut().rev().take(2).filter_map(content_blocks) {
        let cacheable = blocks.iter_mut().rev().find(|b| {
            !matches!(
                b.get("type").and_then(Value::as_str),
                Some("thinking" | "redacted_thinking")
            )
        });
        if let Some(block) = cacheable {
            mark_cached(block);
        }
    }
}

/// `metadata.user_id`: a JSON string with a per-account device id and the session id.
fn user_id(cred: &SubscriptionCredential, session: &str) -> Result<String> {
    let account = cred.account_id.as_deref().unwrap_or_default();
    let who = Some(account)
        .filter(|a| !a.is_empty())
        .or(cred.email.as_deref())
        .unwrap_or("anonymous");
    let device_id = to_hex(&sha256(format!("hrn-claude-device-id-v1:{who}").as_bytes()));
    Ok(serde_json::to_string(&UserId {
        device_id,
        session_id: session,
        account_uuid: account,
    })?)
}

/// Headers of a subscription request (Claude Code's own set).
pub(crate) fn headers(
    version: &str,
    access: &str,
    session: &str,
    betas: &str,
) -> Vec<(String, String)> {
    let fixed = [
        ("accept", "application/json"),
        ("content-type", "application/json"),
        ("x-stainless-arch", stainless_arch()),
        ("x-stainless-os", stainless_os()),
        ("x-stainless-lang", "js"),
        ("x-stainless-package-version", SDK_VERSION),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-runtime-version", RUNTIME_VERSION),
        ("x-stainless-timeout", "600"),
        ("anthropic-dangerous-direct-browser-access", "true"),
        ("anthropic-version", ANTHROPIC_VERSION),
        ("x-app", "cli"),
        ("connection", "keep-alive"),
    ];
    let mut out: Vec<(String, String)> = fixed
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    out.extend([
        (
            "user-agent".to_owned(),
            format!("claude-cli/{version} (external, cli)"),
        ),
        ("x-claude-code-session-id".to_owned(), session.to_owned()),
        ("authorization".to_owned(), format!("Bearer {access}")),
        ("anthropic-beta".to_owned(), betas.to_owned()),
    ]);
    out
}

fn stainless_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// The Stainless SDK's platform name (it would say `Other:<os>` where this says `Unknown`).
fn stainless_os() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "MacOS",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        _ => "Unknown",
    }
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

/// The Claude Code version to present: `HRN_CLAUDE_CODE_VERSION`, else the version a rejection
/// required (persisted in `store`) when newer than the built-in default.
pub(crate) fn resolve_version(env_value: Option<String>, store: &Path) -> String {
    if let Some(v) = env_value.map(|v| v.trim().to_owned())
        && parse_version(&v).is_some()
    {
        return v;
    }
    std::fs::read_to_string(store)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| parse_version(s) > parse_version(DEFAULT_VERSION))
        .unwrap_or_else(|| DEFAULT_VERSION.to_owned())
}

/// The version a `claude_code_version_too_old` rejection asks for, when newer than `current`
/// ("… version X.Y.Z or newer is required").
pub(crate) fn required_version(body: &str, current: &str) -> Option<String> {
    let (before, _) = body.split_once(" or newer is required")?;
    let token = before
        .rsplit(char::is_whitespace)
        .next()?
        .trim_matches(|c: char| !(c.is_ascii_digit() || c == '.'));
    (parse_version(token)? > parse_version(current)?).then(|| token.to_owned())
}

#[cfg(test)]
#[path = "claude_code_tests.rs"]
mod tests;
