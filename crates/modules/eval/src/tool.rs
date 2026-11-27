//! The approval-gated `eval` tool with `[eval]` config, per-kernel loopback HTTP bridge,
//! host-side `%pip` / `%load` magics, 50 KiB head+tail spill to `local://N.log`, and images.

use crate::bridge::BridgeInit;
use crate::config::{EvalConfig, MAX_TIMEOUT_SECS};
use crate::dyn_tools::KernelToolStore;
use crate::kernel::{CallEnd, CallResult, Kernel, SpawnOpts};
use crate::lang::Lang;
use crate::magics::{HostMagic, load_source, parse_host_magic, run_pip};
use crate::pool::Pool;
use crate::spill::apply_spill;
use hrn_edge_tools::EvalRuntimeHints;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, EvalHost, HrnError, ImageAttachment, Result, Tool, ToolApproval, ToolContext,
    ToolEffect, ToolOutput, ToolSpec,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const DESCRIPTION: &str = include_str!("../prompts/eval.md");
pub(crate) const HELP: &str = include_str!("../prompts/help-eval.md");

/// Runs code cells in persistent per-session Python / JavaScript (Bun) interpreters.
pub struct EvalTool {
    pool: Arc<Pool>,
    config: EvalConfig,
    eval_host: Option<Arc<dyn EvalHost>>,
    hints: Arc<EvalRuntimeHints>,
}

impl EvalTool {
    /// Create the tool with default `[eval]` config; no interpreter starts until the first call.
    pub fn new(data_dir: PathBuf) -> Self {
        Self::with_parts(
            data_dir,
            EvalConfig::default(),
            None,
            Arc::new(EvalRuntimeHints::default()),
            Arc::new(KernelToolStore::new()),
        )
    }

    /// Create the tool with explicit config, [`EvalHost`], runtime hints, and dynamic tool store.
    pub fn with_parts(
        data_dir: PathBuf,
        config: EvalConfig,
        eval_host: Option<Arc<dyn EvalHost>>,
        hints: Arc<EvalRuntimeHints>,
        dyn_tools: Arc<KernelToolStore>,
    ) -> Self {
        Self {
            pool: Arc::new(Pool::new(data_dir, dyn_tools)),
            config,
            eval_host,
            hints,
        }
    }

    /// Active `[eval]` configuration.
    #[must_use]
    pub fn config(&self) -> &EvalConfig {
        &self.config
    }

    /// Store of kernel-defined `@tool` functions registered by cells of this tool.
    #[must_use]
    pub fn dyn_tools(&self) -> &Arc<KernelToolStore> {
        self.pool.dyn_tools()
    }

    /// Access the active bridge URL of a live `(session, lang)` kernel (for security tests).
    #[cfg(test)]
    pub(crate) async fn test_bridge_url(&self, session: &str, lang: Lang) -> Option<String> {
        let slot = self.pool.slot(session, lang);
        let guard = slot.lock().await;
        guard.as_ref().map(Kernel::bridge_url)
    }

    /// Access the active bridge token of a live `(session, lang)` kernel (for security tests).
    #[cfg(test)]
    pub(crate) async fn test_bridge_token(&self, session: &str, lang: Lang) -> Option<String> {
        let slot = self.pool.slot(session, lang);
        let guard = slot.lock().await;
        guard.as_ref().map(|k| k.bridge_token().to_owned())
    }
}

impl Tool for EvalTool {
    fn spec(&self) -> ToolSpec {
        let default_secs = self.config.effective_default_secs();
        let mut langs = Vec::new();
        if self.config.py {
            langs.push("py");
        }
        if self.config.js {
            langs.push("js");
        }
        if langs.is_empty() {
            langs.extend(["py", "js"]);
        }
        ToolSpec {
            name: "eval".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "language": {
                        "type": "string",
                        "enum": langs,
                        "description": "Interpreter to run the cell in."
                    },
                    "code": { "type": "string", "description": "The cell: inline code or magic." },
                    "timeout": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": MAX_TIMEOUT_SECS,
                        "description": format!(
                            "Seconds before the interpreter is killed (default {default_secs}; 0 disables)."
                        )
                    },
                    "reset": {
                        "type": "boolean",
                        "description": "Restart this language's interpreter before running (default false)."
                    },
                    "title": { "type": "string", "description": "Short label shown in the transcript." }
                },
                "required": ["language", "code"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    fn summarize_call(&self, args: &Value) -> String {
        let lang = args.get("language").and_then(Value::as_str).unwrap_or("?");
        let label = args
            .get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                args.get("code")
                    .and_then(Value::as_str)
                    .and_then(|code| code.lines().find(|l| !l.trim().is_empty()))
            })
            .unwrap_or("<empty>");
        format!("[{lang}] {label}")
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let ctx = ctx.clone();
        let pool = Arc::clone(&self.pool);
        let config = self.config.clone();
        let eval_host = self.eval_host.clone();
        let hints = Arc::clone(&self.hints);
        Box::pin(async move { execute_cell(&ctx, &pool, &config, eval_host, &hints, &args).await })
    }
}

async fn execute_cell(
    ctx: &ToolContext,
    pool: &Arc<Pool>,
    config: &EvalConfig,
    eval_host: Option<Arc<dyn EvalHost>>,
    hints: &Arc<EvalRuntimeHints>,
    args: &Value,
) -> Result<ToolOutput> {
    let mut request = Request::parse(args, config)?;
    if !config.is_lang_enabled(request.lang) {
        return Ok(ToolOutput::err(format!(
            "eval: language `{}` is disabled by `[eval]` configuration",
            request.lang.token()
        )));
    }

    if let Some(magic) = parse_host_magic(request.lang, &request.code)? {
        match magic {
            HostMagic::Pip { args } => return run_pip(ctx, &args).await,
            HostMagic::Load { target } => {
                request.code = load_source(ctx, &target).await?;
            }
        }
    }

    let slot = pool.slot(&ctx.session_id, request.lang);
    let mut guard = slot.lock().await;

    let mut notes = Vec::new();
    if request.reset
        && let Some(old) = guard.take()
    {
        pool.dyn_tools().clear_kernel(&ctx.session_id, request.lang);
        old.kill()?;
    }
    if guard.as_ref().is_some_and(Kernel::is_dead) {
        guard.take();
        pool.dyn_tools().clear_kernel(&ctx.session_id, request.lang);
        notes.push("Kernel restarted (previous kernel exited; its state was lost).");
    }
    if request.code.trim().is_empty() {
        return Ok(ToolOutput::ok(if request.reset {
            format!("{} kernel reset.", request.lang.token())
        } else {
            "(no code)".to_owned()
        }));
    }
    if guard.is_none() {
        let script = pool.script(request.lang).await?;
        let bridge_init = BridgeInit {
            eval_host,
            dyn_tools: Arc::clone(pool.dyn_tools()),
            hints: Arc::clone(hints),
            tools_enabled: config.tools_enabled,
            slot: Arc::downgrade(&slot),
            lang: request.lang,
        };
        let opts = SpawnOpts {
            computer_enabled: hints.computer_prelude_active(),
            max_concurrency: hints.max_concurrency(),
        };
        *guard = Some(Kernel::spawn(request.lang, &script, &ctx.cwd, bridge_init, opts).await?);
    }
    let Some(kernel) = guard.as_mut() else {
        return Err(HrnError::Internal("eval: kernel slot empty".to_owned()));
    };
    let token = kernel.bridge_token().to_owned();
    let CallResult {
        output,
        images,
        end,
    } = kernel.call(ctx, &request.code, request.timeout).await;
    if matches!(end, CallEnd::Died | CallEnd::TimedOut | CallEnd::Cancelled)
        && let Some(dead) = guard.take()
    {
        pool.dyn_tools().clear_kernel(&ctx.session_id, request.lang);
        dead.kill()?;
    }
    drop(guard);
    if matches!(end, CallEnd::Cancelled) {
        return Err(HrnError::Cancelled);
    }
    Ok(render(
        ctx,
        &notes,
        &output,
        images,
        end,
        request.timeout,
        &token,
    ))
}

/// Validated tool arguments.
struct Request {
    lang: Lang,
    code: String,
    timeout: Option<Duration>,
    reset: bool,
}

impl Request {
    fn parse(args: &Value, config: &EvalConfig) -> Result<Self> {
        let lang = args
            .get("language")
            .and_then(Value::as_str)
            .and_then(Lang::parse)
            .ok_or_else(|| HrnError::Tool("eval: `language` must be \"py\" or \"js\"".into()))?;
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| HrnError::Tool("eval: `code` must be a string".into()))?;
        let requested_secs = match args.get("timeout") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_f64().ok_or_else(|| {
                HrnError::Tool("eval: `timeout` must be a non-negative finite number".into())
            })?),
        };
        let timeout = config.resolve_timeout(requested_secs)?;
        Ok(Self {
            lang,
            code: code.to_owned(),
            timeout,
            reset: args.get("reset").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

/// Append `piece` on its own line.
fn push_line(text: &mut String, piece: &str) {
    if piece.is_empty() {
        return;
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(piece.trim_end_matches('\n'));
    text.push('\n');
}

fn render(
    ctx: &ToolContext,
    notes: &[&str],
    output: &str,
    images: Vec<ImageAttachment>,
    end: CallEnd,
    timeout: Option<Duration>,
    bridge_token: &str,
) -> ToolOutput {
    let mut text = String::new();
    for note in notes {
        push_line(&mut text, note);
    }
    push_line(&mut text, output);
    let failed = match end {
        CallEnd::Reply { value, error } => {
            if let Some(value) = &value {
                push_line(&mut text, value);
            }
            if let Some(error) = &error {
                push_line(&mut text, error);
            }
            error.is_some()
        }
        CallEnd::TimedOut => {
            let secs = timeout.map_or(0.0, |t| t.as_secs_f64());
            push_line(
                &mut text,
                &format!(
                    "[timed out after {secs}s; kernel killed, state lost — the next call starts fresh]"
                ),
            );
            true
        }
        CallEnd::Died => {
            push_line(
                &mut text,
                "[kernel exited unexpectedly; state lost — the next call starts fresh]",
            );
            true
        }
        CallEnd::Cancelled => true,
    };
    let body = if text.trim().is_empty() {
        "(no output)".to_owned()
    } else {
        text.trim_end().to_owned()
    };
    apply_spill(ctx, &body, images, Some(bridge_token), failed)
}
