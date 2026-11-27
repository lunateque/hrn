//! Unit tests for `hrn exec` prompt resolution, DB isolation, session resume, and tool scoping.

use super::*;
use crate::cli::{Cli, Command};
use clap::Parser;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_kernel::{
    ChunkSink, LlmProvider, LlmRequest, LlmResponse, ModelInfo, ScopedFuture, Tool, ToolApproval,
    ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use serde_json::Value;
use std::io::Cursor;

struct DummyLlm;

impl LlmProvider for DummyLlm {
    fn stream_chat<'a>(
        &'a self,
        _request: &'a LlmRequest,
        _cancel: &'a CancelFlag,
        _sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>> {
        Box::pin(async { Err(HrnError::Config("dummy llm".to_owned())) })
    }

    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
}

struct FakeTool {
    name: &'static str,
    effect: ToolEffect,
}

impl Tool for FakeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_owned(),
            description: "fake".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn effect(&self) -> ToolEffect {
        self.effect
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        match self.effect {
            ToolEffect::ReadOnly => ToolApproval::read(),
            ToolEffect::Mutating => ToolApproval::write(),
        }
    }

    fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
        Box::pin(async { Ok(ToolOutput::ok("ok")) })
    }
}

fn sample_runtime(store: Arc<SqliteStore>) -> AgentRuntime {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FakeTool {
        name: "read",
        effect: ToolEffect::ReadOnly,
    }));
    tools.register(Arc::new(FakeTool {
        name: "grep",
        effect: ToolEffect::ReadOnly,
    }));
    tools.register(Arc::new(FakeTool {
        name: "write",
        effect: ToolEffect::Mutating,
    }));
    tools.register(Arc::new(FakeTool {
        name: "bash",
        effect: ToolEffect::Mutating,
    }));
    AgentRuntime::new(
        Arc::new(DummyLlm),
        store,
        tools,
        Arc::new(StdWorkspaceFs),
        hrn_harness::HarnessConfig::default(),
    )
}

#[test]
fn exec_bridge_and_isolation_flags_parse() {
    let cli = Cli::try_parse_from([
        "hrn",
        "exec",
        "--resume",
        "sess_42",
        "--db",
        "/tmp/isolated/hrn.db",
        "--no-mcp",
        "--read-only",
        "--tools",
        "read,grep,glob,find",
        "--mode",
        "json",
    ])
    .expect("exec without positional prompt should parse for stdin piping");
    let Some(Command::Exec(args)) = cli.command else {
        unreachable!("exec");
    };
    assert_eq!(args.resume.as_deref(), Some("sess_42"));
    assert_eq!(args.db.as_deref(), Some(Path::new("/tmp/isolated/hrn.db")));
    assert!(args.no_mcp);
    assert!(args.read_only);
    assert_eq!(args.tools.as_deref(), Some("read,grep,glob,find"));
    assert!(!args.no_tools);
    assert!(args.prompt.is_none());

    let no_tools = Cli::try_parse_from(["hrn", "exec", "--no-tools", "-"]).expect("no-tools");
    let Some(Command::Exec(args)) = no_tools.command else {
        unreachable!("exec");
    };
    assert!(args.no_tools);
    assert_eq!(args.prompt.as_deref(), Some("-"));

    assert!(
        Cli::try_parse_from(["hrn", "exec", "--no-tools", "--tools", "read", "hi"]).is_err(),
        "--no-tools and --tools must conflict"
    );

    let bad_resume =
        Cli::try_parse_from(["hrn", "--no-session", "exec", "--resume", "s1", "hi"]).expect("pre");
    assert!(crate::apply_no_session(bad_resume.command, bad_resume.no_session).is_err());

    let bad_db =
        Cli::try_parse_from(["hrn", "--no-session", "exec", "--db", "a.db", "hi"]).expect("pre");
    assert!(crate::apply_no_session(bad_db.command, bad_db.no_session).is_err());
}

#[test]
fn resolve_prompt_handles_positional_dash_stdin_and_goal() {
    assert_eq!(
        resolve_prompt_from(Some("hello"), None, true, Cursor::new("")).expect("positional"),
        "hello"
    );
    assert_eq!(
        resolve_prompt_from(Some("-"), None, true, Cursor::new("from stdin")).expect("dash"),
        "from stdin"
    );
    assert!(resolve_prompt_from(Some("-"), None, true, Cursor::new("   ")).is_err());
    assert_eq!(
        resolve_prompt_from(None, None, false, Cursor::new("piped task")).expect("piped"),
        "piped task"
    );
    assert_eq!(
        resolve_prompt_from(None, Some("ship it"), false, Cursor::new("")).expect("goal fallback"),
        "ship it"
    );
    assert_eq!(
        resolve_prompt_from(None, Some("ship it"), true, Cursor::new("")).expect("goal tty"),
        "ship it"
    );
    assert!(resolve_prompt_from(None, None, true, Cursor::new("")).is_err());
}

#[test]
fn tool_scope_filters_read_only_allowlist_and_no_tools() {
    let store = Arc::new(SqliteStore::open_in_memory().expect("mem"));
    let cwd = std::env::temp_dir();

    let parse_exec = |argv: &[&str]| -> ExecArgs {
        let cli = Cli::try_parse_from(argv).expect("parse");
        let Some(Command::Exec(args)) = cli.command else {
            unreachable!("exec");
        };
        args
    };

    let rt = apply_tool_scope(
        sample_runtime(Arc::clone(&store)),
        &parse_exec(&["hrn", "exec", "--no-tools", "hi"]),
        &cwd,
    );
    assert!(rt.tools().names().is_empty());

    let rt = apply_tool_scope(
        sample_runtime(Arc::clone(&store)),
        &parse_exec(&["hrn", "exec", "--read-only", "hi"]),
        &cwd,
    );
    let mut names = rt.tools().names();
    names.sort();
    assert_eq!(names, vec!["grep", "read"]);

    let rt = apply_tool_scope(
        sample_runtime(Arc::clone(&store)),
        &parse_exec(&["hrn", "exec", "--read-only", "--tools", "read,write", "hi"]),
        &cwd,
    );
    assert_eq!(rt.tools().names(), vec!["read"]);
}

#[test]
fn isolated_db_and_resume_preserve_session_id_and_update_overrides() {
    let dir = std::env::temp_dir().join(format!("hrn-exec-db-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = dir.join("nested").join("advisor.db");
    let store = open_exec_store(&dir.join("default.db"), Some(&db), false).expect("open db");
    assert!(db.is_file(), "isolated DB file should be created");

    let rt = sample_runtime(Arc::clone(&store));
    let cli = Cli::try_parse_from(["hrn", "exec", "first turn"]).expect("parse");
    let Some(Command::Exec(args1)) = cli.command else {
        unreachable!("exec");
    };
    let created = resolve_exec_session(
        &rt,
        &store,
        &dir,
        &args1,
        "first turn",
        "anthropic/claude-opus-5-5".to_owned(),
        ThinkingLevel::Medium,
        PermissionMode::Default,
    )
    .expect("create");

    let prefix: String = created.id.chars().take(8).collect();
    let cli2 = Cli::try_parse_from([
        "hrn",
        "exec",
        "--resume",
        &prefix,
        "--think",
        "high",
        "--yes",
        "second turn",
    ])
    .expect("parse");
    let Some(Command::Exec(args2)) = cli2.command else {
        unreachable!("exec");
    };
    let resumed = resolve_exec_session(
        &rt,
        &store,
        &dir,
        &args2,
        "second turn",
        "anthropic/claude-opus-5-5".to_owned(),
        ThinkingLevel::High,
        PermissionMode::Yolo,
    )
    .expect("resume");

    assert_eq!(resumed.id, created.id);
    assert_eq!(resumed.thinking, ThinkingLevel::High);
    assert_eq!(resumed.permission_mode, PermissionMode::Yolo);
    let _ = std::fs::remove_dir_all(&dir);
}
