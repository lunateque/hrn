//! Session-attributed `/commit` and `hrn commit`: stages ONLY hunks and newly created files
//! attributed to the active session (and its descendant subagents), generates a conventional
//! commit message via the `commit` model role, optionally bumps `CHANGELOG.md`, and commits
//! without shell interpolation.

#[path = "commit_git.rs"]
mod git_ops;

use crate::cli::CommitArgs;
use crate::config::{CommitConfig, Config};
use crate::term::{err_line, out, sanitize_cli_inline, sanitize_cli_multiline};
use git_ops::{
    apply_patch_cached, ensure_no_staged_changes, has_head, hrn_tui_commit_staged, list_untracked,
    run_git, untracked_preview_diff,
};
use hrn_edge_http::HttpClient;
use hrn_edge_llm::{LlmConfig, LlmRouter, expand_selector};
use hrn_edge_oauth::OAuthManager;
use hrn_harness::{filter_diff_to_attributed, load_session_attribution, normalize_rel_path};
use hrn_kernel::{
    BoxFuture, CancelFlag, ChunkSink, HrnError, LlmChunk, LlmProvider, LlmRequest, Msg, Result,
    SessionMeta, SessionStore, ThinkingLevel,
};
use hrn_tui::{CommitPreview, find_repo_root};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Maximum characters of diff context passed to the commit-message LLM prompt.
const MAX_DIFF_PROMPT_CHARS: usize = 24_000;
/// Maximum output tokens requested for commit message generation.
const COMMIT_MSG_MAX_TOKENS: u32 = 256;

const COMMIT_SYSTEM_PROMPT: &str = "\
You write concise Conventional Commit messages (e.g. `feat(scope): summary`, `fix: summary`, \
`refactor(module): summary`). Output ONLY the commit message (a single imperative subject line \
<= 72 characters, optionally followed by a blank line and short bullet points if needed). \
Do NOT wrap in markdown code fences or quotes, and do NOT include AI attribution or trailers.";

struct NullSink;

impl ChunkSink for NullSink {
    fn on_chunk(&self, _chunk: LlmChunk) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// Prepared attributed commit plan ready for dry-run display, TUI preview, or staging + commit.
#[derive(Clone, Debug)]
pub(crate) struct CommitPlan {
    pub(crate) repo_root: PathBuf,
    pub(crate) session_id: String,
    pub(crate) filtered_patch: String,
    pub(crate) attributed_untracked: Vec<String>,
    pub(crate) files: Vec<String>,
    pub(crate) hunks_kept: usize,
    pub(crate) hunks_skipped: usize,
    pub(crate) diff_preview: String,
    pub(crate) has_changelog_file: bool,
}

impl CommitPlan {
    pub(crate) fn into_tui_preview(
        self,
        suggested_message: String,
        changelog_enabled: bool,
    ) -> CommitPreview {
        CommitPreview {
            files: self.files,
            hunks_kept: self.hunks_kept,
            hunks_skipped: self.hunks_skipped,
            diff_preview: self.diff_preview,
            suggested_message,
            changelog_enabled,
            has_changelog_file: self.has_changelog_file,
        }
    }
}

/// Fail closed when `[commit] enabled = false`.
pub(crate) fn ensure_commit_enabled(cfg: &CommitConfig) -> Result {
    if cfg.enabled {
        return Ok(());
    }
    Err(HrnError::Config(
        "commit automation is disabled (`[commit] enabled = false`); set `enabled = true` under `[commit]` in your global config to enable `/commit` and `hrn commit`"
            .to_owned(),
    ))
}

/// Resolve the target session metadata (explicit `--session` or newest session for `cwd`).
fn resolve_session(
    store: &dyn SessionStore,
    cwd: &Path,
    repo_root: &Path,
    explicit: Option<&str>,
) -> Result<SessionMeta> {
    if let Some(raw) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(meta) = store.get_session(raw)? {
            return Ok(meta);
        }
        let matches: Vec<SessionMeta> = store
            .list_sessions(200)?
            .into_iter()
            .filter(|m| m.id.starts_with(raw))
            .collect();
        return match matches.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(HrnError::Store(format!("unknown session `{raw}`"))),
            _ => Err(HrnError::Store(format!(
                "ambiguous session prefix `{raw}` matches {} sessions",
                matches.len()
            ))),
        };
    }
    let cwd_str = cwd.to_string_lossy();
    if let Some(meta) = store.list_recent_for(&cwd_str, 1)?.into_iter().next() {
        return Ok(meta);
    }
    let root_str = repo_root.to_string_lossy();
    if root_str != cwd_str
        && let Some(meta) = store.list_recent_for(&root_str, 1)?.into_iter().next()
    {
        return Ok(meta);
    }
    Err(HrnError::Store(
        "no active session found for this directory; pass `--session <id>` or edit files in a session first"
            .to_owned(),
    ))
}

/// Build an attributed commit plan for `session_override` (or latest session in `cwd`).
pub(crate) fn prepare_commit_plan(
    cwd: &Path,
    store: &dyn SessionStore,
    session_override: Option<&str>,
) -> Result<CommitPlan> {
    let repo_root = find_repo_root(cwd).map_err(HrnError::Tool)?;
    ensure_no_staged_changes(&repo_root)?;
    let session = resolve_session(store, cwd, &repo_root, session_override)?;
    let session_id = session.id;
    let mut attrib = load_session_attribution(store, &session_id)?;
    let session_cwd = if session.cwd.trim().is_empty() {
        cwd
    } else {
        Path::new(&session.cwd)
    };
    if let Some(prefix) = subdir_prefix(&repo_root, session_cwd) {
        attrib = attrib.with_prefix(&prefix);
    }
    if attrib.is_empty() {
        return Err(HrnError::Tool(format!(
            "session `{session_id}` has no recorded file edits to commit"
        )));
    }

    let raw_diff = if has_head(&repo_root) {
        run_git(&repo_root, &["diff", "HEAD", "--no-color", "--unified=3"])?
    } else {
        run_git(&repo_root, &["diff", "--no-color", "--unified=3"])?
    };
    let filtered = filter_diff_to_attributed(&raw_diff, &attrib);

    let untracked = list_untracked(&repo_root)?;
    let attributed_untracked: Vec<String> = untracked
        .into_iter()
        .filter(|p| {
            normalize_rel_path(&repo_root, p).is_some_and(|norm| attrib.files.contains_key(&norm))
        })
        .collect();

    let mut files: BTreeSet<String> = filtered.files.into_iter().collect();
    for p in &attributed_untracked {
        files.insert(p.clone());
    }
    let files: Vec<String> = files.into_iter().collect();
    if filtered.patch.trim().is_empty() && attributed_untracked.is_empty() {
        return Err(HrnError::Tool(
            "no session-attributed working-tree changes remain to commit".to_owned(),
        ));
    }

    let mut diff_preview = filtered.patch.clone();
    for rel in &attributed_untracked {
        if !diff_preview.is_empty() && !diff_preview.ends_with('\n') {
            diff_preview.push('\n');
        }
        diff_preview.push_str(&untracked_preview_diff(&repo_root, rel));
    }

    let has_changelog_file =
        fs::symlink_metadata(repo_root.join("CHANGELOG.md")).is_ok_and(|m| m.file_type().is_file());

    Ok(CommitPlan {
        repo_root,
        session_id,
        filtered_patch: filtered.patch,
        hunks_kept: filtered.hunks_kept + attributed_untracked.len(),
        hunks_skipped: filtered.hunks_skipped,
        attributed_untracked,
        files,
        diff_preview,
        has_changelog_file,
    })
}

/// Stage ONLY the attributed hunks and attributed untracked files in `plan`.
pub(crate) fn stage_commit_plan(plan: &CommitPlan) -> Result {
    ensure_no_staged_changes(&plan.repo_root)?;
    if !plan.filtered_patch.trim().is_empty() {
        apply_patch_cached(&plan.repo_root, &plan.filtered_patch)?;
    }
    for rel in &plan.attributed_untracked {
        if rel.contains('\0') {
            return Err(HrnError::Tool(
                "attributed path contains NUL byte".to_owned(),
            ));
        }
        let _ = run_git(&plan.repo_root, &["add", "--", rel])?;
    }
    Ok(())
}

/// Resolve the configured `[roles] commit` model alias (falling back through `smol -> fast -> default`).
pub(crate) fn resolve_commit_model(llm_cfg: &LlmConfig) -> Option<String> {
    expand_selector(llm_cfg, Some("commit"))
        .or_else(|_| expand_selector(llm_cfg, Some("@default")))
        .ok()
        .map(|r| r.alias)
}

/// Generate a conventional commit message using the `commit` role model.
pub(crate) async fn generate_commit_message(
    llm: &dyn LlmProvider,
    llm_cfg: &LlmConfig,
    diff_text: &str,
) -> Result<String> {
    let model = resolve_commit_model(llm_cfg);
    let snippet = truncate_chars(diff_text, MAX_DIFF_PROMPT_CHARS);
    let req = LlmRequest {
        model,
        messages: vec![
            Msg::system(COMMIT_SYSTEM_PROMPT),
            Msg::user(format!(
                "Generate a Conventional Commit message for this attributed diff:\n\n{snippet}"
            )),
        ],
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        thinking_budgets: None,
        temperature: Some(0.2),
        max_tokens: Some(COMMIT_MSG_MAX_TOKENS),
        session_id: None,
        service_tier: None,
    };
    let cancel = CancelFlag::new();
    let resp = llm.stream_chat(&req, &cancel, &NullSink).await?;
    let cleaned = clean_commit_message(&resp.content);
    if cleaned.is_empty() {
        return Err(HrnError::Tool(
            "commit model returned an empty commit message".to_owned(),
        ));
    }
    Ok(cleaned)
}

/// If `CHANGELOG.md` is a regular file in `repo_root`, prepend `- <subject>` under
/// `## [Unreleased]` (creating that section if absent) and stage `CHANGELOG.md`.
pub(crate) fn bump_changelog(repo_root: &Path, commit_message: &str) -> Result<bool> {
    let path = repo_root.join("CHANGELOG.md");
    let Ok(meta) = fs::symlink_metadata(&path) else {
        return Ok(false);
    };
    if !meta.file_type().is_file() {
        return Ok(false);
    }
    let subject = commit_message
        .lines()
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if subject.is_empty() {
        return Ok(false);
    }
    let existing = fs::read_to_string(&path)?;
    let entry = format!("- {subject}");
    let updated = insert_changelog_entry(&existing, &entry);
    fs::write(&path, updated)?;
    let _ = run_git(repo_root, &["add", "--", "CHANGELOG.md"])?;
    Ok(true)
}

/// Commit staged changes via `git commit -F -` (stdin, zero shell).
pub(crate) fn execute_git_commit(repo_root: &Path, message: &str) -> Result<String> {
    hrn_tui_commit_staged(repo_root, message)
}

/// Stage attributed changes, optionally bump `CHANGELOG.md`, and run `git commit`.
pub(crate) fn apply_commit_plan(
    plan: &CommitPlan,
    message: &str,
    changelog: bool,
) -> Result<String> {
    stage_commit_plan(plan)?;
    let bumped = if changelog {
        bump_changelog(&plan.repo_root, message)?
    } else {
        false
    };
    let summary = execute_git_commit(&plan.repo_root, message)?;
    if bumped {
        Ok(format!("{summary} (updated CHANGELOG.md)"))
    } else {
        Ok(summary)
    }
}

/// Entry point for `hrn commit [--yes] [--dry-run] [--changelog] [--session <id>]`.
pub(crate) async fn run_cli(
    cfg: &Config,
    store: &dyn SessionStore,
    args: &CommitArgs,
) -> Result<ExitCode> {
    ensure_commit_enabled(&cfg.commit)?;
    let cwd = std::env::current_dir()?;
    let plan = prepare_commit_plan(&cwd, store, args.session.as_deref())?;
    let router = LlmRouter::try_new(cfg.llm.clone(), HttpClient::new()?, OAuthManager::default())?;
    let message = generate_commit_message(&router, &cfg.llm, &plan.diff_preview).await?;
    let want_changelog = (cfg.commit.changelog || args.changelog) && plan.has_changelog_file;

    out(&format!(
        "Session: {}\nAttributed files ({}, {} hunk(s) kept, {} skipped):\n",
        sanitize_cli_inline(&plan.session_id),
        plan.files.len(),
        plan.hunks_kept,
        plan.hunks_skipped,
    ));
    for f in &plan.files {
        out(&format!("  - {}\n", sanitize_cli_inline(f)));
    }
    if want_changelog {
        out("  - CHANGELOG.md (auto-bump under [Unreleased])\n");
    }
    let safe_message = sanitize_cli_multiline(&message);
    out("\nProposed commit message:\n");
    for line in safe_message.lines() {
        out(&format!("  {line}\n"));
    }

    if args.dry_run {
        out("\n(dry-run: nothing staged or committed)\n");
        return Ok(ExitCode::SUCCESS);
    }

    if !args.yes && !confirm_prompt()? {
        err_line("aborted: nothing staged or committed");
        return Ok(ExitCode::SUCCESS);
    }

    let summary = apply_commit_plan(&plan, &safe_message, want_changelog)?;
    out(&format!("{}\n", sanitize_cli_inline(&summary)));
    Ok(ExitCode::SUCCESS)
}

fn confirm_prompt() -> Result<bool> {
    out("\nStage attributed hunks and commit? [y/N] ");
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line)?;
    if read == 0 {
        return Ok(false);
    }
    let ans = line.trim().to_ascii_lowercase();
    Ok(matches!(ans.as_str(), "y" | "yes"))
}

fn clean_commit_message(raw: &str) -> String {
    let trimmed = raw.trim();
    let unfenced = if let Some(rest) = trimmed.strip_prefix("```") {
        let body = rest.split_once('\n').map_or(rest, |(_, after)| after);
        body.strip_suffix("```").unwrap_or(body).trim()
    } else {
        trimmed
    };
    let unquoted = unfenced
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`'))
        .trim();
    sanitize_cli_multiline(unquoted).trim().to_owned()
}

fn insert_changelog_entry(existing: &str, entry: &str) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    if let Some(pos) = lines.iter().position(|l| {
        l.trim().eq_ignore_ascii_case("## [unreleased]")
            || l.trim().eq_ignore_ascii_case("## unreleased")
    }) {
        let mut out = Vec::with_capacity(lines.len() + 2);
        for (i, line) in lines.iter().enumerate() {
            out.push((*line).to_owned());
            if i == pos {
                out.push(entry.to_owned());
            }
        }
        let mut joined = out.join("\n");
        joined.push('\n');
        return joined;
    }
    format!("## [Unreleased]\n\n{entry}\n\n{existing}")
}

fn subdir_prefix(repo_root: &Path, cwd: &Path) -> Option<String> {
    let root = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let cur = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let rel = cur.strip_prefix(&root).ok()?;
    normalize_rel_path(&root, &rel.to_string_lossy())
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}\n... (diff truncated)", text.get(..cut).unwrap_or(text)),
        None => text.to_owned(),
    }
}

#[cfg(test)]
use git_ops::MAX_UNTRACKED_BYTES;

#[cfg(test)]
#[path = "commit_tests.rs"]
mod tests;
