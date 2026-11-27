use super::*;
use crate::repair::{PostEditRepair, SyntaxChecker};
use hrn_edge_ast::protocol::SyntaxData;
use hrn_edge_tools::{EditModeHint, FileReadTracker, read_sync_with_anchors};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::testing::{TempDir, tool_context};
use hrn_kernel::{
    BoxFuture, CancelFlag, ChunkSink, FinishReason, Host, LlmProvider, LlmRequest, LlmResponse,
    ModelInfo, Registered, Result, ScopedFuture, Shared,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct MockLlm {
    models: Vec<ModelInfo>,
    reply: String,
    calls: AtomicUsize,
}

impl MockLlm {
    fn with_model(provider: &str, id: &str, alias: &str) -> Self {
        Self {
            models: vec![ModelInfo {
                alias: alias.to_owned(),
                id: id.to_owned(),
                provider: provider.to_owned(),
                default: true,
            }],
            reply: String::new(),
            calls: AtomicUsize::new(0),
        }
    }

    fn with_reply(reply: &str) -> Self {
        Self {
            models: vec![
                ModelInfo {
                    alias: "default".to_owned(),
                    id: "claude-sonnet".to_owned(),
                    provider: "anthropic".to_owned(),
                    default: true,
                },
                ModelInfo {
                    alias: "smol".to_owned(),
                    id: "claude-haiku".to_owned(),
                    provider: "anthropic".to_owned(),
                    default: false,
                },
            ],
            reply: reply.to_owned(),
            calls: AtomicUsize::new(0),
        }
    }
}

impl LlmProvider for MockLlm {
    fn stream_chat<'a>(
        &'a self,
        _request: &'a LlmRequest,
        _cancel: &'a CancelFlag,
        _sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let reply = self.reply.clone();
        Box::pin(async move {
            Ok(LlmResponse {
                content: reply,
                reasoning: String::new(),
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                usage: None,
                model_used: "smol".to_owned(),
                fallback_from: None,
                finish_reason: FinishReason::Stop,
            })
        })
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
}

fn reg_with(
    config: Value,
    llm: Option<Arc<dyn LlmProvider>>,
    shared: Arc<Shared>,
    env_variant: Option<&str>,
    strict_env: Option<&str>,
) -> Registered {
    let table: BTreeMap<String, Value> = match config {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    };
    let host = Host {
        cwd: ".".into(),
        data_dir: ".".into(),
    };
    let mut reg = Registry::new(host, table, shared);
    if let Some(provider) = llm {
        reg = reg.with_llm(provider);
    }
    reg.module("edit", |r| {
        let raw: config::RawEditTable = r.config()?;
        let parsed = EditConfig::from_raw(raw)?;
        register_with_env(r, &parsed, env_variant, strict_env)
    })
    .and_then(Registry::finish)
    .expect("edit registers")
}

#[test]
fn edit_mode_none_unregisters() {
    let shared = Arc::new(Shared::new());
    let reg = reg_with(
        json!({"edit": {"mode": "none"}}),
        None,
        Arc::clone(&shared),
        None,
        None,
    );
    assert!(
        reg.tools.iter().all(|t| t.spec().name != "edit"),
        "mode=none must unregister edit tool"
    );
    let hint = shared
        .get_or_init(|| Ok(EditModeHint::new()))
        .expect("hint");
    assert!(
        !hint.hashline_anchors(),
        "mode=none must drop hashline anchors in shared EditModeHint"
    );

    // Verify `read` drops `#TAG` anchors when consulting the shared hint.
    let dir = TempDir::new("edit-none-read");
    std::fs::write(dir.path().join("demo.rs"), "fn a() {}\n").expect("write");
    let tracker = shared
        .get_or_init(|| Ok(FileReadTracker::new()))
        .expect("tracker");
    let ctx = tool_context(dir.path().to_path_buf());
    let out =
        read_sync_with_anchors(&tracker, &ctx, "demo.rs", hint.hashline_anchors()).expect("read");
    assert!(!out.content.contains("[demo.rs#"), "{}", out.content);
    assert!(out.content.starts_with("File: demo.rs"), "{}", out.content);
}

#[test]
fn model_variants_select_replace() {
    let shared = Arc::new(Shared::new());
    let llm = Arc::new(MockLlm::with_model("zai", "glm-4.5-air", "glm"));
    let reg = reg_with(
        json!({
            "edit": {
                "model_variants": {
                    "glm": "hashline",
                    "glm-4.5": "replace"
                }
            }
        }),
        Some(llm),
        Arc::clone(&shared),
        None,
        None,
    );
    let edit_tool = reg
        .tools
        .iter()
        .find(|t| t.spec().name == "edit")
        .expect("edit registered");
    assert!(
        edit_tool.spec().parameters["properties"]["old"].is_object(),
        "longest substring `glm-4.5` must select replace mode"
    );

    // Config order breaks ties on equal-length matching substrings.
    let cfg = EditConfig::default();
    let tied = cfg
        .resolve_mode_with_order(
            &[
                ("foo".to_owned(), "replace".to_owned()),
                ("bar".to_owned(), "hashline".to_owned()),
            ],
            &["acme/foo-bar".to_owned()],
            None,
            None,
        )
        .expect("resolve");
    assert_eq!(tied.mode, EditMode::Replace);

    // Data-driven model-family fallback picks `replace` for deepseek unless HRN_STRICT_EDIT_MODE.
    let fallback = cfg
        .resolve_mode(&["deepseek/deepseek-chat".to_owned()], None, None)
        .expect("fallback");
    assert_eq!(fallback.mode, EditMode::Replace);
    let strict = cfg
        .resolve_mode(&["deepseek/deepseek-chat".to_owned()], None, Some("1"))
        .expect("strict");
    assert_eq!(strict.mode, EditMode::Hashline);

    // Aliases `patch` and `sloppy` map to replace; `HRN_EDIT_VARIANT` pins mode unless `auto`.
    let sloppy = cfg
        .resolve_mode(&["anthropic/claude".to_owned()], Some("sloppy"), None)
        .expect("sloppy");
    assert_eq!(sloppy.mode, EditMode::Replace);
}

#[tokio::test]
async fn block_auto_generated_cites_glob() {
    let dir = TempDir::new("edit-gen-block");
    let lock = dir.path().join("Cargo.lock");
    std::fs::write(&lock, "version = 3\n").expect("write");

    let shared = Arc::new(Shared::new());
    let reg = reg_with(
        json!({"edit": {"mode": "replace", "enforce_seen_lines": false}}),
        None,
        Arc::clone(&shared),
        None,
        None,
    );
    let edit_tool = reg
        .tools
        .iter()
        .find(|t| t.spec().name == "edit")
        .expect("edit");
    let ctx = tool_context(dir.path().to_path_buf());
    let err = edit_tool
        .execute(
            &ctx,
            json!({"path": "Cargo.lock", "old": "version = 3", "new": "version = 4"}),
        )
        .await
        .expect_err("Cargo.lock must be refused")
        .to_string();
    assert!(err.contains("Cargo.lock"), "{err}");
    assert!(err.contains("matched glob"), "{err}");

    // Also works for wildcard globs like `*.generated.*`.
    std::fs::write(dir.path().join("api.generated.ts"), "export const x = 1;\n").expect("gen");
    let err_gen = edit_tool
        .execute(
            &ctx,
            json!({"path": "api.generated.ts", "old": "x = 1", "new": "x = 2"}),
        )
        .await
        .expect_err("*.generated.* must be refused")
        .to_string();
    assert!(err_gen.contains("*.generated.*"), "{err_gen}");

    // Setting `block_auto_generated = false` allows editing Cargo.lock.
    let allowed_reg = reg_with(
        json!({
            "edit": {
                "mode": "replace",
                "block_auto_generated": false,
                "enforce_seen_lines": false
            }
        }),
        None,
        Arc::new(Shared::new()),
        None,
        None,
    );
    let allowed_tool = allowed_reg
        .tools
        .iter()
        .find(|t| t.spec().name == "edit")
        .expect("edit");
    allowed_tool
        .execute(
            &ctx,
            json!({"path": "Cargo.lock", "old": "version = 3", "new": "version = 4"}),
        )
        .await
        .expect("allowed when block_auto_generated=false");
    assert_eq!(
        std::fs::read_to_string(&lock).expect("read"),
        "version = 4\n"
    );
}

#[test]
fn magic_mode_is_parsed_and_ignored_with_debug_note() {
    let shared = Arc::new(Shared::new());
    let reg = reg_with(
        json!({"edit": {"mode": "magic"}}),
        None,
        Arc::clone(&shared),
        None,
        None,
    );
    assert!(reg.tools.iter().any(|t| t.spec().name == "edit"));
    let hint = shared
        .get_or_init(|| Ok(EditModeHint::new()))
        .expect("hint");
    assert!(
        hint.debug_notes().iter().any(|n| n.contains("magic")),
        "{:?}",
        hint.debug_notes()
    );
}

struct BraceChecker;
impl SyntaxChecker for BraceChecker {
    fn check(
        &self,
        _path: &Path,
        content: &str,
        _cancel: &CancelFlag,
    ) -> BoxFuture<Result<SyntaxData>> {
        let opens = content.matches('{').count();
        let closes = content.matches('}').count();
        let diff = opens.abs_diff(closes);
        Box::pin(async move {
            Ok(SyntaxData {
                supported: true,
                error_count: diff,
                errors: if diff > 0 {
                    vec!["line 1:1: unbalanced brace".to_owned()]
                } else {
                    Vec::new()
                },
            })
        })
    }
}

#[tokio::test]
async fn auto_repair_accepts_clean_reparse_and_discards_broken_repair() {
    let dir = TempDir::new("edit-autorepair");
    let file = dir.path().join("lib.rs");
    std::fs::write(&file, "fn ok() {\n    let x = 1;\n}\n").expect("write");
    let tracker = Arc::new(FileReadTracker::new());
    let ctx = tool_context(dir.path().to_path_buf());

    // 1. Clean repair from @smol is accepted after reparse confirms zero syntax errors.
    let good_llm = Arc::new(MockLlm::with_reply(
        "```rust\nfn ok() {\n    let x = 2;\n}\n```",
    ));
    let repair_ok =
        PostEditRepair::with_checker(true, false, false, Some(good_llm), Arc::new(BraceChecker));
    let before = repair_ok.capture_before(std::slice::from_ref(&file));
    // Simulate a broken edit written to disk (missing closing brace).
    std::fs::write(&file, "fn ok() {\n    let x = 2;\n").expect("broken write");
    let out = repair_ok
        .run(
            &ctx,
            &tracker,
            &before,
            hrn_kernel::ToolOutput::ok("edited"),
        )
        .await
        .expect("repair run");
    assert!(out.content.contains("reparse clean"), "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&file).expect("read"),
        "fn ok() {\n    let x = 2;\n}\n"
    );

    // 2. Broken repair from @smol is rejected; blackbox appends full source.
    let bad_llm = Arc::new(MockLlm::with_reply("```rust\nfn ok() {{\n```"));
    let repair_bad =
        PostEditRepair::with_checker(true, true, false, Some(bad_llm), Arc::new(BraceChecker));
    let before2 = repair_bad.capture_before(std::slice::from_ref(&file));
    std::fs::write(&file, "fn ok() {\n    let x = 3;\n").expect("broken write");
    let out2 = repair_bad
        .run(
            &ctx,
            &tracker,
            &before2,
            hrn_kernel::ToolOutput::ok("edited"),
        )
        .await
        .expect("repair bad run");
    assert!(out2.content.contains("discarded"), "{}", out2.content);
    assert!(out2.content.contains("[blackbox"), "{}", out2.content);
    assert_eq!(
        std::fs::read_to_string(&file).expect("read"),
        "fn ok() {\n    let x = 3;\n"
    );
}

#[tokio::test]
async fn auto_repair_preserves_bom_crlf_and_rejects_unseen_line_edits() {
    use std::fmt::Write as _;
    let dir = TempDir::new("edit-autorepair-sec");
    let file = dir.path().join("lib.rs");
    let mut tail_crlf = String::new();
    let mut tail_lf = String::new();
    let mut sneaked_tail = String::new();
    for i in 4..=20 {
        let _ = write!(tail_crlf, "// tail line {i}\r\n");
        let _ = writeln!(tail_lf, "// tail line {i}");
        if i == 19 {
            sneaked_tail.push_str("// sneaked unseen edit\n");
        } else {
            let _ = writeln!(sneaked_tail, "// tail line {i}");
        }
    }
    let initial = format!("\u{feff}fn ok() {{\r\n    let x = 1;\r\n}}\r\n{tail_crlf}");
    std::fs::write(&file, &initial).expect("write bom crlf");
    let tracker = Arc::new(FileReadTracker::new());
    let ctx = tool_context(dir.path().to_path_buf());
    // Read only lines 1-3 (with context -> 1-6).
    read_sync_with_anchors(&tracker, &ctx, "lib.rs:1-3", true).expect("read window");

    // 1. Repair that modifies an unseen tail line (line 19) is rejected when enforce_seen_lines=true.
    let sneaked = format!("fn ok() {{\n    let x = 2;\n}}\n{sneaked_tail}");
    let sneak_llm = Arc::new(MockLlm::with_reply(&format!("```rust\n{sneaked}```")));
    let repair_sneak =
        PostEditRepair::with_checker(true, false, true, Some(sneak_llm), Arc::new(BraceChecker));
    let before = repair_sneak.capture_before(std::slice::from_ref(&file));
    let broken = format!("\u{feff}fn ok() {{\r\n    let x = 2;\r\n{tail_crlf}");
    std::fs::write(&file, &broken).expect("broken write");
    let out_sneak = repair_sneak
        .run(
            &ctx,
            &tracker,
            &before,
            hrn_kernel::ToolOutput::ok("edited"),
        )
        .await
        .expect("sneak run");
    assert!(
        out_sneak.content.contains("touched unseen lines"),
        "{}",
        out_sneak.content
    );
    assert_eq!(std::fs::read(&file).expect("unchanged"), broken.as_bytes());

    // 2. Clean repair touching ONLY seen lines preserves UTF-8 BOM and CRLF verbatim.
    let clean = format!("fn ok() {{\n    let x = 2;\n}}\n{tail_lf}");
    let clean_llm = Arc::new(MockLlm::with_reply(&format!("```rust\n{clean}```")));
    let repair_clean =
        PostEditRepair::with_checker(true, false, true, Some(clean_llm), Arc::new(BraceChecker));
    let out_clean = repair_clean
        .run(
            &ctx,
            &tracker,
            &before,
            hrn_kernel::ToolOutput::ok("edited"),
        )
        .await
        .expect("clean run");
    assert!(
        out_clean.content.contains("reparse clean"),
        "{}",
        out_clean.content
    );
    let expected = format!("\u{feff}{}", clean.replace('\n', "\r\n"));
    assert_eq!(
        std::fs::read(&file).expect("repaired bytes"),
        expected.as_bytes()
    );
}
