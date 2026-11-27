//! Core canonical IR, event-sourced DAG session types, port traits and the module `Registry` of
//! `hrn`.
//!
//! `hrn-kernel` intentionally has zero async runtime or I/O dependencies — only `serde` and
//! `serde_json` so every edge, module, and frontend communicates through one stable contract.

mod btw;
mod caller;
mod dag;
mod effect;
mod error;
mod form;
mod goal;
mod help;
mod inbox;
mod ir;
mod paths;
mod ports;
mod proc;
pub mod prompt_budget;
mod registry;
#[cfg(feature = "testing")]
pub mod testing;
mod thinking;
mod tier;
pub mod todo;
mod traits;
mod urls;

pub use btw::{BtwExchange, BtwStatus, BtwTopic, extract_inject_block};
pub use caller::ToolCaller;
pub use dag::{
    AgentEvent, AgentStatus, ApprovalRequest, AskRequest, DEFAULT_PHASE, DagNode, DagPayload,
    ExitReason, NodeId, PendingToolCall, PermissionMode, SessionCommand, SessionId, SessionMeta,
    TodoItem, TodoPhase, TodoStatus, TurnId,
};
pub use effect::{GoalOp, RewindTo, SessionEffect};
pub use error::{CancelFlag, Cancelled, HrnError, Result};
pub use form::{Answer, Choice, Field, Form, Item, Kind, Spec, Submission, TextKind};
pub use goal::{Goal, GoalStatus};
pub use help::{HelpPages, HelpScheme};
pub use inbox::{Changed, Expectation, Inbox, InboxItem, format_inbox};
pub use ir::{
    FinishReason, ImageAttachment, LlmChunk, LlmRequest, LlmResponse, ModelInfo, ModelPrice, Msg,
    ProviderCapabilities, RemoteCompaction, Role, ThinkingBlock, ToolCall, ToolEffect, ToolOutput,
    ToolSpec, Usage, UsageRow,
};
pub use paths::{SourceLayer, resource_dirs};
pub use ports::{ArtifactStore, EvalHost, SnapRenderer};
pub use proc::{ProcEntry, ProcSource, ProcTable};
pub use registry::{Command, CommandFn, CommandOutcome, Host, Registered, Registry, Shared};
pub use thinking::{LevelSuffix, ThinkingBudgets, ThinkingLevel, split_level_suffix};
pub use tier::{
    ApprovalDecision, ApprovalGrant, ApprovalGrantScope, Tier, TitleSource, ToolApproval,
    ToolApprovalPolicy, ToolTier, TraceRecord,
};
pub use traits::{
    ApprovalGate, AskHandler, BoxFuture, ChunkSink, Clock, DeferredTool, EventSink, HarnessHook,
    HookEvent, HookVerdict, LlmProvider, PathKind, ScopedFuture, SessionStore, SystemClock, Tool,
    ToolContext, WorkspaceFs,
};
pub use urls::{UrlBody, UrlRouter, UrlScheme};

/// The wire-value crates behind the tool and config contracts, re-exported so module crates (which
/// may not depend on external crates) use exactly the versions the kernel's signatures use. A
/// module derives with `#[serde(crate = "hrn_kernel::serde")]`.
pub use serde;
pub use serde_json;
