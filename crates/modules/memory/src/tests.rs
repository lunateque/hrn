//! End-to-end tests of the module's public surface over a real `SQLite` file.

use crate::{MemoryParts, memory_parts};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, EventSink, Msg, PermissionMode, Tool, ToolContext,
    ToolOutput, UrlBody,
};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

/// A scratch directory holding the database and two fake repositories.
struct Fixture {
    root: PathBuf,
    parts: MemoryParts,
}

impl Fixture {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "hrn-mem-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for repo in ["a", "b"] {
            fs::create_dir_all(root.join(repo).join(".git")).expect("repo");
        }
        let parts = memory_parts(root.join("data").join("memory.db"), &root.join("a"));
        Self { root, parts }
    }

    fn db(&self) -> PathBuf {
        self.root.join("data").join("memory.db")
    }

    fn ctx(&self, repo: &str) -> ToolContext {
        ToolContext {
            session_id: "s1".to_owned(),
            turn_id: "t".to_owned(),
            call_id: "c".to_owned(),
            cwd: self.root.join(repo),
            permission_mode: PermissionMode::Yolo,
            cancel: CancelFlag::new(),
            events: Arc::new(NullSink),
            ask: None,
            approval: None,
            urls: Arc::default(),
            root_session_id: "s1".to_owned(),
            inbox: Arc::default(),
            caller: None,
            todos: Arc::new(hrn_kernel::todo::TodoBoards::new()),
            yield_data: Arc::default(),
            is_subagent: false,
        }
    }

    fn tool(&self, name: &str) -> Arc<dyn Tool> {
        let found = self.parts.tools.iter().find(|t| t.short_name == name);
        Arc::clone(&found.expect("tool").tool)
    }

    async fn call(&self, repo: &str, name: &str, args: Value) -> ToolOutput {
        self.tool(name)
            .execute(&self.ctx(repo), args)
            .await
            .expect("tool call")
    }

    fn read(&self, repo: &str, rest: &str) -> hrn_kernel::Result<String> {
        match self.parts.scheme.read(&self.ctx(repo), rest)? {
            UrlBody::Text(text) => Ok(text),
            UrlBody::Path(path) => panic!("unexpected path body {}", path.display()),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The id out of `… stored (ids: abc123).`
fn stored_id(out: &ToolOutput) -> String {
    let (_, tail) = out.content.rsplit_once(": ").expect("id in output");
    tail.chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect::<String>()
}

#[tokio::test]
async fn project_memories_stay_in_their_project_and_global_ones_reach_all() {
    let fx = Fixture::new();
    let local = fx
        .call(
            "a",
            "retain",
            json!({ "items": [
                { "content": "Deploys of the zephyr service go through the blue pipeline" },
                { "content": "The user prefers terse commit messages", "scope": "global" },
            ]}),
        )
        .await;
    assert!(!local.is_error, "{}", local.content);

    let in_a = fx
        .call(
            "a",
            "recall",
            json!({ "query": "how do zephyr deploys work?" }),
        )
        .await;
    assert!(in_a.content.contains("blue pipeline"), "{}", in_a.content);
    assert!(
        in_a.content.contains("[retain/project]"),
        "{}",
        in_a.content
    );

    let in_b = fx
        .call("b", "recall", json!({ "query": "zephyr deploys" }))
        .await;
    assert_eq!(in_b.content, "No relevant memories found.");
    let global_in_b = fx
        .call("b", "recall", json!({ "query": "commit messages" }))
        .await;
    assert!(
        global_in_b.content.contains("terse commit"),
        "{}",
        global_in_b.content
    );
    assert!(
        global_in_b.content.contains("[retain/global]"),
        "{}",
        global_in_b.content
    );
}

#[tokio::test]
async fn identical_memories_are_stored_once_and_secrets_are_masked() {
    let fx = Fixture::new();
    let item = json!({ "items": [{ "content": "the staging password=hunter2hunter2 is rotated monthly" }] });
    let first = fx.call("a", "retain", item.clone()).await;
    let again = fx.call("a", "retain", item).await;
    assert_eq!(stored_id(&first), stored_id(&again));
    let full = fx.read("a", &stored_id(&first)).expect("read");
    assert!(full.contains("password=[redacted]"), "{full}");
    assert!(!full.contains("hunter2"), "{full}");
}

#[tokio::test]
async fn edits_update_the_index_and_invalidated_memories_are_not_recalled() {
    let fx = Fixture::new();
    let a = stored_id(
        &fx.call(
            "a",
            "retain",
            json!({ "items": [{ "content": "build with cargo build" }] }),
        )
        .await,
    );
    let b = stored_id(
        &fx.call(
            "a",
            "learn",
            json!({ "memory": "the quokka cache must be cleared before a release" }),
        )
        .await,
    );

    let upd = fx
        .call(
            "a",
            "memory_edit",
            json!({ "op": "update", "id": a, "content": "build with just ci" }),
        )
        .await;
    assert!(upd.content.contains("updated"), "{}", upd.content);
    assert!(
        fx.call("a", "recall", json!({ "query": "cargo" }))
            .await
            .content
            .contains("No relevant")
    );
    assert!(
        fx.call("a", "recall", json!({ "query": "just ci" }))
            .await
            .content
            .contains(&a)
    );

    let inv = fx
        .call(
            "a",
            "memory_edit",
            json!({ "op": "invalidate", "id": b, "replacement_id": a }),
        )
        .await;
    assert!(inv.content.contains("invalidated"), "{}", inv.content);
    assert!(
        fx.call("a", "recall", json!({ "query": "quokka" }))
            .await
            .content
            .contains("No relevant")
    );
    let kept = fx.read("a", &b).expect("invalidated rows stay readable");
    assert!(
        kept.contains("status: invalidated") && kept.contains(&format!("replacement_id: {a}")),
        "{kept}"
    );

    let gone = fx
        .call("a", "memory_edit", json!({ "op": "forget", "id": a }))
        .await;
    assert!(gone.content.contains("deleted"), "{}", gone.content);
    assert!(fx.read("a", &a).is_err());
    let missing = fx
        .call("a", "memory_edit", json!({ "op": "forget", "id": a }))
        .await;
    assert!(
        missing.content.contains("was not found"),
        "{}",
        missing.content
    );
    let lessons = fx.read("a", "root/learned.md").expect("lessons");
    assert!(
        !lessons.contains("quokka"),
        "invalidated lesson listed: {lessons}"
    );
}

#[tokio::test]
async fn the_first_prompt_pulls_a_bounded_frozen_block_into_the_system_prompt() {
    let fx = Fixture::new();
    let mut msgs = vec![
        Msg::system("base"),
        Msg::user("tell me about zephyr deploys"),
    ];

    // Nothing retained yet: the database file must not even be created.
    fx.parts.hook.before_llm(&mut msgs).await.expect("hook");
    assert_eq!(msgs[0].content, "base");
    assert!(
        !fx.db().exists(),
        "an empty session must not create the database"
    );

    for n in 0..20 {
        fx.call("a", "retain", json!({ "items": [{ "content": format!("zephyr deploy note {n}: {}", "x".repeat(900)) }] })).await;
    }
    // A new session (new first prompt) sees them; the block is capped in count and size.
    let mut fresh = vec![Msg::system("base"), Msg::user("zephyr deploy notes please")];
    fx.parts.hook.before_llm(&mut fresh).await.expect("hook");
    let block = fresh[0]
        .content
        .strip_prefix("base\n\n")
        .expect("block appended");
    assert!(block.starts_with("## Memories"), "{block}");
    assert!(
        block.matches("(id: ").count() <= 8 && block.len() <= 4000,
        "{}",
        block.len()
    );

    // Frozen: memories retained later do not change the session's prompt.
    fx.call(
        "a",
        "retain",
        json!({ "items": [{ "content": "zephyr deploy note fresh" }] }),
    )
    .await;
    let mut again = vec![Msg::system("base"), Msg::user("zephyr deploy notes please")];
    fx.parts.hook.before_llm(&mut again).await.expect("hook");
    assert_eq!(again[0].content, fresh[0].content);
}
