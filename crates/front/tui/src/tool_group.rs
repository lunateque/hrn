//! Antigravity-style tool-call grouping (`collapse_done` / `keep_open` on errors).
//!
//! While a tool group is still open (accepting consecutive tool calls or running/awaiting), or
//! contains any failed/denied/cancelled call, or is expanded (`Ctrl+O`), every individual
//! [`ToolCard`] renders verbatim through [`crate::tool::render`]. Once closed with all calls
//! succeeded, the group collapses into a single summary line.

use crate::imgview;
use crate::render::{self, Ctx};
use crate::symbols;
use crate::theme;
use crate::tool;
use crate::toolfmt;
use crate::wrap;
use hrn_kernel::ImageAttachment;
use ratatui::text::{Line, Span};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolState {
    Running,
    Awaiting,
    Done,
    Failed,
    Denied,
    Cancelled,
}

pub(crate) struct ToolCard {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) key: String,
    pub(crate) state: ToolState,
    pub(crate) live: String,
    pub(crate) output: String,
    pub(crate) diff: Option<String>,
    pub(crate) started: Option<Instant>,
    pub(crate) elapsed: Option<Duration>,
}

impl ToolCard {
    pub(crate) fn new(id: &str, name: &str, arguments: &str, started: Option<Instant>) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            key: toolfmt::key_arg(name, arguments),
            state: ToolState::Running,
            live: String::new(),
            output: String::new(),
            diff: None,
            started,
            elapsed: None,
        }
    }

    pub(crate) fn finish(&mut self, output: &str, is_error: bool) {
        self.state = if !is_error {
            ToolState::Done
        } else if output.starts_with("user denied") {
            ToolState::Denied
        } else if output.starts_with("cancelled") || output.starts_with("not executed") {
            ToolState::Cancelled
        } else {
            ToolState::Failed
        };
        output.clone_into(&mut self.output);
        self.live.clear();
    }
}

/// A contiguous run of tool calls rendered either as individual cards (while active, errored, or
/// expanded via `Ctrl+O`) or as a single collapsed Antigravity summary line once done.
pub(crate) struct ToolGroup {
    pub(crate) cards: Vec<ToolCard>,
    pub(crate) images: Vec<Vec<(Option<usize>, ImageAttachment)>>,
    pub(crate) closed: bool,
}

impl ToolGroup {
    pub(crate) fn new(card: ToolCard) -> Self {
        Self {
            cards: vec![card],
            images: vec![Vec::new()],
            closed: false,
        }
    }

    pub(crate) fn push(&mut self, card: ToolCard) {
        self.cards.push(card);
        self.images.push(Vec::new());
    }

    pub(crate) fn has_active(&self) -> bool {
        self.cards
            .iter()
            .any(|c| matches!(c.state, ToolState::Running | ToolState::Awaiting))
    }

    pub(crate) fn has_error(&self) -> bool {
        self.cards.iter().any(|c| {
            matches!(
                c.state,
                ToolState::Failed | ToolState::Denied | ToolState::Cancelled
            )
        })
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.closed && !self.has_active()
    }

    pub(crate) fn should_collapse(&self, expanded: bool) -> bool {
        self.closed && !self.has_active() && !self.has_error() && !expanded
    }
}

/// Render `group`: collapsed summary line when closed & all succeeded & not expanded, otherwise
/// every individual [`ToolCard`] verbatim through [`crate::tool::render`].
pub(crate) fn render(group: &ToolGroup, expanded: bool, ctx: &Ctx) -> Vec<Line<'static>> {
    if group.should_collapse(expanded) {
        let mut lines = vec![summary_line(group, ctx.width)];
        for imgs in &group.images {
            if !imgs.is_empty() {
                lines.extend(imgview::render_gallery(imgs, ctx.width, 8));
            }
        }
        return lines;
    }

    let max_img_rows = if expanded { 14 } else { 8 };
    let mut lines = Vec::new();
    for (idx, card) in group.cards.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::default());
        }
        lines.extend(tool::render(card, expanded, ctx));
        if let Some(imgs) = group.images.get(idx)
            && !imgs.is_empty()
        {
            lines.extend(imgview::render_gallery(imgs, ctx.width, max_img_rows));
        }
    }
    lines
}

#[derive(Default)]
struct Buckets {
    explored: usize,
    searched: usize,
    edited: usize,
    ran: usize,
    fetched: usize,
    agents: usize,
    todo: usize,
    other: usize,
}

impl Buckets {
    fn record(&mut self, name: &str) {
        match name {
            "read" | "glob" | "ls" | "list_dir" | "tree" | "outline" | "ast_outline" | "find" => {
                self.explored += 1;
            }
            "grep" | "search" | "web_search" | "tool_search" | "ast_search" | "lsp" => {
                self.searched += 1;
            }
            "edit" | "write" | "apply_patch" | "multi_edit" | "ast_edit" => {
                self.edited += 1;
            }
            "bash" | "shell" => self.ran += 1,
            "web_fetch" | "browser" => self.fetched += 1,
            "task" | "subagent" | "wait" | "agent_send" | "agent_kill" => self.agents += 1,
            "todo" => self.todo += 1,
            _ => self.other += 1,
        }
    }

    fn parts(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.explored > 0 {
            out.push(plural(self.explored, "Explored", "file", "files"));
        }
        if self.searched > 0 {
            out.push(format!("Searched {}", self.searched));
        }
        if self.edited > 0 {
            out.push(plural(self.edited, "Edited", "file", "files"));
        }
        if self.ran > 0 {
            out.push(format!("Ran {}", self.ran));
        }
        if self.fetched > 0 {
            out.push(format!("Fetched {}", self.fetched));
        }
        if self.agents > 0 {
            out.push(format!("Agents {}", self.agents));
        }
        if self.todo > 0 {
            out.push(if self.todo == 1 {
                "Updated todo".to_owned()
            } else {
                format!("Updated todo {}", self.todo)
            });
        }
        if self.other > 0 {
            out.push(plural(self.other, "Used", "tool", "tools"));
        }
        out
    }
}

fn plural(n: usize, verb: &str, singular: &str, plural: &str) -> String {
    let noun = if n == 1 { singular } else { plural };
    format!("{verb} {n} {noun}")
}

/// Format the one-line Antigravity summary for a completed, non-error tool group.
pub(crate) fn summary_text(group: &ToolGroup) -> String {
    let syms = symbols::current();
    let sep = format!(" {} ", syms.dot);
    let mut buckets = Buckets::default();
    for card in &group.cards {
        buckets.record(&card.name);
    }
    let parts = buckets.parts();
    let mut text = if parts.is_empty() {
        "Used tools".to_owned()
    } else {
        parts.join(&sep)
    };
    if let [only] = group.cards.as_slice() {
        let key = wrap::sanitize(&only.key);
        let key = key.trim();
        if !key.is_empty() {
            text.push_str(&sep);
            text.push_str(key);
        }
    }
    text
}

fn total_elapsed(group: &ToolGroup) -> Option<Duration> {
    let total: Duration = group.cards.iter().filter_map(|c| c.elapsed).sum();
    (total.as_secs() >= 1).then_some(total)
}

fn summary_line(group: &ToolGroup, width: usize) -> Line<'static> {
    let syms = symbols::current();
    let hint = total_elapsed(group).map_or_else(
        || "  (ctrl+o)".to_owned(),
        |d| format!(" {} {}  (ctrl+o)", syms.dot, render::duration(d)),
    );
    let prefix = " ▸ ";
    let reserved = prefix.chars().count() + hint.chars().count();
    let room = width.saturating_sub(reserved).max(8);
    let body = wrap::truncate(&summary_text(group), room).into_owned();
    Line::from(vec![
        Span::raw(" "),
        Span::styled("▸ ", theme::muted()),
        Span::styled(body, theme::muted()),
        Span::styled(hint, theme::dim()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;

    fn ctx(verbose: bool) -> Ctx {
        Ctx {
            width: 100,
            verbose,
            frame: 0,
            now: Instant::now(),
        }
    }

    fn line_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn done_card(id: &str, name: &str, args: &str) -> ToolCard {
        let mut c = ToolCard::new(id, name, args, None);
        c.finish("ok", false);
        c
    }

    #[test]
    fn open_group_renders_individual_cards_verbatim() {
        let card = ToolCard::new("1", "read", r#"{"path":"a.rs"}"#, None);
        let individual = tool::render(&card, false, &ctx(false));
        let group = ToolGroup::new(card);
        assert!(!group.should_collapse(false));
        let rendered = render(&group, false, &ctx(false));
        assert_eq!(rendered, individual);
    }

    #[test]
    fn transcript_groups_consecutive_tools_and_settles_on_close() {
        use crate::transcript::Transcript;

        let now = Instant::now();
        let mut t = Transcript::default();
        t.tool_start("1", "read", r#"{"path":"a.rs"}"#, now);
        t.tool_done_with_images("1", "ok", false, None, Vec::new(), now);
        t.tool_start("2", "grep", r#"{"pattern":"x"}"#, now);
        t.tool_done_with_images("2", "ok", false, None, Vec::new(), now);

        // Consecutive tools merge into one open (unsettled) group until a non-tool entry closes it.
        assert_eq!(t.len(), 1);
        assert_eq!(t.settled(), 0);
        assert!(t.lines(0, &ctx(false)).len() > 2);

        t.text("done", now);
        assert_eq!(t.len(), 2);
        assert_eq!(t.settled(), 1);
        let collapsed = line_text(t.lines(0, &ctx(false)));
        assert!(collapsed.contains("Explored 1 file"), "got: {collapsed}");
        assert!(collapsed.contains("Searched 1"), "got: {collapsed}");
    }

    #[test]
    fn closed_done_group_collapses_into_antigravity_summary_and_expands_on_verbose() {
        let mut group = ToolGroup::new(done_card("1", "read", r#"{"path":"a.rs"}"#));
        group.push(done_card("2", "read", r#"{"path":"b.rs"}"#));
        group.push(done_card("3", "grep", r#"{"pattern":"foo"}"#));
        group.push(done_card("4", "edit", r#"{"path":"a.rs"}"#));
        group.push(done_card("5", "bash", r#"{"command":"cargo test"}"#));
        group.closed = true;

        let collapsed = render(&group, false, &ctx(false));
        assert_eq!(collapsed.len(), 1);
        let text = line_text(&collapsed);
        assert!(text.contains("Explored 2 files"), "got: {text}");
        assert!(text.contains("Searched 1"), "got: {text}");
        assert!(text.contains("Edited 1 file"), "got: {text}");
        assert!(text.contains("Ran 1"), "got: {text}");
        assert!(text.contains("(ctrl+o)"), "got: {text}");

        let expanded = render(&group, true, &ctx(true));
        assert!(expanded.len() > 5);
    }

    #[test]
    fn errored_group_stays_open_even_when_closed() {
        let mut group = ToolGroup::new(done_card("1", "read", r#"{"path":"a.rs"}"#));
        let mut failed = ToolCard::new("2", "bash", r#"{"command":"false"}"#, None);
        failed.finish("exit status 1", true);
        group.push(failed);
        group.closed = true;

        assert!(!group.should_collapse(false));
        let rendered = render(&group, false, &ctx(false));
        assert!(rendered.len() > 2);
        let text = line_text(&rendered);
        assert!(text.contains("exit status 1"), "got: {text}");
    }
}
