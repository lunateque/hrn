//! The staged proposals `ast_edit` previews leave behind, plus `resolve` / `reject`.

use hrn_edge_ast::protocol::Rule;
use hrn_kernel::{HrnError, Registry, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// What a preview promised, so apply can rerun and compare.
pub(crate) struct Proposal {
    /// The working directory the preview ran in (absolute, forward slashes).
    pub(crate) cwd: String,
    /// The scope expression the preview covered.
    pub(crate) path: String,
    /// Rewrite rules as given.
    pub(crate) rules: Vec<Rule>,
    /// The language alias override of the preview, if any.
    pub(crate) lang: Option<String>,
    /// Files the preview may touch.
    pub(crate) max_files: usize,
    /// The counts one preview produced; apply must reproduce them.
    pub(crate) expected: Expected,
}

/// The counts one preview produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Expected {
    pub(crate) total: usize,
    pub(crate) files: usize,
    pub(crate) per_file: Vec<(String, usize)>,
}

/// The pending proposals, one per session.
#[derive(Default)]
pub(crate) struct Staged {
    pending: Mutex<HashMap<String, Proposal>>,
}

impl Staged {
    /// Replace any pending proposal of `session`.
    pub(crate) fn stage(&self, session: &str, proposal: Proposal) {
        if let Ok(mut slot) = self.pending.lock() {
            slot.insert(session.to_owned(), proposal);
        }
    }

    /// Take the pending proposal of `session`, leaving nothing behind.
    pub(crate) fn take(&self, session: &str) -> Result<Proposal> {
        self.pending
            .lock()
            .map_err(|_| HrnError::Internal("ast proposal lock poisoned".into()))?
            .remove(session)
            .ok_or_else(|| {
                HrnError::Tool(
                    "nothing is staged: run `ast_edit` first to preview a rewrite".into(),
                )
            })
    }
}

/// The process-wide proposal slot (it outlives one runtime rebuild).
pub(crate) fn shared(registry: &Registry) -> Result<Arc<Staged>> {
    registry
        .shared()
        .get_or_init::<Staged>(|| Ok(Staged::default()))
}
