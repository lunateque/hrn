use super::*;
use crate::lifecycle::deduplicate_diagnostics;
use crate::rpc::LspClient;
use crate::servers::load_merged_servers;
use crate::uri::path_to_uri;
use hrn_edge_tools::FileReadTracker;
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, EventSink, HarnessHook, Host, PermissionMode, Registry,
    Shared, Tool, ToolContext, ToolOutput,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

fn make_ctx(cwd: PathBuf) -> ToolContext {
    ToolContext {
        session_id: "s".to_owned(),
        turn_id: "t".to_owned(),
        call_id: "c".to_owned(),
        cwd,
        permission_mode: PermissionMode::Yolo,
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

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hrn-lsp-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::canonicalize(&dir).unwrap_or(dir)
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

fn spawn_mock_lsp(
    c2s_read: tokio::io::DuplexStream,
    mut s2c_write: tokio::io::DuplexStream,
    mock_uri: String,
    format_edits: Option<Value>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut r = BufReader::new(c2s_read);
        while let Some(msg) = read_lsp(&mut r).await {
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            let id = msg.get("id").cloned();
            match method {
                "textDocument/didOpen" | "textDocument/didSave" => {
                    write_lsp(
                        &mut s2c_write,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {
                                "uri": mock_uri,
                                "diagnostics": [{
                                    "range": {
                                        "start": { "line": 0, "character": 25 },
                                        "end": { "line": 0, "character": 30 }
                                    },
                                    "severity": 1,
                                    "source": "rustc",
                                    "code": "E0308",
                                    "message": "mismatched types: expected u32, found &str"
                                }]
                            }
                        }),
                    )
                    .await;
                }
                "textDocument/hover" => {
                    write_lsp(
                        &mut s2c_write,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "contents": { "kind": "markdown", "value": "```rust\nfn main()\n```" } }
                        }),
                    )
                    .await;
                }
                "textDocument/definition" => {
                    write_lsp(
                        &mut s2c_write,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": [{
                                "uri": mock_uri,
                                "range": {
                                    "start": { "line": 0, "character": 3 },
                                    "end": { "line": 0, "character": 7 }
                                }
                            }]
                        }),
                    )
                    .await;
                }
                "textDocument/formatting" => {
                    let result = format_edits.clone().unwrap_or(Value::Null);
                    write_lsp(
                        &mut s2c_write,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": result
                        }),
                    )
                    .await;
                }
                _ => {}
            }
        }
    })
}

#[tokio::test]
async fn lsp_tool_and_diagnostics_hook_work_over_jsonrpc() {
    let dir = temp_dir("basic");
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("cargo");
    let rs_file = dir.join("main.rs");
    std::fs::write(&rs_file, "fn main() { let x: u32 = \"bad\"; }\n").expect("rs");

    let (c2s_read, c2s_write) = tokio::io::duplex(64 * 1024);
    let (s2c_read, s2c_write) = tokio::io::duplex(64 * 1024);
    let server_task = spawn_mock_lsp(c2s_read, s2c_write, path_to_uri(&rs_file), None);

    let client = LspClient::from_streams(s2c_read, c2s_write);
    let mgr = Arc::new(LspManager::new(LspConfig::default()));
    let tool = LspTool::new(Arc::clone(&mgr));
    let hook = LspDiagnosticsHook::new(Arc::clone(&mgr), dir.clone());
    mgr.insert_test_server("rust-analyzer", dir.clone(), client)
        .await;

    let ctx = make_ctx(dir.clone());
    let diags = tool
        .execute(&ctx, json!({"action": "diagnostics", "path": "main.rs"}))
        .await
        .expect("diagnostics");
    assert!(diags.content.contains("E0308"), "{}", diags.content);
    assert!(diags.content.contains("main.rs:1:26"), "{}", diags.content);

    let hover = tool
        .execute(
            &ctx,
            json!({"action": "hover", "path": "main.rs", "line": 1, "col": 4}),
        )
        .await
        .expect("hover");
    assert!(hover.content.contains("fn main()"), "{}", hover.content);

    let def = tool
        .execute(
            &ctx,
            json!({"action": "definition", "path": "main.rs", "line": 1, "col": 4}),
        )
        .await
        .expect("def");
    assert!(def.content.contains("main.rs:1:4"), "{}", def.content);

    let _ = hook
        .before_tool(&ctx, "write", &json!({"path": "main.rs"}))
        .await
        .expect("before");
    let after = hook
        .after_tool(
            &ctx,
            "write",
            &json!({"path": "main.rs"}),
            ToolOutput::ok("wrote main.rs"),
        )
        .await
        .expect("after");
    assert!(
        after.content.contains("[LSP diagnostics]"),
        "{}",
        after.content
    );
    assert!(after.content.contains("E0308"), "{}", after.content);

    server_task.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn lsp_diagnostics_on_edit_false() {
    let dir = temp_dir("diag-edit-false");
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("cargo");
    let rs_file = dir.join("main.rs");
    std::fs::write(&rs_file, "fn main() { let x: u32 = \"bad\"; }\n").expect("rs");

    let (c2s_read, c2s_write) = tokio::io::duplex(64 * 1024);
    let (s2c_read, s2c_write) = tokio::io::duplex(64 * 1024);
    let server_task = spawn_mock_lsp(c2s_read, s2c_write, path_to_uri(&rs_file), None);

    // Defaults: diagnostics_on_write = true, diagnostics_on_edit = false.
    let cfg = LspConfig::default();
    assert!(cfg.diagnostics_on_write && !cfg.diagnostics_on_edit);
    let mgr = Arc::new(LspManager::new(cfg));
    mgr.insert_test_server(
        "rust-analyzer",
        dir.clone(),
        LspClient::from_streams(s2c_read, c2s_write),
    )
    .await;
    let hook = LspDiagnosticsHook::new(Arc::clone(&mgr), dir.clone());
    let ctx = make_ctx(dir.clone());

    let edit_args = json!({"path": "main.rs", "old": "bad", "new": "still_bad"});
    let _ = hook
        .before_tool(&ctx, "edit", &edit_args)
        .await
        .expect("before");
    let after_edit = hook
        .after_tool(&ctx, "edit", &edit_args, ToolOutput::ok("edited main.rs"))
        .await
        .expect("after edit");
    assert!(
        !after_edit.content.contains("[LSP diagnostics]"),
        "edit must not emit diagnostics when diagnostics_on_edit=false: {}",
        after_edit.content
    );

    let write_args = json!({"path": "main.rs"});
    let _ = hook
        .before_tool(&ctx, "write", &write_args)
        .await
        .expect("before write");
    let after_write = hook
        .after_tool(&ctx, "write", &write_args, ToolOutput::ok("wrote main.rs"))
        .await
        .expect("after write");
    assert!(
        after_write.content.contains("[LSP diagnostics]"),
        "write must still emit diagnostics when diagnostics_on_write=true: {}",
        after_write.content
    );

    server_task.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lsp_disabled_unregisters() {
    let dir = temp_dir("disabled");
    let host = Host {
        cwd: dir.clone(),
        data_dir: dir.clone(),
    };
    let table = BTreeMap::from([("lsp".to_owned(), json!({"enabled": false}))]);
    let registered = Registry::new(host, table, Arc::new(Shared::new()))
        .module("lsp", register)
        .and_then(Registry::finish)
        .expect("lsp registers cleanly when disabled");
    assert!(
        registered.tools.is_empty(),
        "disabled lsp must register no tools"
    );
    assert!(
        registered.hooks.is_empty(),
        "disabled lsp must register no hooks"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn lsp_user_server_disabled_and_custom() {
    let root = temp_dir("user-servers");
    let data_dir = root.join("data");
    let ws = root.join("ws");
    std::fs::create_dir_all(&data_dir).expect("data");
    std::fs::create_dir_all(ws.join(".harness")).expect("harness");

    // Global `<data_dir>/lsp.toml` defines a custom server for `.demo` files.
    std::fs::write(
        data_dir.join("lsp.toml"),
        r#"
[[servers]]
name = "demo-lsp"
command = "demo-langserver"
args = ["--stdio"]
file_types = [".demo"]
language_id = "demo"
root_markers = ["demo.toml"]
settings = { strict = true }
is_linter = true
warmup_timeout_ms = 5000
"#,
    )
    .expect("write global lsp.toml");

    // Project `<cwd>/.harness/lsp.toml` disables built-in `rust-analyzer`.
    std::fs::write(
        ws.join(".harness").join("lsp.toml"),
        r#"
[[servers]]
name = "rust-analyzer"
disabled = true
"#,
    )
    .expect("write project lsp.toml");

    let loaded = load_merged_servers(Some(&data_dir), &ws);
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    let cfg = LspConfig::from_table(&LspTableConfig::default(), loaded.servers, loaded.warnings);
    let mgr = Arc::new(LspManager::with_context(cfg, Some(data_dir), None));

    let status = mgr.status().await;
    assert!(
        !status.contains("rust-analyzer"),
        "disabled rust-analyzer must be omitted from status: {status}"
    );
    assert!(
        status.contains("demo-lsp"),
        "custom server must appear in status: {status}"
    );

    // Rust file query refuses because rust-analyzer is disabled.
    std::fs::write(ws.join("a.rs"), "fn main() {}\n").expect("a.rs");
    let err = mgr
        .diagnostics(Path::new("a.rs"), &ws)
        .await
        .expect_err("disabled server must refuse")
        .to_string();
    assert!(err.contains("no enabled LSP server"), "{err}");

    // Custom `.demo` file resolves to `demo-lsp` (fails only because mock binary isn't on PATH).
    std::fs::write(ws.join("x.demo"), "hello").expect("x.demo");
    let err_custom = mgr
        .diagnostics(Path::new("x.demo"), &ws)
        .await
        .expect_err("missing binary")
        .to_string();
    assert!(
        err_custom.contains("demo-lsp") && err_custom.contains("demo-langserver"),
        "{err_custom}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn lsp_format_on_write_mock() {
    let dir = temp_dir("format-on-write");
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("cargo");
    let rs_file = dir.join("main.rs");
    std::fs::write(&rs_file, "fn main(){\nlet x=1;\n}\n").expect("rs");

    let edits = json!([
        {
            "range": { "start": { "line": 0, "character": 9 }, "end": { "line": 0, "character": 10 } },
            "newText": " {"
        },
        {
            "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 8 } },
            "newText": "    let x = 1;"
        }
    ]);
    let (c2s_read, c2s_write) = tokio::io::duplex(64 * 1024);
    let (s2c_read, s2c_write) = tokio::io::duplex(64 * 1024);
    let server_task = spawn_mock_lsp(c2s_read, s2c_write, path_to_uri(&rs_file), Some(edits));

    let table = LspTableConfig {
        format_on_write: true,
        diagnostics_on_write: false,
        ..LspTableConfig::default()
    };
    let tracker = Arc::new(FileReadTracker::new());
    let cfg = LspConfig::from_table(&table, builtin_server_specs(), Vec::new());
    let mgr = Arc::new(LspManager::with_context(
        cfg,
        None,
        Some(Arc::clone(&tracker)),
    ));
    mgr.insert_test_server(
        "rust-analyzer",
        dir.clone(),
        LspClient::from_streams(s2c_read, c2s_write),
    )
    .await;
    let hook = LspDiagnosticsHook::new(Arc::clone(&mgr), dir.clone());
    let ctx = make_ctx(dir.clone());

    let args = json!({"path": "main.rs"});
    let _ = hook
        .before_tool(&ctx, "write", &args)
        .await
        .expect("before");
    let out = hook
        .after_tool(&ctx, "write", &args, ToolOutput::ok("Wrote `main.rs`"))
        .await
        .expect("after");
    assert!(out.content.contains("[LSP formatted]"), "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&rs_file).expect("read formatted"),
        "fn main() {\n    let x = 1;\n}\n"
    );

    server_task.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn lsp_reload_rereads() {
    let root = temp_dir("reload");
    let data_dir = root.join("data");
    let ws = root.join("ws");
    std::fs::create_dir_all(&data_dir).expect("data");
    std::fs::create_dir_all(&ws).expect("ws");

    let (c2s_read, c2s_write) = tokio::io::duplex(64 * 1024);
    let (s2c_read, s2c_write) = tokio::io::duplex(64 * 1024);
    let server_task = spawn_mock_lsp(c2s_read, s2c_write, path_to_uri(&ws.join("main.rs")), None);

    let loaded = load_merged_servers(Some(&data_dir), &ws);
    let cfg = LspConfig::from_table(&LspTableConfig::default(), loaded.servers, loaded.warnings);
    let mgr = Arc::new(LspManager::with_context(cfg, Some(data_dir.clone()), None));
    mgr.insert_test_server(
        "rust-analyzer",
        ws.clone(),
        LspClient::from_streams(s2c_read, c2s_write),
    )
    .await;
    let tool = LspTool::new(Arc::clone(&mgr));
    let ctx = make_ctx(ws.clone());

    // Initially rust-analyzer is active and `custom-reloaded` does not exist.
    let before = tool
        .execute(&ctx, json!({"action": "status"}))
        .await
        .expect("status");
    assert!(
        before.content.contains("rust-analyzer [running]"),
        "{}",
        before.content
    );
    assert!(
        !before.content.contains("custom-reloaded"),
        "{}",
        before.content
    );

    // Write a new `<data_dir>/lsp.toml` and invoke `lsp action: "reload"`.
    std::fs::write(
        data_dir.join("lsp.toml"),
        r#"
[[servers]]
name = "rust-analyzer"
disabled = true

[[servers]]
name = "custom-reloaded"
command = "reloaded-lsp"
file_types = [".rld"]
language_id = "reloaded"
"#,
    )
    .expect("write lsp.toml");

    let reloaded = tool
        .execute(&ctx, json!({"action": "reload"}))
        .await
        .expect("reload");
    assert!(
        reloaded.content.contains("shut down 1 active client"),
        "{}",
        reloaded.content
    );
    let after = tool
        .execute(&ctx, json!({"action": "status"}))
        .await
        .expect("status after");
    assert!(
        after.content.contains("Active LSP servers: none"),
        "{}",
        after.content
    );
    assert!(
        after.content.contains("custom-reloaded"),
        "{}",
        after.content
    );
    assert!(
        !after.content.contains("rust-analyzer"),
        "{}",
        after.content
    );

    server_task.abort();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn deduplicate_diagnostics_and_evict_idle_clients() {
    let mut items = vec![
        DiagnosticItem {
            line: 1,
            col: 2,
            severity: 1,
            source: Some("rustc".to_owned()),
            code: Some("E0308".to_owned()),
            message: "type mismatch".to_owned(),
        },
        DiagnosticItem {
            line: 1,
            col: 2,
            severity: 1,
            source: Some("clippy".to_owned()),
            code: Some("E0308".to_owned()),
            message: "type mismatch".to_owned(),
        },
    ];
    deduplicate_diagnostics(&mut items);
    assert_eq!(items.len(), 1);

    let dir = temp_dir("idle-evict");
    let (c2s_read, c2s_write) = tokio::io::duplex(8 * 1024);
    let (s2c_read, s2c_write) = tokio::io::duplex(8 * 1024);
    let task = spawn_mock_lsp(c2s_read, s2c_write, path_to_uri(&dir.join("a.rs")), None);
    let table = LspTableConfig {
        idle_timeout_secs: 60,
        ..LspTableConfig::default()
    };
    let mgr = Arc::new(LspManager::new(LspConfig::from_table(
        &table,
        builtin_server_specs(),
        Vec::new(),
    )));
    mgr.insert_test_server(
        "rust-analyzer",
        dir.clone(),
        LspClient::from_streams(s2c_read, c2s_write),
    )
    .await;
    mgr.backdate_last_used_for_test(Duration::from_mins(2))
        .await;
    assert_eq!(mgr.evict_idle().await, 1);
    assert!(mgr.status().await.contains("Active LSP servers: none"));
    task.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
