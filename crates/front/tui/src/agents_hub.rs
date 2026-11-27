//! `/agents` definition hub: lists bundled, user, and project agent profiles on top of
//! [`crate::views::ListView`], allowing enable/disable (`[task] disabled_agents`) and per-agent
//! model override (`[task.agent_models]`) edits.

use crate::port::AgentDefRow;
use crate::views::{ListNav, ListRow, ListView};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::fmt::Write as _;

/// Action emitted by [`AgentsHub::key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentsHubAction {
    Stay,
    Close,
    ToggleEnabled { name: String, enabled: bool },
    SetModelOverride { name: String, model: Option<String> },
}

enum Mode {
    Browse,
    EditModel { name: String, draft: String },
}

/// Interactive `/agents` definition hub backed by [`ListView`].
pub(crate) struct AgentsHub {
    defs: Vec<AgentDefRow>,
    list: ListView,
    mode: Mode,
}

impl AgentsHub {
    pub(crate) fn new(defs: Vec<AgentDefRow>) -> Self {
        let rows = to_list_rows(&defs);
        let list = ListView::new(
            "agents (definitions)",
            "close · Enter/Space toggle · m model override · d clear override",
            rows,
        );
        Self {
            defs,
            list,
            mode: Mode::Browse,
        }
    }

    pub(crate) fn refresh(&mut self, defs: Vec<AgentDefRow>) {
        self.defs = defs;
        self.list.set_rows(to_list_rows(&self.defs));
    }

    pub(crate) fn key(&mut self, key: KeyEvent, area: Rect) -> AgentsHubAction {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return AgentsHubAction::Close;
        }
        match &mut self.mode {
            Mode::EditModel { .. } => self.key_edit_model(key),
            Mode::Browse => self.key_browse(key, area),
        }
    }

    fn key_edit_model(&mut self, key: KeyEvent) -> AgentsHubAction {
        let Mode::EditModel { name, draft } = &mut self.mode else {
            return AgentsHubAction::Stay;
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.list
                    .set_hint("close · Enter/Space toggle · m model override · d clear override");
                AgentsHubAction::Stay
            }
            KeyCode::Enter => {
                let agent = name.clone();
                let trimmed = draft.trim().to_owned();
                self.mode = Mode::Browse;
                self.list
                    .set_hint("close · Enter/Space toggle · m model override · d clear override");
                AgentsHubAction::SetModelOverride {
                    name: agent,
                    model: (!trimmed.is_empty()).then_some(trimmed),
                }
            }
            KeyCode::Backspace => {
                draft.pop();
                AgentsHubAction::Stay
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !c.is_whitespace() {
                    draft.push(c);
                }
                AgentsHubAction::Stay
            }
            _ => AgentsHubAction::Stay,
        }
    }

    fn key_browse(&mut self, key: KeyEvent, area: Rect) -> AgentsHubAction {
        match self.list.handle_key(key, area) {
            ListNav::Close => return AgentsHubAction::Close,
            ListNav::Moved => return AgentsHubAction::Stay,
            ListNav::Select(idx) => {
                if let Some(def) = self.defs.get(idx) {
                    return AgentsHubAction::ToggleEnabled {
                        name: def.name.clone(),
                        enabled: !def.enabled,
                    };
                }
                return AgentsHubAction::Stay;
            }
            ListNav::Unhandled => {}
        }
        let Some(def) = self.defs.get(self.list.selected()) else {
            return AgentsHubAction::Stay;
        };
        match key.code {
            KeyCode::Char(' ' | 'e') => AgentsHubAction::ToggleEnabled {
                name: def.name.clone(),
                enabled: !def.enabled,
            },
            KeyCode::Char('m') => {
                let name = def.name.clone();
                let draft = def.model_override.clone().unwrap_or_default();
                self.mode = Mode::EditModel { name, draft };
                self.list
                    .set_hint("cancel · Enter save model override (blank clears)");
                AgentsHubAction::Stay
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                AgentsHubAction::SetModelOverride {
                    name: def.name.clone(),
                    model: None,
                }
            }
            _ => AgentsHubAction::Stay,
        }
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        let footer = match &self.mode {
            Mode::Browse => None,
            Mode::EditModel { name, draft } => Some(format!(
                " Model override for `{name}` (blank clears): {draft}_"
            )),
        };
        self.list.draw(area, buf, footer.as_deref());
    }
}

fn to_list_rows(defs: &[AgentDefRow]) -> Vec<ListRow> {
    defs.iter()
        .map(|def| {
            let badge = if def.enabled { "[on] " } else { "[off]" }.to_owned();
            let primary = if def.aliases.is_empty() {
                def.name.clone()
            } else {
                format!("{} ({})", def.name, def.aliases.join(","))
            };
            let mode = if def.read_only { "ro" } else { "rw" };
            let mut meta = format!("{} · {mode}", def.source);
            if let Some(ov) = &def.model_override {
                let _ = write!(meta, " · model={ov} (override)");
            } else if let Some(m) = &def.default_model {
                let _ = write!(meta, " · model={m}");
            }
            if !def.description.is_empty() {
                let _ = write!(meta, " — {}", def.description);
            }
            ListRow {
                badge,
                primary,
                meta,
                active: def.enabled,
                dim: !def.enabled,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_defs() -> Vec<AgentDefRow> {
        vec![
            AgentDefRow {
                name: "scout".into(),
                aliases: vec!["explore".into()],
                source: "built-in".into(),
                description: "Read-only explorer".into(),
                read_only: true,
                enabled: true,
                default_model: Some("fast".into()),
                model_override: None,
            },
            AgentDefRow {
                name: "reviewer".into(),
                aliases: vec![],
                source: ".harness".into(),
                description: "Project reviewer".into(),
                read_only: true,
                enabled: false,
                default_model: Some("reviewer".into()),
                model_override: Some("opus".into()),
            },
        ]
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn agents_hub_toggles_and_edits_model_override() {
        let area = Rect::new(0, 0, 100, 20);
        let mut hub = AgentsHub::new(sample_defs());
        assert_eq!(hub.list.len(), 2);

        // Space toggles first row (`scout` enabled -> false).
        assert_eq!(
            hub.key(key(KeyCode::Char(' ')), area),
            AgentsHubAction::ToggleEnabled {
                name: "scout".into(),
                enabled: false,
            }
        );

        // Move down to `reviewer`, press `m` to edit model override.
        assert_eq!(hub.key(key(KeyCode::Down), area), AgentsHubAction::Stay);
        assert_eq!(
            hub.key(key(KeyCode::Char('m')), area),
            AgentsHubAction::Stay
        );
        // Existing draft is "opus"; append ":high" and submit.
        for c in ":high".chars() {
            assert_eq!(hub.key(key(KeyCode::Char(c)), area), AgentsHubAction::Stay);
        }
        assert_eq!(
            hub.key(key(KeyCode::Enter), area),
            AgentsHubAction::SetModelOverride {
                name: "reviewer".into(),
                model: Some("opus:high".into()),
            }
        );

        // `d` clears override.
        assert_eq!(
            hub.key(key(KeyCode::Char('d')), area),
            AgentsHubAction::SetModelOverride {
                name: "reviewer".into(),
                model: None,
            }
        );
        assert_eq!(hub.key(key(KeyCode::Esc), area), AgentsHubAction::Close);
    }
}
