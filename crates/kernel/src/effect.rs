//! The session-DAG changes a tool asks the runtime to make: `checkpoint` marks a point in the
//! conversation, `rewind` returns to one. A tool cannot write the DAG itself, so it puts one of
//! these on its [`ToolOutput`] and the step loop applies it after the tool results are stored.

use crate::dag::TodoPhase;
use serde::{Deserialize, Serialize};

/// Where a rewind returns the session's active branch to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "to", content = "id", rename_all = "snake_case")]
pub enum RewindTo {
    /// The marker node labelled so (see [`SessionEffect::Mark`]).
    Mark(String),
    /// The user message that opened this turn.
    TurnStart(String),
}

/// The goal-mode operation the hidden `goal` tool asks the runtime to perform. Only the runtime
/// owns goal state, so the tool names the operation and the runtime applies it to the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalOp {
    /// Start goal mode with `objective` (and optional budgets); requires no goal on the session.
    Create,
    /// Report the current goal and remaining budget.
    Get,
    /// Mark the goal complete: the autonomous loop stops.
    Complete,
    /// Stop continuation while the goal blocks on the user.
    Pause,
    /// Reactivate a paused goal.
    Resume,
    /// Discard the goal and leave goal mode.
    Drop,
    /// Replace `token_budget` (and `time_budget_secs`); `None` clears that budget.
    Budget,
}

/// A change a tool makes to the session's DAG, which only the runtime may write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum SessionEffect {
    /// Append a marker node labelled `label` after this step's tool results.
    Mark {
        /// The marker's label.
        label: String,
    },
    /// Return the active branch to `to` and continue from `report`; the abandoned nodes stay in
    /// the store as a side branch.
    Rewind {
        /// The node to return to.
        to: RewindTo,
        /// What the agent learned, kept as the next message.
        report: String,
    },
    /// Replace the session's todo checklist with `phases` (a successful `todo` mutation), stored
    /// as a DAG node so resume, rewind and fork keep the list with the branch that earned it.
    TodoEdit {
        /// The full phase list after the edit.
        phases: Vec<TodoPhase>,
    },
    /// Change the session's goal-mode state (the hidden `goal` tool).
    Goal {
        /// The operation to apply.
        op: GoalOp,
        /// `op=create`: the objective.
        objective: Option<String>,
        /// `op=create|budget`: total token budget (`None`: unbounded).
        token_budget: Option<u64>,
        /// `op=create|budget`: wall-clock budget in seconds (`None`: unbounded).
        time_budget_secs: Option<u64>,
    },
}
