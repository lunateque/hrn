//! Test support for crates that exercise a [`Tool`](crate::Tool): a self-cleaning temp directory
//! and a no-op [`ToolContext`]. Behind the `testing` feature; depend on it from
//! `[dev-dependencies]` only.

use crate::{AgentEvent, BoxFuture, CancelFlag, EventSink, PermissionMode, ToolContext};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    /// Create the directory; `tag` shows up in its name.
    ///
    /// # Panics
    ///
    /// When the directory cannot be created.
    #[expect(
        clippy::expect_used,
        reason = "test support: failing loudly is the point"
    )]
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hrn-test-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Write `rel` (creating parents) and return its absolute path.
    ///
    /// # Panics
    ///
    /// When the file cannot be written.
    #[expect(
        clippy::expect_used,
        reason = "test support: failing loudly is the point"
    )]
    pub fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&path, content).expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a leftover temp dir must not fail the test that used it.
        let _removed = fs::remove_dir_all(&self.0);
    }
}

struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

/// A [`ToolContext`] rooted at `cwd`: yolo permissions, no approval or ask channel, events
/// discarded.
pub fn tool_context(cwd: PathBuf) -> ToolContext {
    ToolContext {
        session_id: "test-session".to_owned(),
        turn_id: "test-turn".to_owned(),
        call_id: "test-call".to_owned(),
        cwd,
        permission_mode: PermissionMode::Yolo,
        cancel: CancelFlag::new(),
        events: Arc::new(NullSink),
        ask: None,
        approval: None,
        urls: Arc::default(),
        root_session_id: "test-session".to_owned(),
        inbox: Arc::default(),
        caller: None,
        todos: Arc::new(crate::todo::TodoBoards::new()),
        yield_data: Arc::default(),
        is_subagent: false,
    }
}
