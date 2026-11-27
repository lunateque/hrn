//! `proc://`: every background job of the session (agents, bash jobs, services) through the
//! internal URL router: status and output tail, the full output, and cancellation.

use crate::agents::AgentRegistry;
use crate::agents::{detail, line};
use hrn_kernel::{
    HrnError, ProcEntry, ProcSource, ProcTable, Result, ToolContext, UrlBody, UrlScheme,
};
use std::sync::Arc;

impl ProcSource for AgentRegistry {
    fn entries(&self) -> Vec<ProcEntry> {
        self.records()
            .iter()
            .map(|agent| {
                let s = agent.snapshot();
                ProcEntry {
                    id: agent.id.clone(),
                    root: agent.root.clone(),
                    owner: agent.owner.clone(),
                    running: agent.is_running(),
                    waitable: !agent.is_main,
                    started_ms: agent.started_ms,
                    finished_ms: s.finished_ms,
                    line: line(agent, self.clock().as_ref()),
                }
            })
            .collect()
    }

    fn detail(&self, id: &str) -> Option<String> {
        self.get(id)
            .map(|agent| detail(&agent, self.artifacts().as_deref(), self.clock().as_ref()))
    }

    fn streams(&self, id: &str) -> Option<String> {
        let agent = self.get(id)?;
        let s = agent.snapshot();
        Some(if agent.is_running() {
            "(the agent is still running; its output arrives when it settles)".to_owned()
        } else {
            s.output
        })
    }

    fn kill(&self, id: &str) -> Option<Result> {
        self.get(id)?;
        Some(Self::kill(self, id))
    }
}

/// `proc://`: the jobs of this session.
pub struct ProcScheme {
    table: Arc<ProcTable>,
}

impl ProcScheme {
    /// Serve the jobs of `table`.
    pub fn new(table: Arc<ProcTable>) -> Self {
        Self { table }
    }

    /// `id` when it belongs to the caller's tree.
    fn visible<'a>(&self, ctx: &ToolContext, id: &'a str) -> Result<&'a str> {
        if self
            .table
            .in_root(&ctx.root_session_id)
            .iter()
            .any(|e| e.id == id)
        {
            Ok(id)
        } else {
            Err(HrnError::Tool(format!(
                "no background job `{id}` in this session (list them with `read proc://`)"
            )))
        }
    }
}

impl UrlScheme for ProcScheme {
    fn scheme(&self) -> &'static str {
        "proc"
    }

    fn about(&self) -> &'static str {
        "background jobs of this session: agents, `bash` jobs (`async`, `pty`) and services \
         (`name`); bare `proc://` lists them, `proc://<id>` is the status and output tail, \
         `proc://<id>/streams` the full output so far, `write proc://<id>` sends input to a \
         `pty` job (Enter is appended unless the content ends with a newline), and \
         `write proc://<id>/kill` with empty content stops it"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let rest = rest.trim_matches('/');
        if rest.is_empty() {
            let out = self
                .table
                .in_root(&ctx.root_session_id)
                .into_iter()
                .map(|e| e.line)
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(UrlBody::Text(if out.is_empty() {
                "(no background jobs yet)".to_owned()
            } else {
                out
            }));
        }
        let (id, sub) = rest.split_once('/').unwrap_or((rest, ""));
        let id = self.visible(ctx, id)?;
        let text = match sub {
            "" => self.table.ask(id, |s, id| s.detail(id)),
            "streams" => self.table.ask(id, |s, id| s.streams(id)),
            other => {
                return Err(HrnError::Tool(format!(
                    "unknown `proc://{id}/{other}`; use `proc://{id}` or `proc://{id}/streams`"
                )));
            }
        };
        text.map(UrlBody::Text)
            .ok_or_else(|| HrnError::Tool(format!("background job `{id}` is gone")))
    }

    fn write(&self, ctx: &ToolContext, rest: &str, content: &str) -> Result {
        let rest = rest.trim_matches('/');
        let (id, sub) = rest.split_once('/').unwrap_or((rest, ""));
        match sub {
            "" if !id.is_empty() => {
                let id = self.visible(ctx, id)?;
                self.table
                    .ask(id, |s, id| s.write(id, content))
                    .unwrap_or_else(|| {
                        Err(HrnError::Tool(format!(
                            "`{id}` takes no input: only a `bash` job started with `pty: true` does"
                        )))
                    })
            }
            "kill" => {
                if !content.trim().is_empty() {
                    return Err(HrnError::Tool(
                        "`proc://<id>/kill` takes empty content".to_owned(),
                    ));
                }
                let id = self.visible(ctx, id)?;
                self.table.ask(id, |s, id| s.kill(id)).unwrap_or_else(|| {
                    Err(HrnError::Tool(format!("background job `{id}` is gone")))
                })
            }
            _ => Err(HrnError::Tool(
                "`proc://` accepts `write proc://<id>` (input for a pty job; Enter is appended) \
                 and `write proc://<id>/kill` with empty content"
                    .to_owned(),
            )),
        }
    }
}
