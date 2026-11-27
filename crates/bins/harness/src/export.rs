//! `/export` and `hrn sessions export`: the active branch of a session as markdown or one
//! self-contained html page (no scripts, no external assets).

#[path = "export_plain.rs"]
mod plain;

pub(crate) use plain::render_plain_text;

use crate::cli::{ExportArgs, ExportFormat};
use crate::commands::format_utc;
use crate::resume::find_session;
use crate::stats::dollars;
use crate::term::out;
use hrn_kernel::{
    DagNode, DagPayload, HrnError, Msg, Result, Role, SessionMeta, SessionStore, Usage,
};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One displayable item of the conversation, in order.
#[derive(Debug, PartialEq, Eq)]
enum Block {
    User(String),
    Assistant(String),
    Reasoning(String),
    Call {
        name: String,
        args: String,
    },
    Output {
        name: String,
        text: String,
        is_error: bool,
    },
    Note(String),
}

/// What the page header says about the session.
struct Header {
    meta: SessionMeta,
    usage: Usage,
}

impl Header {
    /// `label: value` rows, shared by both formats.
    fn rows(&self) -> Vec<(&'static str, String)> {
        let meta = &self.meta;
        let mut tokens = format!(
            "{} in / {} out",
            self.usage.prompt_tokens, self.usage.completion_tokens
        );
        if let Some(micros) = self.usage.cost_micros {
            tokens.push_str(" · ");
            tokens.push_str(&dollars(micros));
        }
        vec![
            ("Session", meta.id.clone()),
            ("Directory", meta.cwd.clone()),
            (
                "Model",
                meta.model.clone().unwrap_or_else(|| "-".to_owned()),
            ),
            ("Created (UTC)", format_utc(meta.created_at_ms)),
            ("Updated (UTC)", format_utc(meta.updated_at_ms)),
            ("Tokens", tokens),
        ]
    }
}

/// Tool arguments pretty-printed when they are valid JSON, as sent otherwise.
fn pretty(args: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| args.to_owned())
}

fn message_blocks(msg: &Msg, out: &mut Vec<Block>) {
    match msg.role {
        Role::System => out.push(Block::Note(format!("system: {}", msg.content))),
        Role::User => {
            out.push(Block::User(msg.content.clone()));
            if !msg.images.is_empty() {
                out.push(Block::Note(format!(
                    "{} image(s) attached",
                    msg.images.len()
                )));
            }
        }
        Role::Assistant => {
            if let Some(reasoning) = msg.reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
                out.push(Block::Reasoning(reasoning.to_owned()));
            }
            if !msg.content.trim().is_empty() {
                out.push(Block::Assistant(msg.content.clone()));
            }
            for call in &msg.tool_calls {
                out.push(Block::Call {
                    name: call.name.clone(),
                    args: pretty(&call.arguments),
                });
            }
        }
        Role::Tool => out.push(Block::Output {
            name: msg.tool_name.clone().unwrap_or_else(|| "tool".to_owned()),
            text: msg.content.clone(),
            is_error: msg.is_error,
        }),
    }
}

fn blocks(nodes: &[DagNode]) -> Vec<Block> {
    let mut out = Vec::new();
    for node in nodes {
        match &node.payload {
            DagPayload::Message { msg, .. } => message_blocks(msg, &mut out),
            DagPayload::Summary { text, .. } => {
                out.push(Block::Note(format!("earlier turns, compacted: {text}")));
            }
            DagPayload::ForkMarker { label } => out.push(Block::Note(format!("fork: {label}"))),
            DagPayload::ToolsActivated { names } => {
                out.push(Block::Note(format!(
                    "tools activated: {}",
                    names.join(", ")
                )));
            }
            DagPayload::TodoBoard { phases } => {
                out.push(Block::Note(format!(
                    "todo list:\n{}",
                    hrn_kernel::todo::to_markdown(phases)
                )));
            }
            DagPayload::BranchSummary { text, .. } => {
                out.push(Block::Note(format!("branch summary: {text}")));
            }
            DagPayload::Label {
                target_node_id,
                label: Some(label),
            } => {
                out.push(Block::Note(format!("label {target_node_id}: {label}")));
            }
            DagPayload::ModelChange { selector, .. } => {
                out.push(Block::Note(format!("model switched to {selector}")));
            }
            DagPayload::Label { label: None, .. } | DagPayload::SessionExit { .. } => {}
        }
    }
    out
}

/// A code fence longer than any backtick run inside `text`.
fn fence(text: &str) -> String {
    let (mut longest, mut run) = (0, 0);
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    "`".repeat(longest.max(2) + 1)
}

fn fenced(lang: &str, text: &str) -> String {
    let fence = fence(text);
    format!("{fence}{lang}\n{}\n{fence}\n\n", text.trim_end())
}

fn markdown(head: &Header, blocks: &[Block]) -> String {
    let mut o = format!("# {}\n\n", head.meta.title.trim());
    for (label, value) in head.rows() {
        let _ = writeln!(o, "- {label}: `{value}`");
    }
    o.push('\n');
    for block in blocks {
        match block {
            Block::User(text) => {
                let _ = write!(o, "## User\n\n{}\n\n", text.trim());
            }
            Block::Assistant(text) => {
                let _ = write!(o, "## Assistant\n\n{}\n\n", text.trim());
            }
            Block::Reasoning(text) => {
                let _ = write!(
                    o,
                    "<details><summary>Reasoning</summary>\n\n{}\n\n</details>\n\n",
                    text.trim()
                );
            }
            Block::Call { name, args } => {
                let _ = write!(o, "### Tool call: `{name}`\n\n{}", fenced("json", args));
            }
            Block::Output {
                name,
                text,
                is_error,
            } => {
                let flag = if *is_error { " (error)" } else { "" };
                let _ = write!(
                    o,
                    "### Tool result: `{name}`{flag}\n\n{}",
                    fenced("text", text)
                );
            }
            Block::Note(text) => {
                let _ = write!(o, "> {}\n\n", text.trim_end().replace('\n', "\n> "));
            }
        }
    }
    o
}

fn escape(text: &str) -> String {
    let mut o = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c => o.push(c),
        }
    }
    o
}

const STYLE: &str = "\
:root{color-scheme:light dark;--bg:#fff;--fg:#1f2328;--dim:#656d76;--line:#d0d7de;--user:#ddf4ff;--code:#f6f8fa}\
@media(prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--dim:#8b949e;--line:#30363d;--user:#12263f;--code:#161b22}}\
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}\
main{max-width:56rem;margin:0 auto;padding:1.5rem}\
h1{font-size:1.3rem;margin:0 0 .5rem}\
.meta{color:var(--dim);font-size:.85rem;margin:0 0 1.5rem}.meta b{color:var(--fg);font-weight:600}\
.msg{border:1px solid var(--line);border-radius:6px;margin:.75rem 0;padding:.5rem .8rem}\
.msg.user{background:var(--user)}\
.role{font-size:.75rem;text-transform:uppercase;letter-spacing:.05em;color:var(--dim);margin-bottom:.25rem}\
pre{margin:0;white-space:pre-wrap;word-break:break-word;font:13px/1.45 ui-monospace,Consolas,monospace}\
details{margin:.75rem 0;border:1px solid var(--line);border-radius:6px;padding:.4rem .8rem;background:var(--code)}\
details.error{border-color:#cf222e}\
summary{cursor:pointer;color:var(--dim);font-size:.85rem}details pre{margin-top:.5rem}\
.note{color:var(--dim);font-style:italic;margin:.75rem 0;font-size:.85rem}";

fn html(head: &Header, blocks: &[Block]) -> String {
    let title = escape(head.meta.title.trim());
    let mut o = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{STYLE}</style></head><body><main>\n<h1>{title}</h1>\n<p class=\"meta\">"
    );
    for (label, value) in head.rows() {
        let _ = write!(o, "{label}: <b>{}</b><br>", escape(&value));
    }
    o.push_str("</p>\n");
    for block in blocks {
        match block {
            Block::User(text) => {
                let _ = writeln!(
                    o,
                    "<div class=\"msg user\"><div class=\"role\">User</div><pre>{}</pre></div>",
                    escape(text.trim())
                );
            }
            Block::Assistant(text) => {
                let _ = writeln!(
                    o,
                    "<div class=\"msg\"><div class=\"role\">Assistant</div><pre>{}</pre></div>",
                    escape(text.trim())
                );
            }
            Block::Reasoning(text) => {
                let _ = writeln!(
                    o,
                    "<details><summary>Reasoning</summary><pre>{}</pre></details>",
                    escape(text.trim())
                );
            }
            Block::Call { name, args } => {
                let _ = writeln!(
                    o,
                    "<details><summary>Tool call · <b>{}</b></summary><pre>{}</pre></details>",
                    escape(name),
                    escape(args)
                );
            }
            Block::Output {
                name,
                text,
                is_error,
            } => {
                let (class, flag) = if *is_error {
                    (" class=\"error\"", " · error")
                } else {
                    ("", "")
                };
                let _ = writeln!(
                    o,
                    "<details{class}><summary>Tool result · <b>{}</b>{flag}</summary><pre>{}</pre></details>",
                    escape(name),
                    escape(text)
                );
            }
            Block::Note(text) => {
                let _ = writeln!(o, "<p class=\"note\">{}</p>", escape(text));
            }
        }
    }
    o.push_str("</main></body></html>\n");
    o
}

fn load_session_blocks(store: &dyn SessionStore, id: &str) -> Result<(Header, Vec<Block>)> {
    let meta = store
        .get_session(id)?
        .ok_or_else(|| HrnError::Store(format!("unknown session `{id}`")))?;
    let Some(head_id) = meta.head_node_id.clone() else {
        return Err(HrnError::Config(format!(
            "session `{id}` has no messages yet"
        )));
    };
    let blocks = blocks(&store.load_branch(id, &head_id)?);
    let head = Header {
        usage: store.session_usage(id)?,
        meta,
    };
    Ok((head, blocks))
}

/// The session `id` rendered as `format`.
///
/// # Errors
/// The session is unknown, empty, or unreadable.
fn render(store: &dyn SessionStore, id: &str, format: ExportFormat) -> Result<String> {
    let (head, blocks) = load_session_blocks(store, id)?;
    Ok(match format {
        ExportFormat::Md => markdown(&head, &blocks),
        ExportFormat::Html => html(&head, &blocks),
    })
}

/// Markdown unless asked for html, by `--format` or by the file extension.
fn pick_format(explicit: Option<ExportFormat>, path: Option<&Path>) -> ExportFormat {
    explicit.unwrap_or_else(|| {
        let html = path
            .and_then(Path::extension)
            .is_some_and(|ext| ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm"));
        if html {
            ExportFormat::Html
        } else {
            ExportFormat::Md
        }
    })
}

/// Write the session to `path`, or to `hrn-session-<id>.<ext>` in `dir`; returns the file.
///
/// # Errors
/// The session cannot be rendered or the file cannot be written.
pub(crate) fn write_file(
    store: &dyn SessionStore,
    id: &str,
    path: Option<&Path>,
    format: Option<ExportFormat>,
    dir: &Path,
) -> Result<PathBuf> {
    if path == Some(Path::new("-")) {
        return Err(HrnError::Config(
            "`-` (stdout) is only for `hrn sessions export`".to_owned(),
        ));
    }
    let format = pick_format(format, path);
    let text = render(store, id, format)?;
    let target = path.map_or_else(
        || {
            let ext = match format {
                ExportFormat::Md => "md",
                ExportFormat::Html => "html",
            };
            let stem: String = id
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            dir.join(format!("hrn-session-{stem}.{ext}"))
        },
        Path::to_path_buf,
    );
    std::fs::write(&target, text)
        .map_err(|err| HrnError::Io(format!("write {}: {err}", target.display())))?;
    Ok(target)
}

/// `hrn sessions export`.
///
/// # Errors
/// The session is unknown or cannot be exported.
pub(crate) fn run(store: &dyn SessionStore, args: &ExportArgs) -> Result {
    let id = find_session(store, &args.session)?;
    if args.output.as_deref() == Some(Path::new("-")) {
        out(&render(
            store,
            &id,
            args.format.unwrap_or(ExportFormat::Md),
        )?);
        return Ok(());
    }
    let written = write_file(
        store,
        &id,
        args.output.as_deref(),
        args.format,
        &std::env::current_dir()?,
    )?;
    out(&format!("Exported to: {}\n", written.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::{AgentStatus, PermissionMode, ThinkingLevel, ToolCall};

    fn head() -> Header {
        Header {
            meta: SessionMeta {
                pinned: false,
                id: "s1".into(),
                title: "Fix <it>".into(),
                cwd: "/w".into(),
                branch: None,
                head_node_id: Some("n".into()),
                model: Some("flash".into()),
                thinking: ThinkingLevel::Medium,
                goal: None,
                permission_mode: PermissionMode::Default,
                parent_session_id: None,
                parent_node_id: None,
                status: AgentStatus::Idle,
                title_source: None,
                exit_ms: None,
                created_at_ms: 0,
                updated_at_ms: 0,
            },
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                cost_micros: Some(12_300),
                ..Usage::default()
            },
        }
    }

    fn node(msg: Msg) -> DagNode {
        DagNode {
            id: "n".into(),
            session_id: "s1".into(),
            parent_id: None,
            created_at_ms: 0,
            payload: DagPayload::Message {
                turn_id: "t".into(),
                msg,
            },
        }
    }

    #[test]
    fn a_turn_becomes_ordered_blocks() {
        let mut reply = Msg::assistant("done");
        reply.tool_calls.push(ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: r#"{"path":"a"}"#.into(),
            signature: None,
        });
        let got = blocks(&[node(Msg::user("hi")), node(reply)]);
        assert_eq!(got.len(), 3);
        assert_eq!(got.first(), Some(&Block::User("hi".into())));
        assert!(matches!(got.get(2), Some(Block::Call { name, args })
            if name == "read" && args.contains("\n  \"path\"")));
    }

    #[test]
    fn markdown_fences_outgrow_the_backticks_they_hold() {
        let blocks = [
            Block::User("hi".into()),
            Block::Output {
                name: "bash".into(),
                text: "```\nx\n```".into(),
                is_error: true,
            },
        ];
        let md = markdown(&head(), &blocks);
        assert!(md.starts_with("# Fix <it>\n"), "{md}");
        assert!(md.contains("- Tokens: `10 in / 5 out · $0.0123`"), "{md}");
        assert!(md.contains("## User\n\nhi"), "{md}");
        assert!(
            md.contains("### Tool result: `bash` (error)\n\n````text\n```\nx\n```\n````"),
            "{md}"
        );
    }

    #[test]
    fn html_escapes_everything_it_shows() {
        let page = html(
            &head(),
            &[Block::Assistant("<script>alert(1)</script>".into())],
        );
        assert!(!page.contains("<script>"), "{page}");
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(page.contains("<title>Fix &lt;it&gt;</title>"));
    }

    #[test]
    fn the_extension_picks_the_format() {
        assert_eq!(
            pick_format(None, Some(Path::new("a.HTML"))),
            ExportFormat::Html
        );
        assert_eq!(
            pick_format(None, Some(Path::new("a.txt"))),
            ExportFormat::Md
        );
        assert_eq!(pick_format(None, None), ExportFormat::Md);
        assert_eq!(
            pick_format(Some(ExportFormat::Md), Some(Path::new("a.html"))),
            ExportFormat::Md
        );
    }
}
