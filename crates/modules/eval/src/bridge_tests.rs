//! Acceptance and security tests for Wave 10c eval HTTP bridge, tools, agents, and workpool.

use crate::{EvalConfig, EvalTool, KernelToolStore, Lang};
use hrn_edge_proc::resolve_on_path;
use hrn_edge_tools::{DynamicToolLookup, EvalRuntimeHints, LocalScheme};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, EvalHost, EventSink, HrnError, InboxItem, PermissionMode,
    Result, Tool, ToolContext, ToolOutput, UrlRouter,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

pub(crate) fn has_python() -> bool {
    resolve_on_path("python").is_some() || resolve_on_path("python3").is_some()
}

pub(crate) fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hrn-eval-10c-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

pub(crate) fn make_ctx(session: &str, cwd: PathBuf, local_root: PathBuf) -> ToolContext {
    let urls = Arc::new(UrlRouter::new().with_builtin(Arc::new(LocalScheme::new(local_root))));
    ToolContext {
        session_id: session.to_owned(),
        turn_id: "t1".to_owned(),
        call_id: "c1".to_owned(),
        cwd,
        permission_mode: PermissionMode::Yolo,
        cancel: CancelFlag::new(),
        events: Arc::new(NullSink),
        ask: None,
        approval: None,
        urls,
        root_session_id: session.to_owned(),
        inbox: Arc::default(),
        caller: None,
        todos: Arc::new(hrn_kernel::todo::TodoBoards::new()),
        yield_data: Arc::default(),
        is_subagent: false,
    }
}

struct MockAgent {
    id: String,
    output: String,
    done: bool,
}

#[derive(Default)]
pub(crate) struct MockHost {
    pub(crate) tool_calls: Mutex<Vec<(String, Value)>>,
    agents: Mutex<BTreeMap<String, MockAgent>>,
    next_agent: Mutex<u64>,
    pub(crate) complete_calls: Mutex<Vec<(String, Option<String>)>>,
    valid_tools: Vec<String>,
}

impl MockHost {
    pub(crate) fn with_tools(names: &[&str]) -> Self {
        Self {
            valid_tools: names.iter().map(|s| (*s).to_owned()).collect(),
            ..Self::default()
        }
    }
}

impl EvalHost for MockHost {
    fn call_tool(
        &self,
        _ctx: &ToolContext,
        tool_name: &str,
        args: Value,
    ) -> BoxFuture<Result<ToolOutput>> {
        self.tool_calls
            .lock()
            .expect("lock")
            .push((tool_name.to_owned(), args.clone()));
        let valid = self.valid_tools.clone();
        let name = tool_name.to_owned();
        Box::pin(async move {
            if !valid.is_empty() && !valid.iter().any(|v| v == &name) {
                return Ok(ToolOutput::err(format!(
                    "unknown tool `{name}` (valid tools: {})",
                    valid.join(", ")
                )));
            }
            match name.as_str() {
                "read" => {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
                    Ok(ToolOutput::ok(format!("listing:{path}")))
                }
                "desktop" => {
                    let action = args.get("action").and_then(Value::as_str).unwrap_or("?");
                    Ok(ToolOutput::ok(format!("desktop:{action}")))
                }
                _ => Ok(ToolOutput::ok(format!("ok:{name}:{args}"))),
            }
        })
    }

    fn spawn_agent(&self, ctx: &ToolContext, request: Value) -> BoxFuture<Result<Value>> {
        let prompt = request
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let id = {
            let mut n = self.next_agent.lock().expect("lock");
            *n += 1;
            format!("agent-{}", *n)
        };
        let output = format!("done:{prompt}");
        self.agents.lock().expect("lock").insert(
            id.clone(),
            MockAgent {
                id: id.clone(),
                output: output.clone(),
                done: true,
            },
        );
        ctx.inbox.push(
            &ctx.session_id,
            InboxItem {
                source: id.clone(),
                text: format!("<agent id=\"{id}\">{output}</agent>"),
                summary: format!("{id} idle"),
            },
        );
        Box::pin(async move {
            Ok(json!({
                "id": id,
                "name": id,
                "profile": "scout",
                "status": "running",
            }))
        })
    }

    fn wait_agents(
        &self,
        ctx: &ToolContext,
        ids: Vec<String>,
        _timeout_ms: Option<u64>,
    ) -> BoxFuture<Result<Value>> {
        let agents = self.agents.lock().expect("lock");
        let mut results = Vec::new();
        for id in &ids {
            let Some(agent) = agents.get(id) else {
                let missing = id.clone();
                return Box::pin(async move {
                    Err(HrnError::Tool(format!("unknown agent `{missing}`")))
                });
            };
            let _ = ctx.inbox.take_source(&ctx.session_id, id);
            results.push(json!({
                "id": agent.id,
                "status": if agent.done { "idle" } else { "running" },
                "output": agent.output,
                "done": agent.done,
                "timed_out": false,
            }));
        }
        Box::pin(async move { Ok(Value::Array(results)) })
    }

    fn complete(
        &self,
        _ctx: &ToolContext,
        prompt: String,
        role: Option<String>,
    ) -> BoxFuture<Result<String>> {
        self.complete_calls
            .lock()
            .expect("lock")
            .push((prompt.clone(), role.clone()));
        Box::pin(async move {
            let tag = role.unwrap_or_else(|| "judge".to_owned());
            Ok(format!("[{tag}] {prompt}"))
        })
    }
}

pub(crate) fn make_tool(
    data_dir: PathBuf,
    host: Arc<MockHost>,
    hints: Arc<EvalRuntimeHints>,
    cfg: EvalConfig,
) -> Arc<EvalTool> {
    Arc::new(EvalTool::with_parts(
        data_dir,
        cfg,
        Some(host),
        hints,
        Arc::new(KernelToolStore::new()),
    ))
}

#[tokio::test]
async fn eval_tool_bridge_roundtrip() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("bridge-roundtrip");
    let host = Arc::new(MockHost::with_tools(&["read", "write", "desktop"]));
    let hints = Arc::new(EvalRuntimeHints::default());
    hints.set_desktop_eval_prelude(true);
    hints.set_desktop_tool_registered(true);
    let tool = make_tool(
        dir.join("data"),
        Arc::clone(&host),
        hints,
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-rt", dir.clone(), dir.join("local"));

    let out = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "tool.read({'path': '.'})" }),
        )
        .await
        .expect("exec");
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("listing:."), "{}", out.content);

    // Unknown tool must report valid registered tool names.
    let bad = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "tool.nonexistent_xyz({})" }),
        )
        .await
        .expect("exec");
    assert!(bad.is_error, "{}", bad.content);
    assert!(bad.content.contains("valid tools:"), "{}", bad.content);
    assert!(bad.content.contains("read"), "{}", bad.content);

    // computer.* prelude delegates to tool.desktop when enabled.
    let comp = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "computer.displays()" }),
        )
        .await
        .expect("exec");
    assert!(!comp.is_error, "{}", comp.content);
    assert!(
        comp.content.contains("desktop:displays"),
        "{}",
        comp.content
    );

    // judge() and judge_batch() route through /complete with role "judge".
    let judged = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "(str(judge('ok?')), judge_batch(['a', 'b']))" }),
        )
        .await
        .expect("judge");
    assert!(!judged.is_error, "{}", judged.content);
    assert!(judged.content.contains("[judge] ok?"), "{}", judged.content);
    assert!(judged.content.contains("[judge] b"), "{}", judged.content);

    // If Bun is available, verify JS async tool proxy too.
    if resolve_on_path("bun").is_some() {
        let js_out = tool
            .execute(
                &ctx,
                json!({ "language": "js", "code": "await tool.read({ path: 'src' })" }),
            )
            .await
            .expect("js exec");
        assert!(!js_out.is_error, "{}", js_out.content);
        assert!(js_out.content.contains("listing:src"), "{}", js_out.content);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_tool_rejects_checkpoint() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("reject-ckpt");
    let host = Arc::new(MockHost::with_tools(&[
        "read",
        "checkpoint",
        "rewind",
        "yield",
    ]));
    let tool = make_tool(
        dir.join("data"),
        Arc::clone(&host),
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-ckpt", dir.clone(), dir.join("local"));

    for code in [
        "tool.checkpoint({})",
        "tool.rewind({})",
        "getattr(tool, 'yield')({})",
    ] {
        let out = tool
            .execute(&ctx, json!({ "language": "py", "code": code }))
            .await
            .expect("exec");
        assert!(
            out.is_error,
            "expected error for {code}, got {}",
            out.content
        );
        assert!(
            out.content.contains("cannot be called from eval"),
            "missing omp refusal wording for {code}: {}",
            out.content
        );
    }
    assert!(host.tool_calls.lock().expect("lock").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_agent_handle_waits() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("agent-wait");
    let host = Arc::new(MockHost::with_tools(&["read", "write"]));
    let tool = make_tool(
        dir.join("data"),
        host,
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-aw", dir.clone(), dir.join("local"));

    let out = tool
        .execute(
            &ctx,
            json!({
                "language": "py",
                "code": "h = agent('inspect module')\nres = wait([h])\n(h.id, res[0]['status'], res[0]['output'])"
            }),
        )
        .await
        .expect("exec");
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("agent-1"), "{}", out.content);
    assert!(
        out.content.contains("done:inspect module"),
        "{}",
        out.content
    );
    assert_eq!(ctx.inbox.queued("s-aw"), 0);

    let awaited = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "r = await agent('async task')\nr['output']" }),
        )
        .await
        .expect("await");
    assert!(!awaited.is_error, "{}", awaited.content);
    assert!(
        awaited.content.contains("done:async task"),
        "{}",
        awaited.content
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_unwaited_agent_auto_delivers() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("agent-unwaited");
    let host = Arc::new(MockHost::with_tools(&["read", "write"]));
    let tool = make_tool(
        dir.join("data"),
        host,
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-unw", dir.clone(), dir.join("local"));

    let out = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "h = agent('background job')\nh.id" }),
        )
        .await
        .expect("exec");
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("agent-1"), "{}", out.content);

    let delivered = ctx.inbox.take("s-unw");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].source, "agent-1");
    assert!(delivered[0].text.contains("done:background job"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_workpool_bounded() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("workpool-bounded");
    let host = Arc::new(MockHost::default());
    let hints = Arc::new(EvalRuntimeHints::default());
    hints.set_max_concurrency(2);
    let tool = make_tool(dir.join("data"), host, hints, EvalConfig::default());
    let ctx = make_ctx("s-wp", dir.clone(), dir.join("local"));

    let code = r"
import time
wp = workpool(max_concurrency=10)
def task(i):
    time.sleep(0.05)
    return i * 2
for i in range(6):
    wp.push(task, i)
results = wp.close()
st = wp.status()
(wp.max_concurrency, st['peak_active'], results)
";
    let out = tool
        .execute(&ctx, json!({ "language": "py", "code": code }))
        .await
        .expect("exec");
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("(2, 2, [0, 2, 4, 6, 8, 10])")
            || out.content.contains("(2, 1, [0, 2, 4, 6, 8, 10])"),
        "expected max_concurrency=2 and peak<=2, got: {}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

async fn send_raw_http(addr: &str, raw_request: &str) -> String {
    let host_port = addr.strip_prefix("http://").unwrap_or(addr);
    let mut stream = TcpStream::connect(host_port).await.expect("connect");
    stream
        .write_all(raw_request.as_bytes())
        .await
        .expect("write");
    stream.flush().await.expect("flush");
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp).await;
    String::from_utf8_lossy(&resp).into_owned()
}

#[tokio::test]
async fn eval_bridge_rejects_missing_or_wrong_token() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("bridge-auth");
    let host = Arc::new(MockHost::with_tools(&["read"]));
    let tool = make_tool(
        dir.join("data"),
        host,
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-auth", dir.clone(), dir.join("local"));

    let out = tool
        .execute(
            &ctx,
            json!({
                "language": "py",
                "code": "import os\n('HRN_EVAL_TOKEN' in os.environ, 'HRN_EVAL_TOKEN' in env(), 'computer' in globals())"
            }),
        )
        .await
        .expect("spawn");
    assert_eq!(out.content, "(False, False, False)");

    let url = tool
        .test_bridge_url("s-auth", Lang::Py)
        .await
        .expect("bridge url");
    let token = tool
        .test_bridge_token("s-auth", Lang::Py)
        .await
        .expect("bridge token");
    assert_eq!(token.len(), 64);

    let no_auth = send_raw_http(
        &url,
        "POST /watchdog/pause HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\n\r\n{}",
    )
    .await;
    assert!(no_auth.starts_with("HTTP/1.1 401 "), "{no_auth}");

    let wrong_auth = send_raw_http(
        &url,
        "POST /watchdog/pause HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer 0000000000000000000000000000000000000000000000000000000000000000\r\nContent-Length: 2\r\n\r\n{}",
    )
    .await;
    assert!(wrong_auth.starts_with("HTTP/1.1 401 "), "{wrong_auth}");

    let too_big = send_raw_http(
        &url,
        &format!(
            "POST /tool HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 5000000\r\n\r\n"
        ),
    )
    .await;
    assert!(too_big.starts_with("HTTP/1.1 413 "), "{too_big}");

    let valid = send_raw_http(
        &url,
        &format!(
            "POST /watchdog/pause HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    )
    .await;
    assert!(valid.starts_with("HTTP/1.1 200 "), "{valid}");

    let reg = tool
        .execute(
            &ctx,
            json!({
                "language": "py",
                "code": "@tool\ndef triple(n: int) -> int:\n    '''Multiply by 3.'''\n    return n * 3\n'ok'"
            }),
        )
        .await
        .expect("register");
    assert!(!reg.is_error, "{}", reg.content);
    let resolved = tool
        .dyn_tools()
        .resolve("s-auth", &["triple".to_owned()])
        .expect("resolve");
    assert_eq!(resolved.len(), 1);
    let call_out = resolved[0]
        .execute(&ctx, json!({ "n": 14 }))
        .await
        .expect("call dyn tool");
    assert!(!call_out.is_error, "{}", call_out.content);
    assert_eq!(call_out.content, "42");

    let _ = std::fs::remove_dir_all(&dir);
}
