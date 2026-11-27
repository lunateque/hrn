//! Test doubles shared by the harness unit tests (in-memory fs, store, scripted LLM, executor).

use hrn_kernel::{
    AgentEvent, ApprovalGrant, BoxFuture, ChunkSink, DagNode, EventSink, HrnError, LlmChunk,
    LlmProvider, LlmRequest, LlmResponse, ModelInfo, PathKind, Result, ScopedFuture, SessionMeta,
    SessionStore, TitleSource, TraceRecord, Usage, UsageRow, WorkspaceFs,
};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Drive a future that never needs a real reactor (all test doubles resolve immediately).
pub(crate) fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    for _ in 0..100_000 {
        if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
    panic!("future did not complete: a test double is pending forever");
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A [`ToolContext`](hrn_kernel::ToolContext) rooted at `/ws` for hook unit tests.
pub(crate) fn hook_ctx() -> hrn_kernel::ToolContext {
    hrn_kernel::ToolContext {
        session_id: "test-session".into(),
        turn_id: "t".into(),
        call_id: "c".into(),
        cwd: "/ws".into(),
        permission_mode: hrn_kernel::PermissionMode::Yolo,
        cancel: hrn_kernel::CancelFlag::new(),
        events: std::sync::Arc::new(VecEvents::default()),
        ask: None,
        approval: None,
        urls: std::sync::Arc::new(hrn_kernel::UrlRouter::default()),
        root_session_id: "test-session".into(),
        inbox: std::sync::Arc::new(hrn_kernel::Inbox::default()),
        caller: None,
        todos: std::sync::Arc::new(hrn_kernel::todo::TodoBoards::new()),
        yield_data: std::sync::Arc::new(std::sync::Mutex::new(None)),
        is_subagent: false,
    }
}

/// In-memory [`WorkspaceFs`]; directories are implied by file paths.
#[derive(Default)]
pub(crate) struct MemFs {
    files: BTreeMap<PathBuf, String>,
    home: Option<PathBuf>,
}

impl MemFs {
    pub(crate) fn with(mut self, path: &str, body: &str) -> Self {
        self.files.insert(PathBuf::from(path), body.to_owned());
        self
    }

    pub(crate) fn with_home(mut self, path: &str) -> Self {
        self.home = Some(PathBuf::from(path));
        self
    }
}

impl WorkspaceFs for MemFs {
    fn read_to_string(&self, path: &Path) -> Result<Option<String>> {
        Ok(self.files.get(path).cloned())
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let mut entries = std::collections::BTreeSet::new();
        for file in self.files.keys() {
            if let Ok(rel) = file.strip_prefix(path)
                && let Some(first) = rel.components().next()
            {
                entries.insert(path.join(first));
            }
        }
        Ok(entries.into_iter().collect())
    }

    fn kind(&self, path: &Path) -> PathKind {
        if self.files.contains_key(path) {
            PathKind::File
        } else if self.files.keys().any(|p| p.starts_with(path)) {
            PathKind::Dir
        } else {
            PathKind::Missing
        }
    }

    fn home_dir(&self) -> Option<PathBuf> {
        self.home.clone()
    }
}

/// In-memory [`SessionStore`] with the same parent/head semantics as the `SQLite` edge.
#[derive(Default)]
pub(crate) struct MemStore {
    sessions: Mutex<BTreeMap<String, SessionMeta>>,
    nodes: Mutex<Vec<DagNode>>,
    usage: Mutex<Vec<(String, Usage)>>,
    btw_topics: Mutex<BTreeMap<(String, String), (String, i64)>>,
    grants: Mutex<BTreeMap<String, ApprovalGrant>>,
    traces: Mutex<Vec<TraceRecord>>,
}

impl SessionStore for MemStore {
    fn create_session(&self, meta: &SessionMeta) -> Result {
        lock(&self.sessions).insert(meta.id.clone(), meta.clone());
        Ok(())
    }

    fn update_session(&self, meta: &SessionMeta) -> Result {
        lock(&self.sessions).insert(meta.id.clone(), meta.clone());
        Ok(())
    }

    fn get_session(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        Ok(lock(&self.sessions).get(session_id).cloned())
    }

    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMeta>> {
        // As in the real store: pinned first, then the most recently updated.
        let mut all: Vec<SessionMeta> = lock(&self.sessions).values().cloned().collect();
        all.sort_by_key(|m| {
            (
                std::cmp::Reverse(m.pinned),
                std::cmp::Reverse(m.updated_at_ms),
            )
        });
        all.truncate(limit);
        Ok(all)
    }

    fn delete_session(&self, session_id: &str) -> Result<bool> {
        // As in the real store: a session's subagent sessions go with it, nodes with them.
        let mut sessions = lock(&self.sessions);
        let mut doomed: Vec<String> = vec![session_id.to_owned()];
        loop {
            let before = doomed.len();
            for id in doomed.clone() {
                doomed.extend(
                    sessions
                        .values()
                        .filter(|m| m.parent_session_id.as_deref() == Some(id.as_str()))
                        .map(|m| m.id.clone()),
                );
                doomed.dedup();
            }
            if doomed.len() == before {
                break;
            }
        }
        let removed = doomed
            .iter()
            .filter(|id| sessions.remove(id.as_str()).is_some())
            .count();
        drop(sessions);
        lock(&self.nodes).retain(|n| !doomed.contains(&n.session_id));
        Ok(removed > 0)
    }

    fn rename_session(&self, session_id: &str, title: &str, source: TitleSource) -> Result {
        match lock(&self.sessions).get_mut(session_id) {
            Some(meta) => {
                if source == TitleSource::Auto && meta.title_source == Some(TitleSource::User) {
                    return Ok(());
                }
                title.clone_into(&mut meta.title);
                meta.title_source = Some(source);
                Ok(())
            }
            None => Err(HrnError::Store(format!("unknown session `{session_id}`"))),
        }
    }

    fn list_recent_for(&self, cwd: &str, limit: usize) -> Result<Vec<SessionMeta>> {
        let mut all: Vec<SessionMeta> = lock(&self.sessions)
            .values()
            .filter(|m| m.parent_session_id.is_none() && m.cwd == cwd)
            .cloned()
            .collect();
        all.sort_by_key(|m| std::cmp::Reverse(m.updated_at_ms));
        all.truncate(limit);
        Ok(all)
    }

    fn list_children(&self, parent_session_id: &str) -> Result<Vec<SessionMeta>> {
        let mut all: Vec<SessionMeta> = lock(&self.sessions)
            .values()
            .filter(|m| m.parent_session_id.as_deref() == Some(parent_session_id))
            .cloned()
            .collect();
        all.sort_by_key(|m| m.created_at_ms);
        Ok(all)
    }

    fn put_btw_topic(
        &self,
        session_id: &str,
        topic_id: &str,
        json: &str,
        updated_at_ms: i64,
    ) -> Result {
        lock(&self.btw_topics).insert(
            (session_id.to_owned(), topic_id.to_owned()),
            (json.to_owned(), updated_at_ms),
        );
        Ok(())
    }

    fn get_btw_topics(&self, session_id: &str) -> Result<Vec<(String, String, i64)>> {
        let mut items: Vec<(String, String, i64)> = lock(&self.btw_topics)
            .iter()
            .filter(|((sid, _), _)| sid == session_id)
            .map(|((_, tid), (json, ts))| (tid.clone(), json.clone(), *ts))
            .collect();
        items.sort_by_key(|(_, _, ts)| std::cmp::Reverse(*ts));
        Ok(items)
    }

    fn put_approval_grant(&self, grant: &ApprovalGrant) -> Result {
        lock(&self.grants).insert(grant.key.clone(), grant.clone());
        Ok(())
    }

    fn list_approval_grants(&self) -> Result<Vec<ApprovalGrant>> {
        Ok(lock(&self.grants).values().cloned().collect())
    }

    fn clear_approval_grants(&self) -> Result {
        lock(&self.grants).clear();
        Ok(())
    }

    fn record_trace(&self, trace: &TraceRecord) -> Result {
        lock(&self.traces).push(trace.clone());
        Ok(())
    }

    fn list_traces(&self, session_id: &str) -> Result<Vec<TraceRecord>> {
        Ok(lock(&self.traces)
            .iter()
            .filter(|t| t.session_id == session_id)
            .cloned()
            .collect())
    }

    fn vacuum_and_checkpoint(&self) -> Result {
        Ok(())
    }

    fn set_pinned(&self, session_id: &str, pinned: bool) -> Result {
        match lock(&self.sessions).get_mut(session_id) {
            Some(meta) => {
                meta.pinned = pinned;
                Ok(())
            }
            None => Err(HrnError::Store(format!("unknown session `{session_id}`"))),
        }
    }

    fn pinned_sessions(&self) -> Result<Vec<String>> {
        Ok(lock(&self.sessions)
            .values()
            .filter(|m| m.pinned)
            .map(|m| m.id.clone())
            .collect())
    }

    fn append_node(&self, node: &DagNode) -> Result {
        lock(&self.nodes).push(node.clone());
        if let Some(meta) = lock(&self.sessions).get_mut(&node.session_id) {
            meta.head_node_id = Some(node.id.clone());
        }
        Ok(())
    }

    fn load_branch(&self, session_id: &str, head_node_id: &str) -> Result<Vec<DagNode>> {
        let nodes = lock(&self.nodes);
        let mut out = Vec::new();
        let mut cur = Some(head_node_id.to_owned());
        while let Some(id) = cur {
            let node = nodes
                .iter()
                .find(|n| n.session_id == session_id && n.id == id)
                .ok_or_else(|| HrnError::Store(format!("missing node {id}")))?;
            cur = node.parent_id.clone();
            out.push(node.clone());
        }
        out.reverse();
        Ok(out)
    }

    fn load_tree(&self, session_id: &str) -> Result<Vec<DagNode>> {
        Ok(lock(&self.nodes)
            .iter()
            .filter(|n| n.session_id == session_id)
            .cloned()
            .collect())
    }

    fn record_usage(&self, session_id: &str, _at_ms: i64, _model: &str, usage: &Usage) -> Result {
        lock(&self.usage).push((session_id.to_owned(), *usage));
        Ok(())
    }

    fn session_usage(&self, session_id: &str) -> Result<Usage> {
        let mut total = Usage::default();
        for (_, usage) in lock(&self.usage).iter().filter(|(id, _)| id == session_id) {
            total.accumulate(*usage);
        }
        Ok(total)
    }

    fn usage_report(&self) -> Result<Vec<UsageRow>> {
        Err(HrnError::Store(
            "the in-memory test store keeps no usage calendar".into(),
        ))
    }

    fn session_usage_report(&self, _session_id: &str) -> Result<Vec<UsageRow>> {
        Err(HrnError::Store(
            "the in-memory test store keeps no usage calendar".into(),
        ))
    }
}

/// Collects emitted events.
#[derive(Default)]
pub(crate) struct VecEvents(pub(crate) Mutex<Vec<AgentEvent>>);

impl EventSink for VecEvents {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        lock(&self.0).push(event);
        Box::pin(async { true })
    }
}

/// LLM returning queued responses and recording the requests it saw.
#[derive(Default)]
pub(crate) struct ScriptedLlm {
    pub(crate) responses: Mutex<VecDeque<Result<LlmResponse>>>,
    pub(crate) requests: Mutex<Vec<LlmRequest>>,
    /// What `models()` reports (empty by default).
    pub(crate) models: Vec<ModelInfo>,
}

impl LlmProvider for ScriptedLlm {
    fn stream_chat<'a>(
        &'a self,
        request: &'a LlmRequest,
        _cancel: &'a hrn_kernel::CancelFlag,
        sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>> {
        lock(&self.requests).push(request.clone());
        let next = lock(&self.responses).pop_front().unwrap_or_else(|| {
            Err(HrnError::Provider {
                message: "script exhausted".into(),
                retryable: false,
                retry_after_secs: None,
            })
        });
        Box::pin(async move {
            if let Ok(resp) = &next
                && !resp.content.is_empty()
            {
                sink.on_chunk(LlmChunk::ContentDelta(resp.content.clone()))
                    .await;
            }
            next
        })
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
}
