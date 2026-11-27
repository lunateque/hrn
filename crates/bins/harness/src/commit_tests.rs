use super::*;
use hrn_edge_llm::{ModelPreset, ProviderEntry, ProviderKind};
use hrn_edge_store::SqliteStore;
use hrn_harness::record_edit_trace;
use hrn_kernel::{FinishReason, LlmResponse, ModelInfo, ScopedFuture, SessionMeta, Usage};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempGitRepo {
    dir: PathBuf,
}

impl TempGitRepo {
    fn new(tag: &str) -> Self {
        let n = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("hrn-commit-test-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create dir");
        git(&dir, &["init"]);
        git(&dir, &["config", "user.name", "Test User"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "core.autocrlf", "false"]);
        git(&dir, &["config", "core.eol", "lf"]);
        Self { dir }
    }
}

impl Drop for TempGitRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct RecordingLlm {
    recorded_models: Mutex<Vec<Option<String>>>,
    reply: String,
}

impl LlmProvider for RecordingLlm {
    fn stream_chat<'a>(
        &'a self,
        request: &'a LlmRequest,
        _cancel: &'a CancelFlag,
        _sink: &'a dyn ChunkSink,
    ) -> ScopedFuture<'a, Result<LlmResponse>> {
        let model = request.model.clone();
        self.recorded_models
            .lock()
            .expect("lock")
            .push(model.clone());
        let reply = self.reply.clone();
        Box::pin(async move {
            Ok(LlmResponse {
                model_used: model.unwrap_or_else(|| "default".to_owned()),
                fallback_from: None,
                content: reply,
                reasoning: String::new(),
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                usage: Some(Usage::default()),
                finish_reason: FinishReason::Stop,
            })
        })
    }

    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
}

fn make_session(store: &SqliteStore, id: &str, cwd: &Path) {
    let meta = SessionMeta {
        id: id.into(),
        title: "test".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        updated_at_ms: 1_700_000_000_000,
        ..SessionMeta::default()
    };
    store.create_session(&meta).expect("create session");
}

#[test]
fn commit_only_attributed_hunks() {
    let repo = TempGitRepo::new("hunks");
    let mut seed_lines: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
    let seed = format!("{}\n", seed_lines.join("\n"));
    fs::write(repo.dir.join("app.rs"), &seed).expect("write app.rs");
    fs::write(repo.dir.join("other.rs"), "const OTHER: u32 = 1;\n").expect("write other.rs");
    git(&repo.dir, &["add", "--", "app.rs", "other.rs"]);
    git(&repo.dir, &["commit", "-m", "chore: initial commit"]);

    // Modify BOTH top (line 2) AND bottom (line 29) of app.rs, plus unrelated other.rs.
    seed_lines[1] = "line 2 ATTRIBUTED_CHANGE".to_owned();
    seed_lines[28] = "line 29 UNATTRIBUTED_USER_EDIT".to_owned();
    fs::write(
        repo.dir.join("app.rs"),
        format!("{}\n", seed_lines.join("\n")),
    )
    .expect("write modified app.rs");
    fs::write(repo.dir.join("other.rs"), "const OTHER: u32 = 999;\n").expect("write other.rs");

    // Record session attribution ONLY for line 2 of app.rs (not line 29, not other.rs).
    let store = SqliteStore::open_in_memory().expect("in-memory store");
    make_session(&store, "sess-root", &repo.dir);
    record_edit_trace(&store, "sess-root", "app.rs", &[(2, 2)], false).expect("record edit trace");

    let plan = prepare_commit_plan(&repo.dir, &store, Some("sess-root")).expect("prepare plan");
    assert_eq!(plan.files, vec!["app.rs".to_owned()]);
    assert_eq!(plan.hunks_kept, 1);
    assert_eq!(plan.hunks_skipped, 2);

    let summary =
        apply_commit_plan(&plan, "feat(app): apply attributed edit", false).expect("commit");
    assert!(
        summary.contains("feat(app): apply attributed edit"),
        "{summary}"
    );

    // Committed diff must contain ONLY the attributed top hunk, never the unattributed hunk or file.
    let committed = git(&repo.dir, &["show", "HEAD", "--no-color"]);
    assert!(
        committed.contains("line 2 ATTRIBUTED_CHANGE"),
        "{committed}"
    );
    assert!(
        !committed.contains("line 29 UNATTRIBUTED_USER_EDIT"),
        "unattributed hunk leaked into commit: {committed}"
    );
    assert!(
        !committed.contains("other.rs"),
        "unattributed file leaked into commit: {committed}"
    );

    // Unattributed edits must remain unstaged in the working tree!
    let remaining = git(&repo.dir, &["diff", "--no-color"]);
    assert!(
        remaining.contains("line 29 UNATTRIBUTED_USER_EDIT"),
        "unattributed hunk should remain unstaged: {remaining}"
    );
    assert!(
        remaining.contains("const OTHER: u32 = 999;"),
        "unattributed file should remain unstaged: {remaining}"
    );
}

#[tokio::test]
async fn commit_message_uses_commit_role() {
    let mut llm_cfg = LlmConfig::default();
    llm_cfg.providers.insert(
        "mock".to_owned(),
        ProviderEntry {
            kind: ProviderKind::OpenaiChat,
            base_url: Some("https://example.invalid".to_owned()),
            ..ProviderEntry::default()
        },
    );
    llm_cfg.presets.insert(
        "main-model".to_owned(),
        ModelPreset {
            provider: "mock".to_owned(),
            id: "main-id".to_owned(),
            ..ModelPreset::default()
        },
    );
    llm_cfg.presets.insert(
        "commit-model".to_owned(),
        ModelPreset {
            provider: "mock".to_owned(),
            id: "commit-id".to_owned(),
            ..ModelPreset::default()
        },
    );
    llm_cfg
        .roles
        .insert("default".to_owned(), "main-model".to_owned());
    llm_cfg
        .roles
        .insert("commit".to_owned(), "commit-model".to_owned());

    let mock = RecordingLlm {
        recorded_models: Mutex::new(Vec::new()),
        reply: "```text\nfeat(auth): validate session tokens\n```".to_owned(),
    };
    let msg = generate_commit_message(&mock, &llm_cfg, "+fn validate() {}\n")
        .await
        .expect("generate commit message");
    assert_eq!(msg, "feat(auth): validate session tokens");

    let calls = mock.recorded_models.lock().expect("lock").clone();
    assert_eq!(calls, vec![Some("commit-model".to_owned())]);
}

#[test]
fn commit_disabled_refuses_cleanly_and_changelog_bumps() {
    let disabled = CommitConfig {
        enabled: false,
        changelog: false,
    };
    let err = ensure_commit_enabled(&disabled).expect_err("must reject disabled commit");
    assert!(err.to_string().contains("[commit]"), "{err}");

    let repo = TempGitRepo::new("changelog");
    fs::write(
        repo.dir.join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n- existing entry\n",
    )
    .expect("write changelog");
    git(&repo.dir, &["add", "--", "CHANGELOG.md"]);
    git(&repo.dir, &["commit", "-m", "chore: seed changelog"]);

    let bumped = bump_changelog(&repo.dir, "feat: add dark mode\n\ndetails").expect("bump");
    assert!(bumped);
    let content = fs::read_to_string(repo.dir.join("CHANGELOG.md")).expect("read");
    assert!(
        content.contains("## [Unreleased]\n- feat: add dark mode\n"),
        "{content}"
    );
}

#[test]
fn commit_refuses_when_unattributed_changes_are_already_staged() {
    let repo = TempGitRepo::new("pre-staged");
    fs::write(repo.dir.join("app.rs"), "fn a() {}\n").expect("write app");
    fs::write(repo.dir.join("secret.rs"), "const S: u32 = 1;\n").expect("write secret");
    git(&repo.dir, &["add", "--", "app.rs", "secret.rs"]);
    git(&repo.dir, &["commit", "-m", "chore: seed"]);

    // User manually stages an unattributed file, while agent edits app.rs.
    fs::write(repo.dir.join("secret.rs"), "const S: u32 = 2;\n").expect("edit secret");
    git(&repo.dir, &["add", "--", "secret.rs"]);
    fs::write(repo.dir.join("app.rs"), "fn a() { let x = 1; }\n").expect("edit app");

    let store = SqliteStore::open_in_memory().expect("store");
    make_session(&store, "sess-1", &repo.dir);
    record_edit_trace(&store, "sess-1", "app.rs", &[(1, 1)], false).expect("trace");

    let err = prepare_commit_plan(&repo.dir, &store, Some("sess-1"))
        .expect_err("must refuse when index already has staged files");
    assert!(
        err.to_string().contains("already has 1 staged file"),
        "{err}"
    );
    assert!(err.to_string().contains("secret.rs"), "{err}");

    // Also verify stage_commit_plan refuses if an unattributed file is staged AFTER preview prep.
    git(&repo.dir, &["restore", "--staged", "--", "secret.rs"]);
    let plan = prepare_commit_plan(&repo.dir, &store, Some("sess-1")).expect("clean index plan");
    git(&repo.dir, &["add", "--", "secret.rs"]);
    let late_err = apply_commit_plan(&plan, "feat: late stage check", false)
        .expect_err("must refuse commit if index became dirty before staging");
    assert!(late_err.to_string().contains("secret.rs"), "{late_err}");

    // Also verify unborn repository (no HEAD) with staged file is refused.
    let unborn = TempGitRepo::new("pre-staged-unborn");
    fs::write(unborn.dir.join("staged.txt"), "unattributed\n").expect("write staged");
    git(&unborn.dir, &["add", "--", "staged.txt"]);
    fs::write(unborn.dir.join("attr.txt"), "attributed\n").expect("write attr");
    let ustore = SqliteStore::open_in_memory().expect("store");
    make_session(&ustore, "sess-u", &unborn.dir);
    record_edit_trace(&ustore, "sess-u", "attr.txt", &[], true).expect("trace");
    let uerr = prepare_commit_plan(&unborn.dir, &ustore, Some("sess-u"))
        .expect_err("must refuse unborn repo with pre-staged file");
    assert!(uerr.to_string().contains("staged.txt"), "{uerr}");
}

#[test]
fn commit_subdir_attribution_does_not_match_root_file_of_same_name() {
    let repo = TempGitRepo::new("subdir");
    let sub = repo.dir.join("sub");
    fs::create_dir_all(&sub).expect("create sub");
    fs::write(repo.dir.join("Cargo.toml"), "name = \"root\"\n").expect("write root");
    fs::write(sub.join("Cargo.toml"), "name = \"sub\"\n").expect("write sub");
    git(&repo.dir, &["add", "--", "Cargo.toml", "sub/Cargo.toml"]);
    git(&repo.dir, &["commit", "-m", "chore: seed"]);

    // Modify BOTH root Cargo.toml (unattributed) and sub/Cargo.toml (attributed in sub cwd).
    fs::write(repo.dir.join("Cargo.toml"), "name = \"root-manual\"\n").expect("edit root");
    fs::write(sub.join("Cargo.toml"), "name = \"sub-agent\"\n").expect("edit sub");

    let store = SqliteStore::open_in_memory().expect("store");
    make_session(&store, "sess-sub", &sub);
    record_edit_trace(&store, "sess-sub", "Cargo.toml", &[(1, 1)], false).expect("trace");

    let plan = prepare_commit_plan(&sub, &store, Some("sess-sub")).expect("prepare subdir plan");
    assert_eq!(plan.files, vec!["sub/Cargo.toml".to_owned()]);

    let _ = apply_commit_plan(&plan, "chore(sub): update manifest", false).expect("commit");
    let committed = git(&repo.dir, &["show", "HEAD", "--no-color"]);
    assert!(committed.contains("sub-agent"), "{committed}");
    assert!(
        !committed.contains("root-manual"),
        "root file leaked into subdir attributed commit: {committed}"
    );
    let remaining = git(&repo.dir, &["diff", "--no-color"]);
    assert!(remaining.contains("root-manual"), "{remaining}");
}

#[test]
fn commit_untracked_preview_bounds_oversized_file_and_cleans_ansi_message() {
    let repo = TempGitRepo::new("oversized");
    fs::write(repo.dir.join("seed.txt"), "seed\n").expect("seed");
    git(&repo.dir, &["add", "--", "seed.txt"]);
    git(&repo.dir, &["commit", "-m", "chore: seed"]);

    let big = vec![b'a'; MAX_UNTRACKED_BYTES + 4096];
    fs::write(repo.dir.join("big.txt"), &big).expect("write big");
    let preview = untracked_preview_diff(&repo.dir, "big.txt");
    assert_eq!(preview, "--- /dev/null\n+++ b/big.txt\n@@ -0,0 +1,0 @@\n");

    let cleaned = clean_commit_message(
        "```\n\x1b[31mfeat:\x1b[0m \x1b]0;title\x07safe \u{202E}msg\r\n\n- bullet\x07\n```",
    );
    assert_eq!(cleaned, "feat: safe msg\n\n- bullet");
}
