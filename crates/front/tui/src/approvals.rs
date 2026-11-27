//! Interactive approval dock: mode-appropriate choices, reason line, `Ctrl+O` detail toggle,
//! 500 ms accidental-press guard, remappable approve/deny chords, and composer-draft
//! `"Deny and send message"` highlight.

use crate::card::{Frame, Section};
use crate::diff::{self, Limit};
use crate::keymap::Keymap;
use crate::theme;
use crate::toolfmt::Preview;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hrn_kernel::{ApprovalDecision, ApprovalGrantScope, ApprovalRequest, PermissionMode};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use std::time::{Duration, Instant};

/// Accidental-press guard window: approve/deny keystrokes are ignored for the first 500 ms after
/// the dock opens (or advances to the next queued request).
pub(crate) const GUARD_WINDOW: Duration = Duration::from_millis(500);

/// Default preview rows when the dock is collapsed.
const COMPACT_PREVIEW: usize = 12;

/// One selectable row in the approval dock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DockChoice {
    Once,
    File,
    Directory,
    ToolSession,
    Persistent,
    Deny,
    DenyAndSend,
}

impl DockChoice {
    fn label(self, tool: &str) -> String {
        match self {
            Self::Once => "Allow once".to_owned(),
            Self::File => "Allow this file (session)".to_owned(),
            Self::Directory => "Allow directory (direct children)".to_owned(),
            Self::ToolSession => format!("Always allow {tool} (session)"),
            Self::Persistent => "Remember across sessions".to_owned(),
            Self::Deny => "Deny".to_owned(),
            Self::DenyAndSend => "Deny and send message".to_owned(),
        }
    }

    fn shortcut(self) -> &'static str {
        match self {
            Self::Once => "y",
            Self::File => "f",
            Self::Directory => "d",
            Self::ToolSession => "a",
            Self::Persistent => "r",
            Self::Deny | Self::DenyAndSend => "n",
        }
    }

    /// Convert this choice into a kernel [`ApprovalDecision`].
    pub(crate) fn into_decision(self, draft: Option<String>) -> ApprovalDecision {
        match self {
            Self::Once => ApprovalDecision::Approve {
                scope: ApprovalGrantScope::Once,
            },
            Self::File => ApprovalDecision::Approve {
                scope: ApprovalGrantScope::File,
            },
            Self::Directory => ApprovalDecision::Approve {
                scope: ApprovalGrantScope::Directory,
            },
            Self::ToolSession => ApprovalDecision::Approve {
                scope: ApprovalGrantScope::ToolSession,
            },
            Self::Persistent => ApprovalDecision::Approve {
                scope: ApprovalGrantScope::Persistent,
            },
            Self::Deny => ApprovalDecision::Deny,
            Self::DenyAndSend => match draft.filter(|m| !m.trim().is_empty()) {
                Some(message) => ApprovalDecision::DenyAndSend { message },
                None => ApprovalDecision::Deny,
            },
        }
    }
}

/// Live UI state for the front-most pending approval prompt.
#[derive(Clone, Debug)]
pub(crate) struct ApprovalDock {
    pub(crate) opened_at: Instant,
    pub(crate) selected: usize,
    pub(crate) expanded: bool,
}

impl ApprovalDock {
    /// Open a fresh dock at `now`; when the composer already holds draft text, highlight
    /// `"Deny and send message"` by default.
    pub(crate) fn open(now: Instant, choices: &[DockChoice], has_draft: bool) -> Self {
        let selected = if has_draft {
            choices
                .iter()
                .position(|c| *c == DockChoice::DenyAndSend)
                .unwrap_or(0)
        } else {
            0
        };
        Self {
            opened_at: now,
            selected,
            expanded: false,
        }
    }

    /// Whether the 500 ms accidental-press guard is still active at `now`.
    pub(crate) fn is_guarded(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.opened_at) < GUARD_WINDOW
    }

    /// When the guard window ends (used by `next_wake`).
    pub(crate) fn guard_until(&self) -> Instant {
        self.opened_at + GUARD_WINDOW
    }
}

/// Whether `request` permits only plain Approve/Deny (e.g. restricted folder or critical rule).
pub(crate) fn is_plain_only(request: &ApprovalRequest) -> bool {
    request.choices.as_slice() == [ApprovalGrantScope::Once]
}

/// Build the ordered choice list for `request` under `mode`.
pub(crate) fn choices_for(
    request: &ApprovalRequest,
    mode: PermissionMode,
    has_draft: bool,
) -> Vec<DockChoice> {
    let mut out = Vec::new();
    if !request.choices.is_empty() {
        for scope in &request.choices {
            let choice = match scope {
                ApprovalGrantScope::Once => DockChoice::Once,
                ApprovalGrantScope::File => DockChoice::File,
                ApprovalGrantScope::Directory => DockChoice::Directory,
                ApprovalGrantScope::ToolSession => DockChoice::ToolSession,
                ApprovalGrantScope::Persistent => DockChoice::Persistent,
            };
            if !out.contains(&choice) {
                out.push(choice);
            }
        }
    } else if mode == PermissionMode::AskCached {
        out.push(DockChoice::Once);
        if !request.targets.is_empty() && request.tool_name != "bash" {
            out.push(DockChoice::File);
            out.push(DockChoice::Directory);
        }
        out.push(DockChoice::ToolSession);
        out.push(DockChoice::Persistent);
    } else {
        out.push(DockChoice::Once);
        out.push(DockChoice::ToolSession);
    }
    out.push(if has_draft {
        DockChoice::DenyAndSend
    } else {
        DockChoice::Deny
    });
    out
}

/// Outcome of routing one key event to the approval dock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DockAction {
    /// Key does not belong to the dock (caller may try global hotkeys).
    Ignored,
    /// Key was consumed (navigation, `Ctrl+O` toggle, or blocked by the 500 ms guard).
    Consumed,
    /// User committed a decision after the 500 ms guard elapsed.
    Decide(DockChoice),
}

/// Handle one terminal key event while an approval dock is active.
pub(crate) fn handle_key(
    dock: &mut ApprovalDock,
    key: KeyEvent,
    keymap: &Keymap,
    choices: &[DockChoice],
    now: Instant,
) -> DockAction {
    if keymap.is("app.tools.expand", key) {
        dock.expanded = !dock.expanded;
        return DockAction::Consumed;
    }
    if choices.is_empty() {
        return DockAction::Ignored;
    }
    let len = choices.len();
    dock.selected = dock.selected.min(len - 1);
    match key.code {
        KeyCode::Up if key.modifiers.is_empty() => {
            dock.selected = (dock.selected + len - 1) % len;
            return DockAction::Consumed;
        }
        KeyCode::Down if key.modifiers.is_empty() => {
            dock.selected = (dock.selected + 1) % len;
            return DockAction::Consumed;
        }
        _ => {}
    }

    let candidate = resolve_decision_key(key, keymap, choices, dock.selected);
    let Some(choice) = candidate else {
        return DockAction::Ignored;
    };
    if dock.is_guarded(now) {
        return DockAction::Consumed;
    }
    DockAction::Decide(choice)
}

fn resolve_decision_key(
    key: KeyEvent,
    keymap: &Keymap,
    choices: &[DockChoice],
    selected: usize,
) -> Option<DockChoice> {
    if keymap.is("app.approval.approve", key) {
        return Some(DockChoice::Once);
    }
    // `Esc` / `app.approval.deny` ALWAYS performs plain `Deny` so an unfinished composer draft
    // is never accidentally submitted; pressing `Enter` on the highlighted "Deny and send message"
    // row commits `DenyAndSend`.
    if keymap.is("app.approval.deny", key) {
        return Some(DockChoice::Deny);
    }
    if key.code == KeyCode::Enter && key.modifiers.is_empty() {
        return choices.get(selected).copied();
    }
    let plain = (key.modifiers - KeyModifiers::SHIFT).is_empty();
    if !plain {
        return None;
    }
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    let mapped = match c.to_ascii_lowercase() {
        'a' => DockChoice::ToolSession,
        'f' => DockChoice::File,
        'd' => DockChoice::Directory,
        'r' => DockChoice::Persistent,
        _ => return None,
    };
    choices.contains(&mapped).then_some(mapped)
}

/// Render the approval dock card for `request`.
#[expect(
    clippy::too_many_arguments,
    reason = "pure render inputs from App state without coupling to App internals"
)]
pub(crate) fn render(
    request: &ApprovalRequest,
    preview: Preview,
    dock: &ApprovalDock,
    choices: &[DockChoice],
    waiting: usize,
    keymap: &Keymap,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let frame = Frame {
        width,
        border: theme::WARNING,
        bg: None,
        pad: 1,
    };
    let inner = frame.inner();
    let header = vec![
        Span::styled("Allow ", theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(
            request.tool_name.clone(),
            theme::fg(theme::WARNING).add_modifier(Modifier::BOLD),
        ),
        Span::styled("?", theme::accent().add_modifier(Modifier::BOLD)),
    ];

    let summary = wrap::sanitize(&request.summary);
    let mut head = wrap::wrap_text(summary.trim(), theme::plain(), inner, &[], &[]);
    if !dock.expanded {
        head.truncate(3);
    }
    if let Some(reason) = request
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        let reason_line = format!("Reason: {}", wrap::sanitize(reason));
        let wrapped = wrap::wrap_text(&reason_line, theme::fg(theme::WARNING), inner, &[], &[]);
        head.extend(wrapped);
    }
    if dock.expanded && !request.targets.is_empty() {
        let targets_text = format!("Targets: {}", request.targets.join(", "));
        head.extend(wrap::wrap_text(
            &targets_text,
            theme::muted(),
            inner,
            &[],
            &[],
        ));
    }

    let choice_rows = choices.len();
    let overhead = head.len() + choice_rows + 5;
    let preview_cap = if dock.expanded {
        max_rows.saturating_sub(overhead)
    } else {
        max_rows.saturating_sub(overhead).min(COMPACT_PREVIEW)
    };
    let body = render_preview(preview, inner, preview_cap, dock.expanded, waiting);
    let choice_lines = render_choices(choices, dock.selected, &request.tool_name, inner);

    let mut sections = vec![Section::plain(head)];
    if !body.is_empty() {
        sections.push(Section::labelled(vec![Span::raw("")], body));
    }
    sections.push(Section::labelled(vec![Span::raw("")], choice_lines));

    let actions = footer_hints(keymap, dock.expanded);
    frame.render(header, sections, actions)
}

fn render_preview(
    preview: Preview,
    inner: usize,
    preview_rows: usize,
    expanded: bool,
    waiting: usize,
) -> Vec<Line<'static>> {
    let mut body = Vec::new();
    let mut hidden = 0usize;
    match preview {
        Preview::Diff(text) if preview_rows > 0 => {
            let (rows, more) = diff::render(&text, inner, false, None, Limit::rows(preview_rows));
            body.extend(rows);
            hidden = more;
        }
        Preview::Text(text) if preview_rows > 0 => {
            let text = wrap::sanitize(&text);
            let all: Vec<&str> = text.trim_end().lines().collect();
            for line in all.iter().take(preview_rows) {
                let rows = wrap::wrap_text(line, theme::code_block(), inner, &[], &[]);
                let take = if expanded { rows.len() } else { 2 };
                body.extend(rows.into_iter().take(take));
            }
            hidden = all.len().saturating_sub(preview_rows);
        }
        _ => {}
    }
    if hidden > 0 {
        body.push(Line::from(Span::styled(
            format!("… {hidden} more lines (ctrl+o to expand)"),
            theme::dim(),
        )));
    }
    if waiting > 1 {
        body.push(Line::from(Span::styled(
            format!("+{} more waiting", waiting - 1),
            theme::dim(),
        )));
    }
    body
}

fn render_choices(
    choices: &[DockChoice],
    selected: usize,
    tool: &str,
    inner: usize,
) -> Vec<Line<'static>> {
    let selected = selected.min(choices.len().saturating_sub(1));
    choices
        .iter()
        .enumerate()
        .map(|(i, choice)| {
            let on = i == selected;
            let prefix = if on { "❯ " } else { "  " };
            let label = choice.label(tool);
            let hint = format!("[{}] ", choice.shortcut());
            let style = if on {
                theme::focus().add_modifier(Modifier::BOLD)
            } else {
                theme::plain()
            };
            let mut spans = vec![
                Span::styled(prefix, theme::focus()),
                Span::styled(hint, theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(label, style),
            ];
            if on {
                let used: usize = spans.iter().map(Span::width).sum();
                spans.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
                for span in &mut spans {
                    span.style = span.style.bg(theme::SELECTED_BG);
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn footer_hints(keymap: &Keymap, expanded: bool) -> Vec<Span<'static>> {
    let approve = keymap.chords("app.approval.approve");
    let deny = keymap.chords("app.approval.deny");
    let expand = keymap.chords("app.tools.expand");
    let detail_label = if expanded { " compact" } else { " details" };
    vec![
        Span::styled(approve, theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(" approve  ", theme::muted()),
        Span::styled(deny, theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(" deny  ", theme::muted()),
        Span::styled(expand, theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(detail_label, theme::muted()),
    ]
}
