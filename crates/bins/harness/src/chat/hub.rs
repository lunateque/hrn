//! Live Agent Hub snapshot bridge, child-agent focusing/steering/revive/abort, and `/agents`
//! definition hub persistence (`[task] disabled_agents` + `[task.agent_models]`) via `toml_write`.

use super::ChatBackend;
use super::chat_ui::UiEvents;
use crate::toml_write;
use crate::wiring::build_runtime_with_tools;
use hrn_edge_llm::LlmConfig;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::{
    AgentActivityKind as HarnessActivityKind, AgentRecord, AgentRegistry, JobStatus, TaskConfig,
    default_subagent_profiles, load_subagent_profiles,
};
use hrn_kernel::{AgentStatus, DagNode, HrnError, Result, SessionMeta, SessionStore};
use hrn_tui::{
    AgentActivityItem, AgentActivityKind, AgentDefRow, AgentHubEntry, AgentHubSnapshot,
    AgentHubStatus,
};
use std::path::Path;
use std::sync::Arc;
use toml_edit::{Item, Table, Value as EditValue};

fn map_status(status: JobStatus) -> AgentHubStatus {
    match status {
        JobStatus::Running => AgentHubStatus::Running,
        JobStatus::Idle => AgentHubStatus::Idle,
        JobStatus::Parked => AgentHubStatus::Parked,
        JobStatus::Aborted => AgentHubStatus::Aborted,
        JobStatus::Failed => AgentHubStatus::Failed,
    }
}

fn map_activity_kind(kind: HarnessActivityKind) -> AgentActivityKind {
    match kind {
        HarnessActivityKind::Tool => AgentActivityKind::Tool,
        HarnessActivityKind::Response => AgentActivityKind::Response,
        HarnessActivityKind::Error => AgentActivityKind::Error,
        HarnessActivityKind::Irc => AgentActivityKind::Irc,
        HarnessActivityKind::Lifecycle => AgentActivityKind::Lifecycle,
    }
}

fn lookup_window(llm: &LlmConfig, model: Option<&str>) -> Option<u64> {
    let alias = model?;
    llm.presets
        .get(alias)
        .and_then(|p| p.context_window)
        .or(Some(200_000))
}

fn record_to_entry(
    record: &AgentRecord,
    store: &dyn SessionStore,
    llm: &LlmConfig,
) -> (AgentHubEntry, Vec<AgentActivityItem>) {
    let snap = record.snapshot();
    let session_model = snap
        .session_id
        .as_deref()
        .and_then(|sid| store.get_session(sid).ok().flatten())
        .and_then(|m| m.model);
    let model = snap.model.or(session_model);
    let context_window = lookup_window(llm, model.as_deref());
    let entry = AgentHubEntry {
        id: record.id.clone(),
        name: record.name.clone(),
        role: record.profile.clone(),
        status: map_status(snap.status),
        is_main: record.is_main,
        parent: record.parent.clone(),
        session_id: snap.session_id.clone(),
        model,
        task: record.task.clone(),
        activity: snap.activity,
        unread: snap.unread,
        started_ms: record.started_ms,
        finished_ms: snap.finished_ms,
        generation: snap.generation,
        steps: snap.steps,
        requests: snap.requests,
        tool_calls: snap.tool_calls,
        input_tokens: snap.input_tokens,
        output_tokens: snap.output_tokens,
        cost_usd: (snap.cost_usd > 0.0).then_some(snap.cost_usd),
        context_tokens: (snap.context_tokens > 0).then_some(snap.context_tokens),
        context_window,
        current_tool: snap.current_tool,
        current_tool_args: snap.current_tool_args,
        current_tool_started_ms: snap.current_tool_started_ms,
        last_intent: snap.last_intent,
        retry_state: snap.retry_state,
        output_path: snap.artifact_path.map(|p| p.display().to_string()),
        patch_path: snap.patch_path.map(|p| p.display().to_string()),
        isolated_branch: snap.isolated_branch,
    };
    let events = snap
        .events
        .into_iter()
        .map(|ev| AgentActivityItem {
            ts_ms: ev.ts_ms,
            agent_id: ev.agent_id,
            kind: map_activity_kind(ev.kind),
            summary: ev.summary,
            detail: ev.detail,
            session_id: ev.session_id,
            node_id: ev.node_id,
        })
        .collect();
    (entry, events)
}

/// Build a live [`AgentHubSnapshot`] for `root` from the registry and session store.
pub(crate) fn build_hub_snapshot(
    agents: &AgentRegistry,
    store: &dyn SessionStore,
    llm: &LlmConfig,
    root: &str,
) -> AgentHubSnapshot {
    let records = agents.in_tree(root);
    let has_main = records.iter().any(|r| r.is_main);
    let mut entries = Vec::with_capacity(records.len() + usize::from(!has_main));
    let mut activity = Vec::new();
    if !has_main && let Ok(Some(meta)) = store.get_session(root) {
        let status = match meta.status {
            AgentStatus::Working | AgentStatus::Blocked => AgentHubStatus::Running,
            AgentStatus::Error => AgentHubStatus::Failed,
            AgentStatus::Idle => AgentHubStatus::Idle,
        };
        let window = lookup_window(llm, meta.model.as_deref());
        entries.push(AgentHubEntry {
            id: "main".to_owned(),
            name: "main".to_owned(),
            role: "main".to_owned(),
            status,
            is_main: true,
            parent: None,
            session_id: Some(meta.id),
            model: meta.model,
            task: meta.title,
            activity: String::new(),
            unread: 0,
            started_ms: meta.created_at_ms,
            finished_ms: if status == AgentHubStatus::Running {
                0
            } else {
                meta.updated_at_ms
            },
            generation: 1,
            steps: 0,
            requests: 0,
            tool_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: None,
            context_tokens: None,
            context_window: window,
            current_tool: None,
            current_tool_args: None,
            current_tool_started_ms: None,
            last_intent: None,
            retry_state: None,
            output_path: None,
            patch_path: None,
            isolated_branch: None,
        });
    }
    for rec in records {
        let (entry, evs) = record_to_entry(&rec, store, llm);
        entries.push(entry);
        activity.extend(evs);
    }
    activity.sort_by_key(|ev| ev.ts_ms);
    AgentHubSnapshot {
        agents: entries,
        activity,
    }
}

/// Persist an enable/disable toggle into `[task] disabled_agents` via comment-preserving
/// [`toml_write::set_key`], mutating `task` in place.
pub(crate) fn persist_agent_enabled(
    config_path: &Path,
    task: &mut TaskConfig,
    name: &str,
    enabled: bool,
) -> Result {
    let key = name.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Err(HrnError::Config("agent name must not be empty".to_owned()));
    }
    task.disabled_agents
        .retain(|item| !item.eq_ignore_ascii_case(&key));
    if !enabled {
        task.disabled_agents.push(key);
    }
    let arr: Vec<toml::Value> = task
        .disabled_agents
        .iter()
        .cloned()
        .map(toml::Value::String)
        .collect();
    toml_write::set_key(
        config_path,
        "task.disabled_agents",
        &toml::Value::Array(arr),
    )
}

/// Persist or remove a per-agent model override under `[task.agent_models]` via comment-preserving
/// [`toml_write::edit_document`], mutating `task` in place.
pub(crate) fn persist_agent_model_override(
    config_path: &Path,
    task: &mut TaskConfig,
    name: &str,
    model: Option<&str>,
) -> Result {
    let key = name.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Err(HrnError::Config("agent name must not be empty".to_owned()));
    }
    let cleaned = model.map(str::trim).filter(|s| !s.is_empty());
    match cleaned {
        Some(sel) => {
            task.agent_models.insert(key.clone(), sel.to_owned());
        }
        None => {
            task.agent_models
                .retain(|k, _| !k.eq_ignore_ascii_case(&key));
        }
    }
    let _ = toml_write::edit_document(config_path, |doc| {
        if let Some(sel) = cleaned {
            if !doc.contains_key("task") {
                let mut t = Table::new();
                t.set_implicit(true);
                doc.insert("task", Item::Table(t));
            }
            let Some(task_tbl) = doc.get_mut("task").and_then(Item::as_table_mut) else {
                return Err(HrnError::Config(
                    "`task` is not a table in config file".into(),
                ));
            };
            if !task_tbl.contains_key("agent_models") {
                task_tbl.insert("agent_models", Item::Table(Table::new()));
            }
            let Some(models_tbl) = task_tbl
                .get_mut("agent_models")
                .and_then(Item::as_table_mut)
            else {
                return Err(HrnError::Config(
                    "`task.agent_models` is not a table in config file".into(),
                ));
            };
            models_tbl.insert(&key, Item::Value(EditValue::from(sel)));
        } else if let Some(models_tbl) = doc
            .get_mut("task")
            .and_then(Item::as_table_mut)
            .and_then(|t| t.get_mut("agent_models"))
            .and_then(Item::as_table_mut)
        {
            models_tbl.remove(&key);
        }
        Ok(())
    })?;
    Ok(())
}

impl ChatBackend {
    pub(super) fn clear_focus_sink(&self) {
        if let (Some(root), Some(agent_id)) =
            (self.session.as_deref(), self.focused_agent.as_deref())
            && let Some(rec) = self.runtime.agents().get_in_tree(root, agent_id)
        {
            rec.set_focus_sink(None);
        }
    }

    pub(super) fn agent_hub_snapshot_live(&self) -> AgentHubSnapshot {
        let Some(root) = self.session.as_deref() else {
            return AgentHubSnapshot::default();
        };
        build_hub_snapshot(
            self.runtime.agents().as_ref(),
            self.store.as_ref(),
            &self.llm,
            root,
        )
    }

    pub(super) fn focus_agent_live(
        &mut self,
        agent_id: &str,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        let root = self
            .session
            .clone()
            .ok_or_else(|| HrnError::Store("no active root session".to_owned()))?;
        let record = self
            .runtime
            .agents()
            .get_in_tree(&root, agent_id)
            .ok_or_else(|| HrnError::Tool(format!("unknown agent `{agent_id}`")))?;
        let status = record.snapshot().status;
        if matches!(status, JobStatus::Parked | JobStatus::Aborted) {
            let _ = self.runtime.agents().revive_to_idle(&root, agent_id)?;
        }
        self.clear_focus_sink();
        record.mark_read();
        record.set_focus_sink(Some(Arc::new(UiEvents::tui_only(self.tx.clone()))));
        let child_sid = record.snapshot().session_id.ok_or_else(|| {
            HrnError::Tool(format!("agent `{agent_id}` has not created a session yet"))
        })?;
        let meta = self
            .store
            .get_session(&child_sid)?
            .ok_or_else(|| HrnError::Store(format!("unknown child session `{child_sid}`")))?;
        let nodes = match &meta.head_node_id {
            Some(head) => self.store.load_branch(&meta.id, head)?,
            None => Vec::new(),
        };
        if self.focused_agent.is_none() {
            self.saved_main_settings = Some(self.settings.clone());
        }
        if let Some(model) = &meta.model {
            self.settings.model.clone_from(model);
        }
        self.settings.thinking = meta.thinking;
        self.settings.mode = meta.permission_mode;
        self.focused_agent = Some(agent_id.to_owned());
        self.focused_on_child
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.runtime.agents().inbox().wake();
        Ok((meta, nodes))
    }

    pub(super) fn unfocus_agent_live(&mut self) -> Result<Option<(SessionMeta, Vec<DagNode>)>> {
        self.clear_focus_sink();
        self.focused_agent = None;
        self.focused_on_child
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(saved) = self.saved_main_settings.take() {
            self.settings = saved;
        }
        let Some(root) = self.session.as_deref() else {
            return Ok(None);
        };
        let Some(meta) = self.store.get_session(root)? else {
            return Ok(None);
        };
        let nodes = match &meta.head_node_id {
            Some(head) => self.store.load_branch(&meta.id, head)?,
            None => Vec::new(),
        };
        self.runtime.agents().inbox().wake();
        Ok(Some((meta, nodes)))
    }

    pub(super) fn steer_or_prompt_agent_live(
        &mut self,
        agent_id: &str,
        text: &str,
    ) -> Result<bool> {
        let root = self
            .session
            .clone()
            .ok_or_else(|| HrnError::Store("no active root session".to_owned()))?;
        let record = self
            .runtime
            .agents()
            .get_in_tree(&root, agent_id)
            .ok_or_else(|| HrnError::Tool(format!("unknown agent `{agent_id}`")))?;
        record.set_focus_sink(Some(Arc::new(UiEvents::tui_only(self.tx.clone()))));
        if let Some(child_sid) = record.snapshot().session_id {
            let _ = self.sync_settings(&child_sid);
        }
        let now = self.runtime.agents().clock().now_ms();
        if record.is_running() {
            record.control.push_steer(text.to_owned());
            record.note_irc(text, now);
            if let Some(child_sid) = record.snapshot().session_id {
                self.runtime.agents().inbox().notify_steer(&child_sid);
            } else {
                self.runtime.agents().inbox().wake();
            }
            Ok(true)
        } else {
            let _gen = self.runtime.agents().revive(&root, agent_id, text)?;
            self.runtime.agents().inbox().wake();
            Ok(false)
        }
    }

    pub(super) fn revive_agent_live(&mut self, agent_id: &str) -> Result<String> {
        let root = self
            .session
            .clone()
            .ok_or_else(|| HrnError::Store("no active root session".to_owned()))?;
        let _ = self.runtime.agents().revive_to_idle(&root, agent_id)?;
        Ok(format!("revived agent `{agent_id}`"))
    }

    pub(super) fn abort_agent_live(&mut self, agent_id: &str) -> Result<String> {
        self.runtime.agents().kill(agent_id)?;
        Ok(format!("aborted agent `{agent_id}`"))
    }

    pub(super) fn agent_definitions_list(&self) -> Vec<AgentDefRow> {
        let profiles = load_subagent_profiles(&StdWorkspaceFs, &self.cwd)
            .unwrap_or_else(|_| default_subagent_profiles());
        profiles
            .into_values()
            .map(|p| {
                let enabled = !self.cfg.task.is_agent_disabled(&p.name);
                let model_override = self
                    .cfg
                    .task
                    .model_override_for(&p.name, &p.aliases)
                    .map(str::to_owned);
                AgentDefRow {
                    name: p.name,
                    aliases: p.aliases,
                    source: p.source.to_string(),
                    description: p.description,
                    read_only: p.read_only,
                    enabled,
                    default_model: p.model,
                    model_override,
                }
            })
            .collect()
    }

    pub(super) fn set_agent_enabled_live(&mut self, name: &str, enabled: bool) -> Result<String> {
        let mut staged = self.cfg.clone();
        persist_agent_enabled(&self.config_path, &mut staged.task, name, enabled)?;
        self.apply_rebuilt_config(staged)?;
        let state = if enabled { "enabled" } else { "disabled" };
        Ok(format!("agent `{name}` {state}"))
    }

    pub(super) fn set_agent_model_override_live(
        &mut self,
        name: &str,
        model: Option<&str>,
    ) -> Result<String> {
        let mut staged = self.cfg.clone();
        persist_agent_model_override(&self.config_path, &mut staged.task, name, model)?;
        self.apply_rebuilt_config(staged)?;
        match model.map(str::trim).filter(|s| !s.is_empty()) {
            Some(sel) => Ok(format!("agent `{name}` model override → {sel}")),
            None => Ok(format!("cleared model override for `{name}`")),
        }
    }

    fn apply_rebuilt_config(&mut self, staged: crate::config::Config) -> Result {
        let new_runtime = build_runtime_with_tools(
            &staged,
            Arc::clone(&self.sqlite),
            &self.cwd,
            &self.mcp_tools,
            &self.background,
        )?;
        self.runtime = new_runtime;
        self.llm = staged.llm.clone();
        self.model_names = crate::stats::names(&staged.llm);
        self.cfg = staged;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn agents_hub_toggle_persists() {
        let dir =
            std::env::temp_dir().join(format!("hrn-agents-hub-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let seed = "\
# User header comment
thinking = \"medium\"

[[providers]]
name = \"local\"
kind = \"openai_chat\"
base_url = \"http://localhost:11434/v1\"

[models.fast]
provider = \"local\"
id = \"qwen-fast\"
context_window = 128000

[models.strong]
provider = \"local\"
id = \"qwen-strong\"
context_window = 128000

[roles]
default = \"fast\"

[task]
# Preserve this task comment
max_concurrency = 16
";
        toml_write::atomic_write(&path, seed).expect("seed config");

        let load_isolated = |p: &std::path::Path| {
            let text = std::fs::read_to_string(p).expect("read temp config");
            Config::parse(&text).expect("parse temp config")
        };
        let mut cfg = load_isolated(&path);
        assert!(!cfg.task.is_agent_disabled("security-reviewer"));

        // Disable `security-reviewer` and set a per-agent model override on `reviewer`.
        persist_agent_enabled(&path, &mut cfg.task, "security-reviewer", false)
            .expect("disable security-reviewer");
        persist_agent_model_override(&path, &mut cfg.task, "reviewer", Some("strong:high"))
            .expect("override reviewer model");

        let raw = std::fs::read_to_string(&path).expect("read written");
        assert!(raw.contains("# User header comment"), "{raw}");
        assert!(raw.contains("# Preserve this task comment"), "{raw}");

        let reloaded = load_isolated(&path);
        assert!(reloaded.task.is_agent_disabled("security-reviewer"));
        assert_eq!(reloaded.task.max_concurrency, 16);
        assert_eq!(
            reloaded.task.model_override_for("reviewer", &[]),
            Some("strong:high")
        );

        // Re-enable `security-reviewer` and clear `reviewer` override.
        let mut cfg2 = reloaded;
        persist_agent_enabled(&path, &mut cfg2.task, "security-reviewer", true)
            .expect("re-enable security-reviewer");
        persist_agent_model_override(&path, &mut cfg2.task, "reviewer", None)
            .expect("clear reviewer override");

        let reloaded2 = load_isolated(&path);
        assert!(!reloaded2.task.is_agent_disabled("security-reviewer"));
        assert_eq!(reloaded2.task.model_override_for("reviewer", &[]), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
