//! Tests for extended semantic LSP actions: `type_definition`, `implementation`,
//! `incoming_calls`, `outgoing_calls`, `code_actions`, and `format`.

use crate::config::LspConfig;
use crate::pool::LspManager;
use crate::rpc::LspClient;
use crate::tool::LspTool;
use crate::uri::path_to_uri;
use hrn_edge_tools::FileReadTracker;
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, EventSink, PermissionMode, Tool, ToolContext, ToolTier,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

fn make_ctx(cwd: PathBuf, permission_mode: PermissionMode) -> ToolContext {
    ToolContext {
        session_id: "s".to_owned(),
        turn_id: "t".to_owned(),
        call_id: "c".to_owned(),
        cwd,
        permission_mode,
        cancel: CancelFlag::new(),
        events: Arc::new(NullSink),
        ask: None,
        approval: None,
        urls: Arc::default(),
        root_session_id: String::new(),
        inbox: Arc::default(),
        caller: None,
        todos: Arc::new(hrn_kernel::todo::TodoBoards::new()),
        yield_data: Arc::default(),
        is_subagent: false,
    }
}

async fn write_lsp<W: AsyncWriteExt + Unpin>(w: &mut W, payload: &Value) {
    let body = serde_json::to_vec(payload).expect("encode");
    let hdr = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(hdr.as_bytes()).await.expect("hdr");
    w.write_all(&body).await.expect("body");
    w.flush().await.expect("flush");
}

async fn read_lsp<R: tokio::io::AsyncRead + Unpin>(r: &mut BufReader<R>) -> Option<Value> {
    let mut len = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let t = line.trim();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':')
            && k.trim().eq_ignore_ascii_case("Content-Length")
        {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut buf = vec![0u8; len?];
    r.read_exact(&mut buf).await.ok()?;
    serde_json::from_slice(&buf).ok()
}

fn spawn_extended_mock(
    c2s_read: tokio::io::DuplexStream,
    mut s2c_write: tokio::io::DuplexStream,
    uri: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut r = BufReader::new(c2s_read);
        while let Some(msg) = read_lsp(&mut r).await {
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            let id = msg.get("id").cloned();
            let result = match method {
                "textDocument/typeDefinition" => json!([{
                    "uri": uri,
                    "range": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 12 } }
                }]),
                "textDocument/implementation" => json!([{
                    "uri": uri,
                    "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 10 } }
                }]),
                "textDocument/prepareCallHierarchy" => json!([{
                    "name": "run",
                    "kind": 12,
                    "uri": uri,
                    "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 12 } },
                    "selectionRange": { "start": { "line": 2, "character": 3 }, "end": { "line": 2, "character": 6 } }
                }]),
                "callHierarchy/incomingCalls" => json!([{
                    "from": {
                        "name": "main",
                        "kind": 12,
                        "uri": uri,
                        "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 10 } },
                        "selectionRange": { "start": { "line": 3, "character": 3 }, "end": { "line": 3, "character": 7 } }
                    },
                    "fromRanges": [
                        { "start": { "line": 3, "character": 8 }, "end": { "line": 3, "character": 11 } },
                        { "start": { "line": 4, "character": 8 }, "end": { "line": 4, "character": 11 } }
                    ]
                }]),
                "callHierarchy/outgoingCalls" => json!([{
                    "to": {
                        "name": "helper",
                        "kind": 12,
                        "uri": uri,
                        "range": { "start": { "line": 4, "character": 0 }, "end": { "line": 4, "character": 10 } },
                        "selectionRange": { "start": { "line": 4, "character": 3 }, "end": { "line": 4, "character": 9 } }
                    },
                    "fromRanges": [{ "start": { "line": 2, "character": 10 }, "end": { "line": 2, "character": 16 } }]
                }]),
                "textDocument/codeAction" => json!([
                    {
                        "title": "Import std::fmt::Display",
                        "kind": "quickfix",
                        "isPreferred": true,
                        "edit": {
                            "changes": {
                                uri.clone(): [{
                                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                                    "newText": "use std::fmt::Display;\n"
                                }]
                            }
                        }
                    },
                    {
                        "title": "Extract function",
                        "kind": "refactor.extract",
                        "command": { "title": "Extract", "command": "rust-analyzer.extract" }
                    }
                ]),
                "textDocument/formatting" => json!([{
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 15 } },
                    "newText": "struct MyType;"
                }]),
                _ => continue,
            };
            write_lsp(
                &mut s2c_write,
                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            )
            .await;
        }
    })
}

async fn setup_extended_lsp_fixture() -> (PathBuf, PathBuf, LspTool, tokio::task::JoinHandle<()>) {
    let dir = std::env::temp_dir().join(format!("hrn-lsp-ext-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"demo\"\n").expect("cargo");
    let file = dir.join("lib.rs");
    std::fs::write(
        &file,
        "struct  MyType;\nimpl Trait for MyType {}\nfn run() { helper(); }\nfn main() { run(); }\nfn helper() {}\n",
    )
    .expect("write");

    let (c2s_write, c2s_read) = tokio::io::duplex(65_536);
    let (s2c_write, s2c_read) = tokio::io::duplex(65_536);
    let mock = spawn_extended_mock(c2s_read, s2c_write, path_to_uri(&file));

    let tracker = Arc::new(FileReadTracker::new());
    let mgr = Arc::new(LspManager::with_context(
        LspConfig::default(),
        None,
        Some(tracker),
    ));
    let client = LspClient::from_streams(s2c_read, c2s_write);
    mgr.insert_test_server("rust-analyzer", dir.clone(), client)
        .await;
    (dir, file, LspTool::new(mgr), mock)
}

#[tokio::test]
async fn extended_lsp_actions_cover_type_impl_hierarchy_actions_and_format() {
    let (dir, file, tool, mock) = setup_extended_lsp_fixture().await;

    assert_eq!(
        tool.approval(&json!({"action": "format", "path": "lib.rs"}))
            .tier,
        ToolTier::Write
    );
    assert_eq!(
        tool.approval(&json!({"action": "type_definition", "path": "lib.rs"}))
            .tier,
        ToolTier::Read
    );

    let ctx = make_ctx(dir.clone(), PermissionMode::Yolo);
    let td = tool
        .execute(
            &ctx,
            json!({"action": "type_definition", "path": "lib.rs", "line": 3, "col": 4}),
        )
        .await
        .expect("type_definition");
    assert!(td.content.contains("lib.rs:1:8"), "{}", td.content);

    let imp = tool
        .execute(
            &ctx,
            json!({"action": "implementation", "path": "lib.rs", "line": 1, "col": 8}),
        )
        .await
        .expect("implementation");
    assert!(imp.content.contains("lib.rs:2:1"), "{}", imp.content);

    let inc = tool
        .execute(
            &ctx,
            json!({"action": "incoming_calls", "path": "lib.rs", "line": 3, "col": 4}),
        )
        .await
        .expect("incoming_calls");
    assert!(
        inc.content.contains("[fn] main") && inc.content.contains("2 call sites"),
        "{}",
        inc.content
    );

    let out = tool
        .execute(
            &ctx,
            json!({"action": "outgoing_calls", "path": "lib.rs", "line": 3, "col": 4}),
        )
        .await
        .expect("outgoing_calls");
    assert!(
        out.content.contains("[fn] helper") && out.content.contains("lib.rs:5:4"),
        "{}",
        out.content
    );

    let actions = tool
        .execute(
            &ctx,
            json!({"action": "code_actions", "path": "lib.rs", "line": 1, "col": 1, "end_line": 1, "end_col": 10}),
        )
        .await
        .expect("code_actions");
    assert!(
        actions
            .content
            .contains("[quickfix] Import std::fmt::Display (preferred)"),
        "{}",
        actions.content
    );
    assert!(
        actions
            .content
            .contains("[refactor.extract] Extract function"),
        "{}",
        actions.content
    );

    // Format is rejected in Plan mode.
    let plan_ctx = make_ctx(dir.clone(), PermissionMode::Plan);
    let denied = tool
        .execute(&plan_ctx, json!({"action": "format", "path": "lib.rs"}))
        .await
        .expect_err("format must fail in Plan mode");
    assert!(denied.to_string().contains("plan"), "{denied}");

    // Format succeeds in Yolo mode and updates file on disk.
    let fmt = tool
        .execute(&ctx, json!({"action": "format", "path": "lib.rs"}))
        .await
        .expect("format");
    assert!(
        fmt.content.contains("Formatted lib.rs (1 edit(s))."),
        "{}",
        fmt.content
    );
    let disk = std::fs::read_to_string(&file).expect("read");
    assert!(disk.starts_with("struct MyType;\n"), "{disk}");

    mock.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
