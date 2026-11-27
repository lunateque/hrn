//! `/tree` and `/branch` (`/rewind`): interactive session DAG navigator with shared leaf rules,
//! `Shift+L` inline node labels, `Ctrl+O` filter cycling, and abandoned-branch summary prompts.

use crate::theme;
use crate::tree_rows::{
    BRANCH_NODE_CAP, LeafSelection, TreeFilter, TreeRow, build_rows, latest_nodes, resolve_labels,
    resolve_leaf_selection, switch_abandons_nodes,
};
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hrn_kernel::DagNode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};
use std::collections::HashMap;

/// Opening mode of [`TreeOverlay`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeMode {
    /// Full session DAG (`/tree`).
    Tree,
    /// User-turns-first branch/rewind navigator over the latest 600 nodes (`/branch` / `/rewind`).
    Branch,
}

pub(crate) enum TreeAction {
    Stay,
    Close,
    /// Apply the shared leaf rule for `selection`, optionally summarizing abandoned nodes.
    Select {
        selection: LeafSelection,
        summarize: bool,
    },
    /// Selected node is already the current leaf ("Already at this point").
    AlreadyAtLeaf,
    /// Persist a `DagPayload::Label` update (`None` clears the label).
    SetLabel {
        node_id: String,
        label: Option<String>,
    },
    Fork(String),
}

enum OverlayPrompt {
    None,
    /// Typing a `/` search filter.
    Filter,
    /// Editing the label of `node_id` (`Shift+L`).
    LabelEdit {
        node_id: String,
        draft: String,
    },
    /// Offering `No summary / Summarize` before switching away from abandoned nodes.
    BranchSummary {
        selection: LeafSelection,
        summarize_selected: bool,
    },
}

pub(crate) struct TreeOverlay {
    mode: TreeMode,
    head_id: Option<String>,
    nodes: Vec<DagNode>,
    labels: HashMap<String, String>,
    rows: Vec<TreeRow>,
    query: String,
    filter: TreeFilter,
    prompt: OverlayPrompt,
    branch_summary_default: bool,
    selected: usize,
}

impl TreeOverlay {
    pub(crate) fn new(
        head_id: Option<String>,
        nodes: Vec<DagNode>,
        mode: TreeMode,
        branch_summary_default: bool,
    ) -> Self {
        let labels = resolve_labels(&nodes);
        let nodes = match mode {
            TreeMode::Tree => nodes,
            TreeMode::Branch => latest_nodes(nodes, BRANCH_NODE_CAP),
        };
        let filter = match mode {
            TreeMode::Tree => TreeFilter::Default,
            TreeMode::Branch => TreeFilter::UserOnly,
        };
        let mut overlay = Self {
            mode,
            head_id,
            nodes,
            labels,
            rows: Vec::new(),
            query: String::new(),
            filter,
            prompt: OverlayPrompt::None,
            branch_summary_default,
            selected: 0,
        };
        overlay.rebuild();
        if let Some(pos) = overlay.rows.iter().position(|r| r.is_head) {
            overlay.selected = pos;
        } else if !overlay.rows.is_empty() {
            overlay.selected = overlay.rows.len() - 1;
        }
        overlay
    }

    /// Update the local label map immediately after a `Shift+L` commit so the overlay refreshes.
    pub(crate) fn apply_local_label(&mut self, node_id: &str, label: Option<&str>) {
        match label.map(str::trim).filter(|s| !s.is_empty()) {
            Some(text) => {
                self.labels.insert(node_id.to_owned(), text.to_owned());
            }
            None => {
                self.labels.remove(node_id);
            }
        }
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let selected_id = self.rows.get(self.selected).map(|r| r.node_id.clone());
        self.rows = build_rows(
            &self.nodes,
            self.head_id.as_deref(),
            self.filter,
            &self.labels,
        );
        if let Some(id) = selected_id
            && let Some(pos) = self.rows.iter().position(|r| r.node_id == id)
        {
            self.selected = pos;
        } else if let Some(pos) = self.rows.iter().position(|r| r.is_head) {
            self.selected = pos;
        } else {
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
    }

    fn visible(&self) -> Vec<&TreeRow> {
        let q = self.query.to_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                q.is_empty()
                    || r.preview.to_lowercase().contains(&q)
                    || r.role.contains(&q)
                    || r.node_id.starts_with(&q)
                    || r.label
                        .as_deref()
                        .is_some_and(|l| l.to_lowercase().contains(&q))
            })
            .collect()
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> TreeAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return TreeAction::Close;
        }
        match &mut self.prompt {
            OverlayPrompt::Filter => return self.filter_key(key),
            OverlayPrompt::LabelEdit { .. } => return self.label_key(key),
            OverlayPrompt::BranchSummary { .. } => return self.summary_choice_key(key),
            OverlayPrompt::None => {}
        }
        if ctrl && matches!(key.code, KeyCode::Char('o' | 'O')) {
            self.filter = self.filter.next();
            self.rebuild();
            return TreeAction::Stay;
        }
        let count = self.visible().len();
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return TreeAction::Close,
            KeyCode::Enter => return self.activate_selected(shift),
            KeyCode::Char('L') => {
                if let Some(row) = self.visible().get(self.selected) {
                    let node_id = row.node_id.clone();
                    let draft = self.labels.get(&node_id).cloned().unwrap_or_default();
                    self.prompt = OverlayPrompt::LabelEdit { node_id, draft };
                }
            }
            KeyCode::Char('f' | 'F') => {
                return self
                    .visible()
                    .get(self.selected)
                    .map_or(TreeAction::Stay, |r| TreeAction::Fork(r.node_id.clone()));
            }
            KeyCode::Char('t' | 'T') => {
                self.filter = if self.filter == TreeFilter::All {
                    TreeFilter::Default
                } else {
                    TreeFilter::All
                };
                self.rebuild();
            }
            KeyCode::Char('/') => {
                self.prompt = OverlayPrompt::Filter;
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::PageDown => {
                self.selected = (self.selected + 10).min(count.saturating_sub(1));
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = count.saturating_sub(1),
            _ => {}
        }
        TreeAction::Stay
    }

    fn activate_selected(&mut self, force_summary_prompt: bool) -> TreeAction {
        let Some(selected_id) = self.visible().get(self.selected).map(|r| r.node_id.clone()) else {
            return TreeAction::Stay;
        };
        let Some(selection) =
            resolve_leaf_selection(&self.nodes, self.head_id.as_deref(), &selected_id)
        else {
            return TreeAction::Stay;
        };
        if selection.already_at_leaf {
            return TreeAction::AlreadyAtLeaf;
        }
        let abandons = switch_abandons_nodes(
            &self.nodes,
            self.head_id.as_deref(),
            selection.target_head.as_deref(),
        );
        if abandons && (self.branch_summary_default || force_summary_prompt) {
            self.prompt = OverlayPrompt::BranchSummary {
                selection,
                summarize_selected: self.branch_summary_default,
            };
            return TreeAction::Stay;
        }
        TreeAction::Select {
            selection,
            summarize: false,
        }
    }

    fn filter_key(&mut self, key: KeyEvent) -> TreeAction {
        let count = self.visible().len();
        match key.code {
            KeyCode::Esc => {
                self.prompt = OverlayPrompt::None;
                self.query.clear();
                self.selected = 0;
            }
            KeyCode::Enter => {
                self.prompt = OverlayPrompt::None;
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        TreeAction::Stay
    }

    fn label_key(&mut self, key: KeyEvent) -> TreeAction {
        let OverlayPrompt::LabelEdit { node_id, draft } = &mut self.prompt else {
            return TreeAction::Stay;
        };
        match key.code {
            KeyCode::Esc => self.prompt = OverlayPrompt::None,
            KeyCode::Enter => {
                let id = node_id.clone();
                let label = (!draft.trim().is_empty()).then(|| draft.trim().to_owned());
                self.prompt = OverlayPrompt::None;
                return TreeAction::SetLabel { node_id: id, label };
            }
            KeyCode::Backspace => {
                draft.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                draft.push(c);
            }
            _ => {}
        }
        TreeAction::Stay
    }

    fn summary_choice_key(&mut self, key: KeyEvent) -> TreeAction {
        let OverlayPrompt::BranchSummary {
            selection,
            summarize_selected,
        } = &mut self.prompt
        else {
            return TreeAction::Stay;
        };
        match key.code {
            KeyCode::Esc => self.prompt = OverlayPrompt::None,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                *summarize_selected = !*summarize_selected;
            }
            KeyCode::Char('n' | 'N') => {
                let selection = selection.clone();
                self.prompt = OverlayPrompt::None;
                return TreeAction::Select {
                    selection,
                    summarize: false,
                };
            }
            KeyCode::Char('s' | 'S') => {
                let selection = selection.clone();
                self.prompt = OverlayPrompt::None;
                return TreeAction::Select {
                    selection,
                    summarize: true,
                };
            }
            KeyCode::Enter => {
                let selection = selection.clone();
                let summarize = *summarize_selected;
                self.prompt = OverlayPrompt::None;
                return TreeAction::Select {
                    selection,
                    summarize,
                };
            }
            _ => {}
        }
        TreeAction::Stay
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 16 || area.height < 6 {
            return;
        }
        Clear.render(area, buf);
        let heading = match self.mode {
            TreeMode::Tree => "Session tree",
            TreeMode::Branch => "Branch / rewind",
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(
                format!(
                    " {heading} ({} nodes · {}) ",
                    self.rows.len(),
                    self.filter.label()
                ),
                theme::bold(),
            ));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 4 {
            return;
        }
        buf.set_line(inner.x, inner.y, &self.header_line(), inner.width);

        let visible = self.visible();
        let rows = usize::from(inner.height.saturating_sub(3));
        let selected = self.selected.min(visible.len().saturating_sub(1));
        let first = (selected + 1).saturating_sub(rows);
        let inner_width = usize::from(inner.width);

        for (i, row) in visible.iter().enumerate().skip(first).take(rows) {
            let marker = if row.is_head {
                Span::styled("★ ", theme::fg(theme::ACCENT).add_modifier(Modifier::BOLD))
            } else if row.on_active {
                Span::styled("● ", theme::accent())
            } else {
                Span::styled("○ ", theme::muted())
            };
            let role_style = match row.role {
                "user" => theme::accent(),
                "asst" => theme::plain(),
                "sum" | "hndf" | "bsum" => theme::fg(theme::YELLOW),
                _ => theme::dim(),
            };
            let role_tag = format!("{:<4} ", row.role);
            let label_tag = row
                .label
                .as_deref()
                .map(|l| format!("[{l}] "))
                .unwrap_or_default();
            let used =
                2 + wrap::str_width(&row.prefix) + 2 + role_tag.len() + wrap::str_width(&label_tag);
            let room = inner_width.saturating_sub(used);
            let mut spans = vec![
                Span::raw(if i == selected { "▸ " } else { "  " }),
                Span::styled(row.prefix.clone(), theme::dim()),
                marker,
                Span::styled(role_tag, role_style),
            ];
            if !label_tag.is_empty() {
                spans.push(Span::styled(
                    label_tag,
                    theme::fg(theme::GOLD).add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::raw(wrap::truncate(&row.preview, room).into_owned()));
            let mut line = Line::from(spans);
            if i == selected {
                line = line.style(theme::focus().add_modifier(Modifier::BOLD));
            }
            let y = inner.y + 1 + u16::try_from(i - first).unwrap_or(0);
            buf.set_line(inner.x, y, &line, inner.width);
        }
        if visible.is_empty() {
            let line = Line::from(Span::styled("no matching nodes", theme::dim()));
            buf.set_line(
                inner.x + 2,
                inner.y + 1,
                &line,
                inner.width.saturating_sub(2),
            );
        }

        let footer_y = inner.bottom() - 1;
        let footer = Line::from(vec![
            Span::styled("enter", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" select  ", theme::muted()),
            Span::styled("shift+enter", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" summarize?  ", theme::muted()),
            Span::styled("L", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" label  ", theme::muted()),
            Span::styled("ctrl+o", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" filter  ", theme::muted()),
            Span::styled("f", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" fork  ", theme::muted()),
            Span::styled("esc", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" close", theme::muted()),
        ]);
        buf.set_line(inner.x, footer_y, &footer, inner.width);
    }

    fn header_line(&self) -> Line<'static> {
        match &self.prompt {
            OverlayPrompt::Filter => Line::from(vec![
                Span::styled("/ ", theme::accent()),
                if self.query.is_empty() {
                    Span::styled("type to filter", theme::dim())
                } else {
                    Span::raw(self.query.clone())
                },
            ]),
            OverlayPrompt::LabelEdit { draft, .. } => Line::from(vec![
                Span::styled(
                    "label: ",
                    theme::fg(theme::GOLD).add_modifier(Modifier::BOLD),
                ),
                Span::raw(draft.clone()),
                Span::styled("  (enter save, empty clears, esc cancel)", theme::dim()),
            ]),
            OverlayPrompt::BranchSummary {
                summarize_selected, ..
            } => {
                let no_style = if *summarize_selected {
                    theme::muted()
                } else {
                    theme::focus().add_modifier(Modifier::BOLD)
                };
                let yes_style = if *summarize_selected {
                    theme::focus().add_modifier(Modifier::BOLD)
                } else {
                    theme::muted()
                };
                Line::from(vec![
                    Span::styled("Abandoning nodes: ", theme::fg(theme::WARNING)),
                    Span::styled("[No summary]", no_style),
                    Span::raw("  "),
                    Span::styled("[Summarize]", yes_style),
                    Span::styled("  (n/s/enter/esc)", theme::dim()),
                ])
            }
            OverlayPrompt::None if !self.query.is_empty() => Line::from(vec![
                Span::styled("/ ", theme::accent()),
                Span::raw(self.query.clone()),
            ]),
            OverlayPrompt::None => Line::from(vec![
                Span::styled("★", theme::fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(" head  ", theme::dim()),
                Span::styled("●", theme::accent()),
                Span::styled(" active  ", theme::dim()),
                Span::styled("○", theme::muted()),
                Span::styled(" other", theme::dim()),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::{DagPayload, Msg};

    fn msg_node(id: &str, parent: Option<&str>, msg: Msg) -> DagNode {
        DagNode {
            id: id.to_owned(),
            session_id: "s1".to_owned(),
            parent_id: parent.map(str::to_owned),
            created_at_ms: 0,
            payload: DagPayload::Message {
                turn_id: "t1".to_owned(),
                msg,
            },
        }
    }

    #[test]
    fn renders_branches_and_applies_shared_leaf_rule_and_fork() {
        let nodes = vec![
            msg_node("n1", None, Msg::user("start")),
            msg_node("n2", Some("n1"), Msg::assistant("branch a")),
            msg_node("n3", Some("n1"), Msg::assistant("branch b")),
        ];
        let mut tree = TreeOverlay::new(Some("n3".into()), nodes, TreeMode::Tree, false);
        assert_eq!(tree.rows.len(), 3);
        assert_eq!(tree.selected, 2);
        // Selecting current leaf n3 is a no-op ("Already at this point").
        assert!(matches!(
            tree.key(KeyEvent::from(KeyCode::Enter)),
            TreeAction::AlreadyAtLeaf
        ));
        // Selecting assistant node n2 lands on n2.
        tree.key(KeyEvent::from(KeyCode::Up));
        assert!(matches!(
            tree.key(KeyEvent::from(KeyCode::Enter)),
            TreeAction::Select { selection, summarize: false }
                if selection.target_head.as_deref() == Some("n2") && selection.prefill.is_none()
        ));
        assert!(matches!(
            tree.key(KeyEvent::from(KeyCode::Char('f'))),
            TreeAction::Fork(id) if id == "n2"
        ));
        // Selecting user node n1 lands on parent (None) and prefills "start".
        tree.key(KeyEvent::from(KeyCode::Up));
        assert!(matches!(
            tree.key(KeyEvent::from(KeyCode::Enter)),
            TreeAction::Select { selection, .. }
                if selection.target_head.is_none() && selection.prefill.as_deref() == Some("start")
        ));
    }

    #[test]
    fn branch_mode_starts_user_only_cycles_filters_and_edits_labels() {
        let nodes = vec![
            msg_node("u1", None, Msg::user("first")),
            msg_node("a1", Some("u1"), Msg::assistant("reply")),
            msg_node("u2", Some("a1"), Msg::user("second")),
            msg_node("a2", Some("u2"), Msg::assistant("reply 2")),
        ];
        let mut tree = TreeOverlay::new(Some("a2".into()), nodes, TreeMode::Branch, false);
        assert_eq!(tree.filter, TreeFilter::UserOnly);
        assert_eq!(tree.rows.len(), 2);

        // Shift+L edits label on selected user node u2.
        tree.key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
        for c in "checkpoint".chars() {
            tree.key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert!(matches!(
            tree.key(KeyEvent::from(KeyCode::Enter)),
            TreeAction::SetLabel { node_id, label }
                if node_id == "u2" && label.as_deref() == Some("checkpoint")
        ));
        tree.apply_local_label("u2", Some("checkpoint"));

        // Ctrl+O cycles UserOnly -> LabeledOnly.
        tree.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(tree.filter, TreeFilter::LabeledOnly);
        assert_eq!(tree.rows.len(), 1);
        assert_eq!(tree.rows[0].node_id, "u2");

        // Shift+Enter forces the No summary / Summarize choice prompt even when default is false.
        tree.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(matches!(
            tree.key(KeyEvent::from(KeyCode::Char('s'))),
            TreeAction::Select { selection, summarize: true }
                if selection.target_head.as_deref() == Some("a1")
                    && selection.prefill.as_deref() == Some("second")
        ));
    }
}
