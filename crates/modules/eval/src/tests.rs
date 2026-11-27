//! Live tests: they drive real interpreters and skip a language whose interpreter is missing.

use crate::EvalTool;
use hrn_edge_proc::resolve_on_path;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, EventSink, PermissionMode, Tool, ToolContext, ToolOutput,
};
use std::path::PathBuf;
use std::sync::Arc;

fn eval_tool(dir: PathBuf) -> Arc<dyn Tool> {
    Arc::new(EvalTool::new(dir))
}

struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

fn ctx(session: &str, cancel: CancelFlag) -> ToolContext {
    ToolContext {
        session_id: session.to_owned(),
        turn_id: "t".to_owned(),
        call_id: "c".to_owned(),
        cwd: std::env::temp_dir(),
        permission_mode: PermissionMode::Yolo,
        cancel,
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

fn fixture(name: &str) -> (Arc<dyn Tool>, ToolContext) {
    let dir: PathBuf = std::env::temp_dir().join(format!("hrn-eval-test-{name}"));
    (eval_tool(dir), ctx(name, CancelFlag::new()))
}

async fn run(tool: &Arc<dyn Tool>, ctx: &ToolContext, args: Value) -> ToolOutput {
    tool.execute(ctx, args).await.expect("tool call")
}

fn cell(language: &str, code: &str) -> Value {
    json!({ "language": language, "code": code })
}

fn has(program: &str) -> bool {
    let found = resolve_on_path(program).is_some();
    if !found {
        eprintln!("skipping: `{program}` not on PATH");
    }
    found
}

#[tokio::test]
async fn python_state_persists_and_last_expression_echoes() {
    if !has("python") {
        return;
    }
    let (tool, ctx) = fixture("py-state");
    let out = run(&tool, &ctx, cell("py", "x = 20\nprint('hi')\nx + 1")).await;
    assert_eq!(out.content, "hi\n21");
    assert!(!out.is_error);
    let out = run(
        &tool,
        &ctx,
        cell("py", "async def f():\n    return x * 2\nawait f()"),
    )
    .await;
    assert_eq!(out.content, "40");
    let out = run(&tool, &ctx, cell("py", "y = 1")).await;
    assert_eq!(out.content, "(no output)");
}

#[tokio::test]
async fn python_exception_returns_traceback_and_keeps_state() {
    if !has("python") {
        return;
    }
    let (tool, ctx) = fixture("py-error");
    run(&tool, &ctx, cell("py", "keep = 7")).await;
    let out = run(
        &tool,
        &ctx,
        cell("py", "def boom():\n    raise ValueError('bad')\nboom()"),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.contains("Traceback"), "{}", out.content);
    assert!(out.content.contains("ValueError: bad"), "{}", out.content);
    let out = run(&tool, &ctx, cell("py", "keep")).await;
    assert_eq!(out.content, "7");
}

#[tokio::test]
async fn python_helpers_and_stderr_ordering() {
    if !has("python") {
        return;
    }
    let (tool, ctx) = fixture("py-helpers");
    let file = std::env::temp_dir().join("hrn-eval-helper/sub/a.txt");
    let code = format!(
        "import sys\nprint('one')\nprint('two', file=sys.stderr)\nprint('three')\n\
         write({path:?}, 'héllo')\nread({path:?})",
        path = file.to_string_lossy()
    );
    let out = run(&tool, &ctx, cell("py", &code)).await;
    assert_eq!(out.content, "one\ntwo\nthree\n'héllo'");
    if resolve_on_path("bash").is_some() || cfg!(unix) {
        let out = run(
            &tool,
            &ctx,
            cell("py", "sh('echo out; echo err >&2').strip()"),
        )
        .await;
        assert_eq!(out.content, "'out\\nerr'");
        let out = run(&tool, &ctx, cell("py", "sh('exit 3')")).await;
        assert!(out.is_error);
        assert!(
            out.content.contains("exited with code 3"),
            "{}",
            out.content
        );
    }
}

#[tokio::test]
async fn reset_discards_state_and_timeout_kills_the_kernel() {
    if !has("python") {
        return;
    }
    let (tool, ctx) = fixture("py-reset");
    run(&tool, &ctx, cell("py", "z = 1")).await;
    let out = run(
        &tool,
        &ctx,
        json!({"language": "py", "code": "z", "reset": true}),
    )
    .await;
    assert!(
        out.is_error && out.content.contains("NameError"),
        "{}",
        out.content
    );

    run(&tool, &ctx, cell("py", "z = 5")).await;
    let slow = json!({"language": "py", "code": "import time\ntime.sleep(30)", "timeout": 1});
    let out = run(&tool, &ctx, slow).await;
    assert!(
        out.is_error && out.content.contains("timed out"),
        "{}",
        out.content
    );
    let out = run(&tool, &ctx, cell("py", "'z' in globals()")).await;
    assert_eq!(out.content, "False");
}

#[tokio::test]
async fn cancellation_kills_the_kernel() {
    if !has("python") {
        return;
    }
    let dir = std::env::temp_dir().join("hrn-eval-test-cancel");
    let tool = eval_tool(dir);
    let cancel = CancelFlag::new();
    let ctx = ctx("cancel", cancel.clone());
    let flag = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        flag.cancel();
    });
    let res = tool
        .execute(&ctx, cell("py", "import time\ntime.sleep(30)"))
        .await;
    assert!(matches!(res, Err(hrn_kernel::HrnError::Cancelled)));
}

#[tokio::test]
async fn javascript_state_persists_across_declarations_and_imports() {
    if !has("bun") {
        return;
    }
    let (tool, ctx) = fixture("js-state");
    let out = run(
        &tool,
        &ctx,
        cell(
            "js",
            "const a = 20\nlet b = [1, 2]\nconsole.log('hi')\na + b.length",
        ),
    )
    .await;
    assert_eq!(out.content, "hi\n22");
    let out = run(
        &tool,
        &ctx,
        cell(
            "js",
            "const a = 5\nfunction f() { return a * 2 }\nclass K { v() { return a } }",
        ),
    )
    .await;
    assert_eq!(out.content, "(no output)");
    let out = run(
        &tool,
        &ctx,
        cell("js", "a = a + 1\nconst { v } = new K()\n[f(), v(), b]"),
    )
    .await;
    assert_eq!(out.content, "[ 12, 6, [ 1, 2 ] ]");
    let out = run(&tool, &ctx, cell("js", "import path from 'node:path'\nimport { sep } from 'node:fs/promises'\nconst r = await Promise.resolve(path.basename('/x/y.txt'))\nreturn r")).await;
    assert_eq!(out.content, "y.txt");
    let out = run(&tool, &ctx, cell("js", "r")).await;
    assert_eq!(out.content, "y.txt");
}

#[tokio::test]
async fn javascript_errors_helpers_and_template_literals() {
    if !has("bun") {
        return;
    }
    let (tool, ctx) = fixture("js-misc");
    let out = run(&tool, &ctx, cell("js", "null.x")).await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("TypeError"), "{}", out.content);
    // `const` inside a template literal is data, not a declaration.
    let code = "const s = `\nconst inner = 1\n`\ns.trim()";
    let out = run(&tool, &ctx, cell("js", code)).await;
    assert_eq!(out.content, "const inner = 1");
    let out = run(&tool, &ctx, cell("js", "console.error('e1'); console.log('o1'); write('hrn-eval-js.txt', 'x'); read('hrn-eval-js.txt')")).await;
    assert_eq!(out.content, "e1\no1\nx");
    if resolve_on_path("bash").is_some() || cfg!(unix) {
        let out = run(
            &tool,
            &ctx,
            cell("js", "sh('echo out; echo err >&2').trim()"),
        )
        .await;
        assert_eq!(out.content, "out\nerr");
    }
}
