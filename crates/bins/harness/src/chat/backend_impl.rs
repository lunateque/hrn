//! [`hrn_tui::Backend`] implementation for [`ChatBackend`], split out to keep `chat.rs` concise.

use super::ChatBackend;
use crate::setup;
use hrn_edge_llm::resolve_alias;
use hrn_kernel::{DagNode, HrnError, ImageAttachment, Result, SessionMeta, Usage};
use hrn_tui::{
    AgentDefRow, AgentHubSnapshot, Backend, CustomCommandInfo, LoginOutcome, LoginProviderChoice,
    LoginSubmission, LogoutProviderChoice, ModelInfo, RestoreStart, RoleAssignment, SettingItem,
    SettingUpdate, Settings, SetupWizardInfo, WorktreeOutcome,
};
use std::collections::HashMap;
use std::path::PathBuf;

impl Backend for ChatBackend {
    fn model_aliases(&self) -> Vec<String> {
        hrn_edge_llm::enabled_presets(&self.llm)
            .into_iter()
            .map(|(alias, _)| alias.to_owned())
            .collect()
    }
    fn model_catalog(&self) -> Vec<ModelInfo> {
        self.model_catalog_list()
    }
    fn role_rows(&self) -> Vec<RoleAssignment> {
        self.role_rows_list()
    }
    fn settings_items(&self) -> Vec<SettingItem> {
        self.settings_items_live()
    }
    fn set_setting(&mut self, key: &str, value: &str) -> Result<SettingUpdate> {
        self.set_setting_live(key, value)
    }
    fn reset_setting(&mut self, key: &str) -> Result<SettingUpdate> {
        self.reset_setting_live(key)
    }
    fn resolve_model(&self, alias: &str) -> Result<String> {
        resolve_alias(&self.llm, Some(alias))
    }
    fn apply_settings(&mut self, settings: &Settings) {
        self.settings.clone_from(settings);
        if self.focused_agent.is_some()
            && let Some(sid) = self.active_session_id()
        {
            let _ = self.sync_settings(&sid);
            return;
        }
        self.control.set_permission_mode(settings.mode);
        self.control
            .set_advisor(settings.advisor, settings.advisor_model.clone());
    }
    fn pull_goal(&self, settings: &mut Settings) {
        settings.goal = self
            .session
            .as_deref()
            .and_then(|id| self.runtime.goal_of(id).ok())
            .flatten();
        settings.model.clone_from(&self.settings.model);
        settings.thinking = self.settings.thinking;
    }
    fn goal_command(&mut self, arg: Option<&str>) -> Result<String> {
        self.run_goal(arg)
    }
    fn modelpreset_command(&mut self, arg: Option<&str>) -> Result<String> {
        self.run_modelpreset(arg)
    }
    fn submit(&mut self, prompt: &str) -> Result {
        self.submit_with_images(prompt, Vec::new())
    }
    fn submit_with_images(&mut self, prompt: &str, images: Vec<ImageAttachment>) -> Result {
        if self.submit_collab_guest(prompt)? {
            return Ok(());
        }
        self.apply_pending_reload();
        self.ensure_writable()?;
        let session_id = self.session_for(prompt)?;
        self.ensure_writable()?;
        self.sync_settings(&session_id)?;
        self.start_turn(session_id, prompt, images);
        Ok(())
    }
    fn start_inbox_turn(&mut self) -> Result<Option<String>> {
        self.inbox_turn()
    }
    fn steer(&mut self, text: &str) {
        self.control.push_steer(text.to_owned());
    }
    fn interrupt(&mut self) {
        if self.interrupt_collab_guest() {
            return;
        }
        if let (Some(root), Some(agent_id)) =
            (self.session.as_deref(), self.focused_agent.as_deref())
            && let Some(rec) = self.runtime.agents().get_in_tree(root, agent_id)
        {
            rec.cancel();
            return;
        }
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        self.control.interrupt();
    }
    fn compact(&mut self, instructions: Option<&str>) -> Result<bool> {
        Ok(self.run_compact(instructions))
    }
    fn shake(&mut self) -> Result<bool> {
        Ok(self.run_shake())
    }
    fn compaction_auto_continue(&self) -> bool {
        self.cfg.compaction.auto_continue
    }
    fn latest_compaction_summary(&self) -> Option<String> {
        self.latest_compaction_text()
    }
    fn new_session(&mut self) {
        self.set_session(None);
    }
    fn sessions(&self) -> Result<Vec<SessionMeta>> {
        self.list_sessions()
    }
    fn session_usage(&self) -> Result<Usage> {
        self.rollup_usage()
    }
    fn fetch_usage(&mut self) -> Result {
        self.run_usage_task()
    }
    fn stats(&self) -> Result<String> {
        self.stats_report()
    }
    fn export(&self, path: Option<&str>) -> Result<String> {
        self.export_to(path)
    }
    fn resume(&mut self, session_id: &str) -> Result<(SessionMeta, Vec<DagNode>)> {
        self.resume_session_live(session_id)
    }
    fn context_window(&self, model: &str) -> u64 {
        self.runtime.capabilities(Some(model)).context_window
    }
    fn custom_commands(&self) -> Vec<CustomCommandInfo> {
        let mut out: Vec<CustomCommandInfo> = self
            .custom_commands
            .values()
            .map(|c| CustomCommandInfo {
                name: c.name.clone(),
                description: c.description.clone(),
            })
            .collect();
        out.extend(self.skill_commands());
        out
    }
    fn expand_custom_command(&self, name: &str, args: &str) -> Option<String> {
        if let Some(skill) = name.strip_prefix("skill:") {
            return self.expand_skill(skill, args);
        }
        self.custom_commands
            .get(&name.to_lowercase())
            .map(|c| c.expand(args))
    }
    fn current_tree(&self) -> Result<Option<(Option<String>, Vec<DagNode>)>> {
        let Some(id) = &self.session else {
            return Ok(None);
        };
        let meta = self
            .store
            .get_session(id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{id}`")))?;
        let nodes = self.store.load_tree(id)?;
        Ok(Some((meta.head_node_id, nodes)))
    }
    fn checkout_node(&mut self, node_id: &str) -> Result<(SessionMeta, Vec<DagNode>)> {
        self.close_collab_host("host navigated session tree");
        let id = self
            .session
            .as_deref()
            .ok_or_else(|| HrnError::Store("no active session".to_owned()))?;
        self.runtime.switch_head(id, node_id)
    }
    fn fork_at(&mut self, node_id: Option<&str>) -> Result<(SessionMeta, Vec<DagNode>)> {
        let id = self
            .session
            .as_deref()
            .ok_or_else(|| HrnError::Store("no active session".to_owned()))?;
        let forked = self.runtime.fork_session(id, node_id)?;
        let nodes = match &forked.head_node_id {
            Some(head) => self.store.load_branch(&forked.id, head)?,
            None => Vec::new(),
        };
        self.set_session(Some(forked.id.clone()));
        Ok((forked, nodes))
    }
    fn login_providers(&self) -> Vec<LoginProviderChoice> {
        self.login_providers_live()
    }
    fn save_login(&mut self, submission: LoginSubmission) -> Result<LoginOutcome> {
        self.save_login_probed(submission)
    }
    fn start_key_login(&mut self, submission: LoginSubmission) -> Result<Option<LoginOutcome>> {
        self.start_key_login_live(submission)
    }
    fn start_login(&mut self, provider: &str) -> Result {
        self.start_login_live(provider)
    }
    fn submit_login_code(&mut self, input: &str) -> Result {
        self.submit_login_code_live(input)
    }
    fn cancel_login(&mut self) {
        self.cancel_login_live();
    }
    fn finish_login(&mut self) {
        self.finish_login_live();
    }
    fn logout_providers(&self) -> Vec<LogoutProviderChoice> {
        self.logout_providers_live()
    }
    fn logout_provider(&mut self, provider: &str) -> Result<String> {
        self.logout_provider_live(provider)
    }
    fn setup_info(&self) -> SetupWizardInfo {
        self.setup_info_live()
    }
    fn apply_setup_theme(&mut self, theme: &str) -> Result<String> {
        self.apply_setup_theme_live(theme)
    }
    fn auth_status(&self) -> String {
        setup::format_auth_status(&self.cfg)
    }
    fn current_session_id(&self) -> Option<String> {
        self.session.clone()
    }
    fn set_pinned(&mut self, session_id: Option<&str>, pinned: bool) -> hrn_kernel::Result {
        let id = session_id
            .map(str::to_owned)
            .or_else(|| self.session.clone())
            .ok_or_else(|| HrnError::Store("no active session".to_owned()))?;
        self.store.set_pinned(&id, pinned)
    }
    fn delete_session(&mut self, session_id: &str) -> hrn_kernel::Result<bool> {
        if self.session.as_deref() == Some(session_id) {
            self.set_session(None);
        }
        self.store.delete_session(session_id)
    }
    fn rename_session(&mut self, session_id: Option<&str>, title: &str) -> hrn_kernel::Result {
        let id = session_id
            .map(str::to_owned)
            .or_else(|| self.session.clone())
            .ok_or_else(|| HrnError::Store("no active session".to_owned()))?;
        self.runtime
            .rename_session(&id, title, hrn_kernel::TitleSource::User)
    }
    fn regenerate_title(&mut self) -> hrn_kernel::Result {
        self.regenerate_title_live()
    }
    fn discover_external_sessions(&self) -> hrn_kernel::Result<Vec<hrn_tui::ExternalSessionRow>> {
        Ok(crate::import_cmd::discover_for_tui())
    }
    fn import_external_session(
        &mut self,
        source: &str,
        id: &str,
    ) -> hrn_kernel::Result<(SessionMeta, Vec<DagNode>)> {
        self.import_external_session_live(source, id)
    }
    fn context_breakdown(&self) -> hrn_tui::ContextBreakdown {
        self.context_breakdown_report()
    }
    fn mcp_status(&self) -> Vec<hrn_tui::McpStatus> {
        self.mcp_status_report()
    }
    fn tools(&self) -> Vec<hrn_tui::ToolInfo> {
        self.tools_report()
    }
    fn jobs(&self) -> Vec<hrn_tui::JobInfo> {
        self.jobs_report()
    }
    fn todos(&self) -> Vec<hrn_kernel::TodoPhase> {
        let Some(id) = &self.session else {
            return Vec::new();
        };
        let root = self.runtime.agents().root_of(id);
        let phases = self.runtime.todos().phases(&root);
        if !phases.is_empty() {
            return phases;
        }
        self.session_branch()
            .ok()
            .flatten()
            .and_then(|nodes| hrn_harness::summary::latest_todo_board(&nodes))
            .unwrap_or_default()
    }
    fn set_todos(&mut self, phases: Vec<hrn_kernel::TodoPhase>) -> hrn_kernel::Result {
        let id = self
            .session
            .clone()
            .ok_or_else(|| HrnError::Store("no active session".to_owned()))?;
        self.runtime.set_todo_board(&id, phases)
    }
    fn notify(&self) -> hrn_tui::Notify {
        self.cfg.notify.clone()
    }
    fn settings_report(&self) -> String {
        self.settings_report_text()
    }
    fn forget_approvals(&mut self) -> Result {
        self.runtime.forget_approvals()
    }
    fn role_assignments(&self) -> Vec<(String, String)> {
        self.role_assignments_list()
    }
    fn assign_role(&mut self, role: &str, target: &str) -> Result<String> {
        self.assign_role_live(role, target)
    }
    fn switch_model_temporary(
        &mut self,
        selector: &str,
    ) -> Result<(String, Option<hrn_kernel::ThinkingLevel>)> {
        self.switch_temporary_live(selector)
    }
    fn cycle_entries(&self) -> Vec<(String, String, Option<hrn_kernel::ThinkingLevel>)> {
        self.cycle_entries_list()
    }
    fn model_presets(&self) -> Vec<String> {
        self.model_presets_list()
    }
    fn is_session_only_hub(&self) -> bool {
        self.focused_agent.is_some()
    }
    fn session_lifecycles(&self) -> HashMap<String, String> {
        self.session_lifecycles_map()
    }
    fn restore_active(&mut self) -> Result<RestoreStart> {
        self.restore_active_session()
    }
    fn lease_warning(&self) -> Option<String> {
        self.lease_warning_text()
    }
    fn handoff(&mut self, focus: Option<&str>) -> Result<bool> {
        self.run_handoff(focus)
    }
    fn checkout_optional(
        &mut self,
        node_id: Option<&str>,
        summarize: bool,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        self.checkout_optional_live(node_id, summarize)
    }
    fn set_node_label(&mut self, node_id: &str, label: Option<&str>) -> Result {
        self.set_label_live(node_id, label)
    }
    fn start_btw(&mut self, topic_id: Option<&str>, question: &str) -> Result<String> {
        self.start_btw_live(topic_id, question)
    }
    fn cancel_btw(&mut self) {
        self.cancel_btw_live();
    }
    fn btw_topics(&self) -> Result<Vec<hrn_kernel::BtwTopic>> {
        self.list_btw_live()
    }
    fn save_btw_topic(&mut self, topic: &hrn_kernel::BtwTopic) -> Result {
        self.save_btw_live(topic)
    }
    fn supports_service_tier(&self, model: &str) -> bool {
        self.supports_service_tier_live(model)
    }
    fn set_service_tier(&mut self, tier: Option<hrn_kernel::Tier>) {
        self.set_service_tier_live(tier);
    }
    fn slow_mode(&self) -> bool {
        self.slow_mode_live()
    }
    fn set_slow_mode(&mut self, enabled: bool) {
        self.set_slow_mode_live(enabled);
    }
    fn extended_context(&self) -> bool {
        self.extended_context_live()
    }
    fn set_extended_context(&mut self, enabled: bool) {
        self.set_extended_context_live(enabled);
    }
    fn agent_hub_snapshot(&self) -> AgentHubSnapshot {
        self.agent_hub_snapshot_live()
    }
    fn focus_agent(&mut self, agent_id: &str) -> Result<(SessionMeta, Vec<DagNode>)> {
        self.focus_agent_live(agent_id)
    }
    fn unfocus_agent(&mut self) -> Result<Option<(SessionMeta, Vec<DagNode>)>> {
        self.unfocus_agent_live()
    }
    fn steer_or_prompt_agent(&mut self, agent_id: &str, text: &str) -> Result<bool> {
        self.steer_or_prompt_agent_live(agent_id, text)
    }
    fn revive_agent(&mut self, agent_id: &str) -> Result<String> {
        self.revive_agent_live(agent_id)
    }
    fn abort_agent(&mut self, agent_id: &str) -> Result<String> {
        self.abort_agent_live(agent_id)
    }
    fn agent_definitions(&self) -> Vec<AgentDefRow> {
        self.agent_definitions_list()
    }
    fn set_agent_enabled(&mut self, name: &str, enabled: bool) -> Result<String> {
        self.set_agent_enabled_live(name, enabled)
    }
    fn set_agent_model_override(&mut self, name: &str, model: Option<&str>) -> Result<String> {
        self.set_agent_model_override_live(name, model)
    }
    fn mcp_resources(&self) -> Vec<hrn_tui::McpResourceInfo> {
        self.mcp_resources_report()
    }
    fn local_artifacts_dir(&self) -> Option<PathBuf> {
        self.local_artifacts_dir_live()
    }
    fn share_session(&mut self, no_redact: bool) -> Result<String> {
        self.share_session_live(no_redact)
    }
    fn collab_command(&mut self, arg: Option<&str>) -> Result<String> {
        self.collab_command_live(arg)
    }
    fn join_collab(&mut self, link: &str) -> Result<String> {
        self.join_collab_live(link)
    }
    fn leave_collab(&mut self) -> Result<String> {
        Ok(self.leave_collab_live())
    }
    fn collab_guest_read_only(&self) -> bool {
        self.collab_guest_read_only_live()
    }
    fn relocate_worktree(&mut self, branch: Option<&str>) -> Result<WorktreeOutcome> {
        self.relocate_worktree_live(branch)
    }
    fn memory_command(&mut self, arg: Option<&str>) -> Result<String> {
        self.memory_command_live(arg)
    }
    fn init_prompt(&self) -> String {
        Self::init_prompt_live()
    }
    fn traces(&self) -> Result<Vec<hrn_kernel::TraceRecord>> {
        self.traces_live()
    }
    fn dump_transcript(&self) -> Result<String> {
        self.dump_transcript_live()
    }
    fn start_loop_condition(&mut self, check_id: u64, command: &str) -> Result {
        self.start_loop_condition_live(check_id, command)
    }
    fn cancel_loop_condition(&mut self) {
        self.cancel_loop_condition_live();
    }
    fn prepare_commit_preview(&mut self) -> Result<hrn_tui::CommitPreview> {
        self.prepare_commit_preview_live()
    }
    fn execute_commit(&mut self, message: &str, changelog: bool) -> Result<String> {
        self.execute_commit_live(message, changelog)
    }
    fn generate_commit_message_for_staged(&mut self, staged_diff: &str) -> Result<String> {
        self.generate_commit_message_for_staged_live(staged_diff)
    }
    fn remote_command(&mut self, arg: Option<&str>) -> Result<hrn_tui::RemoteCommandOutcome> {
        self.remote_command_live(arg)
    }
    fn finish_remote(&mut self) -> Option<Result<hrn_tui::RemoteCommandOutcome>> {
        self.finish_remote_live()
    }
    fn remote_rotate(&mut self, control: bool) -> Result<hrn_tui::RemoteSessionView> {
        self.remote_rotate_live(control)
    }
    fn remote_stop(&mut self) -> Result<String> {
        Ok(self.remote_stop_live())
    }
    fn agy_command(&mut self, arg: Option<&str>) -> Result<String> {
        self.agy_command_live(arg)
    }
}
