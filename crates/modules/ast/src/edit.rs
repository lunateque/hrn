//! The `ast_edit` tool (preview) and its `resolve` / `reject` follow-ups (apply / discard).
//! Both halves run in the `hrn-ast` sidecar; the proposal lives in-process between them.

use crate::staged::{Expected, Proposal, Staged};
use crate::{Config, check_scope, tags_of};
use hrn_edge_ast::protocol::{Data, Request, Rule};
use hrn_edge_ast::{show_applied, show_rewrite, table};
use hrn_edge_tools::args::opt_str;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, HrnError, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::path::PathBuf;
use std::sync::Arc;

/// The line telling the model a diff is staged, not written.
const PREVIEW_NOTICE: &str = "Staged as a proposal — files NOT modified yet. To apply: \
`resolve <one-sentence reason>`. To discard: `reject <one-sentence reason>`.";

/// Structural AST rewrite: previews a proposal, applies it on `resolve`.
pub(crate) struct AstEditTool {
    cfg: Config,
    staged: Arc<Staged>,
}

impl AstEditTool {
    pub(crate) const fn new(cfg: Config, staged: Arc<Staged>) -> Self {
        Self { cfg, staged }
    }
}

/// The `ops` array as `(pat, out)` rules.
///
/// # Errors
///
/// When `ops` is missing or empty, a `pat` is blank, or two ops repeat one `pat`.
fn read_ops(args: &Value) -> Result<Vec<Rule>> {
    let items = args
        .get("ops")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| {
            HrnError::Tool("ast_edit: `ops` must hold at least one `{ pat, out }` object".into())
        })?;
    let mut rules: Vec<Rule> = Vec::new();
    for item in items {
        let pat = item
            .get("pat")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| HrnError::Tool("ast_edit: every op needs a non-empty `pat`".into()))?;
        if rules.iter().any(|r| r.pat == pat) {
            return Err(HrnError::Tool(format!(
                "ast_edit: duplicate pattern `{pat}` in `ops`"
            )));
        }
        rules.push(Rule {
            pat: pat.to_owned(),
            out: item
                .get("out")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        });
    }
    Ok(rules)
}

/// The `paths` array joined into one scope expression.
///
/// # Errors
///
/// When `paths` is missing, empty or holds a blank entry.
fn read_paths(args: &Value) -> Result<String> {
    let items = args
        .get("paths")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| {
            HrnError::Tool(
                "ast_edit: `paths` must list at least one file, directory or glob".into(),
            )
        })?;
    let parts = items
        .iter()
        .map(|p| p.as_str().map(str::trim).filter(|s| !s.is_empty()))
        .collect::<Option<Vec<&str>>>()
        .ok_or_else(|| {
            HrnError::Tool("ast_edit: every `paths` entry must be a non-empty string".into())
        })?;
    Ok(parts.join(";"))
}

/// The `lang` argument: absent/blank means per-file inference, an unknown alias fails.
fn read_lang(args: &Value) -> std::result::Result<Option<String>, String> {
    let Some(raw) = opt_str(args, "lang")
        .map(str::trim)
        .filter(|l| !l.is_empty())
    else {
        return Ok(None);
    };
    if table::key_of(raw).is_none() {
        return Err(format!(
            "ast_edit: unsupported language `{raw}` ({})",
            table::aliases()
        ));
    }
    Ok(Some(raw.to_owned()))
}

impl Tool for AstEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ast_edit".to_owned(),
            description: crate::DESCRIPTION_EDIT.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "ops": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "pat": { "type": "string", "description": "AST pattern to match." },
                                "out": { "type": "string", "description": "Rewrite template; empty deletes the match." }
                            },
                            "required": ["pat", "out"]
                        },
                        "description": "Rewrite rules; they run in pattern order."
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Files, directories or globs to rewrite."
                    },
                    "lang": {
                        "type": "string",
                        "description": format!("Language override for every file ({}).", table::aliases())
                    }
                },
                "required": ["ops", "paths"]
            }),
        }
    }

    /// A preview touches nothing; the write happens in `resolve`.
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        args.get("ops").and_then(Value::as_array).map_or_else(
            || "ast_edit".to_owned(),
            |ops| format!("{} rewrite(s)", ops.len()),
        )
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let cwd = ctx.cwd.clone();
        let cancel = ctx.cancel.clone();
        let session = ctx.session_id.clone();
        let max_files = self.cfg.max_files;
        let staged = Arc::clone(&self.staged);
        Box::pin(async move {
            let rules = read_ops(&args)?;
            let raw_paths = read_paths(&args)?;
            let lang = match read_lang(&args) {
                Ok(lang) => lang,
                Err(msg) => return Ok(ToolOutput::err(msg)),
            };
            let directory = check_scope(&cwd, &raw_paths, "`paths` entries must not be empty")?;
            let request = Request::Rewrite {
                cwd: cwd.to_string_lossy().into_owned(),
                path: raw_paths.clone(),
                lang: lang.clone(),
                rules: rules.clone(),
                max_files,
                write: false,
            };
            let response = crate::client::run(&request, &cancel).await?;
            let preview = match response.into_data().map_err(HrnError::Tool)? {
                Data::Rewrite(preview) => preview,
                Data::Search(_) | Data::Syntax(_) => {
                    return Err(HrnError::Internal(
                        "ast: the sidecar answered a rewrite with an unexpected payload".to_owned(),
                    ));
                }
            };
            let tag = tags_of(&cwd);
            if preview.total == 0 {
                return Ok(ToolOutput::ok(
                    show_rewrite(&preview, directory, None, &tag).text,
                ));
            }
            let text = show_rewrite(&preview, directory, Some(PREVIEW_NOTICE), &tag).text;
            staged.stage(
                &session,
                Proposal {
                    cwd: cwd.to_string_lossy().into_owned(),
                    path: raw_paths,
                    rules,
                    lang,
                    max_files,
                    expected: Expected {
                        total: preview.total,
                        files: preview.files_touched,
                        per_file: preview.per_file.clone(),
                    },
                },
            );
            Ok(ToolOutput::ok(text))
        })
    }
}

/// Apply the staged `ast_edit` proposal.
pub(crate) struct ResolveTool {
    cfg: Config,
    staged: Arc<Staged>,
}

impl ResolveTool {
    pub(crate) const fn new(cfg: Config, staged: Arc<Staged>) -> Self {
        Self { cfg, staged }
    }
}

impl Tool for ResolveTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "resolve".to_owned(),
            description: crate::DESCRIPTION_RESOLVE.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "description": "Why the proposal is applied." }
                },
                "required": ["reason"]
            }),
        }
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::write()
    }

    fn summarize_call(&self, args: &Value) -> String {
        opt_str(args, "reason")
            .unwrap_or("apply staged rewrite")
            .to_owned()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let reason = opt_str(&args, "reason").unwrap_or("").trim().to_owned();
        let staged = Arc::clone(&self.staged);
        let session = ctx.session_id.clone();
        let cwd = ctx.cwd.to_string_lossy().into_owned();
        let cancel = ctx.cancel.clone();
        let max_files = self.cfg.max_files;
        Box::pin(async move {
            if reason.is_empty() {
                return Ok(ToolOutput::err(
                    "resolve: `reason` must be a non-empty string",
                ));
            }
            let Ok(proposal) = staged.take(&session) else {
                return Ok(ToolOutput::err(
                    "resolve: nothing is staged; run `ast_edit` first to preview a rewrite",
                ));
            };
            let request = Request::Rewrite {
                cwd: proposal.cwd.clone(),
                path: proposal.path.clone(),
                lang: proposal.lang.clone(),
                rules: proposal.rules.clone(),
                max_files: proposal.max_files.min(max_files),
                write: true,
            };
            let response = crate::client::run(&request, &cancel).await?;
            let applied = match response.into_data().map_err(HrnError::Tool)? {
                Data::Rewrite(applied) => applied,
                Data::Search(_) | Data::Syntax(_) => {
                    return Err(HrnError::Internal(
                        "ast: the sidecar answered a rewrite with an unexpected payload".to_owned(),
                    ));
                }
            };
            let apply_cwd = PathBuf::from(&cwd);
            let tag = tags_of(&apply_cwd);
            if applied.total == proposal.expected.total
                && applied.files_touched == proposal.expected.files
                && applied.per_file == proposal.expected.per_file
            {
                return Ok(ToolOutput::ok(show_applied(&applied, &tag)));
            }
            let mut text = format!(
                "Preview is stale / no longer matches: applied {} replacement(s) in {} file(s); \
                 the preview expected {} in {}. No proposal is pending.",
                applied.total,
                applied.files_touched,
                proposal.expected.total,
                proposal.expected.files
            );
            let lines: Vec<String> = applied
                .per_file
                .iter()
                .filter_map(|(p, _)| tag(p).map(|t| format!("[{p}#{t}]")))
                .collect();
            if !lines.is_empty() {
                text.push('\n');
                text.push_str(&lines.join("\n"));
            }
            Ok(ToolOutput::err(text))
        })
    }
}

/// Discard the staged `ast_edit` proposal.
pub(crate) struct RejectTool {
    staged: Arc<Staged>,
}

impl RejectTool {
    pub(crate) const fn new(staged: Arc<Staged>) -> Self {
        Self { staged }
    }
}

impl Tool for RejectTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "reject".to_owned(),
            description: crate::DESCRIPTION_REJECT.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "description": "Why the proposal is discarded." }
                },
                "required": ["reason"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        opt_str(args, "reason")
            .unwrap_or("discard staged rewrite")
            .to_owned()
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let reason = opt_str(&args, "reason").unwrap_or("").trim().to_owned();
        let staged = Arc::clone(&self.staged);
        let session = ctx.session_id.clone();
        Box::pin(async move {
            if reason.is_empty() {
                return Ok(ToolOutput::err(
                    "reject: `reason` must be a non-empty string",
                ));
            }
            let Some(proposal) = staged.take(&session).ok() else {
                return Ok(ToolOutput::err(
                    "reject: nothing is staged; run `ast_edit` first to preview a rewrite",
                ));
            };
            Ok(ToolOutput::ok(format!(
                "Discarded: {} rewrite(s) over {} path(s). Reason: {reason}",
                proposal.rules.len(),
                proposal.path.split(';').count()
            )))
        })
    }
}
