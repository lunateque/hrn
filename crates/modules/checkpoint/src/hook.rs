//! The automatic snapshot before a turn's first editing tool call.

use crate::Shared;
use hrn_edge_tools::Kind;
use hrn_edge_tools::args::blocking;
use hrn_kernel::{AgentEvent, AgentStatus, BoxFuture, HarnessHook, Result, ToolContext};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Snapshots the working tree once per turn, just before the first call of an editing tool.
pub(crate) struct AutoSnapshot {
    shared: Arc<Shared>,
    /// The last turn snapshotted, keyed `session/turn` (turn ids are unique, so entries retire
    /// one turn after use).
    done: Mutex<HashSet<String>>,
}

impl AutoSnapshot {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            done: Mutex::new(HashSet::new()),
        }
    }

    /// Whether this is the first editing call of the turn (and records it).
    fn first_in_turn(&self, ctx: &ToolContext) -> bool {
        let mut done = self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if done.len() > 4_096 {
            done.clear();
        }
        done.insert(format!("{}/{}", ctx.session_id, ctx.turn_id))
    }
}

impl HarnessHook for AutoSnapshot {
    fn before_mutating_tool(&self, ctx: &ToolContext, tool_name: &str) -> BoxFuture<Result> {
        let editing = self.shared.config.auto
            && self.shared.config.tools.iter().any(|t| t == tool_name)
            && self.first_in_turn(ctx);
        let shared = Arc::clone(&self.shared);
        let ctx = ctx.clone();
        let tool_name = tool_name.to_owned();
        Box::pin(async move {
            if !editing {
                return Ok(());
            }
            let cwd = ctx.cwd.clone();
            let (session, turn) = (ctx.session_id.clone(), ctx.turn_id.clone());
            let label = format!("before `{tool_name}`");
            let taken = blocking("checkpoint", move || {
                let _busy = shared.lock();
                shared
                    .writer(&cwd)?
                    .snapshot(Kind::Auto, &label, Some(&session), Some(&turn))
            })
            .await;
            // A tree that cannot be snapshotted must not stop the edit the user asked for, but it
            // must not pass silently either.
            if let Err(err) = taken {
                ctx.events
                    .emit(AgentEvent::StatusChanged {
                        session_id: ctx.session_id.clone(),
                        status: AgentStatus::Working,
                        message: Some(format!("automatic checkpoint failed: {err}")),
                    })
                    .await;
            }
            Ok(())
        })
    }
}
