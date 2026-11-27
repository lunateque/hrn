//! `conflict://`: merge-conflict hunks of the files this session read (the engine is
//! `hrn_edge_tools::conflict_read` / `conflict_write`).

use hrn_edge_tools::{FileReadTracker, conflict_read, conflict_write};
use hrn_kernel::{HrnError, Result, ToolContext, UrlBody, UrlScheme};
use std::sync::Arc;

/// The conflict views and resolutions, anchored on the shared read tracker.
pub(crate) struct ConflictScheme {
    tracker: Arc<FileReadTracker>,
}

impl ConflictScheme {
    pub(crate) fn new(tracker: Arc<FileReadTracker>) -> Self {
        Self { tracker }
    }
}

impl UrlScheme for ConflictScheme {
    fn scheme(&self) -> &'static str {
        "conflict"
    }

    fn about(&self) -> &'static str {
        "conflict://<N>[/ours|/theirs|/base]: one merge conflict of a file you read \
         (`path:conflicts` lists the ids); writing it resolves the hunk"
    }

    /// The hunk with the file's own line numbers (`rest` keeps its trailing selector).
    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        conflict_read(&self.tracker, &ctx.cwd, rest)
            .map(|out| UrlBody::Text(out.content))
            .map_err(|e| match e {
                HrnError::Tool(msg) => HrnError::Tool(msg),
                other => other,
            })
    }

    fn write(&self, ctx: &ToolContext, rest: &str, content: &str) -> Result {
        conflict_write(&self.tracker, &ctx.cwd, rest, content).map(|_| ())
    }
}
