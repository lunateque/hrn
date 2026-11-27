//! `memory://`: read one memory in full, or list what is stored.

use crate::fmt::{PREVIEW_CHARS, bullet, utc_minute};
use crate::project::project_key;
use crate::store::{Entry, Memory};
use hrn_kernel::{HrnError, Result, ToolContext, UrlBody, UrlScheme};
use std::fmt::Write as _;
use std::sync::Arc;

/// Rows listed by `memory://root`.
const LIST_LIMIT: usize = 100;

/// The `memory://` scheme over the shared database.
pub(crate) struct MemoryScheme {
    memory: Arc<Memory>,
}

impl MemoryScheme {
    pub(crate) fn new(memory: Arc<Memory>) -> Self {
        Self { memory }
    }
}

impl UrlScheme for MemoryScheme {
    fn scheme(&self) -> &'static str {
        "memory"
    }

    fn about(&self) -> &'static str {
        "long-term memories: `memory://<id>` is one memory in full (read it before `memory_edit` \
         update; `recall` shows ids); `memory://root/learned.md` lists the lessons from `learn`"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let project = project_key(&ctx.cwd);
        let scopes = [project.as_str(), crate::project::GLOBAL];
        let target = rest.trim_matches('/');
        match target {
            "" | "root" => Err(HrnError::Tool(
                "`memory://root` has no index; use `recall` to search, or `memory://root/learned.md`"
                    .into(),
            )),
            "root/learned.md" => {
                let lessons = self.memory.lessons(scopes, LIST_LIMIT)?;
                Ok(UrlBody::Text(if lessons.is_empty() {
                    "No lessons captured yet.".to_owned()
                } else {
                    let lines: Vec<String> =
                        lessons.iter().map(|e| bullet(e, PREVIEW_CHARS)).collect();
                    format!("# Lessons (newest first)\n\n{}", lines.join("\n"))
                }))
            }
            id => match self.memory.get(id, scopes)? {
                Some(entry) => Ok(UrlBody::Text(render(&entry))),
                None => Err(HrnError::Tool(format!(
                    "memory `{id}` not found in this project's or the global memories"
                ))),
            },
        }
    }
}

/// The full row: YAML front matter, then the content.
fn render(entry: &Entry) -> String {
    let scope = if entry.is_global() {
        "global"
    } else {
        "project"
    };
    let mut out = format!(
        "---\nid: {}\nscope: {scope}\nsource: {}\nimportance: {}\nstatus: {}\n",
        entry.id, entry.source, entry.importance, entry.status
    );
    if let Some(replacement) = &entry.replacement_id {
        let _ = writeln!(out, "replacement_id: {replacement}");
    }
    if !entry.context.is_empty() {
        let _ = writeln!(out, "context: {}", entry.context.replace('\n', " "));
    }
    let _ = write!(
        out,
        "session_id: {}\ncwd: {}\ncreated: {} UTC\nupdated: {} UTC\n---\n{}",
        entry.session_id,
        entry.cwd,
        utc_minute(entry.created_ms),
        utc_minute(entry.updated_ms),
        entry.content
    );
    out
}
