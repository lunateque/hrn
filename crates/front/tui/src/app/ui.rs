//! Composing a frame: the transcript entries that settled (scrollback lines), then the live
//! region: the changing entries, pending images, the status band, the prompt or an
//! approval/question panel, the completion popup and the full-window views. Nothing here draws
//! a box around the whole screen: the layout is full-width rows.

use super::{App, Task};
use crate::landing;
use crate::pinned;
use crate::port::AgentCounts;
use crate::render::Ctx;
use crate::status::{self, Activity, Snapshot};
use crate::symbols;
use crate::theme;
use crate::vim::{CursorShape, VimMode};
use crate::wrap;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Widget};
use std::time::{SystemTime, UNIX_EPOCH};

/// Editor rows before the input scrolls.
const INPUT_ROWS: usize = 10;
/// Rows kept for the transcript when the bottom area grows.
const MIN_BODY: u16 = 3;

fn to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// What fills the bottom area under the band.
enum Bottom {
    Prompt(crate::input::Layout),
    Panel(Vec<Line<'static>>),
    Ask(u16),
}

/// The welcome banner at the head of the scrollback.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Banner {
    /// Not shown: the session started from a resume.
    Off,
    /// To be written with the next frame.
    #[default]
    Due,
    /// Already in the scrollback.
    Written,
}

/// What the terminal's scrollback holds of the transcript, and what forces it to be rewritten.
#[derive(Default)]
pub(super) struct Flow {
    /// Transcript entries already written into the scrollback.
    pub(super) committed: usize,
    pub(super) banner: Banner,
    /// The next frame erases the screen and writes the history again.
    pub(super) replay: bool,
    /// The terminal size of the last frame.
    screen: Option<(u16, u16)>,
    /// A full-window view was open at the last frame.
    overlay_open: bool,
}

/// One frame of the screen, split the way the inline renderer writes it.
pub struct Composed {
    /// Erase everything and write the history again: the width changed, a full-window view
    /// closed, or the transcript was replaced.
    pub reset: bool,
    /// Lines that became final since the last frame.
    pub scrollback: Vec<Line<'static>>,
    /// The live region.
    pub live: Buffer,
    /// The prompt's cursor inside the live region.
    pub cursor: Option<Position>,
    /// Requested DECSCUSR cursor shape for this frame.
    pub cursor_shape: CursorShape,
}

impl App {
    /// The next frame for a test or a tool that wants the screen as data: a `replay` is forced first, so
    /// `scrollback` holds the whole history and `live` the region under it.
    pub fn paint(&mut self, width: u16, height: u16) -> Composed {
        self.flow.replay = true;
        self.compose(width, height)
    }

    /// Build the next frame for a `width` x `height` terminal. Settled transcript entries not
    /// yet written become scrollback lines; the rest of the screen is the live region: the
    /// unsettled entries' tail, the status band and the prompt (or a full-window view).
    pub(crate) fn compose(&mut self, width: u16, height: u16) -> Composed {
        self.width = usize::from(width);
        if width < 10 || height < 4 {
            return Composed {
                reset: false,
                scrollback: Vec::new(),
                live: Buffer::empty(Rect::new(0, 0, width.max(1), 1)),
                cursor: None,
                cursor_shape: CursorShape::Default,
            };
        }
        let overlay = self.has_full_view();
        let flow = &mut self.flow;
        let resized = flow
            .screen
            .replace((width, height))
            .is_some_and(|before| before != (width, height));
        let closed = std::mem::replace(&mut flow.overlay_open, overlay) && !overlay;
        let reset = std::mem::take(&mut flow.replay) || resized || closed;
        if reset {
            flow.committed = 0;
            if flow.banner == Banner::Written {
                flow.banner = Banner::Due;
            }
        }
        if overlay {
            self.pinned_hitboxes.clear();
            return self.compose_overlay(reset, width, height);
        }
        let scrollback = self.settle();
        let ctx = self.ctx();
        let max_rows = usize::from(height.saturating_sub(MIN_BODY + 1)).max(1);
        let hud = self.todo_hud(width as usize);
        let pinned = self.pinned_block(width as usize);
        let pre_len = hud.len() + pinned.lines.len();
        let pre_rows = to_u16(pre_len);
        let bottom = self.bottom(max_rows.saturating_sub(pre_len));
        let pending = self.images.pending_labels();
        let (rows, pending_rows) = match &bottom {
            Bottom::Prompt(layout) => (
                to_u16(layout.lines.len()) + pre_rows,
                u16::from(!pending.is_empty()),
            ),
            Bottom::Panel(lines) => (to_u16(lines.len()) + pre_rows, 0),
            Bottom::Ask(h) => (*h + pre_rows, 0),
        };
        let band_gap = u16::from(!self.transcript.is_empty() || self.flow.banner != Banner::Off);
        let room = usize::from(height.saturating_sub(rows + pending_rows + band_gap + 1));
        let popup = self.popup_card(self.width, room);
        let tail = self.live_tail(room, &ctx);
        let body = to_u16(tail.len().max(popup.len()));
        let gap_row = if popup.is_empty() { band_gap } else { 0 };
        let band_y = body + gap_row + pending_rows;
        let live_h = band_y + 1 + rows;
        self.last_live_origin_y = height.saturating_sub(live_h);
        let mut live = Buffer::empty(Rect::new(0, 0, width, live_h));
        let band = self.band(self.width);
        for (y, line) in (0..).zip(&tail) {
            live.set_line(0, y, line, width);
        }
        if !popup.is_empty() {
            let area = Rect::new(0, body - to_u16(popup.len()), width, to_u16(popup.len()));
            Clear.render(area, &mut live);
            for (y, line) in (area.y..).zip(&popup) {
                live.set_line(0, y, line, width);
            }
        }
        let under = self.draw_live_chrome(
            &mut live,
            width,
            body + gap_row,
            band_y,
            rows,
            &pending,
            &band,
            &hud,
            pinned,
        );
        let cursor = self.draw_bottom(bottom, under, &mut live);
        let cursor_shape = if cursor.is_some() {
            self.vim.cursor_shape()
        } else {
            CursorShape::Default
        };
        Composed {
            reset,
            scrollback,
            live,
            cursor,
            cursor_shape,
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal layout helper forwarding computed compose geometry"
    )]
    fn draw_live_chrome(
        &mut self,
        live: &mut Buffer,
        width: u16,
        pending_y: u16,
        band_y: u16,
        rows: u16,
        pending: &[String],
        band: &Line<'_>,
        hud: &[Line<'_>],
        pinned: pinned::PinnedBlock,
    ) -> Rect {
        if !pending.is_empty() {
            let syms = symbols::for_preset(self.appearance.symbol_preset);
            let joiner = format!(" {} ", syms.dot);
            let spans = vec![
                Span::raw(" "),
                Span::styled(
                    format!("{} {}", syms.camera, pending.join(&joiner)),
                    theme::focus().add_modifier(Modifier::BOLD),
                ),
            ];
            live.set_line(0, pending_y, &Line::from(spans), width);
        }
        live.set_line(0, band_y, band, width);
        let mut under = Rect::new(0, band_y + 1, width, rows);
        for line in hud {
            live.set_line(under.x, under.y, line, under.width);
            under.y += 1;
            under.height -= 1;
        }
        let pinned_y = under.y;
        self.pinned_hitboxes = pinned
            .hitboxes
            .into_iter()
            .map(|(off, id)| (pinned_y + to_u16(off), id))
            .collect();
        for line in &pinned.lines {
            live.set_line(under.x, under.y, line, under.width);
            under.y += 1;
            under.height -= 1;
        }
        under
    }

    /// The area under the HUD band: the prompt with its cursor, the status panel, or the ask
    /// form. Returns the cursor position the terminal should park.
    fn draw_bottom(&self, bottom: Bottom, under: Rect, live: &mut Buffer) -> Option<Position> {
        match bottom {
            Bottom::Prompt(layout) => {
                for (y, line) in (under.y..).zip(&layout.lines) {
                    live.set_line(under.x, y, line, under.width);
                }
                let (x, y) = layout.cursor;
                Some(Position::new(
                    (under.x + x).min(under.width.saturating_sub(1)),
                    (under.y + y).min(under.bottom().saturating_sub(1)),
                ))
            }
            Bottom::Panel(lines) => {
                for (y, line) in (under.y..).zip(&lines) {
                    live.set_line(under.x, y, line, under.width);
                }
                None
            }
            Bottom::Ask(_) => {
                if let Some((panel, _)) = &self.active_ask {
                    panel.draw(under, live);
                }
                None
            }
        }
    }

    /// A full-window view: it takes the whole live region and commits nothing; when it closes
    /// the history is written afresh so the screen returns to the inline flow.
    fn compose_overlay(&mut self, reset: bool, width: u16, height: u16) -> Composed {
        let main = Rect::new(0, 0, width, height - 1);
        self.overlay_area = main;
        let band = self.band(self.width);
        let mut live = Buffer::empty(Rect::new(0, 0, width, height));
        let cursor = self.draw_full_view(main, &mut live);
        live.set_line(0, height - 1, &band, width);
        Composed {
            reset,
            scrollback: Vec::new(),
            live,
            cursor,
            cursor_shape: CursorShape::Default,
        }
    }

    /// The lines of every entry that settled since the last frame, after the banner on a new
    /// session; they are written to the scrollback once and never drawn again.
    fn settle(&mut self) -> Vec<Line<'static>> {
        let ctx = self.ctx();
        let mut lines = Vec::new();
        if self.flow.banner == Banner::Due {
            lines.extend(landing::lines_with_recent_preset(
                self.width,
                &self.recent_sessions,
                now_ms(),
                self.appearance.symbol_preset,
            ));
            self.flow.banner = Banner::Written;
        }
        let settled = self.transcript.settled();
        for index in self.flow.committed..settled {
            let gap = self.gap(index);
            let entry_lines = self.transcript.lines(index, &ctx);
            if entry_lines.is_empty() {
                continue;
            }
            if gap {
                lines.push(Line::default());
            }
            lines.extend_from_slice(entry_lines);
        }
        self.flow.committed = self.flow.committed.max(settled);
        lines
    }

    /// Whether a blank line separates entry `index` from what is above it.
    fn gap(&self, index: usize) -> bool {
        index > 0 || self.flow.banner != Banner::Off
    }

    /// The last `room` rows of the entries that are still changing.
    fn live_tail(&mut self, room: usize, ctx: &Ctx) -> Vec<Line<'static>> {
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut index = self.transcript.len();
        while index > self.flow.committed && rows.len() < room {
            index -= 1;
            let gap = self.gap(index);
            let lines = self.transcript.lines(index, ctx);
            if lines.is_empty() {
                continue;
            }
            let total = lines.len() + usize::from(gap);
            let take = (room - rows.len()).min(total);
            for v in (total - take..total).rev() {
                let line = if gap {
                    v.checked_sub(1).and_then(|i| lines.get(i))
                } else {
                    lines.get(v)
                };
                rows.push(line.cloned().unwrap_or_default());
            }
        }
        rows.reverse();
        rows
    }

    pub(super) fn has_full_view(&self) -> bool {
        self.login.is_some()
            || self.logout.is_some()
            || self.setup.is_some()
            || self.picker.is_some()
            || self.tree.is_some()
            || self.btw.is_some()
            || self.image_modal.is_some()
            || self.history.is_some()
            || self.model_hub.is_some()
            || self.settings_ui.is_some()
            || self.agent_hub.is_some()
            || self.agents_hub.is_some()
            || self.local_picker.is_some()
            || self.link_hints.is_some()
            || self.commit_modal.is_some()
            || self.git_ui.is_some()
            || self.remote_qr.is_some()
            || self.view.is_some()
    }

    fn current_now_ms(&self) -> i64 {
        self.now_ms_override.unwrap_or_else(now_ms)
    }

    fn pinned_block(&self, width: usize) -> pinned::PinnedBlock {
        pinned::render(
            &self.background.hub.agents,
            self.pinned_agents,
            self.subagent_live_preview,
            self.focused_agent.as_deref(),
            self.current_now_ms(),
            width,
        )
    }

    fn draw_full_view(&self, main_area: Rect, buf: &mut Buffer) -> Option<Position> {
        if let Some(login) = &self.login {
            return login.draw(main_area, buf);
        }
        if let Some(logout) = &self.logout {
            logout.draw(main_area, buf);
            return None;
        }
        if let Some(setup) = &self.setup {
            setup.draw(main_area, buf);
            return None;
        }
        if let Some(modal) = &self.commit_modal {
            return modal.draw(main_area, buf);
        }
        if let Some(wb) = &self.git_ui {
            return wb.draw(main_area, buf);
        }
        if let Some(hub) = &self.agent_hub {
            hub.draw(main_area, buf, self.current_now_ms());
        } else if let Some(hub) = &self.agents_hub {
            hub.draw(main_area, buf);
        } else if let Some(picker) = &self.picker {
            picker.draw(main_area, buf, self.current_now_ms());
        } else if let Some(tree) = &self.tree {
            tree.draw(main_area, buf);
        } else if let Some(btw) = &self.btw {
            btw.draw(main_area, buf);
        } else if let Some(modal) = &self.image_modal {
            modal.draw(main_area, buf);
        } else if let Some(search) = &self.history {
            search.draw(main_area, buf);
        } else if let Some(hub) = &self.model_hub {
            return hub.draw(main_area, buf);
        } else if let Some(ui) = &self.settings_ui {
            return ui.draw(main_area, buf);
        } else if let Some(local) = &self.local_picker {
            local.draw(main_area, buf);
        } else if let Some(links) = &self.link_hints {
            links.draw(main_area, buf);
        } else if let Some(qr) = &self.remote_qr {
            qr.draw(main_area, buf);
        } else if let Some(view) = &self.view {
            view.draw(main_area, buf);
        }
        None
    }

    /// The todo HUD above the prompt: phase counters and the active task, or an empty-list hint.
    fn todo_hud(&self, width: usize) -> Vec<Line<'static>> {
        let open: usize = self
            .todo
            .iter()
            .flat_map(|p| &p.tasks)
            .filter(|t| !t.status.closed())
            .count();
        if open == 0 {
            return Vec::new();
        }
        let total: usize = self.todo.iter().map(|p| p.tasks.len()).sum();
        let active = self
            .todo
            .iter()
            .flat_map(|p| &p.tasks)
            .find(|t| t.status == hrn_kernel::TodoStatus::InProgress)
            .map(|t| t.content.clone());
        let syms = symbols::for_preset(self.appearance.symbol_preset);
        let line = Line::from(vec![
            Span::styled(format!(" {} ", syms.divider), theme::border()),
            Span::styled(format!("todo {}/{total}", total - open), theme::dim()),
        ]);
        let mut spans = line.spans.clone();
        if self.todo.len() > 1 {
            for phase in &self.todo {
                if phase.tasks.is_empty() {
                    continue;
                }
                let done = phase.tasks.iter().filter(|t| t.status.closed()).count();
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{} {}/{}", phase.name, done, phase.tasks.len()),
                    if phase.all_closed() {
                        theme::dim()
                    } else {
                        theme::muted()
                    },
                ));
            }
        }
        let line = Line::from(spans);
        match active {
            Some(task) => vec![
                line,
                Line::from(vec![
                    Span::styled(format!(" {} ", syms.todo_active), theme::accent()),
                    Span::styled(
                        wrap::truncate(&wrap::sanitize(&task), width.saturating_sub(6))
                            .into_owned(),
                        theme::fg(theme::TEXT),
                    ),
                ]),
            ],
            None => vec![line],
        }
    }

    /// The bottom area under the band: an approval, a question, or the prompt.
    fn bottom(&self, max_rows: usize) -> Bottom {
        let width = self.width;
        if !self.approvals.is_empty() {
            return Bottom::Panel(self.approval_lines(width, max_rows));
        }
        if let Some((panel, _)) = &self.active_ask {
            let h = to_u16(panel.height(width)).min(to_u16(max_rows));
            return Bottom::Ask(h);
        }
        Bottom::Prompt(self.prompt(width, max_rows))
    }

    /// The prompt: gutter (`╰─ ` / `+- `) and typed text, or the right-aligned hint while empty.
    fn prompt(&self, width: usize, max_rows: usize) -> crate::input::Layout {
        let gutter_color = if self.vim.is_enabled() {
            match self.vim.mode() {
                VimMode::Insert => theme::SUCCESS,
                VimMode::Normal => theme::ACCENT,
                VimMode::Visual | VimMode::VisualLine => theme::GOLD,
                VimMode::Replace => theme::ERROR,
            }
        } else {
            theme::WARNING
        };
        let gutter = Span::styled(
            symbols::for_preset(self.appearance.symbol_preset)
                .gutter
                .clone(),
            theme::fg(gutter_color),
        );
        let selection = self.vim.visual_selection(&self.editor);
        self.editor.layout_with_selection(
            width,
            &gutter,
            theme::fg(theme::TEXT),
            INPUT_ROWS.min(max_rows),
            selection,
        )
    }

    fn band(&self, width: usize) -> Line<'static> {
        let activity = if !self.approvals.is_empty() {
            Activity::Approval
        } else if self.active_ask.is_some() {
            Activity::Question
        } else {
            match self.task {
                Task::Idle => Activity::Idle,
                Task::Turn { .. } => Activity::Working,
                Task::Compact => Activity::Compacting,
            }
        };
        let snap = Snapshot {
            activity,
            speculating_compact: self.speculating_compact,
            elapsed: self.busy_since.map(|since| since.elapsed()),
            frame: self.frame,
            model: &self.settings.model,
            thinking: self.settings.thinking,
            fast_tier: self.settings.service_tier,
            mode: self.settings.mode,
            advisor: self.settings.advisor,
            running: &self.background.running,
            agent_counts: AgentCounts::from_entries(&self.background.hub.agents),
            focused_agent: self.focused_agent.as_deref(),
            prompt_tokens: self.last_prompt_tokens,
            context_window: self.context_window,
            tokens: self.tokens,
            cost: self.cost,
            reasoning_tokens: self.reasoning_tokens,
            cwd: &self.cwd_label,
            branch: self.branch.as_deref(),
            worktree: self.worktree,
            title: self.title.as_deref().map(str::trim),
            goal: self.settings.goal.as_ref(),
            flash: self.flash.as_ref().map(|(text, _)| text.as_str()),
            profile: self.appearance.profile.as_deref(),
            vim_mode: self.vim.is_enabled().then(|| self.vim.mode().label()),
            vim_display: self.appearance.vim_mode_display,
            hook_status: None,
        };
        status::bar_with(
            &snap,
            width,
            &self.appearance.statusline,
            self.appearance.symbol_preset,
        )
    }
}
