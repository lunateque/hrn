//! Interactive `/settings` overlay state: curated category tabs, cross-tab ranked search,
//! inline toggles, searchable enum/theme choice picker delegation, text editing, and stateful
//! viewport scrolling.

use crate::port::{SettingItem, SettingKind};
use crate::settings_catalog::{self, SettingsTab};
use crate::settings_picker::{ChoicePickerState, compute_flat_scroll_top};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::cell::Cell;
use std::collections::BTreeMap;

/// One visual row in the `/settings` list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SettingsRow {
    Section(String),
    Item(usize),
}

/// Inline text editor state for non-boolean/non-enum settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EditState {
    pub(crate) key: String,
    pub(crate) label: String,
    pub(crate) draft: String,
    pub(crate) kind: SettingKind,
}

/// Result of routing a key into [`SettingsUi`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SettingsAction {
    Stay,
    Close,
    Set { key: String, value: String },
    Reset(String),
    PreviewTheme(Option<String>),
}

/// Interactive tabbed `/settings` overlay.
pub(crate) struct SettingsUi {
    items: Vec<SettingItem>,
    tab: SettingsTab,
    query: String,
    selected: usize,
    scroll_top: Cell<usize>,
    editing: Option<EditState>,
    choice_picker: Option<ChoicePickerState>,
    choice_scroll_top: Cell<usize>,
    status: Option<(String, bool)>,
}

impl SettingsUi {
    pub(crate) fn new(items: Vec<SettingItem>) -> Self {
        let mut ui = Self {
            items,
            tab: SettingsTab::Display,
            query: String::new(),
            selected: 0,
            scroll_top: Cell::new(0),
            editing: None,
            choice_picker: None,
            choice_scroll_top: Cell::new(0),
            status: None,
        };
        ui.select_first();
        ui
    }

    pub(crate) fn items(&self) -> &[SettingItem] {
        &self.items
    }

    pub(crate) const fn tab(&self) -> SettingsTab {
        self.tab
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) const fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) const fn editing(&self) -> Option<&EditState> {
        self.editing.as_ref()
    }

    pub(crate) const fn choice_picker(&self) -> Option<&ChoicePickerState> {
        self.choice_picker.as_ref()
    }

    pub(crate) const fn choice_picker_mut(&mut self) -> Option<&mut ChoicePickerState> {
        self.choice_picker.as_mut()
    }

    pub(crate) fn take_choice_picker(&mut self) -> Option<ChoicePickerState> {
        self.choice_scroll_top.set(0);
        self.choice_picker.take()
    }

    pub(crate) fn reset_choice_scroll(&self) {
        self.choice_scroll_top.set(0);
    }

    pub(crate) fn is_previewing_theme(&self) -> bool {
        self.choice_picker
            .as_ref()
            .is_some_and(|picker| picker.preview_theme)
    }

    pub(crate) fn status(&self) -> Option<(&str, bool)> {
        self.status.as_ref().map(|(m, err)| (m.as_str(), *err))
    }

    pub(crate) fn modified_count(&self) -> usize {
        self.items.iter().filter(|i| i.modified).count()
    }

    pub(crate) fn set_status(&mut self, message: String, is_error: bool) {
        self.status = Some((message, is_error));
    }

    /// Compute and persist the top visible row index for the main settings list.
    pub(crate) fn sync_scroll_top(&self, rows: &[SettingsRow], visible_h: usize) -> usize {
        let next = compute_scroll_top(rows, self.selected, visible_h, self.scroll_top.get());
        self.scroll_top.set(next);
        next
    }

    /// Compute and persist the top visible row index for the choice picker list.
    pub(crate) fn sync_choice_scroll_top(
        &self,
        len: usize,
        selected: usize,
        visible_h: usize,
    ) -> usize {
        let next = compute_flat_scroll_top(len, selected, visible_h, self.choice_scroll_top.get());
        self.choice_scroll_top.set(next);
        next
    }

    /// Replace items after a live save/reset while keeping selection anchored on the same key.
    pub(crate) fn refresh(&mut self, items: Vec<SettingItem>, status: Option<(String, bool)>) {
        let prev_key = self.current_item().map(|i| i.key.clone());
        self.items = items;
        self.status = status;
        if let Some(key) = prev_key
            && self.select_key(&key)
        {
            return;
        }
        self.clamp_selection();
    }

    /// Compute the currently visible rows (flat ranked matches across all tabs when searching,
    /// grouped into curated subsections within the active tab when browsing).
    pub(crate) fn visible_rows(&self) -> Vec<SettingsRow> {
        let tokens: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        if !tokens.is_empty() {
            let mut scored: Vec<(u32, usize)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| score_setting(item, &tokens).map(|s| (s, idx)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            return scored
                .into_iter()
                .map(|(_, idx)| SettingsRow::Item(idx))
                .collect();
        }
        let mut groups: BTreeMap<(u8, u16, String), Vec<(u16, usize)>> = BTreeMap::new();
        for (idx, item) in self.items.iter().enumerate() {
            let meta = settings_catalog::classify(item);
            if self.tab != SettingsTab::All && meta.tab != self.tab {
                continue;
            }
            let bucket_key = (
                meta.tab.order(),
                meta.section_order,
                meta.section.into_owned(),
            );
            groups
                .entry(bucket_key)
                .or_default()
                .push((meta.item_order, idx));
        }
        let mut rows = Vec::new();
        for ((_, _, sec), mut members) in groups {
            members.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            rows.push(SettingsRow::Section(sec));
            for (_, idx) in members {
                rows.push(SettingsRow::Item(idx));
            }
        }
        rows
    }

    pub(crate) fn current_item(&self) -> Option<&SettingItem> {
        let rows = self.visible_rows();
        match rows.get(self.selected) {
            Some(SettingsRow::Item(idx)) => self.items.get(*idx),
            _ => None,
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> SettingsAction {
        if self.choice_picker.is_some() {
            return self.key_choice_picker(key);
        }
        if self.editing.is_some() {
            return self.key_editing(key);
        }
        self.key_main(key)
    }

    fn key_editing(&mut self, key: KeyEvent) -> SettingsAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c' | 'C') if ctrl => SettingsAction::Close,
            KeyCode::Esc => {
                self.editing = None;
                SettingsAction::Stay
            }
            KeyCode::Enter => {
                let Some(edit) = self.editing.take() else {
                    return SettingsAction::Stay;
                };
                SettingsAction::Set {
                    key: edit.key,
                    value: edit.draft,
                }
            }
            KeyCode::Char('u' | 'U') if ctrl => {
                if let Some(edit) = &mut self.editing {
                    edit.draft.clear();
                }
                SettingsAction::Stay
            }
            KeyCode::Backspace => {
                if let Some(edit) = &mut self.editing {
                    edit.draft.pop();
                }
                SettingsAction::Stay
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if let Some(edit) = &mut self.editing {
                    edit.draft.push(c);
                }
                SettingsAction::Stay
            }
            _ => SettingsAction::Stay,
        }
    }

    fn key_main(&mut self, key: KeyEvent) -> SettingsAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c' | 'C') if ctrl => SettingsAction::Close,
            KeyCode::Esc => {
                if self.query.is_empty() {
                    SettingsAction::Close
                } else {
                    self.clear_query();
                    SettingsAction::Stay
                }
            }
            KeyCode::Tab => self.switch_tab(self.tab.next()),
            KeyCode::BackTab => self.switch_tab(self.tab.prev()),
            KeyCode::Up => self.nav(-1),
            KeyCode::Down => self.nav(1),
            KeyCode::Char('p' | 'P') if ctrl => self.nav(-1),
            KeyCode::Char('n' | 'N') if ctrl => self.nav(1),
            KeyCode::PageUp => self.nav(-10),
            KeyCode::PageDown => self.nav(10),
            KeyCode::Home => {
                self.scroll_top.set(0);
                self.select_first();
                SettingsAction::Stay
            }
            KeyCode::End => {
                self.select_last();
                SettingsAction::Stay
            }
            KeyCode::Left => self.step_inline_value(false),
            KeyCode::Right => self.step_inline_value(true),
            KeyCode::Enter => self.activate_selected(),
            KeyCode::Delete => self.reset_selected(),
            KeyCode::Char('x' | 'X') if ctrl => self.reset_selected(),
            KeyCode::Char('u' | 'U') if ctrl => {
                self.clear_query();
                SettingsAction::Stay
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.status = None;
                self.scroll_top.set(0);
                self.select_first();
                SettingsAction::Stay
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.query.push(c);
                self.status = None;
                self.scroll_top.set(0);
                self.select_first();
                SettingsAction::Stay
            }
            _ => SettingsAction::Stay,
        }
    }

    fn clear_query(&mut self) {
        self.query.clear();
        self.status = None;
        self.scroll_top.set(0);
        self.select_first();
    }

    fn switch_tab(&mut self, next: SettingsTab) -> SettingsAction {
        self.tab = next;
        self.clear_query();
        SettingsAction::Stay
    }

    fn nav(&mut self, delta: isize) -> SettingsAction {
        self.move_selection(delta);
        SettingsAction::Stay
    }

    fn step_inline_value(&mut self, forward: bool) -> SettingsAction {
        let Some(item) = self.current_item().cloned() else {
            return SettingsAction::Stay;
        };
        match item.kind {
            SettingKind::Bool => SettingsAction::Set {
                key: item.key,
                value: toggle_bool(&item.value).to_owned(),
            },
            SettingKind::Enum => match cycle_enum(&item, forward) {
                Some(value) => SettingsAction::Set {
                    key: item.key,
                    value,
                },
                None => SettingsAction::Stay,
            },
            _ => SettingsAction::Stay,
        }
    }

    fn activate_selected(&mut self) -> SettingsAction {
        let Some(item) = self.current_item().cloned() else {
            return SettingsAction::Stay;
        };
        let meta = settings_catalog::classify(&item);
        match item.kind {
            SettingKind::Bool => SettingsAction::Set {
                key: item.key,
                value: toggle_bool(&item.value).to_owned(),
            },
            SettingKind::Enum if item.values.len() >= 2 => {
                self.choice_scroll_top.set(0);
                self.choice_picker =
                    Some(ChoicePickerState::from_item(&item, meta.label.into_owned()));
                SettingsAction::Stay
            }
            SettingKind::Enum if item.values.len() == 1 => match item.values.first() {
                Some(value) if !value.eq_ignore_ascii_case(&item.value) => SettingsAction::Set {
                    key: item.key,
                    value: value.clone(),
                },
                _ => SettingsAction::Stay,
            },
            _ => {
                let draft = if item.secret || item.value == "(unset)" {
                    String::new()
                } else {
                    item.value
                };
                self.editing = Some(EditState {
                    key: item.key,
                    label: meta.label.into_owned(),
                    draft,
                    kind: item.kind,
                });
                SettingsAction::Stay
            }
        }
    }

    fn reset_selected(&self) -> SettingsAction {
        self.current_item().map_or(SettingsAction::Stay, |item| {
            SettingsAction::Reset(item.key.clone())
        })
    }

    fn select_first(&mut self) {
        let rows = self.visible_rows();
        self.selected = rows
            .iter()
            .position(|r| matches!(r, SettingsRow::Item(_)))
            .unwrap_or(0);
    }

    fn select_last(&mut self) {
        let rows = self.visible_rows();
        self.selected = rows
            .iter()
            .rposition(|r| matches!(r, SettingsRow::Item(_)))
            .unwrap_or(0);
    }

    fn select_key(&mut self, key: &str) -> bool {
        let rows = self.visible_rows();
        if let Some(pos) = rows.iter().position(|r| {
            matches!(r, SettingsRow::Item(idx) if self.items.get(*idx).is_some_and(|i| i.key == key))
        }) {
            self.selected = pos;
            return true;
        }
        false
    }

    fn clamp_selection(&mut self) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            self.selected = 0;
            return;
        }
        if matches!(rows.get(self.selected), Some(SettingsRow::Item(_))) {
            return;
        }
        self.select_first();
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.visible_rows();
        let selectable: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| matches!(r, SettingsRow::Item(_)).then_some(i))
            .collect();
        if selectable.is_empty() {
            self.selected = 0;
            return;
        }
        let cur_pos = selectable
            .iter()
            .position(|&i| i == self.selected)
            .unwrap_or(0);
        let target_pos = if delta < 0 {
            cur_pos.saturating_sub(delta.unsigned_abs())
        } else {
            (cur_pos + delta.unsigned_abs()).min(selectable.len() - 1)
        };
        if let Some(&row_idx) = selectable.get(target_pos) {
            self.selected = row_idx;
        }
    }
}

fn compute_scroll_top(
    rows: &[SettingsRow],
    selected: usize,
    visible_h: usize,
    prev_top: usize,
) -> usize {
    if rows.is_empty() || visible_h == 0 {
        return 0;
    }
    let mut top = prev_top.min(rows.len().saturating_sub(visible_h));
    if selected < top {
        top = selected;
    } else if selected >= top + visible_h {
        top = selected + 1 - visible_h;
    }
    if visible_h >= 2
        && top > 0
        && selected == top
        && matches!(rows.get(top - 1), Some(SettingsRow::Section(_)))
    {
        top -= 1;
    }
    top
}

fn toggle_bool(current: &str) -> &'static str {
    if current.eq_ignore_ascii_case("true") {
        "false"
    } else {
        "true"
    }
}

fn cycle_enum(item: &SettingItem, forward: bool) -> Option<String> {
    let n = item.values.len();
    if n == 0 {
        return None;
    }
    let cur = item
        .values
        .iter()
        .position(|v| v.eq_ignore_ascii_case(&item.value));
    let next = match (cur, forward) {
        (Some(i), true) => (i + 1) % n,
        (Some(i), false) => (i + n - 1) % n,
        (None, true) => 0,
        (None, false) => n - 1,
    };
    item.values.get(next).cloned()
}

fn score_setting(item: &SettingItem, tokens: &[String]) -> Option<u32> {
    let meta = settings_catalog::classify(item);
    let label = meta.label.to_ascii_lowercase();
    let key = item.key.to_ascii_lowercase();
    let sec = meta.section.to_ascii_lowercase();
    let tab = meta.tab.label().to_ascii_lowercase();
    let schema_tab = item.tab.to_ascii_lowercase();
    let desc = item.description.to_ascii_lowercase();
    let val = item.value.to_ascii_lowercase();
    let env = item.env.as_deref().unwrap_or_default().to_ascii_lowercase();
    let mut score = 0_u32;
    for tok in tokens {
        if label == *tok {
            score += 130;
        } else if key == *tok {
            score += 120;
        } else if label.starts_with(tok) {
            score += 100;
        } else if key.starts_with(tok) {
            score += 95;
        } else if label
            .split([' ', '-', '_', '/', '(', ')'])
            .any(|seg| seg.starts_with(tok))
        {
            score += 85;
        } else if key.split(['.', '_', '-']).any(|seg| seg.starts_with(tok)) {
            score += 80;
        } else if label.contains(tok) {
            score += 65;
        } else if key.contains(tok) {
            score += 60;
        } else if sec.contains(tok) || tab.contains(tok) || schema_tab.contains(tok) {
            score += 45;
        } else if desc.contains(tok) || env.contains(tok) || val.contains(tok) {
            score += 25;
        } else {
            return None;
        }
    }
    Some(score)
}
