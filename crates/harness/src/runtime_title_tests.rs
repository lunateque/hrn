//! Acceptance and unit tests for model auto-titles, revision guards, and todo-replan refresh.

use crate::agents::{AgentRegistry, Spawn};
use crate::control::SessionControl;
use crate::registry::ToolRegistry;
use crate::runtime::parse_title_response;
use crate::runtime::{AgentRuntime, HarnessConfig, TurnIo};
use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
use hrn_kernel::{
    AgentEvent, BoxFuture, CancelFlag, ChunkSink, FinishReason, HrnError, LlmChunk, LlmProvider,
    LlmRequest, LlmResponse, ModelInfo, PermissionMode, Result, ScopedFuture, SessionEffect,
    SessionStore, ThinkingLevel, TitleSource, TodoItem, TodoPhase, TodoStatus, Tool, ToolCall,
    ToolContext, ToolOutput, ToolSpec,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

fn ok_reply(text: &str, model: &str) -> LlmResponse {
    LlmResponse {
        model_used: model.to_owned(),
        fallback_from: None,
        content: text.to_owned(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        finish_reason: FinishReason::Stop,
    }
}

fn call_reply(tool_name: &str) -> LlmResponse {
    LlmResponse {
        model_used: "main".to_owned(),
        fallback_from: None,
        content: String::new(),
        reasoning: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            id: "call_todo_1".to_owned(),
            name: tool_name.to_owned(),
            arguments: "{}".to_owned(),
            signature: None,
        }],
        usage: None,
        finish_reason: FinishReason::ToolCalls,
    }
}

fn make_io() -> TurnIo {
    TurnIo {
        cancel: CancelFlag::new(),
        events: Arc::new(VecEvents::default()),
        approval: None,
        ask: None,
    }
}

fn make_runtime(
    responses: Vec<Result<LlmResponse>>,
    auto_title: bool,
    refresh_on_replan: bool,
    tools: ToolRegistry,
) -> (AgentRuntime, Arc<ScriptedLlm>, Arc<MemStore>) {
    let llm = Arc::new(ScriptedLlm {
        responses: Mutex::new(VecDeque::from(responses)),
        requests: Mutex::new(Vec::new()),
        models: Vec::new(),
    });
    let store = Arc::new(MemStore::default());
    let rt = AgentRuntime::new(
        llm.clone(),
        store.clone(),
        tools,
        Arc::new(MemFs::default()),
        HarnessConfig {
            auto_title,
            title_refresh_on_replan: refresh_on_replan,
            ..HarnessConfig::default()
        },
    );
    (rt, llm, store)
}

#[test]
fn title_generated_after_first_turn() {
    // First title role fails -> falls back to `tiny`, demonstrating `title -> tiny -> smol -> model`.
    let (rt, llm, store) = make_runtime(
        vec![
            Ok(ok_reply(
                "Implemented the SQLite WAL migration and added unit tests.",
                "main",
            )),
            Err(HrnError::Config("unknown role `title`".to_owned())),
            Ok(ok_reply("<title>Fix SQLite WAL Migration</title>", "tiny")),
        ],
        true,
        true,
        ToolRegistry::new(),
    );
    let session = rt
        .create_session(
            "",
            Path::new("/repo"),
            Some("main".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    block_on(rt.run_turn(
        &session.id,
        "Fix the SQLite WAL migration bug in store",
        &SessionControl::default(),
        make_io(),
    ))
    .expect("turn 1");

    let updated = store.get_session(&session.id).expect("get").expect("some");
    assert_eq!(updated.title, "Fix SQLite WAL Migration");
    assert_eq!(updated.title_source, Some(TitleSource::Auto));

    let reqs = llm.requests.lock().expect("reqs");
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[1].model.as_deref(), Some("title"));
    assert_eq!(reqs[2].model.as_deref(), Some("tiny"));
}

#[test]
fn user_title_blocks_auto() {
    let (rt, llm, store) = make_runtime(
        vec![Ok(ok_reply("All storage tests now pass.", "main"))],
        true,
        true,
        ToolRegistry::new(),
    );
    let session = rt
        .create_session(
            "Initial",
            Path::new("/repo"),
            Some("main".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    rt.rename_session(&session.id, "Pinned User Title", TitleSource::User)
        .expect("user rename");

    block_on(rt.run_turn(
        &session.id,
        "Refactor session storage layer",
        &SessionControl::default(),
        make_io(),
    ))
    .expect("turn 1");

    let updated = store.get_session(&session.id).expect("get").expect("some");
    assert_eq!(updated.title, "Pinned User Title");
    assert_eq!(updated.title_source, Some(TitleSource::User));
    assert_eq!(
        llm.requests.lock().expect("reqs").len(),
        1,
        "user title must block any auto-title LLM call"
    );
}

#[test]
fn title_none_leaves_untitled() {
    for reply in ["<title/>", "none", "NONE", "<title>none</title>"] {
        let (rt, _llm, store) = make_runtime(
            vec![
                Ok(ok_reply("Checked the status.", "main")),
                Ok(ok_reply(reply, "title")),
            ],
            true,
            true,
            ToolRegistry::new(),
        );
        let session = rt
            .create_session(
                "",
                Path::new("/repo"),
                Some("main".into()),
                ThinkingLevel::Off,
                PermissionMode::Yolo,
            )
            .expect("session");
        block_on(rt.run_turn(
            &session.id,
            "Check repository status for me",
            &SessionControl::default(),
            make_io(),
        ))
        .expect("turn 1");

        let updated = store.get_session(&session.id).expect("get").expect("some");
        assert!(
            updated.title.is_empty(),
            "reply `{reply}` must leave session untitled, got `{}`",
            updated.title
        );
        assert_eq!(updated.title_source, None);
    }
}

#[test]
fn no_title_flag_no_request() {
    let (rt, llm, store) = make_runtime(
        vec![Ok(ok_reply("Completed the requested refactor.", "main"))],
        false,
        false,
        ToolRegistry::new(),
    );
    let session = rt
        .create_session(
            "",
            Path::new("/repo"),
            Some("main".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    block_on(rt.run_turn(
        &session.id,
        "Refactor configuration parser",
        &SessionControl::default(),
        make_io(),
    ))
    .expect("turn 1");

    assert_eq!(
        llm.requests.lock().expect("reqs").len(),
        1,
        "--no-title must suppress all title model requests"
    );
    let updated = store.get_session(&session.id).expect("get").expect("some");
    assert!(updated.title.is_empty());
    assert_eq!(updated.title_source, None);
}

struct ReplanTool;

impl Tool for ReplanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo".to_owned(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }

    fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
        let mut out = ToolOutput::ok("todo updated");
        out.session_effect = Some(SessionEffect::TodoEdit {
            phases: vec![TodoPhase {
                name: "Migration".to_owned(),
                tasks: vec![TodoItem {
                    content: "Implement Codex rollout importer".to_owned(),
                    status: TodoStatus::InProgress,
                }],
            }],
        });
        Box::pin(async move { Ok(out) })
    }
}

#[test]
fn bare_rename_regenerates_user_title_and_replan_refreshes_auto_title() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ReplanTool));
    let (rt, _llm, store) = make_runtime(
        vec![
            Ok(ok_reply("Started inspecting session storage.", "main")),
            Ok(ok_reply("<title>Inspect Session Storage</title>", "title")),
            Ok(call_reply("todo")),
            Ok(ok_reply(
                "Replanned to implement Codex rollout importer.",
                "main",
            )),
            Ok(ok_reply(
                "<title>Implement Codex Rollout Importer</title>",
                "title",
            )),
            Ok(ok_reply(
                "<title>Codex Transcript Importer</title>",
                "title",
            )),
        ],
        true,
        true,
        tools,
    );
    let session = rt
        .create_session(
            "",
            Path::new("/repo"),
            Some("main".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");

    block_on(rt.run_turn(
        &session.id,
        "Inspect session storage architecture",
        &SessionControl::default(),
        make_io(),
    ))
    .expect("turn 1");
    let after1 = store.get_session(&session.id).expect("get").expect("some");
    assert_eq!(after1.title, "Inspect Session Storage");
    assert_eq!(after1.title_source, Some(TitleSource::Auto));

    // Turn 2 triggers a todo replan -> auto-title refreshes.
    block_on(rt.run_turn(
        &session.id,
        "Pivot to building the Codex rollout importer",
        &SessionControl::default(),
        make_io(),
    ))
    .expect("turn 2");
    let after2 = store.get_session(&session.id).expect("get").expect("some");
    assert_eq!(after2.title, "Implement Codex Rollout Importer");
    assert_eq!(after2.title_source, Some(TitleSource::Auto));

    // Bare `/rename` regenerates and persists as TitleSource::User.
    let regen = block_on(rt.regenerate_title(&session.id, &CancelFlag::new())).expect("regen");
    assert_eq!(regen.as_deref(), Some("Codex Transcript Importer"));
    let after3 = store.get_session(&session.id).expect("get").expect("some");
    assert_eq!(after3.title, "Codex Transcript Importer");
    assert_eq!(after3.title_source, Some(TitleSource::User));
}

#[test]
fn parse_title_response_handles_edge_cases() {
    assert_eq!(
        parse_title_response("<title>Add GC Command</title>"),
        Some("Add GC Command".to_owned())
    );
    assert_eq!(
        parse_title_response("<TITLE>  \"Fix Bug.\"  </TITLE>"),
        Some("Fix Bug".to_owned())
    );
    assert_eq!(parse_title_response("<title/>"), None);
    assert_eq!(parse_title_response("<title />"), None);
    assert_eq!(parse_title_response("none"), None);
    assert_eq!(parse_title_response("unwrapped chatty text"), None);
}

#[test]
fn trivial_russian_greetings_skip_auto_title_but_substantive_prompts_run() {
    for greeting in ["Привет", "Привет!", "привет.", "Спасибо", "ок", "Тест"]
    {
        let (rt, llm, store) = make_runtime(
            vec![Ok(ok_reply("Привет. Что делаем?", "main"))],
            true,
            true,
            ToolRegistry::new(),
        );
        let session = rt
            .create_session(
                "",
                Path::new("/repo"),
                Some("main".into()),
                ThinkingLevel::Off,
                PermissionMode::Yolo,
            )
            .expect("session");
        block_on(rt.run_turn(&session.id, greeting, &SessionControl::default(), make_io()))
            .expect("turn");
        assert_eq!(
            llm.requests.lock().expect("reqs").len(),
            1,
            "greeting `{greeting}` must not trigger an auto-title LLM call"
        );
        let updated = store.get_session(&session.id).expect("get").expect("some");
        assert!(updated.title.is_empty());
    }

    let (rt, llm, store) = make_runtime(
        vec![
            Ok(ok_reply("Починил проверку токена авторизации.", "main")),
            Ok(ok_reply("<title>Починка авторизации</title>", "title")),
        ],
        true,
        true,
        ToolRegistry::new(),
    );
    let session = rt
        .create_session(
            "",
            Path::new("/repo"),
            Some("main".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    block_on(rt.run_turn(
        &session.id,
        "Почини баг в авторизации",
        &SessionControl::default(),
        make_io(),
    ))
    .expect("turn");
    assert_eq!(llm.requests.lock().expect("reqs").len(), 2);
    let updated = store.get_session(&session.id).expect("get").expect("some");
    assert_eq!(updated.title, "Починка авторизации");
    assert_eq!(updated.title_source, Some(TitleSource::Auto));
}

#[test]
fn request_title_skips_redundant_role_retries_on_provider_error() {
    let (rt, llm, store) = make_runtime(
        vec![
            Ok(ok_reply("Fixed the parser edge case.", "main")),
            Err(HrnError::Provider {
                message: "upstream timeout".to_owned(),
                retryable: true,
                retry_after_secs: None,
            }),
            Ok(ok_reply("<title>Fix Parser Edge Case</title>", "main")),
        ],
        true,
        true,
        ToolRegistry::new(),
    );
    let session = rt
        .create_session(
            "",
            Path::new("/repo"),
            Some("main".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");
    block_on(rt.run_turn(
        &session.id,
        "Fix the parser edge case in lexer",
        &SessionControl::default(),
        make_io(),
    ))
    .expect("turn");

    let reqs = llm.requests.lock().expect("reqs");
    let asked: Vec<Option<&str>> = reqs.iter().map(|r| r.model.as_deref()).collect();
    assert_eq!(asked, vec![Some("main"), Some("title"), Some("main")]);
    let updated = store.get_session(&session.id).expect("get").expect("some");
    assert_eq!(updated.title, "Fix Parser Edge Case");
}

struct ThreadSpawner {
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    armed_delays: Mutex<Vec<Duration>>,
    cancel_immediately: bool,
}

impl ThreadSpawner {
    fn new(cancel_immediately: bool) -> Self {
        Self {
            handles: Mutex::new(Vec::new()),
            armed_delays: Mutex::new(Vec::new()),
            cancel_immediately,
        }
    }

    fn join_all(&self) {
        let handles: Vec<_> = self.handles.lock().expect("handles").drain(..).collect();
        for h in handles {
            h.join().expect("background auto-title thread");
        }
    }
}

impl Spawn for ThreadSpawner {
    fn spawn(&self, task: BoxFuture<()>) -> Result {
        let handle = std::thread::spawn(move || block_on(task));
        self.handles.lock().expect("handles").push(handle);
        Ok(())
    }

    fn cancel_after(&self, flag: CancelFlag, delay: Duration) {
        self.armed_delays.lock().expect("delays").push(delay);
        if self.cancel_immediately {
            flag.cancel();
        }
    }
}

struct GatedTitleLlm {
    release: Mutex<Option<mpsc::Receiver<()>>>,
    title_started: Mutex<Option<mpsc::Sender<()>>>,
    requests: Mutex<Vec<Option<String>>>,
}

impl LlmProvider for GatedTitleLlm {
    fn stream_chat<'a>(
        &'a self,
        request: &'a LlmRequest,
        cancel: &'a CancelFlag,
        sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>> {
        self.requests
            .lock()
            .expect("reqs")
            .push(request.model.clone());
        let is_title = request.model.as_deref() == Some("title");
        Box::pin(async move {
            if !is_title {
                let text = "Refactored scheduler queue cleanly.".to_owned();
                sink.on_chunk(LlmChunk::ContentDelta(text.clone())).await;
                return Ok(ok_reply(&text, "main"));
            }
            if let Some(tx) = self.title_started.lock().expect("started").take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.release.lock().expect("release").take() {
                let _ = rx.recv_timeout(Duration::from_secs(5));
            }
            if cancel.is_cancelled() {
                return Err(HrnError::Cancelled);
            }
            Ok(ok_reply("<title>Refactor Scheduler Queue</title>", "title"))
        })
    }

    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
}

#[test]
fn background_auto_title_does_not_block_turn_and_emits_event() {
    let (release_tx, release_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::channel();
    let llm = Arc::new(GatedTitleLlm {
        release: Mutex::new(Some(release_rx)),
        title_started: Mutex::new(Some(started_tx)),
        requests: Mutex::new(Vec::new()),
    });
    let store = Arc::new(MemStore::default());
    let spawner = Arc::new(ThreadSpawner::new(false));
    let agents = Arc::new(AgentRegistry::new(spawner.clone()));
    let rt = AgentRuntime::new(
        llm,
        store.clone(),
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            auto_title: true,
            title_refresh_on_replan: true,
            ..HarnessConfig::default()
        },
    )
    .with_agents(agents);
    let session = rt
        .create_session(
            "",
            Path::new("/repo"),
            Some("main".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");

    let events = Arc::new(VecEvents::default());
    let io = TurnIo {
        cancel: CancelFlag::new(),
        events: events.clone(),
        approval: None,
        ask: None,
    };

    // Turn finishes immediately even though the title LLM call is blocked on `release_rx`.
    let turn = block_on(rt.run_turn(
        &session.id,
        "Refactor the scheduler queue module",
        &SessionControl::default(),
        io,
    ))
    .expect("turn");
    assert_eq!(turn.final_text, "Refactored scheduler queue cleanly.");
    assert!(
        events
            .0
            .lock()
            .expect("events")
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnDone { .. })),
        "TurnDone must be emitted before background auto-title finishes"
    );

    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("background auto-title started");
    let _ = release_tx.send(());
    spawner.join_all();

    assert_eq!(
        spawner.armed_delays.lock().expect("delays").as_slice(),
        &[Duration::from_secs(12)]
    );
    let updated = store.get_session(&session.id).expect("get").expect("some");
    assert_eq!(updated.title, "Refactor Scheduler Queue");
    assert_eq!(updated.title_source, Some(TitleSource::Auto));
    assert!(
        events.0.lock().expect("events").iter().any(|e| matches!(
            e,
            AgentEvent::SessionTitle { session_id, title }
                if session_id == &session.id && title == "Refactor Scheduler Queue"
        )),
        "background auto-title must emit AgentEvent::SessionTitle"
    );
}

#[test]
fn background_auto_title_timeout_cancels_without_blocking() {
    let llm = Arc::new(GatedTitleLlm {
        release: Mutex::new(None),
        title_started: Mutex::new(None),
        requests: Mutex::new(Vec::new()),
    });
    let store = Arc::new(MemStore::default());
    let spawner = Arc::new(ThreadSpawner::new(true));
    let agents = Arc::new(AgentRegistry::new(spawner.clone()));
    let rt = AgentRuntime::new(
        llm.clone(),
        store.clone(),
        ToolRegistry::new(),
        Arc::new(MemFs::default()),
        HarnessConfig {
            auto_title: true,
            title_refresh_on_replan: false,
            ..HarnessConfig::default()
        },
    )
    .with_agents(agents);
    let session = rt
        .create_session(
            "",
            Path::new("/repo"),
            Some("main".into()),
            ThinkingLevel::Off,
            PermissionMode::Yolo,
        )
        .expect("session");

    block_on(rt.run_turn(
        &session.id,
        "Investigate slow storage query plan",
        &SessionControl::default(),
        make_io(),
    ))
    .expect("turn");
    spawner.join_all();

    // Cancelled by `cancel_after` before any title LLM request was even sent.
    assert_eq!(llm.requests.lock().expect("reqs").len(), 1);
    let updated = store.get_session(&session.id).expect("get").expect("some");
    assert!(updated.title.is_empty());
}
