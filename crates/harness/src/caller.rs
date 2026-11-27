//! Running one tool call on behalf of another tool: the `bash` tool commands (`read notes.txt`
//! typed in the agent's shell) are executed here, through the same path as a call the model made,
//! so permission mode, approval prompt and hooks gate them identically.

use crate::approvals::{GrantCache, UserApprovalPolicy};
use crate::batch::{BatchEnv, run_call};
use crate::control::SessionControl;
use crate::registry::ToolRegistry;
use crate::turn::StepEnv;
use hrn_kernel::serde_json::Value;
use hrn_kernel::{
    ApprovalGate, AskHandler, BoxFuture, CancelFlag, EventSink, HarnessHook, Inbox, PermissionMode,
    Result, SessionId, SessionStore, ToolCaller, ToolOutput, ToolSpec, UrlRouter, WorkspaceFs,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Turn-level state a nested call is executed with.
#[derive(Clone)]
pub struct NestedCaller {
    registry: ToolRegistry,
    hooks: Vec<Arc<dyn HarnessHook>>,
    session_id: SessionId,
    turn_id: String,
    permission_mode: PermissionMode,
    control: SessionControl,
    events: Arc<dyn EventSink>,
    approval: Option<Arc<dyn ApprovalGate>>,
    ask: Option<Arc<dyn AskHandler>>,
    urls: Arc<UrlRouter>,
    root_session_id: SessionId,
    inbox: Arc<Inbox>,
    todos: Arc<hrn_kernel::todo::TodoBoards>,
    is_subagent: bool,
    approval_policy: UserApprovalPolicy,
    grants: Arc<GrantCache>,
    fs: Arc<dyn WorkspaceFs>,
    store: Option<Arc<dyn SessionStore>>,
}

/// Ids of nested calls, distinct from the model's call ids.
static NESTED: AtomicU64 = AtomicU64::new(0);

impl NestedCaller {
    /// Capture one step's execution environment, for tool commands of that step. The step's own
    /// `caller` is ignored: tool commands do not nest further.
    pub(crate) fn new(
        step: &StepEnv<'_>,
        env: &BatchEnv<'_>,
        fs: Arc<dyn WorkspaceFs>,
        store: Option<Arc<dyn SessionStore>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry: env.registry.clone(),
            hooks: step.hooks.to_vec(),
            session_id: env.session_id.clone(),
            turn_id: env.turn_id.to_owned(),
            permission_mode: env.permission_mode,
            control: step.control.clone(),
            events: Arc::clone(env.events),
            approval: env.approval.cloned(),
            ask: env.ask.cloned(),
            urls: Arc::clone(env.urls),
            root_session_id: env.root_session_id.to_owned(),
            inbox: Arc::clone(env.inbox),
            todos: Arc::clone(&env.todos),
            is_subagent: env.is_subagent,
            approval_policy: env.approval_policy.clone(),
            grants: Arc::clone(env.grants),
            fs,
            store,
        })
    }
}

impl ToolCaller for NestedCaller {
    fn call(
        &self,
        name: &str,
        args: Value,
        cwd: PathBuf,
        cancel: CancelFlag,
    ) -> BoxFuture<Result<ToolOutput>> {
        let owned = name.to_owned();
        let this = self.clone();
        Box::pin(async move {
            let n = NESTED.fetch_add(1, Ordering::Relaxed);
            let call = hrn_kernel::ToolCall {
                id: format!("cmd-{n}"),
                name: owned,
                arguments: args.to_string(),
                signature: None,
            };
            let cwd_ref: &Path = &cwd;
            let env = BatchEnv {
                registry: &this.registry,
                hooks: &this.hooks,
                session_id: &this.session_id,
                turn_id: &this.turn_id,
                cwd: cwd_ref,
                permission_mode: this.permission_mode,
                control: &this.control,
                cancel: &cancel,
                events: &this.events,
                approval: this.approval.as_ref(),
                ask: this.ask.as_ref(),
                urls: &this.urls,
                root_session_id: &this.root_session_id,
                inbox: &this.inbox,
                caller: None,
                todos: Arc::clone(&this.todos),
                yield_data: this.control.yield_data(),
                is_subagent: this.is_subagent,
                approval_policy: &this.approval_policy,
                grants: &this.grants,
                fs: this.fs.as_ref(),
                trace: None,
                store: this.store.as_deref(),
            };
            run_call(&env, &call).await
        })
    }

    fn tool_names(&self) -> Vec<String> {
        self.registry.specs().into_iter().map(|s| s.name).collect()
    }

    fn spec(&self, name: &str) -> Option<ToolSpec> {
        self.registry.get(name).map(|tool| tool.spec())
    }
}
