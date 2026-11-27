//! Plain-text transcript rendering for `/dump [file]`, sharing [`super::Header`] and
//! [`super::blocks`] with markdown/html `/export`.

use super::{Block, Header, load_session_blocks};
use hrn_kernel::{Result, SessionStore};
use std::fmt::Write as _;

/// Render a session's active branch as plain text (no markdown fences or HTML tags).
pub(super) fn plain_text(head: &Header, blocks: &[Block]) -> String {
    let mut o = String::new();
    let title = head.meta.title.trim();
    if !title.is_empty() {
        let _ = writeln!(o, "{title}");
        let _ = writeln!(o, "{}", "=".repeat(title.chars().count().min(72)));
    }
    for (label, value) in head.rows() {
        let _ = writeln!(o, "{label}: {value}");
    }
    o.push('\n');
    for block in blocks {
        match block {
            Block::User(text) => {
                let _ = write!(o, "[User]\n{}\n\n", text.trim());
            }
            Block::Assistant(text) => {
                let _ = write!(o, "[Assistant]\n{}\n\n", text.trim());
            }
            Block::Reasoning(text) => {
                let _ = write!(o, "[Thinking]\n{}\n\n", text.trim());
            }
            Block::Call { name, args } => {
                let _ = write!(o, "[Tool call: {name}]\n{}\n\n", args.trim_end());
            }
            Block::Output {
                name,
                text,
                is_error,
            } => {
                let flag = if *is_error { " (error)" } else { "" };
                let _ = write!(o, "[Tool result: {name}{flag}]\n{}\n\n", text.trim_end());
            }
            Block::Note(text) => {
                let _ = write!(o, "[Note]\n{}\n\n", text.trim_end());
            }
        }
    }
    o
}

/// Load and render session `id` as a plain-text transcript for `/dump`.
///
/// # Errors
/// The session is unknown, has no messages yet, or cannot be read from `store`.
pub(crate) fn render_plain_text(store: &dyn SessionStore, id: &str) -> Result<String> {
    let (head, blocks) = load_session_blocks(store, id)?;
    Ok(plain_text(&head, &blocks))
}

#[cfg(test)]
mod tests {
    use super::render_plain_text;
    use hrn_edge_store::SqliteStore;
    use hrn_kernel::{
        AgentStatus, DagNode, DagPayload, Msg, PermissionMode, SessionMeta, SessionStore,
        ThinkingLevel, ToolCall,
    };

    #[test]
    fn dump_plain_text() {
        let store = SqliteStore::open_in_memory().expect("in-memory store");
        let mut meta = SessionMeta {
            id: "s-dump-1".into(),
            title: "Plain dump demo".into(),
            cwd: "/tmp/work".into(),
            branch: None,
            head_node_id: Some("n3".into()),
            model: Some("fast".into()),
            thinking: ThinkingLevel::Off,
            goal: None,
            permission_mode: PermissionMode::Default,
            parent_session_id: None,
            parent_node_id: None,
            status: AgentStatus::Idle,
            title_source: None,
            exit_ms: None,
            pinned: false,
            created_at_ms: 100,
            updated_at_ms: 200,
        };
        store.create_session(&meta).expect("create session");
        let u = DagNode {
            id: "n1".into(),
            session_id: meta.id.clone(),
            parent_id: None,
            created_at_ms: 101,
            payload: DagPayload::Message {
                turn_id: "t1".into(),
                msg: Msg::user("Check Cargo.toml"),
            },
        };
        store.append_node(&u).expect("user");
        let mut assistant = Msg::assistant("Reading it now.");
        assistant.reasoning = Some("Inspect workspace manifest first.".into());
        assistant.tool_calls.push(ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: r#"{"path":"Cargo.toml"}"#.into(),
            signature: None,
        });
        let a = DagNode {
            id: "n2".into(),
            session_id: meta.id.clone(),
            parent_id: Some(u.id),
            created_at_ms: 102,
            payload: DagPayload::Message {
                turn_id: "t1".into(),
                msg: assistant,
            },
        };
        store.append_node(&a).expect("assistant");
        let t = DagNode {
            id: "n3".into(),
            session_id: meta.id.clone(),
            parent_id: Some(a.id),
            created_at_ms: 103,
            payload: DagPayload::Message {
                turn_id: "t1".into(),
                msg: Msg::tool_result("c1", "read", "[workspace]"),
            },
        };
        store.append_node(&t).expect("tool");
        meta.head_node_id = Some(t.id);
        store.update_session(&meta).expect("update head");

        let dumped = render_plain_text(&store, &meta.id).expect("plain text dump");
        assert!(dumped.contains("Plain dump demo"), "{dumped}");
        assert!(dumped.contains("Model: fast"), "{dumped}");
        assert!(dumped.contains("[User]\nCheck Cargo.toml"), "{dumped}");
        assert!(
            dumped.contains("[Thinking]\nInspect workspace manifest first."),
            "{dumped}"
        );
        assert!(dumped.contains("[Assistant]\nReading it now."), "{dumped}");
        assert!(dumped.contains("[Tool call: read]"), "{dumped}");
        assert!(
            dumped.contains("[Tool result: read]\n[workspace]"),
            "{dumped}"
        );
        assert!(
            !dumped.contains("```"),
            "plain text must not use markdown fences: {dumped}"
        );
        assert!(!dumped.contains("<!doctype html>"), "{dumped}");
    }
}
