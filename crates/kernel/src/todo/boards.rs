//! The live checklists of one runtime, keyed by root session id. The `todo` tool and the runtime
//! hold the same instance (through the process-wide [`crate::Shared`] table), so a tool call
//! always mutates the list the front end shows and a `/todo` edit reaches the model's next call.

use crate::dag::TodoPhase;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

/// The table of live checklists.
#[derive(Default)]
pub struct TodoBoards {
    slots: Mutex<HashMap<String, Vec<TodoPhase>>>,
}

impl TodoBoards {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The checklist of `root`, or an empty one when nothing has seeded it yet.
    #[must_use]
    pub fn phases(&self, root: &str) -> Vec<TodoPhase> {
        lock(&self.slots).get(root).cloned().unwrap_or_default()
    }

    /// Seed the checklist of `root` at turn start (a session always has one once a turn ran).
    pub fn seed(&self, root: &str, phases: Vec<TodoPhase>) {
        lock(&self.slots).insert(root.to_owned(), phases);
    }

    /// Update the checklist of `root`; a root nobody seeded has no list to update.
    pub fn set(&self, root: &str, phases: Vec<TodoPhase>) {
        if let Some(slot) = lock(&self.slots).get_mut(root) {
            *slot = phases;
        }
    }
}

fn lock<T>(cell: &Mutex<T>) -> MutexGuard<'_, T> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
