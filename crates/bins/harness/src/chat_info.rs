//! The read-only views behind `/context`, `/tools`, `/mcp`, `/jobs` and `/settings`: they read
//! the registry, the recorded MCP statuses, the branch and the loaded config. `Backend`
//! forwards here so the session plumbing keeps one home.

use crate::chat::ChatBackend;
use hrn_tui::Backend as _;

impl ChatBackend {
    pub(super) fn context_breakdown_report(&self) -> hrn_tui::ContextBreakdown {
        use hrn_tui::ContextBreakdown;
        let Some(id) = &self.session else {
            return ContextBreakdown::default();
        };
        let Ok(meta) = self.store.get_session(id) else {
            return ContextBreakdown::default();
        };
        let Some(meta) = meta else {
            return ContextBreakdown::default();
        };
        let nodes = self
            .store
            .load_branch(&meta.id, meta.head_node_id.as_deref().unwrap_or(""))
            .unwrap_or_default();
        let activated = self.runtime.active_tools(&meta).unwrap_or_default();
        let system = self.runtime.system_prompt_for(&self.cwd, &self.control);
        let schemas: usize = self
            .runtime
            .request_specs(&activated)
            .iter()
            .map(|spec| {
                spec.name.len() + spec.description.len() + spec.parameters.to_string().len()
            })
            .sum();
        let policy = self.runtime.policy_for(meta.model.as_deref());
        let projected = hrn_harness::project_branch(&nodes, &policy);
        ContextBreakdown {
            system_tokens: system.len() as u64 / 4,
            schema_tokens: (schemas / 4) as u64,
            message_tokens: projected.estimated_tokens,
            window: self.context_window(&self.settings.model),
            last_prompt_tokens: None,
            nodes: nodes.len(),
        }
    }

    pub(super) fn mcp_status_report(&self) -> Vec<hrn_tui::McpStatus> {
        self.mcp_statuses
            .iter()
            .map(|status| {
                let prefix = format!("mcp__{}__", status.name);
                let deferred = self
                    .runtime
                    .tools()
                    .names()
                    .into_iter()
                    .filter(|name| {
                        name.starts_with(&prefix) && self.runtime.tools().is_deferred(name)
                    })
                    .count();
                let loaded = status.tool_count.saturating_sub(deferred);
                hrn_tui::McpStatus {
                    name: status.name.clone(),
                    transport: status.source.clone().replace(".mcp.json", "mcp.json"),
                    source: status.source.clone(),
                    enabled: status.error.as_deref() != Some("disabled"),
                    connected: status.connected,
                    tools: loaded,
                    deferred,
                    note: status.error.clone(),
                }
            })
            .collect()
    }

    pub(super) fn mcp_resources_report(&self) -> Vec<hrn_tui::McpResourceInfo> {
        let resources = self.background.mcp_resources();
        let cancel = hrn_kernel::CancelFlag::new();
        let timeout = std::time::Duration::from_secs(10);
        let Ok(entries) =
            crate::url_adapters::block_on_safe(Some(&self.handle), timeout, async move {
                resources.list_all_resources(&cancel).await
            })
        else {
            return Vec::new();
        };
        entries
            .into_iter()
            .map(|entry| hrn_tui::McpResourceInfo {
                server: entry.server,
                uri: entry.resource.uri,
                name: entry.resource.name,
                description: entry.resource.description,
                mime_type: entry.resource.mime_type,
            })
            .collect()
    }

    pub(super) fn tools_report(&self) -> Vec<hrn_tui::ToolInfo> {
        let registry = self.runtime.tools();
        registry
            .names()
            .into_iter()
            .filter_map(|name| {
                let tool = registry.get(&name)?;
                Some(hrn_tui::ToolInfo {
                    effect: match tool.effect() {
                        hrn_kernel::ToolEffect::ReadOnly => "read",
                        hrn_kernel::ToolEffect::Mutating => "mutating",
                    }
                    .to_owned(),
                    deferred: registry.is_deferred(&name),
                    name,
                })
            })
            .collect()
    }

    pub(super) fn jobs_report(&self) -> Vec<hrn_tui::JobInfo> {
        let Some(id) = &self.session else {
            return Vec::new();
        };
        let root = self.runtime.agents().root_of(id);
        self.runtime
            .procs()
            .in_root(&root)
            .into_iter()
            .map(|entry| hrn_tui::JobInfo {
                id: entry.id,
                running: entry.running,
                waitable: entry.waitable,
                started_ms: entry.started_ms,
                finished_ms: entry.finished_ms,
                line: entry.line,
            })
            .collect()
    }

    pub(super) fn settings_report_text(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = writeln!(out, "config      {}", self.config_path.display());
        let _ = writeln!(out, "cwd         {}", self.cwd.display());
        let _ = writeln!(
            out,
            "model       {} (window {})",
            self.settings.model,
            self.context_window(&self.settings.model)
        );
        let _ = writeln!(out, "thinking    {}", self.settings.thinking.as_str());
        let _ = writeln!(out, "mode        {}", self.settings.mode.as_str());
        let advisor = self
            .settings
            .advisor_model
            .as_deref()
            .map_or(String::new(), |m| format!(" · {m}"));
        let _ = writeln!(
            out,
            "advisor     {}{advisor}",
            if self.settings.advisor { "on" } else { "off" }
        );
        if let Some(secs) = self.cfg.ask_timeout {
            let _ = writeln!(out, "ask timeout {}s", secs.as_secs());
        }
        let _ = writeln!(
            out,
            "notify      {} (after {}s · title {})",
            if self.cfg.notify.enabled { "on" } else { "off" },
            self.cfg.notify.after.as_secs(),
            self.cfg.notify.title
        );
        let _ = writeln!(out, "theme       {}", self.cfg.appearance.theme.summary());
        let _ = writeln!(
            out,
            "symbols     {} · statusline {} ({})",
            self.cfg.appearance.symbol_preset.as_str(),
            self.cfg.appearance.statusline.preset.as_str(),
            self.cfg.appearance.statusline.separator.as_str()
        );
        let _ = writeln!(
            out,
            "mcp         {} configured · budget {} tokens",
            self.cfg.mcp_servers.len(),
            self.cfg.mcp_tool_budget
        );
        let _ = writeln!(out, "\nmodels");
        for alias in &self.llm.cycle_order {
            if let Some(preset) = self.llm.presets.get(alias) {
                let _ = writeln!(out, "  {alias}  {}  via {}", preset.id, preset.provider);
            }
        }
        let _ = writeln!(out, "\nproviders");
        for (name, provider) in &self.llm.providers {
            let _ = writeln!(out, "  {name}  {:?}", provider.kind);
        }
        if !self.cfg.module_tables.is_empty() {
            let _ = writeln!(out, "\nmodules");
            for id in self.cfg.module_tables.keys() {
                let _ = writeln!(out, "  [{id}]");
            }
        }
        out.push_str("live-reloaded on edit · inspect with `hrn config` or `cfg://`");
        out
    }
}
