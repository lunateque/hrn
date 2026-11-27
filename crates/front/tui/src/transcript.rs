//! The conversation as display entries, fed by live runtime events or a stored branch. Every entry
//! caches its rendered lines per width, so repainting a long transcript only renders what changed.

use crate::imgview::ImageTray;
use crate::render::{self, Ctx};
pub(crate) use crate::tool_group::{ToolCard, ToolGroup, ToolState};
use hrn_kernel::{DagNode, DagPayload, ImageAttachment, Msg, Role};
use ratatui::text::Line;
use std::path::Path;
use std::time::{Duration, Instant};

/// Streamed tool output kept per card; older output is dropped first.
const LIVE_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SteerState {
    Queued,
    Sent,
    Returned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Warn,
    Error,
}

pub(crate) enum Entry {
    User(String),
    Steer {
        text: String,
        state: SteerState,
    },
    Assistant(String),
    Reasoning {
        text: String,
        started: Option<Instant>,
        elapsed: Option<Duration>,
        live: bool,
    },
    ToolGroup(ToolGroup),
    Compacted {
        method: String,
        summary: String,
        saved_chars: usize,
    },
    Notice(Level, String),
    Footer(String),
}

impl Entry {
    /// Shows a spinner or a running clock, so it is repainted on every frame.
    fn is_animated(&self) -> bool {
        match self {
            Self::ToolGroup(group) => group.has_active(),
            Self::Reasoning { live, .. } => *live,
            _ => false,
        }
    }
}

struct Cache {
    width: usize,
    verbose: bool,
    lines: Vec<Line<'static>>,
}

pub(crate) struct Item {
    entry: Entry,
    images: Vec<(Option<usize>, ImageAttachment)>,
    expanded: bool,
    cache: Option<Cache>,
}

pub(crate) struct Transcript {
    items: Vec<Item>,
    open_text: Option<usize>,
    open_reasoning: Option<usize>,
    /// `Ctrl+T`: whether thinking blocks render expanded.
    thinking_visible: bool,
    /// `Ctrl+Shift+O`: whether tool call cards are shown in the transcript.
    tools_visible: bool,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            open_text: None,
            open_reasoning: None,
            thinking_visible: true,
            tools_visible: true,
        }
    }
}

impl Transcript {
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// Expand or collapse every thinking block; a new block follows the current setting.
    pub(crate) fn set_thinking_visible(&mut self, visible: bool) {
        self.thinking_visible = visible;
        for item in &mut self.items {
            if matches!(item.entry, Entry::Reasoning { .. }) {
                item.expanded = visible;
                item.cache = None;
            }
        }
    }

    /// Show or hide every tool call card (`Ctrl+Shift+O`).
    pub(crate) fn set_tools_visible(&mut self, visible: bool) {
        self.tools_visible = visible;
        for item in &mut self.items {
            if matches!(item.entry, Entry::ToolGroup(_)) {
                item.cache = None;
            }
        }
    }

    fn close_open_tool_group(&mut self) {
        if let Some(last) = self.items.last_mut()
            && let Entry::ToolGroup(group) = &mut last.entry
            && !group.closed
        {
            group.closed = true;
            last.cache = None;
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn push(&mut self, entry: Entry) -> usize {
        self.push_with_images(entry, Vec::new())
    }

    pub(crate) fn push_with_images(
        &mut self,
        entry: Entry,
        images: Vec<(Option<usize>, ImageAttachment)>,
    ) -> usize {
        if !matches!(entry, Entry::ToolGroup(_)) {
            self.close_open_tool_group();
        }
        self.items.push(Item {
            // Thinking is shown by default; a toggle collapses it to a one-line label.
            expanded: matches!(entry, Entry::Reasoning { .. }) && self.thinking_visible,
            entry,
            images,
            cache: None,
        });
        self.items.len() - 1
    }

    pub(crate) fn last_assistant_text(&self) -> Option<(usize, &str)> {
        self.items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, item)| match &item.entry {
                Entry::Assistant(text) => Some((i, text.as_str())),
                _ => None,
            })
    }

    pub(crate) fn attach_images(
        &mut self,
        index: usize,
        images: Vec<(Option<usize>, ImageAttachment)>,
    ) {
        if images.is_empty() {
            return;
        }
        if let Some(item) = self.items.get_mut(index) {
            item.images.extend(images);
            item.cache = None;
        }
    }

    pub(crate) fn entry(&self, index: usize) -> Option<&Entry> {
        self.items.get(index).map(|item| &item.entry)
    }

    /// Mutable access that also drops the entry's render cache.
    fn touch(&mut self, index: usize) -> Option<&mut Entry> {
        let item = self.items.get_mut(index)?;
        item.cache = None;
        Some(&mut item.entry)
    }

    /// Rendered lines of an entry, from cache when still valid.
    pub(crate) fn lines(&mut self, index: usize, ctx: &Ctx) -> &[Line<'static>] {
        let Some(item) = self.items.get_mut(index) else {
            return &[];
        };
        if !self.tools_visible && matches!(item.entry, Entry::ToolGroup(_)) {
            return &[];
        }
        let stale = item.entry.is_animated()
            || item
                .cache
                .as_ref()
                .is_none_or(|c| c.width != ctx.width || c.verbose != ctx.verbose);
        if stale {
            let lines = render::item(&item.entry, &item.images, item.expanded || ctx.verbose, ctx);
            item.cache = Some(Cache {
                width: ctx.width,
                verbose: ctx.verbose,
                lines,
            });
        }
        item.cache.as_ref().map_or(&[], |c| c.lines.as_slice())
    }

    /// How many leading entries are settled: nothing about them can change any more, so they
    /// may be written into the terminal's scrollback. A streaming message, an open or active
    /// tool group, a live thinking block and a queued steer are not settled, and neither is
    /// anything after them.
    pub(crate) fn settled(&self) -> usize {
        self.items
            .iter()
            .enumerate()
            .take_while(|(index, item)| match &item.entry {
                Entry::Assistant(_) => self.open_text != Some(*index),
                Entry::Reasoning { live, .. } => !live,
                Entry::ToolGroup(group) => group.is_settled(),
                Entry::Steer { state, .. } => *state != SteerState::Queued,
                Entry::User(_) | Entry::Compacted { .. } | Entry::Notice(..) | Entry::Footer(_) => {
                    true
                }
            })
            .count()
    }

    /// End the streaming blocks: the reasoning stops its clock, the text takes no more deltas.
    pub(crate) fn close_blocks(&mut self, now: Instant) {
        if let Some(index) = self.open_reasoning.take()
            && let Some(Entry::Reasoning {
                started,
                elapsed,
                live,
                ..
            }) = self.touch(index)
        {
            *live = false;
            *elapsed = started.map(|s| now.duration_since(s));
        }
        self.open_text = None;
    }

    pub(crate) fn thinking(&mut self, delta: &str, now: Instant) {
        let index = if let Some(index) = self.open_reasoning {
            index
        } else {
            if delta.trim().is_empty() {
                return;
            }
            self.open_text = None;
            let index = self.push(Entry::Reasoning {
                text: String::new(),
                started: Some(now),
                elapsed: None,
                live: true,
            });
            self.open_reasoning = Some(index);
            index
        };
        if let Some(Entry::Reasoning { text, .. }) = self.touch(index) {
            text.push_str(delta);
        }
    }

    pub(crate) fn text(&mut self, delta: &str, now: Instant) {
        let index = if let Some(index) = self.open_text {
            index
        } else {
            if delta.trim().is_empty() {
                return;
            }
            self.close_blocks(now);
            let index = self.push(Entry::Assistant(String::new()));
            self.open_text = Some(index);
            index
        };
        if let Some(Entry::Assistant(text)) = self.touch(index) {
            text.push_str(delta);
        }
    }

    fn push_tool_card(&mut self, card: ToolCard) {
        if let Some(last) = self.items.last_mut()
            && let Entry::ToolGroup(group) = &mut last.entry
            && !group.closed
        {
            group.push(card);
            last.cache = None;
            return;
        }
        self.push(Entry::ToolGroup(ToolGroup::new(card)));
    }

    pub(crate) fn tool_start(&mut self, id: &str, name: &str, arguments: &str, now: Instant) {
        self.close_blocks(now);
        self.push_tool_card(ToolCard::new(id, name, arguments, Some(now)));
    }

    fn tool_loc(&self, id: &str) -> Option<(usize, usize)> {
        self.items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(item_idx, item)| {
                let Entry::ToolGroup(group) = &item.entry else {
                    return None;
                };
                group
                    .cards
                    .iter()
                    .rposition(|card| card.id == id)
                    .map(|card_idx| (item_idx, card_idx))
            })
    }

    pub(crate) fn tool(&self, id: &str) -> Option<&ToolCard> {
        let (item_idx, card_idx) = self.tool_loc(id)?;
        match self.entry(item_idx)? {
            Entry::ToolGroup(group) => group.cards.get(card_idx),
            _ => None,
        }
    }

    pub(crate) fn tool_mut(&mut self, id: &str) -> Option<&mut ToolCard> {
        let (item_idx, card_idx) = self.tool_loc(id)?;
        match self.touch(item_idx)? {
            Entry::ToolGroup(group) => group.cards.get_mut(card_idx),
            _ => None,
        }
    }

    pub(crate) fn tool_delta(&mut self, id: &str, delta: &str) {
        let Some(card) = self.tool_mut(id) else {
            return;
        };
        card.live.push_str(delta);
        if card.live.len() > LIVE_LIMIT {
            let cut = card.live.len() - LIVE_LIMIT / 2;
            let cut = (cut..card.live.len())
                .find(|i| card.live.is_char_boundary(*i))
                .unwrap_or(card.live.len());
            card.live.drain(..cut);
        }
    }

    pub(crate) fn tool_done_with_images(
        &mut self,
        id: &str,
        output: &str,
        is_error: bool,
        diff: Option<&str>,
        images: Vec<(Option<usize>, ImageAttachment)>,
        now: Instant,
    ) {
        if let Some((item_idx, card_idx)) = self.tool_loc(id)
            && let Some(Entry::ToolGroup(group)) = self.touch(item_idx)
        {
            if let Some(card) = group.cards.get_mut(card_idx) {
                card.finish(output, is_error);
                card.diff = diff.map(str::to_owned);
                card.elapsed = card.started.map(|s| now.duration_since(s));
            }
            if !images.is_empty()
                && let Some(slot) = group.images.get_mut(card_idx)
            {
                slot.extend(images);
            }
        }
    }

    pub(crate) fn notice(&mut self, level: Level, text: impl Into<String>, now: Instant) {
        self.close_blocks(now);
        self.push(Entry::Notice(level, text.into()));
    }

    /// Render a mid-turn `out://` note as settled assistant prose while the turn continues.
    pub(crate) fn assistant_note(&mut self, text: &str, now: Instant) {
        if text.trim().is_empty() {
            return;
        }
        self.close_blocks(now);
        self.push(Entry::Assistant(text.to_owned()));
    }

    /// Move the oldest queued steer with this text to `state`.
    pub(crate) fn mark_steer(&mut self, text: &str, state: SteerState) {
        let found = self.items.iter().position(|item| {
            matches!(&item.entry, Entry::Steer { text: t, state: SteerState::Queued } if t == text)
        });
        if let Some(index) = found
            && let Some(Entry::Steer { state: s, .. }) = self.touch(index)
        {
            *s = state;
        }
    }

    /// A follow-up the user queued: its own card, so Alt+Up can take it back.
    pub(crate) fn follow_up(&mut self, text: &str) {
        self.push(Entry::Steer {
            text: text.to_owned(),
            state: SteerState::Queued,
        });
    }

    /// Mark the oldest queued follow-up card with this text as sent.
    pub(crate) fn settle_follow_up(&mut self, text: &str) {
        self.mark_steer(text, SteerState::Sent);
    }

    /// Drop the oldest queued steer card with this text (Alt+Up took it back).
    pub(crate) fn drop_follow_up(&mut self, text: &str) {
        let found = self.items.iter().position(|item| {
            matches!(&item.entry, Entry::Steer { text: t, state: SteerState::Queued } if t == text)
        });
        if let Some(index) = found {
            self.items.remove(index);
        }
    }

    /// Every steer still queued moves to `state`.
    pub(crate) fn settle_steers(&mut self, state: SteerState) {
        for item in &mut self.items {
            if let Entry::Steer { state: s, .. } = &mut item.entry
                && *s == SteerState::Queued
            {
                *s = state;
                item.cache = None;
            }
        }
    }

    /// The turn ended: close streaming blocks and settle cards that never got a result.
    pub(crate) fn turn_finished(&mut self, now: Instant) {
        self.close_blocks(now);
        for item in &mut self.items {
            if let Entry::ToolGroup(group) = &mut item.entry {
                let mut changed = !group.closed;
                group.closed = true;
                for card in &mut group.cards {
                    if matches!(card.state, ToolState::Running | ToolState::Awaiting) {
                        card.state = ToolState::Cancelled;
                        card.elapsed = card.started.map(|s| now.duration_since(s));
                        changed = true;
                    }
                }
                if changed {
                    item.cache = None;
                }
            }
        }
    }

    /// Replace everything with a stored branch; when `running` is `true`, trailing unanswered tool
    /// calls are kept in [`ToolState::Running`] instead of being marked [`ToolState::Cancelled`].
    pub(crate) fn load_with_tray_running(
        &mut self,
        nodes: &[DagNode],
        tray: &mut ImageTray,
        cwd: &Path,
        running: bool,
    ) {
        self.clear();
        tray.clear();
        for node in nodes {
            match &node.payload {
                DagPayload::Message { msg, .. } => self.load_msg(msg, tray, cwd),
                DagPayload::Summary { text, method, .. } => {
                    let method = method.clone().unwrap_or_else(|| "summary".to_owned());
                    self.push(Entry::Compacted {
                        method,
                        summary: text.clone(),
                        saved_chars: 0,
                    });
                }
                DagPayload::ForkMarker { label } => {
                    self.push(Entry::Notice(Level::Info, format!("forked here: {label}")));
                }
                DagPayload::BranchSummary { text, .. } => {
                    self.push(Entry::Notice(
                        Level::Info,
                        format!("branch summary: {text}"),
                    ));
                }
                // The `tool_search` card already shows what was activated; the board node is
                // what the `todo` card and the HUD are built from, not a transcript entry.
                DagPayload::ToolsActivated { .. }
                | DagPayload::TodoBoard { .. }
                | DagPayload::SessionExit { .. }
                | DagPayload::Label { .. }
                | DagPayload::ModelChange { .. } => {}
            }
        }
        if running
            && let Some(last) = self.items.last_mut()
            && let Entry::ToolGroup(group) = &mut last.entry
        {
            for card in group.cards.iter_mut().rev() {
                if card.state == ToolState::Cancelled {
                    card.state = ToolState::Running;
                    last.cache = None;
                } else {
                    break;
                }
            }
        }
        let last_idx = self.items.len().checked_sub(1);
        for (idx, item) in self.items.iter_mut().enumerate() {
            if let Entry::ToolGroup(group) = &mut item.entry {
                let keep_open = running && last_idx == Some(idx);
                if !keep_open && !group.closed {
                    group.closed = true;
                    item.cache = None;
                }
            }
        }
    }

    fn load_msg(&mut self, msg: &Msg, tray: &mut ImageTray, cwd: &Path) {
        match msg.role {
            Role::System => {}
            Role::User => {
                if let Some(text) = msg.content.strip_prefix("[steer] ") {
                    self.push(Entry::Steer {
                        text: text.to_owned(),
                        state: SteerState::Sent,
                    });
                } else if !msg.content.starts_with("[system] ") {
                    let imgs: Vec<(Option<usize>, ImageAttachment)> = msg
                        .images
                        .iter()
                        .map(|img| (Some(tray.register(img.clone())), img.clone()))
                        .collect();
                    self.push_with_images(Entry::User(msg.content.clone()), imgs);
                }
            }
            Role::Assistant => {
                if let Some(reasoning) = msg.reasoning.as_ref().filter(|r| !r.trim().is_empty()) {
                    self.push(Entry::Reasoning {
                        text: reasoning.clone(),
                        started: None,
                        elapsed: None,
                        live: false,
                    });
                }
                if !msg.content.trim().is_empty() {
                    let previews = tray.resolve_assistant_previews(&msg.content, cwd);
                    self.push_with_images(Entry::Assistant(msg.content.clone()), previews);
                }
                for call in &msg.tool_calls {
                    let mut card = ToolCard::new(&call.id, &call.name, &call.arguments, None);
                    card.state = ToolState::Cancelled;
                    self.push_tool_card(card);
                }
            }
            Role::Tool => {
                if let Some(id) = &msg.tool_call_id
                    && let Some((item_idx, card_idx)) = self.tool_loc(id)
                {
                    let imgs: Vec<(Option<usize>, ImageAttachment)> = msg
                        .images
                        .iter()
                        .map(|img| (Some(tray.register(img.clone())), img.clone()))
                        .collect();
                    if let Some(Entry::ToolGroup(group)) = self.touch(item_idx) {
                        if let Some(card) = group.cards.get_mut(card_idx) {
                            card.finish(&msg.content, msg.is_error);
                        }
                        if !imgs.is_empty()
                            && let Some(slot) = group.images.get_mut(card_idx)
                        {
                            slot.extend(imgs);
                        }
                    }
                }
            }
        }
    }
}
