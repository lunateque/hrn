//! Pure, runtime-agnostic core agent harness engine (`hrn-harness`).
//!
//! Zero async-runtime, network or filesystem dependencies (workspace access goes through the
//! kernel's `WorkspaceFs` port): orchestrates event-sourced DAG sessions, mid-turn `steer` &
//! `followUp` queues, `/fork`, 3-tier non-mutating compaction, parallel `ReadOnly` tool batching,
//! deterministic hooks, slash commands, and subagent delegation.

pub mod advisor;
pub mod agents;
pub mod approvals;
pub mod artifacts;
pub mod attachments;
pub mod attrib;
mod batch;
pub mod branch_ops;
pub mod btw;
mod caller;
pub mod cfg_scheme;
pub mod compaction;
pub mod context;
pub mod control;
mod deferred;
pub mod discovery;
mod effects;
mod eval_support;
pub mod goal;
pub mod hooks;
pub mod isolation;
mod join;
pub mod json_schema;
pub mod nested_context;
pub mod out_url;
mod proc_urls;
pub mod profiles;
mod prompt_build;
pub mod prompt_data;
mod registry;
pub mod rules;
pub mod runtime;
pub mod scope;
mod settle;
pub mod skills;
pub mod ssh_urls;
pub mod subagent;
pub mod summary;
pub mod task_config;
pub mod think_yield;
pub mod thinking;
mod trace;
mod turn;
pub mod wait;

#[cfg(test)]
mod agent_lifecycle_tests;
#[cfg(test)]
mod compaction_native_tests;
#[cfg(test)]
mod compaction_tests;
#[cfg(test)]
mod runtime_restore_subagents_tests;
#[cfg(test)]
mod runtime_restore_tests;
#[cfg(test)]
mod runtime_tests;
#[cfg(test)]
mod runtime_title_tests;
#[cfg(test)]
mod session_ops_tests;
#[cfg(test)]
mod task_parity_tests;
#[cfg(test)]
mod testutil;

pub use advisor::AdvisorConfig;
pub use caller::NestedCaller;

pub use agents::TreeRestoreSummary;
pub use agents::{AGENT_URL_GRAMMAR, AgentScheme, AgentUrl, parse_agent_url};
pub use agents::{AgentActivityEvent, AgentActivityKind};
pub use agents::{AgentRecord, AgentRegistry, AgentSnapshot, AgentUsage, JobStatus, Spawn};
pub use approvals::{ApprovalResolution, GrantCache, UserApprovalPolicy};
pub use artifacts::OrphanArtifact;
pub use artifacts::{ArtifactRecord, ArtifactStore, StoredArtifact};
pub use attachments::{
    AttachmentEntry, AttachmentScheme, AttachmentStore, sanitize_attachment_name,
    validate_attachment_key,
};
pub use attrib::{
    EDIT_TRACE_KIND, EditAttribution, EditTracePayload, FileAttribution, FilteredDiff,
    filter_diff_to_attributed, load_session_attribution, normalize_rel_path, record_edit_trace,
};
pub use branch_ops::{
    LeafSelection, compute_abandoned_nodes, resolve_labels, resolve_leaf_selection,
};
pub use btw::{BtwExchange, BtwStatus, BtwTopic, extract_inject_block};
pub use cfg_scheme::{CfgScheme, SettingView, SettingsPort};
pub use compaction::{
    CompactionConfig, IdleCompactionConfig, MIN_IDLE_TIMEOUT_SECS, SPECULATIVE_LEAD_MAX,
    SPECULATIVE_LEAD_MIN, ThresholdSpec, in_speculative_band, normalize_method_name,
    speculative_lead_tokens,
};
pub use compaction::{CompactionPolicy, ProjectedContext, project_branch};
pub use context::{
    ContextSources, CustomCommand, ParsedSlash, discover_custom_commands, discover_project_context,
    parse_slash,
};
pub use control::SessionControl;
pub use discovery::DiscoverySource;
pub use eval_support::EvalRuntimeParts;
pub use hooks::{LoopGuardHook, SecretRedactionHook, ToolOutputCapHook, builtin_hooks};
pub use isolation::{
    DynamicToolResolver, IsolatedWorktree, IsolationFinalize, IsolationOutcome, WorktreeIsolator,
};
pub use join::join_all;
pub use json_schema::validate_json;
pub use nested_context::NestedContextHook;
pub use out_url::{EventEmitter, MAX_OUT_BYTES, OutScheme};
pub use proc_urls::ProcScheme;
pub use profiles::{available_profile_names, lookup_profile};
pub use prompt_data::INIT_PROMPT;
pub use registry::ToolRegistry;
pub use rules::{Rule, RuleScheme, Rulebook, RulesHook, rule_matches_path, touched_paths};
pub use runtime::{AgentRuntime, HarnessConfig, TurnIo, TurnResult};
pub use runtime::{
    CompactionCommit, CompactionReason, CompactionWalkOutcome, HandoffDraft, MethodOutcome,
};
pub use runtime::{
    INTERRUPTED_ASSISTANT_TEXT, INTERRUPTED_TOOL_RESULT, InterruptDiagnosis, SessionLifecycle,
    diagnose_interrupt, pending_tool_calls, record_fatal_exit, session_lifecycle,
};
pub use scope::{ScopeGuardHook, ScopePolicy, path_matches_pattern};
pub use skills::{
    SkillEntry, discover_skills, filter_skills, format_autoloaded_skills, format_skills_catalog,
    parse_skill_patterns,
};
pub use ssh_urls::{
    MAX_SSH_READ_BYTES, SshFileContent, SshScheme, SshTransport, SshUrl, parse_ssh_url,
    posix_single_quote, split_remote_home,
};
pub use subagent::{
    SubagentProfile, SubagentTool, default_subagent_profiles, format_agents_catalog,
    load_subagent_profiles,
};
pub use summary::{
    COMPACTION_METHODS, HANDOFF_TEMPLATE, SummaryPlan, build_handoff_prompt,
    build_summary_prompt_with_instructions, has_goal_section, plan_summary,
    plan_summary_for_tokens,
};
pub use task_config::{TaskConfig, TaskIsolationConfig};
pub use think_yield::{ThinkTool, YieldTool};
pub use wait::WaitTool;
