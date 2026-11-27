//! TUI-facing DTOs for the live Agent Hub (`Alt+A`), pinned Subagents jump block,
//! `/agents` definition hub, and `--restore` startup summary.
//!
//! Keeping these types in `hrn-tui` preserves the star-around-kernel boundary: `hrn-tui` never
//! depends on `hrn-harness`, and `bins/harness/src/chat/hub.rs` maps runtime records into them.

/// Display mode for the pinned Subagents jump block above the editor (`[tui] pinned_agents`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PinnedAgentsMode {
    /// Show a compact summary and up to the top active subagents (default).
    #[default]
    Collapsed,
    /// Show every subagent in the tree with full metadata and live tool previews.
    Full,
    /// Hide the pinned Subagents jump block completely.
    Off,
}

impl PinnedAgentsMode {
    /// Parse a config value (`"collapsed" | "full" | "off"`).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "collapsed" => Some(Self::Collapsed),
            "full" => Some(Self::Full),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// Canonical lowercase label matching config and schema values.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Collapsed => "collapsed",
            Self::Full => "full",
            Self::Off => "off",
        }
    }
}

/// Lifecycle status of one agent row in the live Agent Hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentHubStatus {
    /// Actively running a turn.
    Running,
    /// Finished its latest turn; resident runtime ready for follow-up.
    Idle,
    /// Idle TTL elapsed or resurrected from disk; revivable from stored transcript.
    Parked,
    /// Aborted/killed tombstone; revivable as a fresh generation.
    Aborted,
    /// Terminated with an error.
    Failed,
}

impl AgentHubStatus {
    /// Lower-case status name shown in roster rows and filter matching.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Parked => "parked",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }

    /// Whether pressing `r` or focusing this agent can revive it.
    #[must_use]
    pub const fn can_revive(self) -> bool {
        matches!(self, Self::Idle | Self::Parked | Self::Aborted)
    }
}

/// Category of one entry in the Agent Hub Activity log (`2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentActivityKind {
    /// Tool start or successful completion.
    Tool,
    /// Assistant response summary.
    Response,
    /// Tool failure or turn error.
    Error,
    /// Inter-agent steer, follow-up, broadcast, or progress note.
    Irc,
    /// Lifecycle transition, retry, or fallback status.
    Lifecycle,
}

impl AgentActivityKind {
    /// Compact lowercase tag shown on activity rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Response => "resp",
            Self::Error => "err",
            Self::Irc => "irc",
            Self::Lifecycle => "life",
        }
    }
}

/// One live or persisted agent shown in the Agent Hub roster, inspector, and pinned jump block.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentHubEntry {
    /// Unique agent handle (`main`, `scout`, `lead.worker`, ...).
    pub id: String,
    /// Display or caller-chosen name.
    pub name: String,
    /// Profile/role label (`main`, `scout`, `task`, `reviewer`, ...).
    pub role: String,
    /// Current lifecycle state.
    pub status: AgentHubStatus,
    /// True for the primary root session entry (never parked or killed).
    pub is_main: bool,
    /// Parent agent id for nested subagents (`None` for main or direct children of main).
    pub parent: Option<String>,
    /// Backing transcript session id, when created.
    pub session_id: Option<String>,
    /// Resolved model serving this agent, if known.
    pub model: Option<String>,
    /// Original delegated task summary.
    pub task: String,
    /// Current one-line activity or latest progress note.
    pub activity: String,
    /// Unread response/IRC count since the user last focused this agent.
    pub unread: usize,
    /// Start timestamp in ms since epoch.
    pub started_ms: i64,
    /// Last settle timestamp in ms since epoch (`0` while running).
    pub finished_ms: i64,
    /// Generation number (`>= 1`; incremented on each revive).
    pub generation: u32,
    /// LLM steps taken across generations.
    pub steps: usize,
    /// Total LLM requests issued.
    pub requests: u64,
    /// Total tool calls executed.
    pub tool_calls: u64,
    /// Total prompt/input tokens.
    pub input_tokens: u64,
    /// Total completion/output tokens.
    pub output_tokens: u64,
    /// Total cost in USD (`None` when unpriced/missing).
    pub cost_usd: Option<f64>,
    /// Latest prompt token count (`None` when unknown).
    pub context_tokens: Option<u64>,
    /// Model context window in tokens (`None` when unknown).
    pub context_window: Option<u64>,
    /// Currently running tool name, if any.
    pub current_tool: Option<String>,
    /// Compact preview of active tool arguments, if any.
    pub current_tool_args: Option<String>,
    /// Start timestamp (ms since epoch) of active tool call, for `> 5s` live preview.
    pub current_tool_started_ms: Option<i64>,
    /// Latest intent/progress/response line shown in the inspector.
    pub last_intent: Option<String>,
    /// Active retry or fallback state, if any.
    pub retry_state: Option<String>,
    /// Path to persisted `<id>.md` output artifact, if any.
    pub output_path: Option<String>,
    /// Path to captured `<id>.patch` worktree diff, if any.
    pub patch_path: Option<String>,
    /// Committed isolated git branch (`hrn/task/<id>`), if any.
    pub isolated_branch: Option<String>,
}

/// One timestamped event in the Agent Hub Activity tab (`2`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentActivityItem {
    /// Timestamp in ms since epoch.
    pub ts_ms: i64,
    /// Emitting agent id.
    pub agent_id: String,
    /// Event category.
    pub kind: AgentActivityKind,
    /// Concise single-line summary.
    pub summary: String,
    /// Optional detail snippet (tool args, error excerpt).
    pub detail: Option<String>,
    /// Owning session id for transcript jump.
    pub session_id: Option<String>,
    /// Optional DAG node id for transcript jump.
    pub node_id: Option<String>,
}

/// Full live snapshot of the current root session's agent tree and recent activity log.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentHubSnapshot {
    /// Agents in the current session tree (main + subagents).
    pub agents: Vec<AgentHubEntry>,
    /// Chronological activity log across the tree.
    pub activity: Vec<AgentActivityItem>,
}

/// One discovered subagent profile row shown in the `/agents` definition hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefRow {
    /// Canonical profile name (`scout`, `task`, `reviewer`, custom...).
    pub name: String,
    /// Alternate profile aliases (e.g. `explore` for `scout`).
    pub aliases: Vec<String>,
    /// Discovery origin (`built-in`, `~/.harness`, `.harness`, ...).
    pub source: String,
    /// Concise profile description.
    pub description: String,
    /// Whether this profile is restricted to read-only tools.
    pub read_only: bool,
    /// Whether this profile is currently enabled (not in `[task] disabled_agents`).
    pub enabled: bool,
    /// Default model specified in profile frontmatter/built-in spec, if any.
    pub default_model: Option<String>,
    /// User override configured under `[task.agent_models]`, if any.
    pub model_override: Option<String>,
}

/// Compact subagent lifecycle counts rendered in the status band.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentCounts {
    /// Non-main agents currently running.
    pub running: usize,
    /// Non-main agents currently idle.
    pub idle: usize,
    /// Non-main agents currently parked.
    pub parked: usize,
    /// Total non-main agents in the tree (including aborted/failed).
    pub total: usize,
}

impl AgentCounts {
    /// Tally non-main agent statuses from a hub snapshot.
    #[must_use]
    pub fn from_entries(entries: &[AgentHubEntry]) -> Self {
        let mut counts = Self::default();
        for entry in entries.iter().filter(|e| !e.is_main) {
            counts.total += 1;
            match entry.status {
                AgentHubStatus::Running => counts.running += 1,
                AgentHubStatus::Idle => counts.idle += 1,
                AgentHubStatus::Parked => counts.parked += 1,
                AgentHubStatus::Aborted | AgentHubStatus::Failed => {}
            }
        }
        counts
    }
}

/// Outcome returned by [`crate::Backend::restore_active`] during `hrn chat --restore`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestoreStart {
    /// True when the main session's own interrupted turn started restoring.
    pub main_started: bool,
    /// Number of mid-turn child subagents cold-revived as background jobs first.
    pub subagents_started: usize,
}

impl RestoreStart {
    /// Format the startup flash message for this restore outcome.
    #[must_use]
    pub fn flash_message(self) -> String {
        match (self.main_started, self.subagents_started) {
            (true, n) if n > 0 => {
                format!("restored: going on with the interrupted turn and {n} subagents.")
            }
            (false, n) if n > 0 => {
                format!("restored: {n} subagents going on with their interrupted turns.")
            }
            (true, _) => "restored: going on with the interrupted turn…".to_owned(),
            (false, _) => "restored: nothing was left unfinished".to_owned(),
        }
    }
}
