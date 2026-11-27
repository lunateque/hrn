//! Acceptance tests for Wave 10c magics (`%pip`, `%load`, `!cmd`, runner magics), output spill
//! + `ToolOutput.meta` + images, default 30s timeout, and watchdog pause/resume.

use crate::bridge_tests::{MockHost, has_python, make_ctx, make_tool, temp_dir};
use crate::magics::read_bounded_file;
use crate::{DEFAULT_TIMEOUT_SECS, EvalConfig, EvalTool, Lang};
use hrn_edge_proc::resolve_on_path;
use hrn_edge_tools::EvalRuntimeHints;
use hrn_kernel::serde_json::json;
use hrn_kernel::{AgentEvent, BoxFuture, EventSink, Tool, UrlBody};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn eval_magics_pip_load() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("magics-pip-load");
    let host = Arc::new(MockHost::default());
    let tool = make_tool(
        dir.join("data"),
        host,
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-magics", dir.clone(), dir.join("local"));

    // 1. Host-side standalone %pip
    let pip = tool
        .execute(&ctx, json!({ "language": "py", "code": "%pip --version" }))
        .await
        .expect("pip");
    assert!(!pip.is_error, "{}", pip.content);
    assert!(
        pip.content.to_ascii_lowercase().contains("pip"),
        "{}",
        pip.content
    );

    // 2. Host-side %load from workspace file (state persists into kernel)
    std::fs::write(
        dir.join("helper.py"),
        "LOADED_VAL = 123\nprint('loaded-file')",
    )
    .expect("write");
    let load_file = tool
        .execute(&ctx, json!({ "language": "py", "code": "%load helper.py" }))
        .await
        .expect("load file");
    assert!(!load_file.is_error, "{}", load_file.content);
    assert!(
        load_file.content.contains("loaded-file"),
        "{}",
        load_file.content
    );

    // 3. Host-side %load from local:// URL
    ctx.urls
        .write(
            &ctx,
            "local",
            "snippet.py",
            "LOCAL_VAL = LOADED_VAL + 7\nLOCAL_VAL",
        )
        .expect("write local");
    let load_local = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "%load local://snippet.py" }),
        )
        .await
        .expect("load local");
    assert!(!load_local.is_error, "{}", load_local.content);
    assert!(load_local.content.contains("130"), "{}", load_local.content);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_runner_magics_and_unknown_name_error() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("magics-runner");
    let tool = make_tool(
        dir.join("data"),
        Arc::new(MockHost::default()),
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-magics-run", dir.clone(), dir.join("local"));

    let runner_m = tool
        .execute(
            &ctx,
            json!({
                "language": "py",
                "code": "%env HRN_TEST_MAGIC=yes\n%env HRN_TEST_MAGIC"
            }),
        )
        .await
        .expect("env magic");
    assert!(
        !runner_m.is_error && runner_m.content.contains("yes"),
        "{}",
        runner_m.content
    );

    let cap_m = tool
        .execute(
            &ctx,
            json!({
                "language": "py",
                "code": "%%capture captured_out\nprint('hidden-inside')"
            }),
        )
        .await
        .expect("capture");
    assert!(
        !cap_m.is_error && !cap_m.content.contains("hidden-inside"),
        "{}",
        cap_m.content
    );
    let read_cap = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "captured_out.stdout.strip()" }),
        )
        .await
        .expect("read cap");
    assert!(
        read_cap.content.contains("hidden-inside"),
        "{}",
        read_cap.content
    );

    let unk = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "%not_a_real_magic" }),
        )
        .await
        .expect("unknown magic");
    assert!(
        unk.is_error && unk.content.contains("NameError"),
        "{}",
        unk.content
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_bang_cmd() {
    if !has_python() || (resolve_on_path("bash").is_none() && !cfg!(unix)) {
        return;
    }
    let dir = temp_dir("bang-cmd");
    let tool = make_tool(
        dir.join("data"),
        Arc::new(MockHost::default()),
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-bang", dir.clone(), dir.join("local"));

    let out = tool
        .execute(&ctx, json!({ "language": "py", "code": "!echo hi" }))
        .await
        .expect("bang");
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content.trim(), "hi");

    let cap = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "val = !echo captured\nval" }),
        )
        .await
        .expect("bang assign");
    assert!(!cap.is_error, "{}", cap.content);
    assert!(cap.content.contains("captured"), "{}", cap.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_output_spill_meta() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("spill-meta");
    let tool = make_tool(
        dir.join("data"),
        Arc::new(MockHost::default()),
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-spill", dir.clone(), dir.join("local"));

    // Emit > 60 KiB across many lines plus an inline image via output(image=...).
    let code = r#"
for i in range(1500):
    print(f"row-{i:04d}: " + ("x" * 35))
output(image=b"\x89PNG\r\n\x1a\n", media_type="image/png", label="plot.png")
"#;
    let out = tool
        .execute(&ctx, json!({ "language": "py", "code": code }))
        .await
        .expect("spill");
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.len() <= 55 * 1024,
        "inline too large: {}",
        out.content.len()
    );
    assert!(out.content.contains("local://1.log"), "{}", out.content);
    let meta = out.meta.as_ref().expect("expected spill meta");
    assert_eq!(meta["spill"], "local://1.log");
    assert!(meta["total_bytes"].as_u64().unwrap_or(0) >= 60_000);
    assert_eq!(out.images.len(), 1);
    assert_eq!(out.images[0].label.as_deref(), Some("plot.png"));

    // Spilled file is readable whole through local://1.log.
    let UrlBody::Path(spilled_path) = ctx.urls.read(&ctx, "local", "1.log").expect("read spill")
    else {
        panic!("expected file path");
    };
    let full = std::fs::read_to_string(spilled_path).expect("read full");
    assert!(full.contains("row-0000:") && full.contains("row-1499:"));
    assert!(full.len() >= 60_000);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn eval_default_timeout_30() {
    let cfg = EvalConfig::default();
    assert!(cfg.py);
    assert!(cfg.js);
    assert!(cfg.tools_enabled);
    assert_eq!(cfg.default_timeout_secs, DEFAULT_TIMEOUT_SECS);
    assert_eq!(cfg.default_timeout_secs, 30);
    assert_eq!(cfg.default_timeout().map(|d| d.as_secs()), Some(30));

    let zero = EvalConfig {
        default_timeout_secs: 0,
        ..EvalConfig::default()
    };
    assert_eq!(zero.default_timeout(), None);
    let huge = EvalConfig {
        default_timeout_secs: 99_999,
        ..EvalConfig::default()
    };
    assert_eq!(huge.default_timeout().map(|d| d.as_secs()), Some(3600));
    // With finite default (30s): negative, NaN, inf, and 0.0 are rejected; huge clamps to 3600s.
    assert!(cfg.resolve_timeout(Some(-1.0)).is_err());
    assert!(cfg.resolve_timeout(Some(f64::NAN)).is_err());
    assert!(cfg.resolve_timeout(Some(f64::INFINITY)).is_err());
    assert!(cfg.resolve_timeout(Some(0.0)).is_err());
    assert_eq!(
        cfg.resolve_timeout(Some(9999.0))
            .expect("clamp")
            .map(|d| d.as_secs()),
        Some(3600)
    );
    // With operator default = 0: explicit 0.0 disables timeout cleanly.
    assert_eq!(zero.resolve_timeout(Some(0.0)).expect("zero ok"), None);

    let tool = EvalTool::new(std::env::temp_dir());
    let spec = tool.spec();
    let desc = spec.parameters["properties"]["timeout"]["description"]
        .as_str()
        .unwrap_or("");
    assert!(desc.contains("default 30"), "{desc}");
}

struct RecordingSink {
    deltas: Mutex<Vec<String>>,
}
impl EventSink for RecordingSink {
    fn emit(&self, event: AgentEvent) -> BoxFuture<bool> {
        if let AgentEvent::ToolDelta { delta, .. } = event {
            self.deltas.lock().expect("lock").push(delta);
        }
        Box::pin(async { true })
    }
}

#[tokio::test]
async fn eval_delta_and_output_redact_bridge_token() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("redact-delta");
    let tool = make_tool(
        dir.join("data"),
        Arc::new(MockHost::default()),
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let sink = Arc::new(RecordingSink {
        deltas: Mutex::new(Vec::new()),
    });
    let mut ctx = make_ctx("s-redact", dir.clone(), dir.join("local"));
    ctx.events = Arc::clone(&sink) as Arc<dyn EventSink>;

    let _ = tool
        .execute(&ctx, json!({ "language": "py", "code": "1 + 1" }))
        .await
        .expect("warmup");
    let token = tool
        .test_bridge_token("s-redact", Lang::Py)
        .await
        .expect("token");
    sink.deltas.lock().expect("lock").clear();

    let out = tool
        .execute(
            &ctx,
            json!({
                "language": "py",
                "code": format!("print('leak:' + {token:?}, flush=True)\n'done'")
            }),
        )
        .await
        .expect("print token");
    assert!(!out.content.contains(&token), "{}", out.content);
    assert!(
        out.content.contains("[REDACTED_EVAL_TOKEN]"),
        "{}",
        out.content
    );
    let deltas = sink.deltas.lock().expect("lock").join("");
    assert!(
        !deltas.contains(&token),
        "raw token leaked in delta: {deltas}"
    );
    assert!(
        deltas.contains("[REDACTED_EVAL_TOKEN]"),
        "expected redacted delta, got: {deltas}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_python_startup_ignores_cwd_stdlib_shadow() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("stdlib-shadow");
    let marker = dir.join("pwned.txt");
    std::fs::write(
        dir.join("subprocess.py"),
        format!(
            "with open({:?}, 'w') as f:\n    f.write('pwned')\nraise RuntimeError('hijacked')\n",
            marker.to_string_lossy()
        ),
    )
    .expect("write trap");
    let tool = make_tool(
        dir.join("data"),
        Arc::new(MockHost::default()),
        Arc::new(EvalRuntimeHints::default()),
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-shadow-stdlib", dir.clone(), dir.join("local"));
    let out = tool
        .execute(&ctx, json!({ "language": "py", "code": "1 + 1" }))
        .await
        .expect("eval");
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "2");
    assert!(
        !marker.exists(),
        "workspace subprocess.py MUST NOT execute during kernel bootstrap"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_load_rejects_oversized_and_directory() {
    let dir = temp_dir("load-bounds");
    let dir_err = read_bounded_file(&dir, "dir")
        .await
        .expect_err("directory must be rejected");
    assert!(
        dir_err.to_string().contains("not a regular file"),
        "{dir_err}"
    );

    let big = dir.join("big.py");
    std::fs::write(&big, vec![b'a'; (4 * 1024 * 1024) + 16]).expect("write big");
    let big_err = read_bounded_file(&big, "big.py")
        .await
        .expect_err("oversized file must be rejected");
    assert!(big_err.to_string().contains("exceeds"), "{big_err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eval_tool_rejects_recursive_eval_and_shadowing() {
    if !has_python() {
        return;
    }
    let dir = temp_dir("rec-shadow");
    let hints = Arc::new(EvalRuntimeHints::default());
    hints.set_registered_tool_names(["custom_host_tool".to_owned()]);
    let tool = make_tool(
        dir.join("data"),
        Arc::new(MockHost::with_tools(&["read", "eval"])),
        hints,
        EvalConfig::default(),
    );
    let ctx = make_ctx("s-rec-shadow", dir.clone(), dir.join("local"));

    // Recursive tool.eval is refused without hanging.
    let rec = tool
        .execute(
            &ctx,
            json!({ "language": "py", "code": "tool.eval({'language': 'py', 'code': '1+1'})" }),
        )
        .await
        .expect("exec");
    assert!(
        rec.is_error && rec.content.contains("recursively"),
        "{}",
        rec.content
    );

    // @tool shadowing built-in, mcp__*, or host-registered tool names is rejected.
    for bad_name in ["read", "mcp__srv__op", "custom_host_tool"] {
        let out = tool
            .execute(
                &ctx,
                json!({
                    "language": "py",
                    "code": format!("@tool(name={bad_name:?})\ndef fn_x():\n    return 1")
                }),
            )
            .await
            .expect("exec");
        assert!(
            out.is_error,
            "expected shadowing `{bad_name}` to fail, got {}",
            out.content
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
