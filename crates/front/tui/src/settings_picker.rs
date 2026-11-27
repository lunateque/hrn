//! Searchable enum/theme choice picker submenu state and keyboard handling for `/settings`.

use crate::port::SettingItem;
use crate::settings_catalog;
use crate::settings_ui::{SettingsAction, SettingsUi};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Searchable submenu picker for enum settings (with live theme preview on theme slots).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChoicePickerState {
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) current: String,
    pub(crate) default_value: String,
    pub(crate) choices: Vec<String>,
    pub(crate) query: String,
    pub(crate) selected: usize,
    pub(crate) preview_theme: bool,
}

impl ChoicePickerState {
    /// Construct a fresh choice picker anchored on `item`'s current value.
    pub(crate) fn from_item(item: &SettingItem, title: String) -> Self {
        let selected = item
            .values
            .iter()
            .position(|v| v.eq_ignore_ascii_case(&item.value))
            .unwrap_or(0);
        Self {
            key: item.key.clone(),
            title,
            description: item.description.clone(),
            current: item.value.clone(),
            default_value: item.default.clone(),
            choices: item.values.clone(),
            query: String::new(),
            selected,
            preview_theme: settings_catalog::is_theme_key(&item.key),
        }
    }

    /// Return choices matching the current filter query, ranked best-first.
    pub(crate) fn filtered_choices(&self) -> Vec<&str> {
        let tokens: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        if tokens.is_empty() {
            return self.choices.iter().map(String::as_str).collect();
        }
        let mut scored: Vec<(u32, usize, &str)> = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(idx, choice)| {
                score_choice(choice, &tokens).map(|s| (s, idx, choice.as_str()))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, _, c)| c).collect()
    }

    pub(crate) fn selected_choice(&self) -> Option<String> {
        self.filtered_choices()
            .get(self.selected)
            .map(|c| (*c).to_owned())
    }
}

#[derive(Clone, Copy)]
enum ChoiceQueryEdit {
    Clear,
    Pop,
    Push(char),
}

impl SettingsUi {
    pub(crate) fn key_choice_picker(&mut self, key: KeyEvent) -> SettingsAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c' | 'C') if ctrl => SettingsAction::Close,
            KeyCode::Esc => {
                let was_theme = self.take_choice_picker().is_some_and(|p| p.preview_theme);
                if was_theme {
                    SettingsAction::PreviewTheme(None)
                } else {
                    SettingsAction::Stay
                }
            }
            KeyCode::Enter => {
                let commit = self
                    .choice_picker()
                    .and_then(|p| p.selected_choice().map(|val| (p.key.clone(), val)));
                match commit {
                    Some((key_name, value)) => {
                        let _ = self.take_choice_picker();
                        SettingsAction::Set {
                            key: key_name,
                            value,
                        }
                    }
                    None => SettingsAction::Stay,
                }
            }
            KeyCode::Up => self.move_choice(-1),
            KeyCode::Down => self.move_choice(1),
            KeyCode::Char('p' | 'P') if ctrl => self.move_choice(-1),
            KeyCode::Char('n' | 'N') if ctrl => self.move_choice(1),
            KeyCode::PageUp => self.move_choice(-8),
            KeyCode::PageDown => self.move_choice(8),
            KeyCode::Home => self.jump_choice(false),
            KeyCode::End => self.jump_choice(true),
            KeyCode::Char('u' | 'U') if ctrl => self.edit_choice_query(ChoiceQueryEdit::Clear),
            KeyCode::Backspace => self.edit_choice_query(ChoiceQueryEdit::Pop),
            KeyCode::Char(c) if !ctrl && !alt => self.edit_choice_query(ChoiceQueryEdit::Push(c)),
            _ => SettingsAction::Stay,
        }
    }

    fn move_choice(&mut self, delta: isize) -> SettingsAction {
        let Some(picker) = self.choice_picker_mut() else {
            return SettingsAction::Stay;
        };
        let count = picker.filtered_choices().len();
        if count == 0 {
            picker.selected = 0;
            return SettingsAction::Stay;
        }
        let prev = picker.selected;
        let next = if delta < 0 {
            prev.saturating_sub(delta.unsigned_abs())
        } else {
            (prev + delta.unsigned_abs()).min(count - 1)
        };
        picker.selected = next;
        if next != prev && picker.preview_theme {
            return picker.selected_choice().map_or(SettingsAction::Stay, |c| {
                SettingsAction::PreviewTheme(Some(c))
            });
        }
        SettingsAction::Stay
    }

    fn jump_choice(&mut self, end: bool) -> SettingsAction {
        let Some(picker) = self.choice_picker_mut() else {
            return SettingsAction::Stay;
        };
        let count = picker.filtered_choices().len();
        if count == 0 {
            return SettingsAction::Stay;
        }
        let prev = picker.selected;
        picker.selected = if end { count - 1 } else { 0 };
        let moved = picker.selected != prev;
        let preview = if moved && picker.preview_theme {
            picker.selected_choice()
        } else {
            None
        };
        if !end {
            self.reset_choice_scroll();
        }
        preview.map_or(SettingsAction::Stay, |c| {
            SettingsAction::PreviewTheme(Some(c))
        })
    }

    fn edit_choice_query(&mut self, op: ChoiceQueryEdit) -> SettingsAction {
        let Some(picker) = self.choice_picker_mut() else {
            return SettingsAction::Stay;
        };
        let before = picker.selected_choice();
        match op {
            ChoiceQueryEdit::Clear => picker.query.clear(),
            ChoiceQueryEdit::Pop => {
                picker.query.pop();
            }
            ChoiceQueryEdit::Push(c) => picker.query.push(c),
        }
        if picker.query.is_empty() {
            picker.selected = picker
                .choices
                .iter()
                .position(|c| c.eq_ignore_ascii_case(&picker.current))
                .unwrap_or(0);
        } else {
            picker.selected = 0;
        }
        let after = picker.selected_choice();
        let preview = if picker.preview_theme && after != before {
            after
        } else {
            None
        };
        self.reset_choice_scroll();
        preview.map_or(SettingsAction::Stay, |c| {
            SettingsAction::PreviewTheme(Some(c))
        })
    }
}

pub(crate) fn compute_flat_scroll_top(
    len: usize,
    selected: usize,
    visible_h: usize,
    prev_top: usize,
) -> usize {
    if len == 0 || visible_h == 0 {
        return 0;
    }
    let mut top = prev_top.min(len.saturating_sub(visible_h));
    if selected < top {
        top = selected;
    } else if selected >= top + visible_h {
        top = selected + 1 - visible_h;
    }
    top
}

fn score_choice(choice: &str, tokens: &[String]) -> Option<u32> {
    let lower = choice.to_ascii_lowercase();
    let mut score = 0_u32;
    for tok in tokens {
        if lower == *tok {
            score += 120;
        } else if lower.starts_with(tok) {
            score += 90;
        } else if lower
            .split(['-', '_', ' ', '/'])
            .any(|seg| seg.starts_with(tok))
        {
            score += 70;
        } else if lower.contains(tok) {
            score += 45;
        } else {
            return None;
        }
    }
    Some(score)
}
