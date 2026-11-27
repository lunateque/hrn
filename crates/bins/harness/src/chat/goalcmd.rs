//! `/goal` and `/modelpreset` for the chat backend: goal-mode state lives on the session,
//! presets rewrite the effective `[roles]` and rebuild the runtime (like `/login`).

use super::ChatBackend;
use crate::wiring::build_runtime_with_tools;
use hrn_kernel::{HrnError, Result};
use hrn_tui::Backend as _; // for `submit`
use std::sync::Arc;

impl ChatBackend {
    pub(super) fn run_goal(&mut self, arg: Option<&str>) -> Result<String> {
        let prompt = arg.map_or("", str::trim);
        let session_id = self
            .session
            .clone()
            .filter(|id| self.runtime.goal_of(id).ok().flatten().is_some() || !prompt.is_empty());
        if prompt.is_empty() || prompt == "status" || prompt == "get" {
            let Some(id) = session_id else {
                return Ok("goal mode is off · /goal <objective> starts it".to_owned());
            };
            return match self.runtime.goal_of(&id)? {
                Some(goal) => Ok(hrn_harness::goal::format_goal(&goal)),
                None => Ok("goal mode is off · /goal <objective> starts it".to_owned()),
            };
        }
        let id = match &session_id {
            Some(id) => id.clone(),
            None => self.session_for(prompt)?,
        };
        match prompt {
            "pause" | "resume" | "drop" => {
                let mut meta = self
                    .store
                    .get_session(&id)?
                    .ok_or_else(|| HrnError::Store(format!("unknown session `{id}`")))?;
                let op = match prompt {
                    "pause" => hrn_kernel::GoalOp::Pause,
                    "resume" => hrn_kernel::GoalOp::Resume,
                    _ => hrn_kernel::GoalOp::Drop,
                };
                let now = hrn_kernel::Clock::now_ms(&hrn_kernel::SystemClock);
                let (text, cleared) =
                    hrn_harness::goal::apply_op(&mut meta.goal, op, None, None, None, now)
                        .map_err(HrnError::Tool)?;
                if cleared {
                    meta.goal = None;
                }
                self.store.update_session(&meta)?;
                self.settings.goal = meta.goal;
                Ok(text)
            }
            _ => {
                let goal = self.runtime.start_goal(&id, prompt, None, None)?;
                self.settings.goal = Some(goal);
                // Begin working on it now, like `--goal`.
                self.submit(prompt)?;
                Ok(format!("goal mode started · {prompt}"))
            }
        }
    }

    pub(super) fn run_modelpreset(&mut self, arg: Option<&str>) -> Result<String> {
        let prompt = arg.map_or("", str::trim);
        let (verb, name) = prompt
            .split_once(char::is_whitespace)
            .map_or((prompt, ""), |(v, n)| (v, n.trim()));
        match verb {
            "" | "list" => {
                if self.cfg.llm.role_presets.is_empty() {
                    return Ok("no modelpresets · `/modelpreset save <name>` captures                                the current roles"
                        .to_owned());
                }
                let names: Vec<&String> = self.cfg.llm.role_presets.keys().collect();
                Ok(format!(
                    "modelpresets: {}",
                    names
                        .iter()
                        .map(|n| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            "save" => {
                let name = (!name.is_empty())
                    .then(|| name.to_owned())
                    .or_else(|| self.session.as_ref().map(|id| format!("session-{id}")))
                    .ok_or_else(|| {
                        HrnError::Config("usage: /modelpreset save <name>".to_owned())
                    })?;
                let report = crate::roles::save_preset(
                    &mut self.cfg,
                    &self.config_path,
                    &name,
                    Some(&self.settings.model),
                    self.settings.thinking,
                )?;
                self.llm = self.cfg.llm.clone();
                Ok(report)
            }
            "switch" => {
                let storage_path =
                    crate::roles::role_storage_path(&self.cfg, &self.config_path, &self.cwd);
                let mut staged = self.cfg.clone();
                let (default_alias, next_thinking, report) =
                    crate::roles::apply_preset_switch(&mut staged, &storage_path, name)?;
                let new_runtime = build_runtime_with_tools(
                    &staged,
                    Arc::clone(&self.sqlite),
                    &self.cwd,
                    &self.mcp_tools,
                    &self.background,
                )?;
                self.runtime = new_runtime;
                self.idle.sync_runtime(&self.runtime);
                self.llm = staged.llm.clone();
                self.model_names = crate::stats::names(&staged.llm);
                self.cfg = staged;
                self.settings.model = default_alias;
                self.settings.thinking = next_thinking;
                Ok(report)
            }
            "delete" => {
                let report = crate::roles::delete_preset(&mut self.cfg, &self.config_path, name)?;
                self.llm = self.cfg.llm.clone();
                Ok(report)
            }
            other => Err(HrnError::Config(format!(
                "usage: /modelpreset save|switch|delete|list [name] (got `{other}`)"
            ))),
        }
    }
}
