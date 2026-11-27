//! Wave 11c `ChatBackend` handlers: `/wt`, `/memory` (feature-gated), `/init`, `/trace`,
//! `/dump`, and `/loop --while|--until` condition checks via `hrn-edge-proc`.

use super::ChatBackend;
use super::worktree_cmd::relocate_to_worktree;
use crate::wiring::build_runtime_with_tools;
use hrn_edge_tools::StdWorkspaceFs;
use hrn_harness::approvals::{extract_targets, matching_restricted_folder_for_tool};
use hrn_harness::discover_custom_commands;
use hrn_kernel::{CancelFlag, HrnError, Result, ToolApprovalPolicy, TraceRecord};
use hrn_tui::{UiEvent, WorktreeOutcome};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Timeout for a single `/loop --while|--until` shell condition check.
const LOOP_CONDITION_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum output retained when running a loop condition check.
const LOOP_CONDITION_MAX_OUTPUT: usize = 64 * 1024;

impl ChatBackend {
    /// `/wt [branch]`: create a worktree carrying uncommitted changes, update `SessionMeta.cwd`,
    /// and rebuild the live runtime rooted at the new worktree.
    pub(super) fn relocate_worktree_live(
        &mut self,
        branch: Option<&str>,
    ) -> Result<WorktreeOutcome> {
        let old_cwd = self.cwd.clone();
        let req_branch = branch.map(str::to_owned);
        let sid = self.session.clone();
        let outcome = run_async_safe(&self.handle, async move {
            relocate_to_worktree(&old_cwd, req_branch.as_deref(), sid.as_deref()).await
        })?;
        if let Some(session_id) = self.session.as_deref() {
            self.runtime.relocate_session(
                session_id,
                &outcome.new_cwd,
                Some(outcome.branch.clone()),
            )?;
        }
        let new_runtime = build_runtime_with_tools(
            &self.cfg,
            Arc::clone(&self.sqlite),
            &outcome.new_cwd,
            &self.mcp_tools,
            &self.background,
        )?;
        self.runtime = new_runtime;
        self.idle.sync_runtime(&self.runtime);
        self.cwd.clone_from(&outcome.new_cwd);
        self.custom_commands =
            discover_custom_commands(&StdWorkspaceFs, &self.cwd).unwrap_or_default();
        Ok(outcome)
    }

    /// `/memory [view|stats|search|enqueue|clear]`, routed through the optional `memory` module.
    pub(super) fn memory_command_live(&mut self, arg: Option<&str>) -> Result<String> {
        let db_path = crate::app_data_dir()?.join("memory.db");
        let fallback = self.latest_session_summary_note();
        crate::modules::run_memory_command(
            &db_path,
            &self.cwd,
            self.active_session_id().as_deref(),
            arg,
            fallback.as_deref(),
        )
    }

    /// `/agy [status|[flags] <prompt>]`, routed through the optional `agy` module.
    pub(super) fn agy_command_live(&mut self, arg: Option<&str>) -> Result<String> {
        crate::modules::run_agy_command(&self.handle, self.tx.clone(), self.cwd.clone(), arg)
    }

    fn latest_session_summary_note(&self) -> Option<String> {
        let nodes = self.session_branch().ok().flatten()?;
        nodes.iter().rev().find_map(|n| match &n.payload {
            hrn_kernel::DagPayload::Message { msg, .. }
                if matches!(
                    msg.role,
                    hrn_kernel::Role::User | hrn_kernel::Role::Assistant
                ) && !msg.content.trim().is_empty() =>
            {
                Some(msg.content.trim().to_owned())
            }
            _ => None,
        })
    }

    /// Bundled `/init` prompt text (`crates/harness/prompts/init.md`).
    pub(super) fn init_prompt_live() -> String {
        hrn_harness::INIT_PROMPT.to_owned()
    }

    /// Load structured LLM and tool trace spans for the active session (`/trace`).
    pub(super) fn traces_live(&self) -> Result<Vec<TraceRecord>> {
        let Some(id) = self.active_session_id() else {
            return Ok(Vec::new());
        };
        self.store.list_traces(&id)
    }

    /// Render the active session branch as a plain-text transcript (`/dump`).
    pub(super) fn dump_transcript_live(&self) -> Result<String> {
        let id = self
            .active_session_id()
            .ok_or_else(|| HrnError::Config("no active session to dump yet".to_owned()))?;
        crate::export::render_plain_text(self.store.as_ref(), &id)
    }

    /// Evaluate a `/loop --while|--until` shell condition asynchronously via `hrn-edge-proc`.
    pub(super) fn start_loop_condition_live(&mut self, check_id: u64, command: &str) -> Result {
        validate_loop_condition_policy(&self.cfg.approval_policy, &self.cwd, command)?;
        self.cancel_loop_condition_live();
        let cancel = CancelFlag::new();
        self.loop_cancel = Some(cancel.clone());
        let spec = hrn_edge_proc::shell_run_spec(
            command,
            self.cwd.clone(),
            LOOP_CONDITION_TIMEOUT,
            LOOP_CONDITION_MAX_OUTPUT,
        )?;
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            match hrn_edge_proc::run_streaming(&spec, &cancel, |_| {}).await {
                Ok(out) => {
                    tx.send(UiEvent::LoopCondition {
                        check_id,
                        exit_code: out.exit_code,
                        timed_out: out.timed_out,
                        error: None,
                    });
                }
                Err(HrnError::Cancelled) => {}
                Err(err) => {
                    tx.send(UiEvent::LoopCondition {
                        check_id,
                        exit_code: None,
                        timed_out: false,
                        error: Some(err.to_string()),
                    });
                }
            }
        });
        Ok(())
    }

    /// Cancel any running `/loop` condition command.
    pub(super) fn cancel_loop_condition_live(&mut self) {
        if let Some(cancel) = self.loop_cancel.take() {
            cancel.cancel();
        }
    }
}

/// Enforce `[tools.approval] bash = "deny"` and `[tools] restricted_folders` before running a
/// `/loop --while|--until` shell condition.
fn validate_loop_condition_policy(
    policy: &hrn_harness::UserApprovalPolicy,
    cwd: &std::path::Path,
    command: &str,
) -> Result {
    if policy.policy_for("bash") == Some(ToolApprovalPolicy::Deny) {
        return Err(HrnError::Tool(
            "loop condition refused: `bash` is denied by [tools.approval]".to_owned(),
        ));
    }
    if !policy.restricted_folders.is_empty() {
        let targets = extract_targets("bash", &serde_json::json!({ "command": command }));
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        if let Some(folder) = matching_restricted_folder_for_tool(
            "bash",
            &policy.restricted_folders,
            &targets,
            cwd,
            home.as_deref(),
            Some(&StdWorkspaceFs),
        ) {
            return Err(HrnError::Tool(format!(
                "loop condition refused: touches restricted folder `{folder}`"
            )));
        }
    }
    Ok(())
}

/// Run an async future on `handle` safely from either a blocking thread or a tokio worker.
fn run_async_safe<F, T>(handle: &tokio::runtime::Handle, fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send,
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| handle.block_on(fut))
                .join()
                .map_err(|_| HrnError::Internal("worktree helper thread panicked".into()))?
        })
    } else {
        handle.block_on(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_condition_respects_bash_deny_policy() {
        let mut policy = hrn_harness::UserApprovalPolicy::default();
        policy
            .tool_policies
            .insert("bash".to_owned(), ToolApprovalPolicy::Deny);
        let err = validate_loop_condition_policy(&policy, std::path::Path::new("."), "true")
            .expect_err("bash = deny must block loop conditions");
        assert!(err.to_string().contains("denied"));
    }

    #[tokio::test]
    async fn edge_proc_reports_zero_and_nonzero_exit_codes() {
        let cwd = std::env::temp_dir();
        let ok_spec =
            hrn_edge_proc::shell_run_spec("exit 0", cwd.clone(), Duration::from_secs(10), 8192)
                .unwrap();
        let ok = hrn_edge_proc::run_streaming(&ok_spec, &CancelFlag::new(), |_| {})
            .await
            .unwrap();
        assert_eq!(ok.exit_code, Some(0));

        let fail_spec =
            hrn_edge_proc::shell_run_spec("exit 3", cwd, Duration::from_secs(10), 8192).unwrap();
        let fail = hrn_edge_proc::run_streaming(&fail_spec, &CancelFlag::new(), |_| {})
            .await
            .unwrap();
        assert_eq!(fail.exit_code, Some(3));
    }
}
