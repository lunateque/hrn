//! Optional post-edit AST syntax check (`hrn-edge-ast` / `hrn-ast` sidecar), `@smol` auto-repair,
//! and `blackbox` full-source regression reporting.

use hrn_edge_ast::protocol::{Data, Request, Response, SyntaxData};
use hrn_edge_rt::tokio;
use hrn_edge_rt::tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use hrn_edge_rt::tokio::process::Command;
use hrn_edge_rt::tokio::time::timeout;
use hrn_edge_tools::paths::display_rel;
use hrn_edge_tools::{
    FileReadTracker, TextFile, normalize_eol, unified_diff, verify_diff_lines_seen, write_atomic,
};
use hrn_kernel::{
    BoxFuture, CancelFlag, ChunkSink, HrnError, LlmChunk, LlmProvider, LlmRequest, Msg, Result,
    ToolContext, ToolOutput,
};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const SIDECAR_TIMEOUT: Duration = Duration::from_secs(15);

/// Checks an in-memory buffer for syntax errors using `hrn-edge-ast` / tree-sitter.
pub(crate) trait SyntaxChecker: Send + Sync {
    fn check(
        &self,
        path: &Path,
        content: &str,
        cancel: &CancelFlag,
    ) -> BoxFuture<Result<SyntaxData>>;
}

/// Production syntax checker speaking [`Request::CheckSyntax`] to the `hrn-ast` sidecar binary.
/// Degrades gracefully (`supported: false`) when the sidecar binary is not installed beside `hrn`.
pub(crate) struct SidecarSyntaxChecker;

impl SyntaxChecker for SidecarSyntaxChecker {
    fn check(
        &self,
        path: &Path,
        content: &str,
        cancel: &CancelFlag,
    ) -> BoxFuture<Result<SyntaxData>> {
        let Some(lang) = hrn_edge_ast::table::key_for_path(path) else {
            return Box::pin(async {
                Ok(SyntaxData {
                    supported: false,
                    error_count: 0,
                    errors: Vec::new(),
                })
            });
        };
        let req = Request::CheckSyntax {
            path: path.to_string_lossy().into_owned(),
            lang: Some(lang.to_owned()),
            content: content.to_owned(),
        };
        let cancel = cancel.clone();
        Box::pin(async move {
            let Some(exe) = find_sidecar() else {
                return Ok(SyntaxData {
                    supported: false,
                    error_count: 0,
                    errors: Vec::new(),
                });
            };
            run_sidecar_check(&exe, &req, &cancel).await
        })
    }
}

fn find_sidecar() -> Option<PathBuf> {
    if let Ok(overridden) = std::env::var("HRN_AST_SIDECAR") {
        let path = PathBuf::from(overridden);
        if path.is_file() {
            return Some(path);
        }
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let path = dir.join(format!("hrn-ast{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

async fn run_sidecar_check(exe: &Path, req: &Request, cancel: &CancelFlag) -> Result<SyntaxData> {
    let run = async {
        let mut child = Command::new(exe)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| HrnError::Tool(format!("ast check spawn failed: {e}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| HrnError::Tool("ast sidecar stdin unavailable".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HrnError::Tool("ast sidecar stdout unavailable".to_owned()))?;
        let line = hrn_kernel::serde_json::to_string(req)
            .map_err(|e| HrnError::Internal(format!("ast check encode: {e}")))?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| HrnError::Tool(format!("ast sidecar write: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| HrnError::Tool(format!("ast sidecar flush: {e}")))?;
        drop(stdin);
        let mut lines = BufReader::new(stdout).lines();
        let resp_line = lines
            .next_line()
            .await
            .map_err(|e| HrnError::Tool(format!("ast sidecar read: {e}")))?
            .ok_or_else(|| HrnError::Tool("ast sidecar returned empty response".to_owned()))?;
        let parsed: Response = hrn_kernel::serde_json::from_str(resp_line.trim())
            .map_err(|e| HrnError::Tool(format!("ast sidecar response parse: {e}")))?;
        let _ = child.wait().await;
        match parsed.into_data().map_err(HrnError::Tool)? {
            Data::Syntax(data) => Ok(data),
            Data::Search(_) | Data::Rewrite(_) => Err(HrnError::Internal(
                "ast sidecar answered CheckSyntax with unexpected payload".to_owned(),
            )),
        }
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(HrnError::Cancelled),
        res = timeout(SIDECAR_TIMEOUT, run) => res.unwrap_or_else(|_| Ok(SyntaxData {
            supported: false,
            error_count: 0,
            errors: Vec::new(),
        })),
    }
}

struct NullSink;
impl ChunkSink for NullSink {
    fn on_chunk(&self, _chunk: LlmChunk) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// Coordinates post-edit syntax regression checks, `@smol` repair, and `blackbox` reporting.
#[derive(Clone)]
pub(crate) struct PostEditRepair {
    pub(crate) auto_repair: bool,
    pub(crate) blackbox: bool,
    pub(crate) enforce_seen_lines: bool,
    llm: Option<Arc<dyn LlmProvider>>,
    checker: Arc<dyn SyntaxChecker>,
}

impl PostEditRepair {
    pub(crate) fn new(
        auto_repair: bool,
        blackbox: bool,
        enforce_seen_lines: bool,
        llm: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self::with_checker(
            auto_repair,
            blackbox,
            enforce_seen_lines,
            llm,
            Arc::new(SidecarSyntaxChecker),
        )
    }

    pub(crate) fn with_checker(
        auto_repair: bool,
        blackbox: bool,
        enforce_seen_lines: bool,
        llm: Option<Arc<dyn LlmProvider>>,
        checker: Arc<dyn SyntaxChecker>,
    ) -> Self {
        Self {
            auto_repair,
            blackbox,
            enforce_seen_lines,
            llm,
            checker,
        }
    }

    pub(crate) const fn is_active(&self) -> bool {
        self.auto_repair || self.blackbox
    }

    /// Snapshot pre-edit contents for candidate files when `auto_repair` or `blackbox` is on.
    pub(crate) fn capture_before(&self, paths: &[PathBuf]) -> Vec<(PathBuf, String)> {
        if !self.is_active() {
            return Vec::new();
        }
        paths
            .iter()
            .filter_map(|p| {
                let bytes = std::fs::read(p).ok()?;
                let decoded = TextFile::decode(bytes).ok()?;
                Some((p.clone(), decoded.text))
            })
            .collect()
    }

    /// Inspect each edited file for introduced AST syntax regressions, repairing via `@smol` when
    /// enabled (accepting ONLY if reparsing confirms zero new syntax errors and, when
    /// `enforce_seen_lines` is active, only seen lines are modified) and appending `[blackbox]`
    /// full source when configured.
    pub(crate) async fn run(
        &self,
        ctx: &ToolContext,
        tracker: &FileReadTracker,
        before_files: &[(PathBuf, String)],
        mut output: ToolOutput,
    ) -> Result<ToolOutput> {
        if !self.is_active() || output.is_error || before_files.is_empty() {
            return Ok(output);
        }
        for (path, before) in before_files {
            let Ok(after_bytes) = std::fs::read(path) else {
                continue;
            };
            let Ok(after_file) = TextFile::decode(after_bytes) else {
                continue;
            };
            let after = &after_file.text;
            if after == before {
                continue;
            }
            let Ok(before_syn) = self.checker.check(path, before, &ctx.cancel).await else {
                continue;
            };
            if !before_syn.supported {
                continue;
            }
            let Ok(after_syn) = self.checker.check(path, after, &ctx.cancel).await else {
                continue;
            };
            if after_syn.error_count <= before_syn.error_count {
                continue;
            }
            let shown = display_rel(&ctx.cwd, path);
            let mut still_broken = true;
            if self.auto_repair
                && let Some(llm) = &self.llm
            {
                match self
                    .attempt_repair(
                        llm.as_ref(),
                        path,
                        &shown,
                        after,
                        &after_syn.errors,
                        before_syn.error_count,
                        &ctx.cancel,
                    )
                    .await
                {
                    Ok(Some(repaired)) => {
                        if self.accept_repaired(
                            tracker,
                            path,
                            &shown,
                            before,
                            &after_file,
                            &repaired,
                            &mut output,
                        )? {
                            still_broken = false;
                        }
                    }
                    Ok(None) => {
                        let _ = write!(
                            output.content,
                            "\n[auto_repair] @smol repair for `{shown}` still contained syntax errors; discarded."
                        );
                    }
                    Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
                    Err(_) => {}
                }
            }
            if still_broken && self.blackbox {
                let _ = write!(
                    output.content,
                    "\n[blackbox `{shown}`: syntax regression ({} -> {} error(s))]\n{after}",
                    before_syn.error_count, after_syn.error_count
                );
            }
        }
        Ok(output)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal helper checking seen-line safety and writing BOM/CRLF-preserving repair"
    )]
    fn accept_repaired(
        &self,
        tracker: &FileReadTracker,
        path: &Path,
        shown: &str,
        before: &str,
        after_file: &TextFile,
        repaired: &str,
        output: &mut ToolOutput,
    ) -> Result<bool> {
        let repaired_lf = normalize_eol(repaired);
        let deltas = if self.enforce_seen_lines {
            let Ok(d) =
                verify_diff_lines_seen(tracker, path, shown, &after_file.text, &repaired_lf)
            else {
                let _ = write!(
                    output.content,
                    "\n[auto_repair] @smol repair for `{shown}` touched unseen lines; discarded."
                );
                return Ok(false);
            };
            d
        } else {
            Vec::new()
        };
        let encoded = TextFile {
            text: repaired_lf.clone(),
            ..after_file.clone()
        }
        .encode();
        write_atomic(path, &encoded)
            .map_err(|e| HrnError::Tool(format!("auto_repair write `{shown}` failed: {e}")))?;
        tracker.record_current(path);
        if !deltas.is_empty() {
            tracker.adjust_seen_after_edits(path, &deltas);
        }
        output.diff = Some(unified_diff(shown, before, &repaired_lf));
        let _ = write!(
            output.content,
            "\n[auto_repair] repaired introduced syntax error(s) in `{shown}` via @smol (reparse clean)."
        );
        Ok(true)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal helper coordinating LLM repair and tree-sitter reparse verification"
    )]
    async fn attempt_repair(
        &self,
        llm: &dyn LlmProvider,
        path: &Path,
        shown: &str,
        broken: &str,
        errors: &[String],
        baseline_errors: usize,
        cancel: &CancelFlag,
    ) -> Result<Option<String>> {
        let model = pick_smol_model(llm);
        let prompt = format!(
            "Fix ONLY the syntax error(s) introduced in `{shown}` without changing behavior.\n\
             Errors:\n{}\n\n\
             Return ONLY the complete repaired file inside a single fenced code block.\n\n\
             ```\n{broken}\n```",
            errors.join("\n")
        );
        let req = LlmRequest {
            model,
            messages: vec![
                Msg::system(
                    "You repair syntax errors in source files. Return only the fixed source inside one fenced code block.",
                ),
                Msg::user(prompt),
            ],
            tools: Vec::new(),
            thinking: hrn_kernel::ThinkingLevel::Off,
            thinking_budgets: None,
            temperature: Some(0.0),
            max_tokens: None,
            session_id: None,
            service_tier: None,
        };
        let resp = llm.stream_chat(&req, cancel, &NullSink).await?;
        let candidate = extract_fenced_source(&resp.content);
        if candidate.trim().is_empty() {
            return Ok(None);
        }
        let recheck = self.checker.check(path, &candidate, cancel).await?;
        if recheck.supported && recheck.error_count <= baseline_errors {
            Ok(Some(candidate))
        } else {
            Ok(None)
        }
    }
}

fn pick_smol_model(llm: &dyn LlmProvider) -> Option<String> {
    let models = llm.models();
    for preferred in ["smol", "fast", "tiny"] {
        if models
            .iter()
            .any(|m| m.alias.eq_ignore_ascii_case(preferred))
        {
            return Some(preferred.to_owned());
        }
    }
    None
}

/// Extract the first fenced code block if present, otherwise return trimmed text.
pub(crate) fn extract_fenced_source(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(open) = trimmed.find("```") {
        let after_open = trimmed.get(open + 3..).unwrap_or_default();
        if let Some(nl) = after_open.find('\n') {
            let body = after_open.get(nl + 1..).unwrap_or_default();
            if let Some(close) = body.rfind("```") {
                return body.get(..close).unwrap_or_default().to_owned();
            }
        }
    }
    trimmed.to_owned()
}
