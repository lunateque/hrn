//! Non-mutating context compaction (`project_branch`):
//! - Tier 1: Batched `supersedeReads` (only triggers when reclaimable bytes > 15% to keep prompt caches warm)
//! - Tier 2: Head/tail cap on older tool results outside the recent turn window
//! - Shake overlay: applies persisted `method == "shake"` tool-output elisions without losing conversation turns
//! - Full summary / handoff: projects from the latest non-shake `Summary` node plus verbatim recent tail.

use hrn_kernel::{DagNode, DagPayload, ImageAttachment, Msg, Role};
use serde_json::Value;
use std::collections::HashMap;

/// Tag prefix stored on synthetic `Role::User` summary `Msg::read_key` so Responses / Codex
/// encoders can splice opaque remote-compaction preserve items directly into `input` while other
/// providers ignore `read_key` and seamlessly fall back to `Msg::content`.
pub(crate) const REMOTE_COMPACTION_PREFIX: &str = "hrn:remote_compaction:";

/// Tunable compaction policy for `project_branch` and the compaction method walk.
#[derive(Clone, Debug)]
pub struct CompactionPolicy {
    /// Typed `[compaction]` configuration loaded from `compaction.defaults.json` / user config.
    pub config: CompactionConfig,
    /// Model context window in tokens.
    pub context_window_tokens: u64,
    /// Optional canonical model key (`<provider>/<id>` or alias) for `[compaction.model_thresholds]`.
    pub model_key: Option<String>,
    /// Minimum reclaimable ratio (percent) before firing Tier 1 `supersedeReads`.
    pub supersede_threshold_pct: usize,
    /// Number of most recent turns that stay verbatim in Tier 2 / turn-count summary fallback.
    pub keep_recent_turns: usize,
    /// Maximum byte length of older tool results under Tier 2 truncation.
    pub old_tool_cap_bytes: usize,
    /// Tokens consumed by the system prompt and tool schemas (added to the estimate).
    pub fixed_overhead_tokens: u64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self::from_config(CompactionConfig::default())
    }
}

impl CompactionPolicy {
    /// Build a policy from a `[compaction]` config plus embedded projection defaults.
    #[must_use]
    pub fn from_config(config: CompactionConfig) -> Self {
        let defaults = policy_defaults();
        Self {
            config,
            context_window_tokens: defaults.context_window_tokens,
            model_key: None,
            supersede_threshold_pct: defaults.supersede_threshold_pct,
            keep_recent_turns: defaults.keep_recent_turns,
            old_tool_cap_bytes: defaults.old_tool_cap_bytes,
            fixed_overhead_tokens: 0,
        }
    }

    /// Estimated-token level at which automatic compaction becomes due (`0` when disabled).
    #[must_use]
    pub fn trigger_tokens(&self) -> u64 {
        self.config
            .trigger_tokens(self.context_window_tokens, self.model_key.as_deref())
    }

    /// Effective reserve token floor (`reserve_tokens` or `max(15% * window, 16_384)`).
    #[must_use]
    pub fn reserve_floor(&self) -> u64 {
        self.config.reserve_floor(self.context_window_tokens)
    }

    /// Effective recent tail token budget kept verbatim (`min(keep_recent_tokens, 0.8 * trigger - reserve)`).
    #[must_use]
    pub fn effective_keep_recent_tokens(&self) -> u64 {
        self.config
            .effective_keep_recent_tokens(self.context_window_tokens, self.model_key.as_deref())
    }
}

/// Result of projecting an append-only DAG branch into canonical prompt messages.
#[derive(Clone, Debug)]
pub struct ProjectedContext {
    /// Projected message list (excluding the static system prompt).
    pub messages: Vec<Msg>,
    /// Rough byte-based token estimate (`bytes / 4`, plus the policy's fixed overhead).
    pub estimated_tokens: u64,
    /// Whether Tier 3 incremental summary compaction should run before the next call.
    pub needs_tier3_summary: bool,
    /// Number of duplicate `read` outputs superseded in Tier 1.
    pub superseded_reads: usize,
}

/// Bytes a message contributes to the prompt (text, tool-call payloads, replayed thinking).
pub(crate) fn prompt_bytes(m: &Msg) -> usize {
    let calls: usize = m
        .tool_calls
        .iter()
        .map(|c| c.id.len() + c.name.len() + c.arguments.len())
        .sum();
    let thinking: usize = m
        .thinking_blocks
        .iter()
        .map(|b| {
            b.text.len()
                + b.signature.as_ref().map_or(0, String::len)
                + b.redacted.as_ref().map_or(0, String::len)
        })
        .sum();
    let images = m.images.len().saturating_mul(6000);
    m.content.len() + calls + thinking + images
}

/// Rough byte-based token estimate (`prompt_bytes / 4`) for one message.
pub(crate) fn estimate_msg_tokens(m: &Msg) -> u64 {
    u64::try_from(prompt_bytes(m)).unwrap_or(u64::MAX) / 4
}

/// Whether `payload` is a full branch-folding summary (`summary`/`handoff`/`remote`/`snapcompact`),
/// as opposed to a non-destructive `shake` elision overlay.
pub(crate) fn is_full_summary(payload: &DagPayload) -> bool {
    matches!(
        payload,
        DagPayload::Summary { method, .. } if method.as_deref() != Some("shake")
    )
}

/// Index of the latest full (non-shake) `Summary` node on the branch, if any.
fn latest_full_summary_idx(nodes: &[DagNode]) -> Option<usize> {
    nodes.iter().rposition(|n| is_full_summary(&n.payload))
}

/// Index of the first node whose message is kept verbatim after the latest full summary (0 if none).
pub(crate) fn active_start(nodes: &[DagNode]) -> usize {
    let Some(si) = latest_full_summary_idx(nodes) else {
        return 0;
    };
    match nodes.get(si).map(|n| &n.payload) {
        Some(DagPayload::Summary { covers_node_id, .. }) => nodes
            .iter()
            .position(|n| n.id == *covers_node_id)
            .map_or(si + 1, |p| p + 1),
        _ => 0,
    }
}

/// Collect persisted `shake` elision overlays (`node_id -> text`, `call_id -> text`) appearing
/// after the latest full summary node (later shake passes override earlier ones).
pub(crate) fn collect_shake_elisions(
    nodes: &[DagNode],
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut by_node = HashMap::new();
    let mut by_call = HashMap::new();
    let scan_from = latest_full_summary_idx(nodes).map_or(0, |si| si + 1);
    for node in nodes.get(scan_from..).unwrap_or_default() {
        let DagPayload::Summary {
            method: Some(m),
            preserve: Some(preserve),
            ..
        } = &node.payload
        else {
            continue;
        };
        if m != "shake" {
            continue;
        }
        if let Some(map) = preserve
            .get("elisions")
            .and_then(serde_json::Value::as_object)
        {
            for (k, v) in map {
                if let Some(text) = v.as_str() {
                    by_node.insert(k.clone(), text.to_owned());
                }
            }
        }
        if let Some(map) = preserve
            .get("call_elisions")
            .and_then(serde_json::Value::as_object)
        {
            for (k, v) in map {
                if let Some(text) = v.as_str() {
                    by_call.insert(k.clone(), text.to_owned());
                }
            }
        }
    }
    (by_node, by_call)
}

/// Project a chronological DAG branch into LLM messages without mutating stored history.
pub fn project_branch(nodes: &[DagNode], policy: &CompactionPolicy) -> ProjectedContext {
    let mut msgs: Vec<Msg> = Vec::with_capacity(nodes.len());
    if let Some(si) = latest_full_summary_idx(nodes)
        && let Some(DagPayload::Summary {
            text,
            method,
            preserve,
            ..
        }) = nodes.get(si).map(|n| &n.payload)
    {
        msgs.push(project_summary_msg(
            text,
            method.as_deref(),
            preserve.as_ref(),
        ));
    }
    let (node_elisions, call_elisions) = collect_shake_elisions(nodes);
    for node in nodes.get(active_start(nodes)..).unwrap_or_default() {
        match &node.payload {
            DagPayload::Message { msg, .. } => {
                let mut projected = msg.clone();
                if projected.role == Role::Tool
                    && let Some(replacement) = node_elisions.get(&node.id).or_else(|| {
                        projected
                            .tool_call_id
                            .as_deref()
                            .and_then(|cid| call_elisions.get(cid))
                    })
                {
                    replacement.clone_into(&mut projected.content);
                    projected.images.clear();
                }
                msgs.push(projected);
            }
            DagPayload::BranchSummary { text, .. } => {
                msgs.push(Msg::user(format!("[Summary of abandoned branch]\n{text}")));
            }
            _ => {}
        }
    }

    let superseded_reads = apply_batched_supersede_reads(&mut msgs, policy.supersede_threshold_pct);
    apply_tier2_old_tool_cap(
        &mut msgs,
        policy.keep_recent_turns,
        policy.old_tool_cap_bytes,
    );

    let total_bytes: usize = msgs.iter().map(prompt_bytes).sum();
    let estimated_tokens = (u64::try_from(total_bytes).unwrap_or(u64::MAX) / 4)
        .saturating_add(policy.fixed_overhead_tokens);
    let trigger = policy.trigger_tokens();
    ProjectedContext {
        messages: msgs,
        estimated_tokens,
        needs_tier3_summary: trigger > 0 && estimated_tokens >= trigger,
        superseded_reads,
    }
}

fn apply_batched_supersede_reads(msgs: &mut [Msg], threshold_pct: usize) -> usize {
    let mut latest_idx_by_key: HashMap<String, usize> = HashMap::new();
    for (idx, msg) in msgs.iter().enumerate() {
        if msg.role == Role::Tool
            && let Some(key) = &msg.read_key
        {
            latest_idx_by_key.insert(key.clone(), idx);
        }
    }
    let total_bytes: usize = msgs.iter().map(prompt_bytes).sum();
    if total_bytes == 0 {
        return 0;
    }

    let mut stale: Vec<usize> = Vec::new();
    let mut reclaimable = 0usize;
    for (idx, msg) in msgs.iter().enumerate() {
        if msg.role == Role::Tool
            && let Some(key) = &msg.read_key
            && latest_idx_by_key.get(key).copied() != Some(idx)
            && msg.content.len() > 120
        {
            reclaimable += msg.content.len().saturating_sub(64);
            stale.push(idx);
        }
    }
    if stale.is_empty() || reclaimable * 100 < total_bytes * threshold_pct {
        return 0;
    }
    let count = stale.len();
    for idx in stale {
        if let Some(msg) = msgs.get_mut(idx) {
            let key = msg.read_key.clone().unwrap_or_default();
            msg.content = format!("[superseded by a later read of `{key}`]");
        }
    }
    count
}

fn apply_tier2_old_tool_cap(msgs: &mut [Msg], keep_recent_turns: usize, cap_bytes: usize) {
    let user_positions: Vec<usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i)
        .collect();
    let keep = keep_recent_turns.max(1);
    if user_positions.len() <= keep {
        return;
    }
    let cutoff = user_positions
        .get(user_positions.len() - keep)
        .copied()
        .unwrap_or(0);

    for msg in msgs.iter_mut().take(cutoff) {
        if msg.role == Role::Tool {
            if !msg.images.is_empty() {
                msg.images.clear();
            }
            if msg.content.len() > cap_bytes {
                let head = msg.content.floor_char_boundary((cap_bytes * 60) / 100);
                let tail_start = msg
                    .content
                    .ceil_char_boundary(msg.content.len().saturating_sub((cap_bytes * 35) / 100));
                let omitted = tail_start.saturating_sub(head);
                msg.content = format!(
                    "{}\n... [compacted {omitted} bytes] ...\n{}",
                    msg.content.get(..head).unwrap_or(""),
                    msg.content.get(tail_start..).unwrap_or("")
                );
            }
        }
    }
}

fn project_summary_msg(text: &str, method: Option<&str>, preserve: Option<&Value>) -> Msg {
    match method {
        Some("handoff") => Msg::user(format!("[Handoff from earlier turns]\n{text}")),
        Some("remote") => {
            let mut msg = Msg::user(format!("[Conversation summary from earlier turns]\n{text}"));
            if let Some(val) = preserve
                && let Ok(encoded) = serde_json::to_string(val)
            {
                msg.read_key = Some(format!("{REMOTE_COMPACTION_PREFIX}{encoded}"));
            }
            msg
        }
        Some("snapcompact") => {
            let images: Vec<ImageAttachment> = preserve
                .and_then(|v| v.get("images"))
                .and_then(|imgs| serde_json::from_value(imgs.clone()).ok())
                .unwrap_or_default();
            Msg::user_with_images(
                format!("[Conversation snapshot archive from earlier turns]\n{text}"),
                images,
            )
        }
        _ => Msg::user(format!("[Conversation summary from earlier turns]\n{text}")),
    }
}

#[cfg(test)]
#[path = "compaction_unit_tests.rs"]
mod tests;

// ===== merged from compaction_config.rs =====
// Typed `[compaction]` configuration: trigger thresholds, reserve floor, tail cap, idle timer,
// method walk order, and per-model threshold overrides.
//
// Defaults live as data in `crates/harness/data/compaction.defaults.json`.

use hrn_kernel::{HrnError, Result};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;

/// Embedded baseline defaults for `[compaction]` and projection tuning (data file).
const DEFAULTS_JSON: &str = include_str!("../data/compaction.defaults.json");

/// Minimum automatic reserve floor when `reserve_tokens` is unset (`max(15% * window, 16_384)`).
const MIN_AUTO_RESERVE_TOKENS: u64 = 16_384;
/// Auto reserve percentage of context window (`15%`).
const AUTO_RESERVE_PCT: u64 = 15;
/// Minimum clamped idle timeout in seconds when idle compaction is enabled.
pub const MIN_IDLE_TIMEOUT_SECS: u64 = 60;
/// Minimum speculative compaction lead window in tokens (`clamp(threshold / 8, 8_192, 32_000)`).
pub const SPECULATIVE_LEAD_MIN: u64 = 8_192;
/// Maximum speculative compaction lead window in tokens (`clamp(threshold / 8, 8_192, 32_000)`).
pub const SPECULATIVE_LEAD_MAX: u64 = 32_000;

/// Compute the async speculative compaction lead window: `clamp(threshold / 8, 8_192, 32_000)`.
#[must_use]
pub const fn speculative_lead_tokens(threshold: u64) -> u64 {
    let eighth = threshold / 8;
    if eighth < SPECULATIVE_LEAD_MIN {
        SPECULATIVE_LEAD_MIN
    } else if eighth > SPECULATIVE_LEAD_MAX {
        SPECULATIVE_LEAD_MAX
    } else {
        eighth
    }
}

/// True when `estimated_tokens` lies in the speculative pre-compaction band
/// `[threshold - lead, threshold)`.
#[must_use]
pub const fn in_speculative_band(estimated_tokens: u64, threshold: u64) -> bool {
    if threshold == 0 || estimated_tokens == 0 || estimated_tokens >= threshold {
        return false;
    }
    let band_start = threshold.saturating_sub(speculative_lead_tokens(threshold));
    estimated_tokens >= band_start
}

/// Per-model threshold override in `[compaction.model_thresholds]`.
///
/// Accepts:
/// - `Base(u64)`: base token threshold (e.g. `150000` or `"150000"`), clamped to `window - reserve`.
/// - `Fixed(u64)`: fixed token threshold formatted as `"fN"` (e.g. `"f120000"`).
/// - `Percent(u64)`: percentage of the model context window formatted as `"N%"` (e.g. `"75%"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThresholdSpec {
    /// Base token count (`N`), clamped to `window - reserve`.
    Base(u64),
    /// Fixed token count (`"fN"`), clamped to `window - reserve`.
    Fixed(u64),
    /// Percentage of the context window (`"N%"`, `1..=100`), clamped to `window - reserve`.
    Percent(u64),
}

impl ThresholdSpec {
    /// Parse a string threshold spec (`"150000"`, `"f120000"`, or `"75%"`).
    ///
    /// # Errors
    /// Returns [`HrnError::Config`] when the string is empty, malformed, zero, or out of range.
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(HrnError::Config(
                "compaction threshold spec cannot be empty".to_owned(),
            ));
        }
        if let Some(pct_raw) = s.strip_suffix('%') {
            let pct: u64 = pct_raw.trim().parse().map_err(|_| {
                HrnError::Config(format!("invalid percent threshold `{raw}` (expected `N%`)"))
            })?;
            if pct == 0 || pct > 100 {
                return Err(HrnError::Config(format!(
                    "percent threshold `{raw}` must be between 1% and 100%"
                )));
            }
            return Ok(Self::Percent(pct));
        }
        if let Some(fixed_raw) = s.strip_prefix('f').or_else(|| s.strip_prefix('F')) {
            let tokens: u64 = fixed_raw.trim().parse().map_err(|_| {
                HrnError::Config(format!("invalid fixed threshold `{raw}` (expected `fN`)"))
            })?;
            if tokens == 0 {
                return Err(HrnError::Config(format!(
                    "fixed threshold `{raw}` must be > 0"
                )));
            }
            return Ok(Self::Fixed(tokens));
        }
        let tokens: u64 = s.parse().map_err(|_| {
            HrnError::Config(format!(
                "invalid threshold `{raw}` (expected `N`, `fN`, or `N%`)"
            ))
        })?;
        if tokens == 0 {
            return Err(HrnError::Config(format!(
                "base threshold `{raw}` must be > 0"
            )));
        }
        Ok(Self::Base(tokens))
    }

    /// Evaluate this spec against `context_window` and `reserve_floor`.
    #[must_use]
    pub const fn resolve(self, context_window: u64, reserve_floor: u64) -> u64 {
        let cap = context_window.saturating_sub(reserve_floor);
        let raw = match self {
            Self::Base(n) | Self::Fixed(n) => n,
            Self::Percent(pct) => context_window.saturating_mul(pct) / 100,
        };
        if cap > 0 {
            if raw < cap { raw } else { cap }
        } else if raw < context_window {
            raw
        } else {
            context_window
        }
    }
}

impl Serialize for ThresholdSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Base(n) => serializer.serialize_u64(*n),
            Self::Fixed(n) => serializer.serialize_str(&format!("f{n}")),
            Self::Percent(p) => serializer.serialize_str(&format!("{p}%")),
        }
    }
}

impl<'de> Deserialize<'de> for ThresholdSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct SpecVisitor;
        impl Visitor<'_> for SpecVisitor {
            type Value = ThresholdSpec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a positive integer or string (`N`, `fN`, or `N%`)")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                if v == 0 {
                    return Err(E::custom("compaction threshold must be > 0"));
                }
                Ok(ThresholdSpec::Base(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                if v <= 0 {
                    return Err(E::custom("compaction threshold must be > 0"));
                }
                Ok(ThresholdSpec::Base(v.cast_unsigned()))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                ThresholdSpec::parse(v).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(SpecVisitor)
    }
}

/// Idle-session automatic compaction configuration (`[compaction.idle]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdleCompactionConfig {
    /// Enable automatic compaction after an idle period (default `false`).
    pub enabled: bool,
    /// Minimum estimated tokens before idle compaction triggers (default `200_000`).
    pub threshold_tokens: u64,
    /// Seconds of inactivity before triggering (default `300`; clamped to min `60` when enabled).
    pub timeout_secs: u64,
}

impl Default for IdleCompactionConfig {
    fn default() -> Self {
        defaults_bundle().compaction.idle.clone()
    }
}

impl IdleCompactionConfig {
    /// Effective timeout in seconds (clamped to at least [`MIN_IDLE_TIMEOUT_SECS`] = 60 s).
    #[must_use]
    pub const fn effective_timeout_secs(&self) -> u64 {
        if self.timeout_secs < MIN_IDLE_TIMEOUT_SECS {
            MIN_IDLE_TIMEOUT_SECS
        } else {
            self.timeout_secs
        }
    }
}

/// Typed `[compaction]` configuration section.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent [compaction] boolean configuration flags"
)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// Master switch for automatic compaction (default `true`; manual `/compact` still runs).
    pub enabled: bool,
    /// Trigger threshold as a percentage of the context window (`-1` = unset, `1..=100`).
    pub threshold_percent: i64,
    /// Trigger threshold in tokens (`-1` = unset; positive value wins over `threshold_percent`).
    pub threshold_tokens: i64,
    /// Explicit reserve token floor (`None` = `max(15% * window, 16_384)`).
    pub reserve_tokens: Option<u64>,
    /// Target recent tail token budget kept verbatim (capped at `0.8 * trigger - reserve_floor`).
    pub keep_recent_tokens: u64,
    /// Allow threshold compaction between steps of a single turn (`false` defers until post-turn).
    pub mid_turn_enabled: bool,
    /// Resume automatically after compaction / recovery when progress was made.
    pub auto_continue: bool,
    /// Speculatively prepare a background compaction summary before crossing the threshold.
    pub async_enabled: bool,
    /// Idle compaction settings (`[compaction.idle]`).
    pub idle: IdleCompactionConfig,
    /// Ordered list of compaction methods walked until one commits (`"soft"` aliases `"summary"`).
    pub method_order: Vec<String>,
    /// Per-model threshold overrides (exact key wins, else longest `provider/...*` prefix).
    pub model_thresholds: BTreeMap<String, ThresholdSpec>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        defaults_bundle().compaction.clone()
    }
}

/// Normalize a configured compaction method name (`"soft"` aliases `"summary"`).
#[must_use]
pub fn normalize_method_name(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "handoff" => Some("handoff"),
        "shake" => Some("shake"),
        "summary" | "soft" => Some("summary"),
        "remote" => Some("remote"),
        "snapcompact" => Some("snapcompact"),
        _ => None,
    }
}

impl CompactionConfig {
    /// Effective reserve floor for `context_window`: explicit `reserve_tokens`, else
    /// `max(15% * context_window, 16_384)`.
    #[must_use]
    pub fn reserve_floor(&self, context_window: u64) -> u64 {
        let auto =
            (context_window.saturating_mul(AUTO_RESERVE_PCT) / 100).max(MIN_AUTO_RESERVE_TOKENS);
        self.reserve_tokens.unwrap_or(auto)
    }

    /// Look up the winning [`ThresholdSpec`] for `model_key`: exact match (case-insensitive) wins,
    /// otherwise the longest `prefix*` wildcard match wins.
    #[must_use]
    pub fn match_model_threshold(&self, model_key: &str) -> Option<ThresholdSpec> {
        let needle = model_key.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return None;
        }
        for (pattern, spec) in &self.model_thresholds {
            let trimmed = pattern.trim();
            if !trimmed.ends_with('*') && trimmed.eq_ignore_ascii_case(&needle) {
                return Some(*spec);
            }
        }
        let mut best: Option<(usize, ThresholdSpec)> = None;
        for (pattern, spec) in &self.model_thresholds {
            if let Some(prefix) = pattern.trim().strip_suffix('*') {
                let low = prefix.to_ascii_lowercase();
                if needle.starts_with(&low) && best.is_none_or(|(len, _)| low.len() > len) {
                    best = Some((low.len(), *spec));
                }
            }
        }
        best.map(|(_, spec)| spec)
    }

    /// Compute the effective trigger threshold (in estimated tokens) for `context_window` and
    /// optional canonical `model_key`. Returns `0` when compaction is disabled or window is `0`.
    #[must_use]
    pub fn trigger_tokens(&self, context_window: u64, model_key: Option<&str>) -> u64 {
        if !self.enabled || context_window == 0 {
            return 0;
        }
        let reserve = self.reserve_floor(context_window);
        if let Some(spec) = model_key.and_then(|k| self.match_model_threshold(k)) {
            return spec.resolve(context_window, reserve);
        }
        let window_cap = context_window.saturating_sub(reserve);
        if self.threshold_tokens > 0 {
            let tok = self.threshold_tokens.cast_unsigned();
            return if window_cap > 0 {
                tok.min(window_cap)
            } else {
                tok.min(context_window)
            };
        }
        if self.threshold_percent > 0 {
            let pct = self.threshold_percent.cast_unsigned().min(100);
            let raw = context_window.saturating_mul(pct) / 100;
            return if window_cap > 0 {
                raw.min(window_cap)
            } else {
                raw
            };
        }
        window_cap
    }

    /// Effective tail token budget kept verbatim: `min(keep_recent_tokens, 0.8 * trigger - reserve_floor)`.
    #[must_use]
    pub fn effective_keep_recent_tokens(
        &self,
        context_window: u64,
        model_key: Option<&str>,
    ) -> u64 {
        let trigger = self.trigger_tokens(context_window, model_key);
        let reserve = self.reserve_floor(context_window);
        let cap = (trigger.saturating_mul(80) / 100).saturating_sub(reserve);
        self.keep_recent_tokens.min(cap)
    }

    /// Validate configured threshold bounds and `method_order` entries.
    ///
    /// # Errors
    /// Returns [`HrnError::Config`] when any field is out of range or names an unknown method.
    pub fn validate(&self) -> Result {
        if self.threshold_percent < -1 || self.threshold_percent > 100 {
            return Err(HrnError::Config(format!(
                "compaction.threshold_percent ({}) must be -1 or between 0 and 100",
                self.threshold_percent
            )));
        }
        if self.threshold_tokens < -1 {
            return Err(HrnError::Config(format!(
                "compaction.threshold_tokens ({}) must be >= -1",
                self.threshold_tokens
            )));
        }
        for method in &self.method_order {
            if normalize_method_name(method).is_none() {
                return Err(HrnError::Config(format!(
                    "compaction.method_order contains unknown method `{method}` \
                     (valid: handoff, shake, summary, soft, remote, snapcompact)"
                )));
            }
        }
        Ok(())
    }
}

/// Embedded projection & shake defaults loaded from `compaction.defaults.json`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PolicyDefaults {
    pub(crate) context_window_tokens: u64,
    pub(crate) supersede_threshold_pct: usize,
    pub(crate) keep_recent_turns: usize,
    pub(crate) old_tool_cap_bytes: usize,
    pub(crate) shake_protect_tokens: u64,
    pub(crate) shake_min_savings_tokens: u64,
    pub(crate) shake_min_elide_tokens: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIdleConfig {
    enabled: bool,
    threshold_tokens: u64,
    timeout_secs: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicyDefaults {
    context_window_tokens: u64,
    supersede_threshold_pct: usize,
    keep_recent_turns: usize,
    old_tool_cap_bytes: usize,
    shake_protect_tokens: u64,
    shake_min_savings_tokens: u64,
    shake_min_elide_tokens: u64,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors [compaction] defaults JSON schema"
)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompactionDefaults {
    enabled: bool,
    threshold_percent: i64,
    threshold_tokens: i64,
    reserve_tokens: Option<u64>,
    keep_recent_tokens: u64,
    mid_turn_enabled: bool,
    auto_continue: bool,
    async_enabled: bool,
    idle: RawIdleConfig,
    method_order: Vec<String>,
    model_thresholds: BTreeMap<String, ThresholdSpec>,
    policy: RawPolicyDefaults,
}

struct DefaultsBundle {
    compaction: CompactionConfig,
    policy: PolicyDefaults,
}

fn load_defaults() -> DefaultsBundle {
    match serde_json::from_str::<RawCompactionDefaults>(DEFAULTS_JSON) {
        Ok(raw) => DefaultsBundle {
            compaction: CompactionConfig {
                enabled: raw.enabled,
                threshold_percent: raw.threshold_percent,
                threshold_tokens: raw.threshold_tokens,
                reserve_tokens: raw.reserve_tokens,
                keep_recent_tokens: raw.keep_recent_tokens,
                mid_turn_enabled: raw.mid_turn_enabled,
                auto_continue: raw.auto_continue,
                async_enabled: raw.async_enabled,
                idle: IdleCompactionConfig {
                    enabled: raw.idle.enabled,
                    threshold_tokens: raw.idle.threshold_tokens,
                    timeout_secs: raw.idle.timeout_secs,
                },
                method_order: raw.method_order,
                model_thresholds: raw.model_thresholds,
            },
            policy: PolicyDefaults {
                context_window_tokens: raw.policy.context_window_tokens,
                supersede_threshold_pct: raw.policy.supersede_threshold_pct,
                keep_recent_turns: raw.policy.keep_recent_turns,
                old_tool_cap_bytes: raw.policy.old_tool_cap_bytes,
                shake_protect_tokens: raw.policy.shake_protect_tokens,
                shake_min_savings_tokens: raw.policy.shake_min_savings_tokens,
                shake_min_elide_tokens: raw.policy.shake_min_elide_tokens,
            },
        },
        // `compaction.defaults.json` is unit-tested; fallback avoids any panic if edited badly.
        Err(_) => DefaultsBundle {
            compaction: CompactionConfig {
                enabled: true,
                threshold_percent: -1,
                threshold_tokens: -1,
                reserve_tokens: None,
                keep_recent_tokens: 20_000,
                mid_turn_enabled: true,
                auto_continue: true,
                async_enabled: true,
                idle: IdleCompactionConfig {
                    enabled: false,
                    threshold_tokens: 200_000,
                    timeout_secs: 300,
                },
                method_order: vec!["handoff".into(), "shake".into(), "summary".into()],
                model_thresholds: BTreeMap::new(),
            },
            policy: PolicyDefaults {
                context_window_tokens: 200_000,
                supersede_threshold_pct: 15,
                keep_recent_turns: 6,
                old_tool_cap_bytes: 2000,
                shake_protect_tokens: 40_000,
                shake_min_savings_tokens: 20_000,
                shake_min_elide_tokens: 50,
            },
        },
    }
}

fn defaults_bundle() -> &'static DefaultsBundle {
    static DEFAULTS: OnceLock<DefaultsBundle> = OnceLock::new();
    DEFAULTS.get_or_init(load_defaults)
}

/// Embedded projection & shake tuning defaults loaded from `compaction.defaults.json`.
pub(crate) fn policy_defaults() -> PolicyDefaults {
    defaults_bundle().policy
}

#[cfg(test)]
mod compaction_config_inline_tests {
    use super::*;

    #[test]
    fn defaults_json_parses_strictly() {
        let raw: RawCompactionDefaults = serde_json::from_str(DEFAULTS_JSON)
            .expect("compaction.defaults.json must parse strictly");
        assert!(raw.enabled);
        assert_eq!(raw.threshold_percent, -1);
        assert_eq!(raw.threshold_tokens, -1);
        assert_eq!(raw.reserve_tokens, None);
        assert_eq!(raw.keep_recent_tokens, 20_000);
        assert!(raw.mid_turn_enabled);
        assert!(raw.auto_continue);
        assert!(raw.async_enabled);
        assert!(!raw.idle.enabled);
        assert_eq!(raw.idle.threshold_tokens, 200_000);
        assert_eq!(raw.idle.timeout_secs, 300);
        assert_eq!(raw.method_order, ["handoff", "shake", "summary"]);
        assert!(CompactionConfig::default().validate().is_ok());
    }

    #[test]
    fn threshold_spec_roundtrips_base_fixed_percent() {
        let base: ThresholdSpec = serde_json::from_str("120000").expect("base");
        assert_eq!(base, ThresholdSpec::Base(120_000));
        let fixed: ThresholdSpec = serde_json::from_str("\"f90000\"").expect("fixed");
        assert_eq!(fixed, ThresholdSpec::Fixed(90_000));
        let pct: ThresholdSpec = serde_json::from_str("\"65%\"").expect("pct");
        assert_eq!(pct, ThresholdSpec::Percent(65));
        assert_eq!(serde_json::to_string(&fixed).expect("ser"), "\"f90000\"");
        assert_eq!(serde_json::to_string(&pct).expect("ser"), "\"65%\"");
        assert!(ThresholdSpec::parse("0%").is_err());
        assert!(ThresholdSpec::parse("101%").is_err());
        assert!(ThresholdSpec::parse("f0").is_err());
    }

    #[test]
    fn idle_timeout_clamps_to_sixty_seconds() {
        let cfg = IdleCompactionConfig {
            enabled: true,
            threshold_tokens: 100_000,
            timeout_secs: 15,
        };
        assert_eq!(cfg.effective_timeout_secs(), 60);
    }

    #[test]
    fn method_order_accepts_aliases_and_rejects_unknown() {
        let ok = CompactionConfig {
            method_order: vec![
                "remote".into(),
                "snapcompact".into(),
                "handoff".into(),
                "shake".into(),
                "soft".into(),
            ],
            ..CompactionConfig::default()
        };
        assert!(ok.validate().is_ok());
        assert_eq!(normalize_method_name("soft"), Some("summary"));
        let bad = CompactionConfig {
            method_order: vec!["bogus".into()],
            ..CompactionConfig::default()
        };
        assert!(bad.validate().is_err());
    }
}

// ===== merged from compaction_async.rs =====
// Async speculative compaction: prepares a background compaction draft when estimated context
// enters `[threshold - lead, threshold)` (`lead = clamp(threshold / 8, 8_192, 32_000)`) and
// commits it immediately upon crossing `threshold` with zero extra LLM latency unless invalidated
// by a branch change, newer compaction, or tail overflow past `keep_recent_tokens`.

use crate::runtime::{AgentRuntime, emit};
use crate::runtime::{
    CompactionCommit, CompactionReason, effective_methods, projected_after_summary_with_preserve,
};
use crate::summary::{
    SummaryPlan, build_handoff_prompt, build_summary_prompt_with_instructions,
    extract_modified_files, extract_todos, has_goal_section, plan_summary_for_tokens,
};
use hrn_kernel::{AgentEvent, CancelFlag, EventSink, NodeId, SessionId, SessionMeta};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static SPEC_GEN: AtomicU64 = AtomicU64::new(1);

/// Shared per-session table of armed or running speculative compactions.
pub(crate) type SpeculativeMap = Arc<Mutex<HashMap<SessionId, SpeculativeSlot>>>;

/// Immutable branch snapshot captured when background speculative compaction starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpeculativeSnapshot {
    pub(crate) session_id: SessionId,
    pub(crate) head_node_id: NodeId,
    pub(crate) branch_len: usize,
    pub(crate) covers_node_id: NodeId,
    pub(crate) tokens_at_snap: u64,
}

/// Completed speculative compaction draft awaiting threshold crossing.
#[derive(Clone, Debug)]
pub(crate) struct SpeculativeDraft {
    pub(crate) snapshot: SpeculativeSnapshot,
    pub(crate) method: String,
    pub(crate) text: String,
    pub(crate) preserve: Option<Value>,
    pub(crate) saved_chars: usize,
}

/// Lifecycle state of one session's speculative compaction entry.
#[derive(Clone, Debug)]
pub(crate) enum SpeculativeState {
    /// Background speculative generation is currently in flight.
    Running {
        snapshot: SpeculativeSnapshot,
        cancel: CancelFlag,
    },
    /// Speculative draft completed and is armed for instant commit upon threshold crossing.
    Ready(SpeculativeDraft),
}

/// Per-session slot stored in [`SpeculativeMap`].
#[derive(Clone, Debug)]
pub(crate) struct SpeculativeSlot {
    pub(crate) generation: u64,
    pub(crate) state: SpeculativeState,
}

impl AgentRuntime {
    /// Cancel and discard any in-flight or armed speculative compaction for `session_id`.
    pub fn invalidate_speculation(&self, session_id: &str) {
        let removed = self
            .speculative
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        if let Some(slot) = removed
            && let SpeculativeState::Running { cancel, .. } = slot.state
        {
            cancel.cancel();
        }
    }

    /// Returns `true` when `session_id` currently holds a ready, armed speculative compaction.
    #[must_use]
    pub fn has_armed_speculation(&self, session_id: &str) -> bool {
        self.speculative
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .is_some_and(|slot| matches!(slot.state, SpeculativeState::Ready(_)))
    }

    /// Spawn a background speculative compaction pass when `effective_tokens` lies in
    /// `[threshold - lead, threshold)` and no valid speculative slot is already running or ready.
    pub(crate) fn maybe_arm_speculative(
        &self,
        meta: &SessionMeta,
        branch: &[DagNode],
        policy: &CompactionPolicy,
        effective_tokens: u64,
        events: &Arc<dyn EventSink>,
    ) {
        let trigger = policy.trigger_tokens();
        if !policy.config.enabled
            || !policy.config.async_enabled
            || !in_speculative_band(effective_tokens, trigger)
        {
            return;
        }
        let Some(head_node_id) = meta.head_node_id.clone() else {
            return;
        };

        // Keep any existing valid running or ready slot; discard stale ones before re-arming.
        {
            let mut map = self
                .speculative
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(slot) = map.get(&meta.id) {
                let keep = match &slot.state {
                    SpeculativeState::Running { snapshot, .. } => {
                        is_snapshot_ancestor(snapshot, branch)
                    }
                    SpeculativeState::Ready(draft) => is_speculative_valid(draft, branch, policy),
                };
                if keep {
                    return;
                }
                if let Some(stale) = map.remove(&meta.id)
                    && let SpeculativeState::Running { cancel, .. } = stale.state
                {
                    cancel.cancel();
                }
            }
        }

        let keep_tok = policy.effective_keep_recent_tokens();
        let Some(plan) = plan_summary_for_tokens(branch, keep_tok, policy.keep_recent_turns, true)
        else {
            return;
        };

        let snapshot = SpeculativeSnapshot {
            session_id: meta.id.clone(),
            head_node_id,
            branch_len: branch.len(),
            covers_node_id: plan.covers_node_id.clone(),
            tokens_at_snap: effective_tokens,
        };
        let generation = SPEC_GEN.fetch_add(1, Ordering::Relaxed);
        let spec_cancel = CancelFlag::new();
        {
            let mut map = self
                .speculative
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.insert(
                meta.id.clone(),
                SpeculativeSlot {
                    generation,
                    state: SpeculativeState::Running {
                        snapshot: snapshot.clone(),
                        cancel: spec_cancel.clone(),
                    },
                },
            );
        }

        let rt = self.clone();
        let meta_snap = meta.clone();
        let branch_snap = branch.to_vec();
        let policy_snap = policy.clone();
        let events_snap = Arc::clone(events);
        let session_id = meta.id.clone();

        if self
            .agents
            .spawn(Box::pin(async move {
                run_speculative_task(
                    rt,
                    meta_snap,
                    branch_snap,
                    policy_snap,
                    plan,
                    snapshot,
                    generation,
                    spec_cancel,
                    events_snap,
                )
                .await;
            }))
            .is_err()
        {
            // No executor attached (e.g. detached runtime in unit tests): clean up silently.
            let mut map = self
                .speculative
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if map
                .get(&session_id)
                .is_some_and(|s| s.generation == generation)
            {
                map.remove(&session_id);
            }
        }
    }

    /// Attempt to immediately commit a ready speculative compaction draft upon crossing threshold.
    ///
    /// Returns `Ok(Some(commit))` with zero extra LLM calls when the armed draft is still valid;
    /// otherwise discards/cancels any stale or still-running slot and returns `Ok(None)` so the
    /// caller falls through to the normal synchronous `compact_branch_walk`.
    pub(crate) async fn try_commit_speculative(
        &self,
        meta: &mut SessionMeta,
        branch: &[DagNode],
        policy: &CompactionPolicy,
        events: &Arc<dyn EventSink>,
    ) -> Result<Option<CompactionCommit>> {
        let slot = self
            .speculative
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&meta.id);
        let Some(slot) = slot else {
            return Ok(None);
        };
        let draft = match slot.state {
            SpeculativeState::Running { cancel, .. } => {
                cancel.cancel();
                return Ok(None);
            }
            SpeculativeState::Ready(draft) => draft,
        };
        if !is_speculative_valid(&draft, branch, policy) {
            return Ok(None);
        }

        let before = project_branch(branch, policy);
        let after = projected_after_summary_with_preserve(
            branch,
            policy,
            &draft.snapshot.covers_node_id,
            &draft.text,
            &draft.method,
            draft.preserve.clone(),
        );
        if after.estimated_tokens >= before.estimated_tokens {
            return Ok(None);
        }

        self.append_payload(
            meta,
            DagPayload::Summary {
                covers_node_id: draft.snapshot.covers_node_id.clone(),
                text: draft.text.clone(),
                method: Some(draft.method.clone()),
                preserve: draft.preserve,
            },
        )?;
        emit(
            events,
            AgentEvent::Compacted {
                covers_node_id: draft.snapshot.covers_node_id.clone(),
                method: draft.method.clone(),
                tokens_before: before.estimated_tokens,
                tokens_after: after.estimated_tokens,
                saved_chars: draft.saved_chars,
            },
        )
        .await;
        Ok(Some(CompactionCommit {
            covers_node_id: draft.snapshot.covers_node_id,
            method: draft.method,
            tokens_before: before.estimated_tokens,
            tokens_after: after.estimated_tokens,
            saved_chars: draft.saved_chars,
            summary_text: draft.text,
            no_progress: false,
        }))
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "background speculative task receives immutable snapshot components"
)]
async fn run_speculative_task(
    rt: AgentRuntime,
    meta: SessionMeta,
    branch: Vec<DagNode>,
    policy: CompactionPolicy,
    plan: SummaryPlan,
    snapshot: SpeculativeSnapshot,
    generation: u64,
    cancel: CancelFlag,
    events: Arc<dyn EventSink>,
) {
    emit(&events, AgentEvent::CompactionSpeculating { active: true }).await;
    let draft = if cancel.is_cancelled() {
        None
    } else {
        generate_speculative_draft(&rt, &meta, &branch, &policy, &plan, &snapshot, &cancel).await
    };

    let still_running_newer = {
        let mut map = rt
            .speculative
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = map.get_mut(&snapshot.session_id) {
            if slot.generation == generation {
                if !cancel.is_cancelled()
                    && let Some(ready) = draft
                {
                    slot.state = SpeculativeState::Ready(ready);
                } else {
                    map.remove(&snapshot.session_id);
                }
                false
            } else {
                matches!(slot.state, SpeculativeState::Running { .. })
            }
        } else {
            false
        }
    };

    if !still_running_newer {
        emit(&events, AgentEvent::CompactionSpeculating { active: false }).await;
    }
}

async fn generate_speculative_draft(
    rt: &AgentRuntime,
    meta: &SessionMeta,
    branch: &[DagNode],
    policy: &CompactionPolicy,
    plan: &SummaryPlan,
    snapshot: &SpeculativeSnapshot,
    cancel: &CancelFlag,
) -> Option<SpeculativeDraft> {
    let before = project_branch(branch, policy);
    let methods = effective_methods(policy, CompactionReason::Threshold);
    for method in methods {
        if cancel.is_cancelled() {
            return None;
        }
        let candidate = match method {
            "remote" => match rt
                .prepare_remote_draft(meta, branch, policy, plan, None, cancel)
                .await
            {
                Ok(Some(d)) => {
                    let saved = d.folded_chars.saturating_sub(d.result.summary_text.len());
                    Some((
                        "remote",
                        d.result.summary_text,
                        Some(d.result.preserve),
                        saved,
                    ))
                }
                _ => None,
            },
            "snapcompact" => match rt.prepare_snap_draft(meta, branch, policy, plan) {
                Ok(Ok(d)) => {
                    let saved = d.folded_chars.saturating_sub(d.summary_text.len());
                    Some(("snapcompact", d.summary_text, Some(d.preserve), saved))
                }
                _ => None,
            },
            "handoff" | "summary" => {
                generate_llm_speculative(rt, meta, branch, policy, plan, method, cancel).await
            }
            // Zero-LLM shake is instantaneous synchronously; skip during background speculation.
            _ => None,
        };
        let Some((name, text, preserve, saved_chars)) = candidate else {
            continue;
        };
        let after = projected_after_summary_with_preserve(
            branch,
            policy,
            &plan.covers_node_id,
            &text,
            name,
            preserve.clone(),
        );
        if after.estimated_tokens < before.estimated_tokens {
            return Some(SpeculativeDraft {
                snapshot: snapshot.clone(),
                method: name.to_owned(),
                text,
                preserve,
                saved_chars,
            });
        }
    }
    None
}

async fn generate_llm_speculative(
    rt: &AgentRuntime,
    meta: &SessionMeta,
    branch: &[DagNode],
    policy: &CompactionPolicy,
    plan: &SummaryPlan,
    method: &'static str,
    cancel: &CancelFlag,
) -> Option<(&'static str, String, Option<Value>, usize)> {
    let folded_nodes = branch.get(..=plan.covers_idx).unwrap_or_default();
    let folded = project_branch(folded_nodes, policy);
    let todos = extract_todos(branch);
    let files = extract_modified_files(folded_nodes);
    let prompt = if method == "handoff" {
        build_handoff_prompt(&folded.messages, &todos, &files, None)
    } else {
        build_summary_prompt_with_instructions(&folded.messages, &todos, &files, None)
    };
    let text = rt.summarize(meta, prompt, cancel).await.ok()?;
    if method == "handoff" && !has_goal_section(&text) {
        return None;
    }
    let folded_chars: usize = folded.messages.iter().map(|m| m.content.len()).sum();
    let saved_chars = folded_chars.saturating_sub(text.len());
    Some((method, text, None, saved_chars))
}

fn is_snapshot_ancestor(snapshot: &SpeculativeSnapshot, branch: &[DagNode]) -> bool {
    let Some(snap_idx) = branch.iter().position(|n| n.id == snapshot.head_node_id) else {
        return false;
    };
    if snap_idx + 1 != snapshot.branch_len {
        return false;
    }
    !branch
        .get(snap_idx + 1..)
        .unwrap_or_default()
        .iter()
        .any(|n| is_full_summary(&n.payload))
}

/// Validate that a ready speculative draft still applies to the live `branch`:
/// 1. `branch` descends from `snapshot.head_node_id` without a branch switch.
/// 2. No newer full `Summary` node was appended after the snapshot.
/// 3. `snapshot.covers_node_id` still lies on the branch.
/// 4. Tail growth strictly after `covers_node_id` does not exceed `keep_recent_tokens`.
pub(crate) fn is_speculative_valid(
    draft: &SpeculativeDraft,
    branch: &[DagNode],
    policy: &CompactionPolicy,
) -> bool {
    if !is_snapshot_ancestor(&draft.snapshot, branch) {
        return false;
    }
    let Some(covers_idx) = branch
        .iter()
        .position(|n| n.id == draft.snapshot.covers_node_id)
    else {
        return false;
    };
    let tail_tokens: u64 = branch
        .get(covers_idx + 1..)
        .unwrap_or_default()
        .iter()
        .filter_map(|n| match &n.payload {
            DagPayload::Message { msg, .. } => Some(estimate_msg_tokens(msg)),
            _ => None,
        })
        .sum();
    tail_tokens <= policy.config.keep_recent_tokens
}

// ===== merged from compaction_shake.rs =====
// Zero-LLM `shake` compaction: spills old large tool outputs to `local://` and elides superseded
// or uneventful old tool results while protecting recent tool outputs, `skill://` reads, plan
// reads, and results below the minimum elision size.

use serde_json::{Map, json};
use std::collections::HashSet;

/// Planned zero-LLM shake overlay ready to be persisted as a `DagPayload::Summary`.
#[derive(Clone, Debug)]
pub(crate) struct ShakePlan {
    /// Latest node id affected by this shake pass.
    pub(crate) covers_node_id: NodeId,
    /// Node-id -> replacement placeholder body.
    pub(crate) elisions: BTreeMap<String, String>,
    /// Tool-call-id -> replacement placeholder body (survives `/fork` node-id remapping).
    pub(crate) call_elisions: BTreeMap<String, String>,
    /// Total characters saved across shaken tool results.
    pub(crate) saved_chars: usize,
    /// Projected tokens before applying the shake overlay.
    pub(crate) tokens_before: u64,
    /// Projected tokens after applying the shake overlay.
    pub(crate) tokens_after: u64,
    /// Concise human-readable description stored in `DagPayload::Summary.text`.
    pub(crate) summary_text: String,
}

impl ShakePlan {
    /// Serialize elision overlays into the `preserve` JSON field of `DagPayload::Summary`.
    #[must_use]
    pub(crate) fn preserve_value(&self) -> Value {
        let mut by_node = Map::with_capacity(self.elisions.len());
        for (k, v) in &self.elisions {
            by_node.insert(k.clone(), Value::String(v.clone()));
        }
        let mut by_call = Map::with_capacity(self.call_elisions.len());
        for (k, v) in &self.call_elisions {
            by_call.insert(k.clone(), Value::String(v.clone()));
        }
        json!({
            "elisions": by_node,
            "call_elisions": by_call,
        })
    }
}

/// Plan a zero-LLM `shake` pass over `branch`.
///
/// - In automatic mode (`aggressive == false`), protects the newest 40,000 tool-output tokens and
///   requires at least 20,000 projected token savings (thresholds loaded from
///   `compaction.defaults.json`).
/// - In explicit `/shake` mode (`aggressive == true`), bypasses the 40k recency window and 20k
///   savings floor while still protecting `skill://` reads, plan reads, and results under 50
///   tokens.
pub(crate) fn plan_shake(
    branch: &[DagNode],
    policy: &CompactionPolicy,
    aggressive: bool,
    spill: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<ShakePlan, String> {
    let defaults = policy_defaults();
    let protect_tokens = if aggressive {
        0
    } else {
        defaults.shake_protect_tokens
    };
    let min_savings = if aggressive {
        1
    } else {
        defaults.shake_min_savings_tokens
    };
    let min_elide = defaults.shake_min_elide_tokens;

    let before = project_branch(branch, policy);
    let start = active_start(branch);
    let region = branch.get(start..).unwrap_or_default();
    let (existing_node_elisions, existing_call_elisions) = collect_shake_elisions(branch);

    let recency_protected = find_recency_protected(
        region,
        protect_tokens,
        &existing_node_elisions,
        &existing_call_elisions,
    );
    let draft = collect_candidates(
        region,
        &recency_protected,
        &existing_node_elisions,
        &existing_call_elisions,
        min_elide,
        before.estimated_tokens,
        spill,
    )?;

    let after = project_trial_shake(branch, policy, &draft);
    let saved_tokens = before
        .estimated_tokens
        .saturating_sub(after.estimated_tokens);
    if saved_tokens < min_savings {
        return Err(format!(
            "shake savings ({saved_tokens} tokens) below minimum threshold ({min_savings})"
        ));
    }

    let count = draft.elisions.len();
    Ok(ShakePlan {
        tokens_before: before.estimated_tokens,
        tokens_after: after.estimated_tokens,
        summary_text: format!(
            "Shook {count} old tool result{} (~{saved_tokens} tokens reclaimed)",
            if count == 1 { "" } else { "s" }
        ),
        ..draft
    })
}

fn collect_candidates(
    region: &[DagNode],
    recency_protected: &HashSet<usize>,
    existing_node_elisions: &HashMap<String, String>,
    existing_call_elisions: &HashMap<String, String>,
    min_elide: u64,
    tokens_before: u64,
    spill: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<ShakePlan, String> {
    let call_meta = index_tool_calls(region);
    let latest_read = index_latest_reads(region);
    let mut elisions = BTreeMap::new();
    let mut call_elisions = BTreeMap::new();
    let mut saved_chars = 0usize;
    let mut last_affected: Option<NodeId> = None;

    for (idx, node) in region.iter().enumerate() {
        let DagPayload::Message { msg, .. } = &node.payload else {
            continue;
        };
        if msg.role != Role::Tool || recency_protected.contains(&idx) {
            continue;
        }
        if is_already_elided(node, msg, existing_node_elisions, existing_call_elisions) {
            continue;
        }
        let est_tokens = estimate_msg_tokens(msg);
        if est_tokens < min_elide {
            continue;
        }
        let call_info = msg
            .tool_call_id
            .as_deref()
            .and_then(|cid| call_meta.get(cid))
            .copied();
        if is_protected_tool_result(msg, call_info) {
            continue;
        }
        let is_superseded = msg
            .read_key
            .as_deref()
            .is_some_and(|key| latest_read.get(key).copied() != Some(idx));
        let replacement = build_placeholder(msg, call_info, est_tokens, is_superseded, spill);
        if replacement.len() >= msg.content.len() && msg.images.is_empty() {
            continue;
        }
        saved_chars =
            saved_chars.saturating_add(msg.content.len().saturating_sub(replacement.len()));
        elisions.insert(node.id.clone(), replacement.clone());
        if let Some(cid) = &msg.tool_call_id {
            call_elisions.insert(cid.clone(), replacement);
        }
        last_affected = Some(node.id.clone());
    }

    let Some(covers_node_id) = last_affected else {
        return Err("no eligible tool outputs to shake".to_owned());
    };
    Ok(ShakePlan {
        covers_node_id,
        elisions,
        call_elisions,
        saved_chars,
        tokens_before,
        tokens_after: tokens_before,
        summary_text: String::new(),
    })
}

fn project_trial_shake(
    branch: &[DagNode],
    policy: &CompactionPolicy,
    draft: &ShakePlan,
) -> crate::compaction::ProjectedContext {
    let mut trial = branch.to_vec();
    trial.push(DagNode {
        id: "__shake_trial__".to_owned(),
        session_id: branch
            .first()
            .map(|n| n.session_id.clone())
            .unwrap_or_default(),
        parent_id: branch.last().map(|n| n.id.clone()),
        created_at_ms: 0,
        payload: DagPayload::Summary {
            covers_node_id: draft.covers_node_id.clone(),
            text: String::new(),
            method: Some("shake".to_owned()),
            preserve: Some(draft.preserve_value()),
        },
    });
    project_branch(&trial, policy)
}

fn index_tool_calls(region: &[DagNode]) -> HashMap<&str, (&str, &str)> {
    let mut map = HashMap::new();
    for node in region {
        if let DagPayload::Message { msg, .. } = &node.payload
            && msg.role == Role::Assistant
        {
            for call in &msg.tool_calls {
                map.insert(
                    call.id.as_str(),
                    (call.name.as_str(), call.arguments.as_str()),
                );
            }
        }
    }
    map
}

fn index_latest_reads(region: &[DagNode]) -> HashMap<&str, usize> {
    let mut latest = HashMap::new();
    for (idx, node) in region.iter().enumerate() {
        if let DagPayload::Message { msg, .. } = &node.payload
            && msg.role == Role::Tool
            && let Some(key) = msg.read_key.as_deref()
        {
            latest.insert(key, idx);
        }
    }
    latest
}

fn find_recency_protected(
    region: &[DagNode],
    protect_tokens: u64,
    node_elisions: &HashMap<String, String>,
    call_elisions: &HashMap<String, String>,
) -> HashSet<usize> {
    let mut protected = HashSet::new();
    if protect_tokens == 0 {
        return protected;
    }
    let mut kept_tokens = 0u64;
    for (idx, node) in region.iter().enumerate().rev() {
        let DagPayload::Message { msg, .. } = &node.payload else {
            continue;
        };
        if msg.role != Role::Tool || is_already_elided(node, msg, node_elisions, call_elisions) {
            continue;
        }
        let tok = estimate_msg_tokens(msg);
        if kept_tokens.saturating_add(tok) <= protect_tokens {
            kept_tokens = kept_tokens.saturating_add(tok);
            protected.insert(idx);
        }
    }
    protected
}

fn is_already_elided(
    node: &DagNode,
    msg: &Msg,
    node_elisions: &HashMap<String, String>,
    call_elisions: &HashMap<String, String>,
) -> bool {
    node_elisions.contains_key(&node.id)
        || msg
            .tool_call_id
            .as_deref()
            .is_some_and(|cid| call_elisions.contains_key(cid))
        || msg.content.starts_with("[Output truncated - ")
        || msg.content == "[Uneventful result elided]"
}

/// Protect `skill://` reads and plan reads from being elided by `shake`.
fn is_protected_tool_result(msg: &Msg, call_info: Option<(&str, &str)>) -> bool {
    if let Some(key) = msg.read_key.as_deref()
        && is_protected_path_or_url(key)
    {
        return true;
    }
    if let Some((_, raw_args)) = call_info
        && let Ok(args) = serde_json::from_str::<Value>(raw_args)
    {
        for field in ["path", "url", "file", "target"] {
            if let Some(val) = args.get(field).and_then(Value::as_str)
                && is_protected_path_or_url(val)
            {
                return true;
            }
        }
    }
    false
}

fn is_protected_path_or_url(raw: &str) -> bool {
    let low = raw.trim().to_ascii_lowercase().replace('\\', "/");
    low.starts_with("skill://")
        || low.starts_with("plan://")
        || low.contains("/.harness/plans/")
        || low.contains("/.omp/plans/")
        || low.contains("/plans/")
        || low.ends_with("/skill.md")
        || low == "skill.md"
        || low.ends_with("/plan.md")
        || low == "plan.md"
}

fn build_placeholder(
    msg: &Msg,
    call_info: Option<(&str, &str)>,
    est_tokens: u64,
    is_superseded: bool,
    spill: &dyn Fn(&str) -> Option<String>,
) -> String {
    let tool_name = msg
        .tool_name
        .as_deref()
        .or_else(|| call_info.map(|(n, _)| n))
        .unwrap_or("");
    let uneventful = is_superseded
        || (!msg.is_error
            && matches!(tool_name, "edit" | "write" | "apply_patch" | "todo")
            && est_tokens < 500);
    if uneventful {
        return "[Uneventful result elided]".to_owned();
    }
    match spill(&msg.content) {
        Some(url) => format!("[Output truncated - {est_tokens} tokens; saved to `{url}`]"),
        None => format!("[Output truncated - {est_tokens} tokens]"),
    }
}

// ===== merged from compaction_snap.rs =====
// `snapcompact` deterministic PNG archive compaction for vision-capable models.
//
// Renders folded transcript pages through the kernel [`SnapRenderer`] port so `hrn-harness` stays
// completely free of image-encoding dependencies. Non-vision models skip cleanly so the compaction
// method walk advances to the next configured method.

use crate::runtime::MethodOutcome;
use hrn_kernel::{SnapRenderer, TodoItem};
use std::fmt::Write as _;

/// Target wrapped line width for deterministic transcript pagination (matches `PngSnapRenderer`).
const PAGE_MAX_COLS: usize = 120;
/// Maximum lines per archived PNG page.
const PAGE_MAX_LINES: usize = 80;
/// Hard ceiling on PNG pages attached per `snapcompact` summary.
const MAX_SNAP_PAGES: usize = 12;

/// Prepared `snapcompact` payload prior to DAG commit (shared by synchronous & speculative paths).
pub(crate) struct SnapDraft {
    pub(crate) summary_text: String,
    pub(crate) preserve: Value,
    pub(crate) folded_chars: usize,
}

impl AgentRuntime {
    /// Clone this runtime with a [`SnapRenderer`] backing the `snapcompact` compaction method.
    #[must_use]
    pub fn with_snap_renderer(&self, renderer: Arc<dyn SnapRenderer>) -> Self {
        let rt = Self {
            snap_renderer: Some(renderer),
            ..self.clone()
        };
        rt.agents.attach_host(&rt);
        rt
    }

    /// Configured [`SnapRenderer`], if attached.
    #[must_use]
    pub fn snap_renderer(&self) -> Option<&Arc<dyn SnapRenderer>> {
        self.snap_renderer.as_ref()
    }

    /// Render the folded slice of `branch` into PNG pages without mutating the DAG.
    ///
    /// Returns `Ok(Err(reason))` when the model lacks vision, no renderer is attached, or the
    /// rendered archive produces no pages.
    pub(crate) fn prepare_snap_draft(
        &self,
        meta: &SessionMeta,
        branch: &[DagNode],
        policy: &CompactionPolicy,
        plan: &SummaryPlan,
    ) -> Result<std::result::Result<SnapDraft, String>> {
        if !self.llm.supports_vision(meta.model.as_deref()) {
            return Ok(Err("model does not support vision".to_owned()));
        }
        let Some(renderer) = self.snap_renderer.as_ref() else {
            return Ok(Err("model does not support vision".to_owned()));
        };
        let folded_nodes = branch.get(..=plan.covers_idx).unwrap_or_default();
        let folded = project_branch(folded_nodes, policy);
        let todos = extract_todos(branch);
        let files = extract_modified_files(folded_nodes);
        let pages = paginate_transcript(&folded.messages, &todos, &files);
        if pages.is_empty() {
            return Ok(Err("no transcript content to snapshot".to_owned()));
        }
        let images: Vec<ImageAttachment> = renderer.render_pages(&pages)?;
        if images.is_empty() {
            return Ok(Err("snap renderer produced no pages".to_owned()));
        }

        let summary_text = format_snap_summary(&plan.covers_node_id, images.len(), &files, &todos);
        let preserve = json!({
            "images": images,
            "page_count": images.len(),
        });
        let folded_chars: usize = folded.messages.iter().map(|m| m.content.len()).sum();
        Ok(Ok(SnapDraft {
            summary_text,
            preserve,
            folded_chars,
        }))
    }

    /// Execute the `"snapcompact"` method in `compact_branch_walk`.
    pub(crate) async fn try_snapcompact_method(
        &self,
        meta: &mut SessionMeta,
        branch: &[DagNode],
        policy: &CompactionPolicy,
        events: &Arc<dyn EventSink>,
    ) -> Result<MethodOutcome> {
        if !self.llm.supports_vision(meta.model.as_deref()) || self.snap_renderer.is_none() {
            return Ok(MethodOutcome::Skipped(
                "model does not support vision".into(),
            ));
        }
        let keep_tok = policy.effective_keep_recent_tokens();
        let Some(plan) = plan_summary_for_tokens(branch, keep_tok, policy.keep_recent_turns, true)
        else {
            return Ok(MethodOutcome::Skipped("not enough history to fold".into()));
        };
        let before = project_branch(branch, policy);
        let draft = match self.prepare_snap_draft(meta, branch, policy, &plan)? {
            Ok(draft) => draft,
            Err(why) => return Ok(MethodOutcome::Skipped(why)),
        };

        let after = projected_after_summary_with_preserve(
            branch,
            policy,
            &plan.covers_node_id,
            &draft.summary_text,
            "snapcompact",
            Some(draft.preserve.clone()),
        );
        if after.estimated_tokens >= before.estimated_tokens {
            return Ok(MethodOutcome::Skipped(format!(
                "snapcompact archive ({} tokens) did not reduce context ({} tokens)",
                after.estimated_tokens, before.estimated_tokens
            )));
        }

        let saved_chars = draft.folded_chars.saturating_sub(draft.summary_text.len());
        self.append_payload(
            meta,
            DagPayload::Summary {
                covers_node_id: plan.covers_node_id.clone(),
                text: draft.summary_text.clone(),
                method: Some("snapcompact".to_owned()),
                preserve: Some(draft.preserve),
            },
        )?;
        emit(
            events,
            AgentEvent::Compacted {
                covers_node_id: plan.covers_node_id.clone(),
                method: "snapcompact".to_owned(),
                tokens_before: before.estimated_tokens,
                tokens_after: after.estimated_tokens,
                saved_chars,
            },
        )
        .await;
        Ok(MethodOutcome::Committed(CompactionCommit {
            covers_node_id: plan.covers_node_id,
            method: "snapcompact".to_owned(),
            tokens_before: before.estimated_tokens,
            tokens_after: after.estimated_tokens,
            saved_chars,
            summary_text: draft.summary_text,
            no_progress: false,
        }))
    }
}

fn format_snap_summary(
    covers_node_id: &str,
    page_count: usize,
    files: &[String],
    todos: &[TodoItem],
) -> String {
    let mut out = format!(
        "Archived {page_count} earlier transcript page{} as PNG snapshots (covers through {covers_node_id}).",
        if page_count == 1 { "" } else { "s" }
    );
    if !files.is_empty() {
        let _ = write!(out, "\nFiles modified: {}", files.join(", "));
    }
    if !todos.is_empty() {
        out.push_str("\nActive todos:");
        for todo in todos {
            let _ = write!(out, "\n- ({}) {}", todo.status.as_str(), todo.content);
        }
    }
    out
}

/// Format and deterministically paginate folded conversation messages into fixed-size pages for
/// [`SnapRenderer::render_pages`].
pub(crate) fn paginate_transcript(
    messages: &[Msg],
    todos: &[TodoItem],
    files: &[String],
) -> Vec<String> {
    let mut wrapped: Vec<String> = Vec::new();
    for msg in messages {
        let (tag, limit) = match msg.role {
            Role::System => ("SYSTEM", 2000),
            Role::User => ("USER", 4000),
            Role::Assistant => ("ASSISTANT", 3000),
            Role::Tool => ("TOOL", 1200),
        };
        if !msg.content.trim().is_empty() || msg.tool_calls.is_empty() {
            let clipped = clip_chars(msg.content.trim(), limit);
            push_wrapped(&mut wrapped, &format!("[{tag}]: {clipped}"), PAGE_MAX_COLS);
        }
        for call in &msg.tool_calls {
            let args = clip_chars(&call.arguments, 400);
            push_wrapped(
                &mut wrapped,
                &format!("[ASSISTANT CALLED]: {}({args})", call.name),
                PAGE_MAX_COLS,
            );
        }
    }
    if !files.is_empty() {
        push_wrapped(
            &mut wrapped,
            &format!("[FILES MODIFIED]: {}", files.join(", ")),
            PAGE_MAX_COLS,
        );
    }
    if !todos.is_empty() {
        wrapped.push("[ACTIVE TODOS]:".to_owned());
        for t in todos {
            push_wrapped(
                &mut wrapped,
                &format!("- ({}) {}", t.status.as_str(), t.content),
                PAGE_MAX_COLS,
            );
        }
    }
    if wrapped.is_empty() {
        return Vec::new();
    }

    let mut pages: Vec<String> = wrapped
        .chunks(PAGE_MAX_LINES)
        .map(|chunk| chunk.join("\n"))
        .collect();
    if pages.len() > MAX_SNAP_PAGES {
        let omitted = pages.len() - MAX_SNAP_PAGES;
        let first = pages.first().cloned().unwrap_or_default();
        let tail_start = pages.len().saturating_sub(MAX_SNAP_PAGES - 1);
        let mut bounded = Vec::with_capacity(MAX_SNAP_PAGES);
        bounded.push(first);
        for (idx, page) in pages.into_iter().skip(tail_start).enumerate() {
            if idx == 0 {
                bounded.push(format!("[... {omitted} earlier pages omitted ...]\n{page}"));
            } else {
                bounded.push(page);
            }
        }
        pages = bounded;
    }
    pages
}

fn push_wrapped(out: &mut Vec<String>, block: &str, max_cols: usize) {
    for raw_line in block.lines() {
        let trimmed = raw_line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let mut buf = String::new();
        let mut count = 0usize;
        for ch in trimmed.chars() {
            buf.push(ch);
            count += 1;
            if count >= max_cols {
                out.push(std::mem::take(&mut buf));
                count = 0;
            }
        }
        if !buf.is_empty() {
            out.push(buf);
        }
    }
}

fn clip_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let cut = text.floor_char_boundary(max_bytes);
    format!("{}...", text.get(..cut).unwrap_or_default())
}

// ===== merged from compaction_remote.rs =====
// Provider-native `remote` compaction method (`LlmProvider::compact_remote`) for
// [`AgentRuntime`]: persists opaque provider replay state inside `DagPayload::Summary.preserve`
// while preserving human-readable `summary_text` fallback for non-Responses providers.

use hrn_kernel::RemoteCompaction;

/// Prepared remote compaction payload prior to DAG commit (shared by synchronous & speculative paths).
pub(crate) struct RemoteDraft {
    pub(crate) result: RemoteCompaction,
    pub(crate) folded_chars: usize,
}

impl AgentRuntime {
    /// Invoke `LlmProvider::compact_remote` over the folded slice of `branch` without mutating the DAG.
    pub(crate) async fn prepare_remote_draft(
        &self,
        meta: &SessionMeta,
        branch: &[DagNode],
        policy: &CompactionPolicy,
        plan: &SummaryPlan,
        instructions: Option<&str>,
        cancel: &CancelFlag,
    ) -> Result<Option<RemoteDraft>> {
        let folded_nodes = branch.get(..=plan.covers_idx).unwrap_or_default();
        let folded = project_branch(folded_nodes, policy);
        let Some(result) = self
            .llm
            .compact_remote(
                meta.model.as_deref(),
                &folded.messages,
                instructions,
                cancel,
            )
            .await?
        else {
            return Ok(None);
        };
        let folded_chars: usize = folded.messages.iter().map(|m| m.content.len()).sum();
        Ok(Some(RemoteDraft {
            result,
            folded_chars,
        }))
    }

    /// Execute the `"remote"` method in `compact_branch_walk`.
    pub(crate) async fn try_remote_method(
        &self,
        meta: &mut SessionMeta,
        branch: &[DagNode],
        policy: &CompactionPolicy,
        instructions: Option<&str>,
        cancel: &CancelFlag,
        events: &Arc<dyn EventSink>,
    ) -> Result<MethodOutcome> {
        let keep_tok = policy.effective_keep_recent_tokens();
        let Some(plan) = plan_summary_for_tokens(branch, keep_tok, policy.keep_recent_turns, true)
        else {
            return Ok(MethodOutcome::Skipped("not enough history to fold".into()));
        };
        let before = project_branch(branch, policy);
        let Some(draft) = self
            .prepare_remote_draft(meta, branch, policy, &plan, instructions, cancel)
            .await?
        else {
            return Ok(MethodOutcome::Skipped(
                "provider does not support native remote compaction".into(),
            ));
        };

        let after = projected_after_summary_with_preserve(
            branch,
            policy,
            &plan.covers_node_id,
            &draft.result.summary_text,
            "remote",
            Some(draft.result.preserve.clone()),
        );
        if after.estimated_tokens >= before.estimated_tokens {
            return Ok(MethodOutcome::Committed(CompactionCommit {
                covers_node_id: plan.covers_node_id,
                method: "remote".to_owned(),
                tokens_before: before.estimated_tokens,
                tokens_after: after.estimated_tokens,
                saved_chars: 0,
                summary_text: draft.result.summary_text,
                no_progress: true,
            }));
        }

        let saved_chars = draft
            .folded_chars
            .saturating_sub(draft.result.summary_text.len());
        self.append_payload(
            meta,
            DagPayload::Summary {
                covers_node_id: plan.covers_node_id.clone(),
                text: draft.result.summary_text.clone(),
                method: Some("remote".to_owned()),
                preserve: Some(draft.result.preserve),
            },
        )?;
        emit(
            events,
            AgentEvent::Compacted {
                covers_node_id: plan.covers_node_id.clone(),
                method: "remote".to_owned(),
                tokens_before: before.estimated_tokens,
                tokens_after: after.estimated_tokens,
                saved_chars,
            },
        )
        .await;
        Ok(MethodOutcome::Committed(CompactionCommit {
            covers_node_id: plan.covers_node_id,
            method: "remote".to_owned(),
            tokens_before: before.estimated_tokens,
            tokens_after: after.estimated_tokens,
            saved_chars,
            summary_text: draft.result.summary_text,
            no_progress: false,
        }))
    }
}
