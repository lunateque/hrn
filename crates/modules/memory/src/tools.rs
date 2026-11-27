//! `retain`, `recall`, `reflect`, `memory_edit` and `learn`.

use crate::fmt::{PREVIEW_CHARS, bullet, utc_minute};
use crate::project::{GLOBAL, project_key};
use crate::query::match_expr;
use crate::redact::redact;
use crate::specs::{self, MEMORY_EDIT, RECALL};
use crate::store::{Edit, Memory, NewMemory};
use hrn_kernel::serde_json::Value;
use hrn_kernel::{BoxFuture, Result, Tool, ToolContext, ToolEffect, ToolOutput, ToolSpec};
use std::sync::Arc;

/// Memories one `recall` returns.
const RECALL_LIMIT: usize = 8;
/// Importance given to explicit `retain` writes and to `learn` lessons.
const RETAIN_IMPORTANCE: f64 = 0.75;
const LEARN_IMPORTANCE: f64 = 0.8;
const MAX_CONTENT_CHARS: usize = 4000;
const MAX_CONTEXT_CHARS: usize = 400;
const NO_MATCH: &str = "No relevant memories found.";

/// Which memory tool a [`MemoryTool`] is.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Retain,
    Recall,
    Reflect,
    Edit,
    Learn,
}

/// One memory tool over the shared database.
pub(crate) struct MemoryTool {
    memory: Arc<Memory>,
    kind: Kind,
}

impl MemoryTool {
    pub(crate) fn new(memory: Arc<Memory>, kind: Kind) -> Self {
        Self { memory, kind }
    }
}

impl Tool for MemoryTool {
    fn spec(&self) -> ToolSpec {
        match self.kind {
            Kind::Retain => specs::retain(),
            Kind::Recall => specs::recall(),
            Kind::Reflect => specs::reflect(),
            Kind::Edit => specs::memory_edit(),
            Kind::Learn => specs::learn(),
        }
    }

    /// Memory writes touch only the memory database, never the workspace, so like omp's
    /// (`approval = "read"`) they stay available in plan mode and batch in parallel.
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        let text = match self.kind {
            Kind::Retain => args
                .pointer("/items/0/content")
                .and_then(Value::as_str)
                .map(str::to_owned),
            Kind::Recall | Kind::Reflect => str_arg(args, "query"),
            Kind::Edit => str_arg(args, "id"),
            Kind::Learn => str_arg(args, "memory"),
        };
        crate::fmt::clip(&text.unwrap_or_default(), 100)
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let memory = Arc::clone(&self.memory);
        let kind = self.kind;
        let call = Call {
            project: project_key(&ctx.cwd),
            session: ctx.session_id.clone(),
            cwd: ctx.cwd.to_string_lossy().into_owned(),
        };
        Box::pin(async move {
            match kind {
                Kind::Retain => retain(&memory, &call, &args),
                Kind::Recall => recall(&memory, &call, &args),
                Kind::Reflect => reflect(&memory, &call, &args),
                Kind::Edit => edit(&memory, &call, &args),
                Kind::Learn => learn(&memory, &call, &args),
            }
        })
    }
}

/// What one call knows about where it runs.
struct Call {
    project: String,
    session: String,
    cwd: String,
}

impl Call {
    fn scopes(&self) -> [&str; 2] {
        [&self.project, GLOBAL]
    }

    fn scope_of<'a>(&'a self, requested: Option<&str>) -> &'a str {
        if requested == Some("global") {
            GLOBAL
        } else {
            &self.project
        }
    }
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A bounded, redacted text field; `Err` is the message for the model.
fn text_field(value: &str, what: &str, max: usize) -> std::result::Result<String, String> {
    if value.chars().count() > max {
        return Err(format!(
            "{what} is over {max} characters; split it or shorten it"
        ));
    }
    Ok(redact(value))
}

/// One validated, redacted memory about to be stored.
struct Draft<'a> {
    text: String,
    note: String,
    scope: &'a str,
}

impl<'a> Draft<'a> {
    /// Read `text_key` (the memory) and `context` from `obj`; `Err` is the message for the
    /// model. `label` prefixes error messages.
    fn parse(
        call: &'a Call,
        obj: &Value,
        text_key: &str,
        label: &str,
    ) -> std::result::Result<Self, String> {
        let Some(text) = str_arg(obj, text_key) else {
            return Err(format!("{label}`{text_key}` is required"));
        };
        let note = str_arg(obj, "context").unwrap_or_default();
        Ok(Self {
            text: text_field(&text, &format!("{label}{text_key}"), MAX_CONTENT_CHARS)?,
            note: text_field(&note, &format!("{label}context"), MAX_CONTEXT_CHARS)?,
            scope: call.scope_of(obj.get("scope").and_then(Value::as_str)),
        })
    }
}

/// Write `drafts` and return their ids.
fn store(
    memory: &Memory,
    call: &Call,
    source: &'static str,
    importance: f64,
    drafts: &[Draft<'_>],
) -> Result<Vec<String>> {
    let rows: Vec<NewMemory<'_>> = drafts
        .iter()
        .map(|d| NewMemory {
            scope: d.scope,
            content: &d.text,
            context: &d.note,
            source,
            importance,
            session_id: &call.session,
            cwd: &call.cwd,
        })
        .collect();
    memory.retain(&rows)
}

fn retain(memory: &Memory, call: &Call, args: &Value) -> Result<ToolOutput> {
    let Some(raw) = args
        .get("items")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    else {
        return Ok(ToolOutput::err("`items` must hold at least one memory"));
    };
    let mut drafts = Vec::with_capacity(raw.len());
    for (index, item) in raw.iter().enumerate() {
        match Draft::parse(call, item, "content", &format!("item {} ", index + 1)) {
            Ok(draft) => drafts.push(draft),
            Err(message) => return Ok(ToolOutput::err(message)),
        }
    }
    let ids = store(memory, call, "retain", RETAIN_IMPORTANCE, &drafts)?;
    let noun = if ids.len() == 1 { "memory" } else { "memories" };
    Ok(ToolOutput::ok(format!(
        "{} {noun} stored (ids: {}).",
        ids.len(),
        ids.join(", ")
    )))
}

fn learn(memory: &Memory, call: &Call, args: &Value) -> Result<ToolOutput> {
    let draft = match Draft::parse(call, args, "memory", "") {
        Ok(draft) => draft,
        Err(message) => return Ok(ToolOutput::err(message)),
    };
    let ids = store(memory, call, "learn", LEARN_IMPORTANCE, &[draft])?;
    Ok(ToolOutput::ok(format!(
        "Lesson stored (id: {}).",
        ids.join(", ")
    )))
}

/// Recall `query` and render the bullets; `None` when nothing matched.
fn search_bullets(memory: &Memory, call: &Call, query: &str) -> Result<Option<String>> {
    let Some(expr) = match_expr(query) else {
        return Ok(None);
    };
    let hits = memory.search(&expr, call.scopes(), RECALL_LIMIT)?;
    if hits.is_empty() {
        return Ok(None);
    }
    let lines: Vec<String> = hits.iter().map(|e| bullet(e, PREVIEW_CHARS)).collect();
    Ok(Some(lines.join("\n")))
}

fn recall(memory: &Memory, call: &Call, args: &Value) -> Result<ToolOutput> {
    let Some(query) = str_arg(args, "query") else {
        return Ok(ToolOutput::err("`query` is required"));
    };
    Ok(match search_bullets(memory, call, &query)? {
        Some(lines) => {
            let n = lines.lines().count();
            let noun = if n == 1 { "memory" } else { "memories" };
            ToolOutput::ok(format!(
                "Found {n} relevant {noun} (as of {} UTC):\n\n{lines}",
                utc_minute(crate::fmt::now_ms())
            ))
        }
        None => ToolOutput::ok(NO_MATCH),
    })
}

fn reflect(memory: &Memory, call: &Call, args: &Value) -> Result<ToolOutput> {
    let Some(query) = str_arg(args, "query") else {
        return Ok(ToolOutput::err("`query` is required"));
    };
    let full = match str_arg(args, "context") {
        Some(extra) => format!("{query}\n\nAdditional context:\n{extra}"),
        None => query,
    };
    Ok(match search_bullets(memory, call, &full)? {
        Some(lines) => ToolOutput::ok(format!("Based on recalled memories:\n\n{lines}")),
        None => ToolOutput::ok("No relevant information found to reflect on."),
    })
}

fn edit(memory: &Memory, call: &Call, args: &Value) -> Result<ToolOutput> {
    let (Some(op), Some(id)) = (str_arg(args, "op"), str_arg(args, "id")) else {
        return Ok(ToolOutput::err("`op` and `id` are required"));
    };
    let content =
        match str_arg(args, "content").map(|c| text_field(&c, "content", MAX_CONTENT_CHARS)) {
            Some(Ok(c)) => Some(c),
            Some(Err(message)) => return Ok(ToolOutput::err(message)),
            None => None,
        };
    let importance = args
        .get("importance")
        .and_then(Value::as_f64)
        .map(|v| v.clamp(0.0, 1.0));
    let replacement = str_arg(args, "replacement_id");
    let (change, done) = match op.as_str() {
        "update" => {
            if content.is_none() && importance.is_none() {
                return Ok(ToolOutput::err(format!(
                    "{MEMORY_EDIT} update requires content or importance"
                )));
            }
            (
                Edit::Update {
                    content: content.as_deref(),
                    importance,
                },
                "updated",
            )
        }
        "forget" => (Edit::Forget, "deleted"),
        "invalidate" => {
            if let Some(rep) = &replacement
                && memory.get(rep, call.scopes())?.is_none()
            {
                return Ok(ToolOutput::err(format!(
                    "replacement memory `{rep}` was not found"
                )));
            }
            (
                Edit::Invalidate {
                    replacement_id: replacement.as_deref(),
                },
                "invalidated",
            )
        }
        other => {
            return Ok(ToolOutput::err(format!(
                "unknown op `{other}` (update, forget or invalidate)"
            )));
        }
    };
    Ok(if memory.edit(&id, call.scopes(), &change)? {
        ToolOutput::ok(format!("Memory {id} {done}."))
    } else {
        ToolOutput::ok(format!(
            "Memory {id} was not found in this project's or the global memories; ids come from `{RECALL}`."
        ))
    })
}

/// The five memory tools over `memory`.
pub(crate) fn all(memory: &Arc<Memory>) -> Vec<Arc<dyn Tool>> {
    [
        Kind::Retain,
        Kind::Recall,
        Kind::Reflect,
        Kind::Edit,
        Kind::Learn,
    ]
    .into_iter()
    .map(|kind| Arc::new(MemoryTool::new(Arc::clone(memory), kind)) as Arc<dyn Tool>)
    .collect()
}
