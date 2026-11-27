//! Live Agent Hub (`Alt+A` / double-`←`) interactive state machine.
//!
//! Pure data model, lineage tree ordering, filtering, aggregates, and inspector formatting live
//! in [`crate::hub_model`] and are re-exported here.

pub(crate) use crate::hub_model::{
    ActivityFilter, ActivityScope, AgentHubAction, HubTab, RosterRow, build_roster_rows,
    compute_aggregates, filter_activity, format_cost_usd, format_duration_ms,
    format_inspector_lines,
};
use crate::port::{AgentActivityItem, AgentHubEntry, AgentHubSnapshot};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

/// Interactive state for the live Agent Hub (`Alt+A`) overlay.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent UI toggles: tree_mode, filter_editing, show_inspector, search_editing, follow"
)]
pub(crate) struct AgentHub {
    pub(crate) snapshot: AgentHubSnapshot,
    pub(crate) tab: HubTab,
    pub(crate) tree_mode: bool,
    pub(crate) filter: String,
    pub(crate) filter_editing: bool,
    pub(crate) selected_roster: usize,
    pub(crate) roster_top: usize,
    pub(crate) show_inspector_narrow: bool,
    pub(crate) activity_filter: ActivityFilter,
    pub(crate) activity_scope: ActivityScope,
    pub(crate) activity_search: String,
    pub(crate) activity_search_editing: bool,
    pub(crate) activity_follow: bool,
    pub(crate) selected_activity: usize,
    pub(crate) activity_top: usize,
}

impl AgentHub {
    pub(crate) fn new(snapshot: AgentHubSnapshot) -> Self {
        Self {
            snapshot,
            tab: HubTab::Agents,
            tree_mode: true,
            filter: String::new(),
            filter_editing: false,
            selected_roster: 0,
            roster_top: 0,
            show_inspector_narrow: false,
            activity_filter: ActivityFilter::All,
            activity_scope: ActivityScope::All,
            activity_search: String::new(),
            activity_search_editing: false,
            activity_follow: true,
            selected_activity: 0,
            activity_top: 0,
        }
    }

    pub(crate) fn update(&mut self, snapshot: AgentHubSnapshot) {
        self.snapshot = snapshot;
        self.clamp_selections();
    }

    pub(crate) fn roster_rows(&self) -> Vec<RosterRow<'_>> {
        build_roster_rows(&self.snapshot.agents, self.tree_mode, &self.filter)
    }

    pub(crate) fn selected_entry(&self) -> Option<&AgentHubEntry> {
        let rows = self.roster_rows();
        rows.get(self.selected_roster).map(|r| r.entry)
    }

    pub(crate) fn activity_rows(&self) -> Vec<&AgentActivityItem> {
        let selected_id = self.selected_entry().map(|e| e.id.as_str());
        filter_activity(
            &self.snapshot.activity,
            &self.snapshot.agents,
            selected_id,
            self.activity_filter,
            self.activity_scope,
            &self.activity_search,
        )
    }

    fn clamp_selections(&mut self) {
        let rlen = self.roster_rows().len();
        if rlen == 0 {
            self.selected_roster = 0;
            self.roster_top = 0;
        } else if self.selected_roster >= rlen {
            self.selected_roster = rlen - 1;
        }
        let alen = self.activity_rows().len();
        if alen == 0 {
            self.selected_activity = 0;
            self.activity_top = 0;
        } else if self.activity_follow || self.selected_activity >= alen {
            self.selected_activity = alen - 1;
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent, area: Rect) -> AgentHubAction {
        let visible = usize::from(area.height.saturating_sub(5)).max(1);
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return AgentHubAction::Close;
        }
        if self.tab == HubTab::Agents && self.filter_editing {
            return self.key_edit_filter(key);
        }
        if self.tab == HubTab::Activity && self.activity_search_editing {
            return self.key_edit_search(key);
        }
        match key.code {
            KeyCode::Char('1') => {
                self.tab = HubTab::Agents;
                return AgentHubAction::Stay;
            }
            KeyCode::Char('2') => {
                self.tab = HubTab::Activity;
                self.clamp_selections();
                return AgentHubAction::Stay;
            }
            KeyCode::Tab => {
                self.show_inspector_narrow = !self.show_inspector_narrow;
                return AgentHubAction::Stay;
            }
            _ => {}
        }
        match self.tab {
            HubTab::Agents => self.key_agents(key, visible),
            HubTab::Activity => self.key_activity(key, visible),
        }
    }

    fn key_edit_filter(&mut self, key: KeyEvent) -> AgentHubAction {
        match key.code {
            KeyCode::Esc => {
                self.filter_editing = false;
                self.filter.clear();
                self.clamp_selections();
            }
            KeyCode::Enter => self.filter_editing = false,
            KeyCode::Backspace => {
                self.filter.pop();
                self.clamp_selections();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(c);
                self.clamp_selections();
            }
            _ => {}
        }
        AgentHubAction::Stay
    }

    fn key_edit_search(&mut self, key: KeyEvent) -> AgentHubAction {
        match key.code {
            KeyCode::Esc => {
                self.activity_search_editing = false;
                self.activity_search.clear();
                self.clamp_selections();
            }
            KeyCode::Enter => self.activity_search_editing = false,
            KeyCode::Backspace => {
                self.activity_search.pop();
                self.clamp_selections();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.activity_search.push(c);
                self.clamp_selections();
            }
            _ => {}
        }
        AgentHubAction::Stay
    }

    fn key_agents(&mut self, key: KeyEvent, visible: usize) -> AgentHubAction {
        let count = self.roster_rows().len();
        match key.code {
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.clamp_selections();
                    return AgentHubAction::Stay;
                }
                return AgentHubAction::Close;
            }
            KeyCode::Char('q') => return AgentHubAction::Close,
            KeyCode::Char('t') => {
                self.tree_mode = !self.tree_mode;
                self.clamp_selections();
            }
            KeyCode::Char('/') => self.filter_editing = true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_roster = self.selected_roster.saturating_sub(1);
                keep_visible(self.selected_roster, &mut self.roster_top, visible);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_roster + 1 < count {
                    self.selected_roster += 1;
                }
                keep_visible(self.selected_roster, &mut self.roster_top, visible);
            }
            KeyCode::Enter => {
                if let Some(entry) = self.selected_entry() {
                    return AgentHubAction::Focus(entry.id.clone());
                }
            }
            KeyCode::Char('r') => {
                if let Some(entry) = self.selected_entry() {
                    return AgentHubAction::Revive(entry.id.clone());
                }
            }
            KeyCode::Char('x') => {
                if let Some(entry) = self.selected_entry() {
                    return AgentHubAction::Abort(entry.id.clone());
                }
            }
            _ => {}
        }
        AgentHubAction::Stay
    }

    fn key_activity(&mut self, key: KeyEvent, visible: usize) -> AgentHubAction {
        let count = self.activity_rows().len();
        match key.code {
            KeyCode::Esc => {
                if !self.activity_search.is_empty() {
                    self.activity_search.clear();
                    self.clamp_selections();
                    return AgentHubAction::Stay;
                }
                return AgentHubAction::Close;
            }
            KeyCode::Char('q') => return AgentHubAction::Close,
            KeyCode::Char('f') => {
                self.activity_filter = self.activity_filter.next();
                self.clamp_selections();
            }
            KeyCode::Char('s') => {
                self.activity_scope = self.activity_scope.next();
                self.clamp_selections();
            }
            KeyCode::Char('/') => self.activity_search_editing = true,
            KeyCode::Char(' ') => {
                self.activity_follow = !self.activity_follow;
                if self.activity_follow && count > 0 {
                    self.selected_activity = count - 1;
                    keep_visible(self.selected_activity, &mut self.activity_top, visible);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.activity_follow = false;
                self.selected_activity = self.selected_activity.saturating_sub(1);
                keep_visible(self.selected_activity, &mut self.activity_top, visible);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_activity + 1 < count {
                    self.selected_activity += 1;
                }
                keep_visible(self.selected_activity, &mut self.activity_top, visible);
            }
            KeyCode::Enter => {
                if let Some(item) = self.activity_rows().get(self.selected_activity) {
                    return AgentHubAction::JumpToTranscript {
                        agent_id: item.agent_id.clone(),
                        session_id: item.session_id.clone(),
                        node_id: item.node_id.clone(),
                        summary: item.summary.clone(),
                    };
                }
            }
            _ => {}
        }
        AgentHubAction::Stay
    }
}

fn keep_visible(selected: usize, top: &mut usize, visible: usize) {
    if selected < *top {
        *top = selected;
    } else if visible > 0 && selected >= *top + visible {
        *top = selected + 1 - visible;
    }
}
