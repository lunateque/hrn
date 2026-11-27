//! Approval tiers, service tiers, permission modes, grants, title provenance, and trace records.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Upstream provider service tier requested on an LLM call (`[llm.tier]` / `/fast`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Omit `service_tier` on the wire.
    #[default]
    None,
    /// Provider default automatic routing.
    Auto,
    /// Standard service tier.
    Default,
    /// Lower-cost flexible latency tier.
    Flex,
    /// Dedicated/scale tier.
    Scale,
    /// Priority / fast lane.
    Priority,
    /// Ultra-fast lane (`/fast ultra`).
    Ultrafast,
}

impl Tier {
    /// Canonical `snake_case` spelling used in config, flags, and wire encoders.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Auto => "auto",
            Self::Default => "default",
            Self::Flex => "flex",
            Self::Scale => "scale",
            Self::Priority => "priority",
            Self::Ultrafast => "ultrafast",
        }
    }

    /// Parse a service tier spelling (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "auto" => Some(Self::Auto),
            "default" => Some(Self::Default),
            "flex" => Some(Self::Flex),
            "scale" => Some(Self::Scale),
            "priority" | "fast" => Some(Self::Priority),
            "ultrafast" | "ultra" => Some(Self::Ultrafast),
            _ => None,
        }
    }
}

/// Per-agent tool permission policy mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Read-only inspection only; all mutating tools are blocked.
    Plan,
    /// Read-only tools auto; writes and exec prompt for approval (omp `always-ask`).
    #[default]
    Default,
    /// Prompts reads, writes, and execs with rememberable file/dir/session grants
    /// (omp `always-ask-cached`).
    AskCached,
    /// File edits auto; exec tools still require approval (omp `write`).
    AutoEdit,
    /// All tools auto (intended for isolated git worktrees; safety overrides remain).
    Yolo,
}

impl PermissionMode {
    /// Short display label for status bars and persistence.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Default => "default",
            Self::AskCached => "always-ask-cached",
            Self::AutoEdit => "auto-edit",
            Self::Yolo => "yolo",
        }
    }

    /// Cycle to the next permission mode (keeps the original 4-step cycle for existing callers).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Plan => Self::Default,
            Self::Default | Self::AskCached => Self::AutoEdit,
            Self::AutoEdit => Self::Yolo,
            Self::Yolo => Self::Plan,
        }
    }

    /// Parse a permission mode string, accepting both hrn and omp spellings.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "plan" => Some(Self::Plan),
            "default" | "always-ask" | "always_ask" => Some(Self::Default),
            "always-ask-cached" | "always_ask_cached" | "ask-cached" | "ask_cached" => {
                Some(Self::AskCached)
            }
            "write" | "auto-edit" | "auto_edit" | "accept-edits" => Some(Self::AutoEdit),
            "yolo" | "auto" => Some(Self::Yolo),
            _ => None,
        }
    }
}

/// Risk tier declared by a tool call for the approval ladder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTier {
    /// Read-only inspection (auto under `always-ask`, `write`, `yolo`).
    Read,
    /// Workspace file edits/writes (auto under `write` and `yolo`).
    Write,
    /// Shell execution, external mutations, or unclassified calls (auto only under `yolo`).
    #[default]
    Exec,
}

impl ToolTier {
    /// Canonical lowercase tier name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exec => "exec",
        }
    }

    /// Parse a stored or configured tier name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "exec" => Some(Self::Exec),
            _ => None,
        }
    }

    /// Tier dominance: `Write` covers `Read|Write`; `Read` only covers `Read`; `Exec` covers `Exec`.
    #[must_use]
    pub const fn covers(self, needed: Self) -> bool {
        matches!(
            (self, needed),
            (Self::Read, Self::Read)
                | (Self::Write, Self::Read | Self::Write)
                | (Self::Exec, Self::Exec)
        )
    }

    /// Stronger of two tiers (`Exec` > `Write` > `Read`).
    #[must_use]
    pub const fn stronger(self, other: Self) -> Self {
        match (self, other) {
            (Self::Exec, _) | (_, Self::Exec) => Self::Exec,
            (Self::Write, _) | (_, Self::Write) => Self::Write,
            (Self::Read, Self::Read) => Self::Read,
        }
    }
}

/// Explicit per-call or per-tool approval override policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalPolicy {
    /// Allow without prompting (subject to higher-precedence safety overrides).
    Allow,
    /// Refuse unconditionally.
    Deny,
    /// Always prompt interactively.
    Prompt,
}

/// Per-call approval descriptor returned by [`crate::Tool::approval`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolApproval {
    /// Baseline risk tier of this invocation.
    pub tier: ToolTier,
    /// Optional tool-level override (`Allow`, `Deny`, `Prompt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ToolApprovalPolicy>,
    /// Optional human-readable reason shown in the approval dock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Safety override: forces an interactive prompt even when `mode` would auto-approve.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force_prompt: bool,
}

impl ToolApproval {
    /// Pure read-only tool call.
    #[must_use]
    pub const fn read() -> Self {
        Self {
            tier: ToolTier::Read,
            policy: None,
            reason: None,
            force_prompt: false,
        }
    }

    /// File write/edit tool call.
    #[must_use]
    pub const fn write() -> Self {
        Self {
            tier: ToolTier::Write,
            policy: None,
            reason: None,
            force_prompt: false,
        }
    }

    /// Execution or external side-effect tool call.
    #[must_use]
    pub const fn exec() -> Self {
        Self {
            tier: ToolTier::Exec,
            policy: None,
            reason: None,
            force_prompt: false,
        }
    }
}

/// Scope of an interactive approval decision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalGrantScope {
    /// Approve only this single tool call.
    #[default]
    Once,
    /// Approve matching calls on the target file for this session.
    File,
    /// Approve matching calls on direct children of the target directory for this session.
    Directory,
    /// Approve all calls to this tool for this session.
    ToolSession,
    /// Remember this grant across sessions in persistent storage.
    Persistent,
}

/// User decision returned by [`crate::ApprovalGate::await_decision`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Approved with the chosen grant scope.
    Approve {
        /// Lifetime and granularity of the grant.
        scope: ApprovalGrantScope,
    },
    /// Refused by the user.
    Deny,
    /// Refused by the user along with an inline steering message from the composer.
    DenyAndSend {
        /// Steering text to deliver immediately.
        message: String,
    },
}

impl ApprovalDecision {
    /// Approve this single invocation.
    #[must_use]
    pub const fn once() -> Self {
        Self::Approve {
            scope: ApprovalGrantScope::Once,
        }
    }

    /// Approve this tool for the rest of the session.
    #[must_use]
    pub const fn tool_session() -> Self {
        Self::Approve {
            scope: ApprovalGrantScope::ToolSession,
        }
    }

    /// Convert a legacy boolean answer into `Approve(Once)` or `Deny`.
    #[must_use]
    pub const fn from_bool(approved: bool) -> Self {
        if approved { Self::once() } else { Self::Deny }
    }

    /// Whether the user approved the call.
    #[must_use]
    pub const fn is_approved(&self) -> bool {
        matches!(self, Self::Approve { .. })
    }
}

/// Provenance of a session's title (`auto` never overwrites `user`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// Model-generated automatic title.
    Auto,
    /// Explicit user-supplied title (`/rename`, CLI rename, imported custom title).
    User,
}

impl TitleSource {
    /// Stored text representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::User => "user",
        }
    }

    /// Parse stored text representation.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

/// One persisted cross-session approval grant row (`approval_grants` table).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalGrant {
    /// Normalized target key (canonicalized path, URL, or tool name).
    pub key: String,
    /// Granted tier (`Write` covers `Read`).
    pub tier: ToolTier,
    /// Grant scope (`file`, `directory`, `tool`).
    pub scope: String,
    /// Creation timestamp (Unix ms).
    pub created_ms: i64,
}

/// One persisted execution span or edit attribution row (`traces` table).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRecord {
    /// Owning session identifier.
    pub session_id: String,
    /// Optional associated DAG node identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Span kind (`llm`, `tool`, `edit`).
    pub kind: String,
    /// Span start timestamp (Unix ms).
    pub started_ms: i64,
    /// Span duration in milliseconds.
    pub dur_ms: i64,
    /// Structured JSON metadata for `/trace` or `/commit` attribution.
    pub payload: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_mode_parses_hrn_and_omp_aliases() {
        assert_eq!(
            PermissionMode::parse("always-ask"),
            Some(PermissionMode::Default)
        );
        assert_eq!(
            PermissionMode::parse("always-ask-cached"),
            Some(PermissionMode::AskCached)
        );
        assert_eq!(
            PermissionMode::parse("write"),
            Some(PermissionMode::AutoEdit)
        );
        assert_eq!(PermissionMode::parse("yolo"), Some(PermissionMode::Yolo));
        assert_eq!(PermissionMode::parse("plan"), Some(PermissionMode::Plan));
    }

    #[test]
    fn service_tier_roundtrips() {
        for tier in [
            Tier::None,
            Tier::Auto,
            Tier::Default,
            Tier::Flex,
            Tier::Scale,
            Tier::Priority,
            Tier::Ultrafast,
        ] {
            assert_eq!(Tier::parse(tier.as_str()), Some(tier));
        }
    }
}
