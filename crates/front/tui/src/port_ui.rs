//! Rich catalog, role, and interactive settings view types shared between `front/tui` and
//! backend implementations (`/model`, `/switch`, `/settings`).

use crate::port_hub::PinnedAgentsMode;
use crate::port_info::{DoubleEscapeAction, Notify};
use crate::port_theme::AppearanceConfig;
use hrn_kernel::{ModelPrice, PermissionMode, ThinkingLevel};

/// One configured model preset with display metadata for `/model` and `/switch`.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    /// Configured alias / selector (e.g. `local-gw/example/model-flash`).
    pub alias: String,
    /// Upstream model identifier.
    pub id: String,
    /// Short human-friendly display name (never an unwieldy `provider/org/repo/name` dump).
    pub name: String,
    /// Provider name (e.g. `anthropic`, `local-gw`).
    pub provider: String,
    /// Context window in tokens (`0` if unknown).
    pub context_window: u64,
    /// Optional extended context window ceiling in tokens.
    pub max_context_window: Option<u64>,
    /// Whether the model supports reasoning / thinking effort.
    pub reasoning: bool,
    /// Preset default thinking level, if configured.
    pub thinking: Option<ThinkingLevel>,
    /// Optional USD-per-million-tokens pricing.
    pub price: Option<ModelPrice>,
    /// Roles currently pointing at this model (e.g. `["default", "plan"]`).
    pub roles: Vec<String>,
    /// Optional custom badge `(name, color)` from `modelTags` in the agent config.
    pub tag: Option<(String, Option<String>)>,
}

impl ModelInfo {
    /// Derive a clean short display name from an alias when explicit metadata is absent.
    #[must_use]
    pub fn short_name_from_alias(alias: &str) -> String {
        let base = hrn_kernel::split_level_suffix(alias.trim()).0;
        base.rsplit('/').next().unwrap_or(base).to_owned()
    }

    /// Derive a provider name from an alias prefix (`provider/...`), or `"default"`.
    #[must_use]
    pub fn provider_from_alias(alias: &str) -> String {
        let base = hrn_kernel::split_level_suffix(alias.trim()).0;
        base.split_once('/')
            .map_or_else(|| "default".to_owned(), |(prov, _)| prov.to_owned())
    }
}

/// One role assignment row shown in the `/model` Roles tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleAssignment {
    /// Role name (`default`, `smol`, `plan`, or a user-defined alias role).
    pub role: String,
    /// Assigned target selector, optionally with `:level` suffix.
    pub target: String,
    /// True for built-in semantic roles (`default`, `smol`, `slow`, `vision`, `plan`, …).
    pub is_standard: bool,
}

/// Value editor kind for one row of `/settings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    /// Boolean toggle (`true` / `false`).
    Bool,
    /// Fixed choice list cycled with `←/→` or `Enter`.
    Enum,
    /// Free-form string value.
    String,
    /// Signed/unsigned integer value.
    Integer,
    /// Floating-point numeric value.
    Float,
    /// List of values (edited as comma-separated entries).
    Array,
    /// Structured inline TOML table.
    Table,
}

impl SettingKind {
    /// Human-readable type label shown in the `/settings` inspector.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Enum => "enum",
            Self::String => "string",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Array => "array",
            Self::Table => "table",
        }
    }
}

/// One live setting entry shown and edited in `/settings`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingItem {
    /// Dotted configuration key (e.g. `tui.theme`, `compaction.threshold_percent`).
    pub key: String,
    /// Schema section / tab tag (e.g. `tui`, `models`, `compaction`, `tools`).
    pub tab: String,
    /// Editor kind determining toggle / cycle / text-input behavior.
    pub kind: SettingKind,
    /// Effective formatted value (secrets masked as `********`).
    pub value: String,
    /// Schema default formatted for display (`"(unset)"` if none).
    pub default: String,
    /// True when the effective value differs from the default or comes from a non-default layer.
    pub modified: bool,
    /// Winning layer label (`default`, `global config`, `project config`, `environment variable (…)`).
    pub source: String,
    /// Allowed enum choices when `kind == SettingKind::Enum`.
    pub values: Vec<String>,
    /// Optional `HRN_*` environment variable override name.
    pub env: Option<String>,
    /// True when the value holds a secret and must be masked in display.
    pub secret: bool,
    /// Human-readable description shown in the inspector.
    pub description: String,
}

/// Live UI state deltas returned after saving or resetting a setting in `/settings`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingUpdate {
    /// Concise status toast shown inside `/settings`.
    pub message: String,
    /// Updated appearance config (theme, symbols, statusline, vim) to apply live.
    pub appearance: Option<AppearanceConfig>,
    /// Updated default thinking effort, if changed.
    pub thinking: Option<ThinkingLevel>,
    /// Updated default permission mode, if changed.
    pub approval_mode: Option<PermissionMode>,
    /// Updated double-Esc action, if changed.
    pub double_escape_action: Option<DoubleEscapeAction>,
    /// Updated pinned subagents display mode, if changed.
    pub pinned_agents: Option<PinnedAgentsMode>,
    /// Updated subagent live preview flag, if changed.
    pub subagent_live_preview: Option<bool>,
    /// Updated mouse capture flag, if changed.
    pub mouse: Option<bool>,
    /// Updated branch summary flag, if changed.
    pub branch_summary: Option<bool>,
    /// Updated `/btw` inject flag, if changed.
    pub btw_inject: Option<bool>,
    /// Updated desktop notification config, if changed.
    pub notify: Option<Notify>,
}

/// Snapshot of an active `/remote` pairing overlay (LAN/Tailscale direct PWA or E2E relay).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSessionView {
    /// Human-readable permission label (`"control"` or `"view-only"`).
    pub mode_label: String,
    /// Human-readable transport label (`"LAN / Tailscale"` or `"E2E Relay"`).
    pub transport_label: String,
    /// Primary pairing or E2E room URL encoded in the QR code.
    pub url: String,
    /// Half-block Unicode QR rows (`▀`, `▄`, `█`, ` `) ready to render dark-on-light.
    pub qr_lines: Vec<String>,
    /// Number of currently paired or connected remote devices.
    pub connected: usize,
    /// Whether this QR grants interactive control (`true`) or view-only supervision (`false`).
    pub can_control: bool,
    /// Optional fallback LAN/Tailscale URLs for multi-homed hosts.
    pub alt_urls: Vec<String>,
}

/// Outcome returned by [`crate::Backend::remote_command`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteCommandOutcome {
    /// Open or refresh the interactive QR pairing overlay.
    Open(RemoteSessionView),
    /// Report a one-line status notice in the transcript without opening the QR overlay.
    Notice(String),
}
