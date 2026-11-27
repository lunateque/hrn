//! `history://<session-id>`: a compact transcript of a stored session's active branch.
//! Bare `history://` lists live agents (if wired), on-disk artifacts, and stored sessions.

use hrn_kernel::{
    DagPayload, HrnError, Msg, Result, Role, SessionStore, ToolContext, UrlBody, UrlScheme,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Characters kept of a user or assistant message.
const TEXT_CHARS: usize = 4000;
/// Characters kept of a tool result (the transcript is an index, not a replay).
const TOOL_CHARS: usize = 400;
/// Characters kept of one tool call's arguments.
const ARGS_CHARS: usize = 200;
/// Sessions named by bare `history://`.
const LIST_LIMIT: usize = 50;

/// One live agent entry surfaced by bare `history://`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryAgentEntry {
    /// Agent handle (`<id>` or `<parent>.<child>`).
    pub id: String,
    /// Lifecycle status label (`running`, `idle`, `parked`, `aborted`, `failed`).
    pub status: String,
    /// Short summary or task line.
    pub summary: String,
    /// Backing child session ID, if created.
    pub session_id: Option<String>,
}

/// Callback returning live agents belonging to a root session ID.
pub type LiveAgentsFn = Arc<dyn Fn(&str) -> Vec<HistoryAgentEntry> + Send + Sync>;

/// Resolves `history://` against a [`SessionStore`], optional live agent registry callback, and
/// optional on-disk `<data>/sessions/<root>/artifacts/` directory.
pub struct HistoryScheme {
    store: Arc<dyn SessionStore>,
    live_agents: Option<LiveAgentsFn>,
    data_dir: Option<PathBuf>,
}

impl HistoryScheme {
    /// Read transcripts from `store`.
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            live_agents: None,
            data_dir: None,
        }
    }

    /// Wire live agents for bare `history://` listings.
    #[must_use]
    pub fn with_live_agents(mut self, live_agents: LiveAgentsFn) -> Self {
        self.live_agents = Some(live_agents);
        self
    }

    /// Wire the per-user data directory (`<data>/sessions/<root>/artifacts/`) so bare `history://`
    /// also discovers on-disk agent artifacts.
    #[must_use]
    pub fn with_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.data_dir = Some(data_dir);
        self
    }

    #[cfg(test)]
    fn list(&self) -> Result<String> {
        self.list_for("")
    }

    fn list_for(&self, root_session_id: &str) -> Result<String> {
        let live = self
            .live_agents
            .as_ref()
            .map(|f| f(root_session_id))
            .unwrap_or_default();
        let artifacts = self
            .data_dir
            .as_deref()
            .map(|dir| scan_disk_artifacts(dir, root_session_id))
            .unwrap_or_default();
        let sessions = self.store.list_sessions(LIST_LIMIT)?;

        if live.is_empty() && artifacts.is_empty() && sessions.is_empty() {
            return Ok("(no stored sessions)".to_owned());
        }
        if live.is_empty() && artifacts.is_empty() {
            let mut out = String::new();
            for s in sessions {
                let _ = writeln!(out, "{}  {}  ({})", s.id, clip(&s.title, 80), s.cwd);
            }
            return Ok(out);
        }

        let mut out = String::new();
        if !live.is_empty() {
            out.push_str("## live agents\n");
            for a in &live {
                let _ = write!(
                    out,
                    "agent://{}  [{}]  {}",
                    a.id,
                    a.status,
                    clip(&a.summary, 80)
                );
                if let Some(sid) = &a.session_id {
                    let _ = write!(out, "  (history://{sid})");
                }
                out.push('\n');
            }
        }
        if !artifacts.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("## artifacts\n");
            for (root, id, exts) in &artifacts {
                let _ = writeln!(out, "agent://{id}  ({root})  [{}]", exts.join(","));
            }
        }
        if !sessions.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("## sessions\n");
            for s in sessions {
                let _ = writeln!(out, "{}  {}  ({})", s.id, clip(&s.title, 80), s.cwd);
            }
        }
        Ok(out)
    }

    fn transcript(&self, id: &str) -> Result<String> {
        let meta = self
            .store
            .get_session(id)?
            .ok_or_else(|| HrnError::Tool(format!("no stored session `{id}`")))?;
        let mut out = format!(
            "# {}\nsession {} · cwd {} · model {}\n",
            clip(&meta.title, 120),
            meta.id,
            meta.cwd,
            meta.model.as_deref().unwrap_or("default")
        );
        let Some(head) = meta.head_node_id.as_deref() else {
            out.push_str("\n(empty session)\n");
            return Ok(out);
        };
        for node in self.store.load_branch(id, head)? {
            match node.payload {
                DagPayload::Message { msg, .. } => message(&mut out, &msg),
                DagPayload::Summary { text, .. } => {
                    let _ = write!(out, "\n## summary\n{}\n", clip(&text, TEXT_CHARS));
                }
                DagPayload::ForkMarker { label } => {
                    let _ = write!(out, "\n## fork: {label}\n");
                }
                DagPayload::ToolsActivated { names } => {
                    let _ = write!(out, "\n## tools activated: {}\n", names.join(", "));
                }
                DagPayload::TodoBoard { phases } => {
                    let _ = write!(
                        out,
                        "\n## todo list\n{}",
                        hrn_kernel::todo::to_markdown(&phases)
                    );
                }
                DagPayload::BranchSummary { text, .. } => {
                    let _ = write!(out, "\n## branch summary\n{}\n", clip(&text, TEXT_CHARS));
                }
                DagPayload::Label {
                    target_node_id,
                    label: Some(label),
                } => {
                    let _ = write!(out, "\n## label {target_node_id}: {label}\n");
                }
                DagPayload::ModelChange { selector, .. } => {
                    let _ = write!(out, "\n## model: {selector}\n");
                }
                DagPayload::Label { label: None, .. } | DagPayload::SessionExit { .. } => {}
            }
        }
        Ok(out)
    }
}

/// Scan `<data_dir>/sessions/*/artifacts/` (prioritizing `current_root` first) for `.md`, `.json`,
/// and `.patch` agent artifact files.
fn scan_disk_artifacts(
    data_dir: &Path,
    current_root: &str,
) -> Vec<(String, String, Vec<&'static str>)> {
    let sessions_dir = data_dir.join("sessions");
    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        return Vec::new();
    };
    let mut roots: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with('.') {
            roots.push((name.to_owned(), path.join("artifacts")));
        }
    }
    let sanitized_current = sanitize_root_segment(current_root);
    roots.sort_by(|(a, _), (b, _)| {
        let a_cur = !current_root.is_empty() && *a == sanitized_current;
        let b_cur = !current_root.is_empty() && *b == sanitized_current;
        b_cur.cmp(&a_cur).then_with(|| a.cmp(b))
    });

    let mut out = Vec::new();
    for (root_name, art_dir) in roots {
        let Ok(files) = fs::read_dir(&art_dir) else {
            continue;
        };
        let mut by_id: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
        for file in files.flatten() {
            let p = file.path();
            if !p.is_file() {
                continue;
            }
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let Some((stem, ext)) = split_artifact_ext(name) else {
                continue;
            };
            by_id.entry(stem.to_owned()).or_default().push(ext);
        }
        for (id, mut exts) in by_id {
            exts.sort_unstable();
            out.push((root_name.clone(), id, exts));
        }
    }
    out
}

fn sanitize_root_segment(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn split_artifact_ext(name: &str) -> Option<(&str, &'static str)> {
    if let Some(stem) = name.strip_suffix(".md") {
        Some((stem, "md"))
    } else if let Some(stem) = name.strip_suffix(".json") {
        Some((stem, "json"))
    } else if let Some(stem) = name.strip_suffix(".patch") {
        Some((stem, "patch"))
    } else {
        None
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!(
            "{} … [{} more chars]",
            text.get(..cut).unwrap_or_default(),
            text.chars().count() - max_chars
        ),
        None => text.to_owned(),
    }
}

fn message(out: &mut String, msg: &Msg) {
    match msg.role {
        Role::Tool => {
            let _ = write!(
                out,
                "\n## tool {}{}\n{}\n",
                msg.tool_name.as_deref().unwrap_or("?"),
                if msg.is_error { " (error)" } else { "" },
                clip(&msg.content, TOOL_CHARS)
            );
        }
        role => {
            let label = match role {
                Role::System => "system",
                Role::User => "user",
                _ => "assistant",
            };
            let _ = write!(out, "\n## {label}\n");
            if !msg.content.is_empty() {
                let _ = writeln!(out, "{}", clip(&msg.content, TEXT_CHARS));
            }
            for call in &msg.tool_calls {
                let _ = writeln!(
                    out,
                    "-> {}({})",
                    call.name,
                    clip(&call.arguments, ARGS_CHARS)
                );
            }
        }
    }
}

impl UrlScheme for HistoryScheme {
    fn scheme(&self) -> &'static str {
        "history"
    }

    fn about(&self) -> &'static str {
        "read-only transcripts of stored sessions: `history://<session-id>` (compact, tool \
         results shortened), bare `history://` lists live agents, on-disk artifacts, and stored \
         sessions"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let id = rest.trim_matches('/');
        if id.is_empty() {
            return self.list_for(&ctx.root_session_id).map(UrlBody::Text);
        }
        self.transcript(id).map(UrlBody::Text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStore;
    use hrn_kernel::{
        AgentEvent, AgentStatus, BoxFuture, CancelFlag, DagNode, EventSink, Inbox, PermissionMode,
        SessionMeta, ThinkingLevel, ToolCall, UrlRouter,
    };

    struct NoopSink;
    impl EventSink for NoopSink {
        fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
            Box::pin(async { true })
        }
    }

    fn test_ctx(root: &str) -> ToolContext {
        ToolContext {
            session_id: root.into(),
            turn_id: "t".into(),
            call_id: "c".into(),
            cwd: PathBuf::from("/w"),
            permission_mode: PermissionMode::Yolo,
            cancel: CancelFlag::new(),
            events: Arc::new(NoopSink),
            ask: None,
            approval: None,
            urls: Arc::new(UrlRouter::new()),
            root_session_id: root.into(),
            inbox: Arc::new(Inbox::default()),
            caller: None,
            todos: Arc::new(hrn_kernel::todo::TodoBoards::new()),
            yield_data: Arc::default(),
            is_subagent: false,
        }
    }

    fn meta() -> SessionMeta {
        SessionMeta {
            pinned: false,
            title_source: None,
            exit_ms: None,
            id: "s1".into(),
            title: "demo".into(),
            cwd: "/w".into(),
            branch: None,
            head_node_id: None,
            model: None,
            thinking: ThinkingLevel::default(),
            goal: None,
            permission_mode: PermissionMode::Default,
            parent_session_id: None,
            parent_node_id: None,
            status: AgentStatus::Idle,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn node(id: &str, parent: Option<&str>, msg: Msg) -> DagNode {
        DagNode {
            id: id.into(),
            session_id: "s1".into(),
            parent_id: parent.map(Into::into),
            created_at_ms: 1,
            payload: DagPayload::Message {
                turn_id: "t".into(),
                msg,
            },
        }
    }

    #[test]
    fn transcript_lists_roles_calls_and_shortened_results() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.create_session(&meta()).unwrap();
        let call = ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
            signature: None,
        };
        store
            .append_node(&node("n1", None, Msg::user("hello")))
            .unwrap();
        store
            .append_node(&node(
                "n2",
                Some("n1"),
                Msg::assistant_tool_calls("looking", None, vec![call]),
            ))
            .unwrap();
        store
            .append_node(&node(
                "n3",
                Some("n2"),
                Msg::tool_result("c1", "read_file", "x".repeat(1000)),
            ))
            .unwrap();
        let scheme = HistoryScheme::new(store);
        let text = scheme.transcript("s1").unwrap();
        assert!(text.contains("## user\nhello"), "{text}");
        assert!(text.contains("-> read_file({\"path\":\"a.rs\"})"), "{text}");
        assert!(text.contains("## tool read_file\n"), "{text}");
        assert!(text.contains("[600 more chars]"), "{text}");
        assert!(scheme.list().unwrap().contains("s1  demo"));
        assert!(scheme.transcript("nope").is_err());
    }

    #[test]
    fn history_bare_lists_agents() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.create_session(&meta()).unwrap();
        let data_dir =
            std::env::temp_dir().join(format!("hrn-history-bare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&data_dir);
        let art_dir = data_dir.join("sessions").join("s1").join("artifacts");
        fs::create_dir_all(&art_dir).unwrap();
        fs::write(art_dir.join("lead.worker.md"), "# nested output").unwrap();
        fs::write(art_dir.join("lead.worker.json"), r#"{"ok":true}"#).unwrap();

        let scheme = HistoryScheme::new(store)
            .with_data_dir(data_dir.clone())
            .with_live_agents(Arc::new(|root| {
                assert_eq!(root, "s1");
                vec![HistoryAgentEntry {
                    id: "alpha".into(),
                    status: "idle".into(),
                    summary: "[explore] inspect repo".into(),
                    session_id: Some("sess_child_1".into()),
                }]
            }));
        let UrlBody::Text(body) = scheme.read(&test_ctx("s1"), "").unwrap() else {
            panic!("expected text body");
        };
        assert!(body.contains("## live agents"), "{body}");
        assert!(body.contains("agent://alpha  [idle]"), "{body}");
        assert!(body.contains("history://sess_child_1"), "{body}");
        assert!(body.contains("## artifacts"), "{body}");
        assert!(
            body.contains("agent://lead.worker  (s1)  [json,md]"),
            "{body}"
        );
        assert!(body.contains("## sessions"), "{body}");
        assert!(body.contains("s1  demo"), "{body}");

        let _ = fs::remove_dir_all(&data_dir);
    }
}
