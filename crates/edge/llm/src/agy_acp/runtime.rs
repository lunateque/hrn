//! Stateful `AgyAcpRuntime` bridging stateless `LlmProvider::stream_chat` onto a warm `agy acp`
//! child process, loopback Streamable HTTP MCP bridge, ephemeral custom agent, and DAG session map.

use super::agent_profile::{EphemeralAgentProfile, cleanup_stale_agent_dirs};
use super::binary::{discover_agy_binary, merged_no_proxy_env};
use super::bridge::{McpBridge, McpBridgeOptions};
use super::client::{AcpClient, AcpSpawnSpec};
use super::config::{AgyProviderConfig, AgyToolMode};
use super::handoff::{
    BootstrapStrategy, HandoffSummarizer, prepare_bootstrap_prompt, render_user_delta,
    split_history_and_new_turn,
};
use super::runtime_turn::{
    ActiveTurn, CONTROL_RPC_TIMEOUT, SESSION_INIT_RPC_TIMEOUT, TurnStep, abort_turn,
    build_prompt_blocks, drive_turn, match_trailing_tool_results, race_cancel, should_reset_client,
    sync_model_and_effort,
};
use super::session_map::{AgySessionEntry, RouteDecision, SessionMap, hash_messages_prefix};
use super::wire::{detect_quota_exhaustion, now_epoch_secs};
use hrn_kernel::{
    CancelFlag, ChunkSink, FinishReason, HrnError, LlmRequest, LlmResponse, Msg, Result, ToolSpec,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

/// Default batching window for collecting parallel `tools/call` requests before returning to `hrn`.
pub const DEFAULT_TOOL_BATCH_WINDOW: Duration = Duration::from_millis(30);

/// Telemetry marker identifying which DAG routing path handled the most recent `stream_chat` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    /// Case 1: resolved pending bridged `tools/call`s inside an already-open ACP prompt turn.
    ResumeToolCall,
    /// Case 2: linear continuation on the active ACP session (only new user delta sent).
    Linear,
    /// Case 3: switched back to an earlier DAG branch via `session/resume`.
    ResumeBranch,
    /// Case 4a: native ACP `session/fork` (when advertised by future `agy` releases).
    NativeFork,
    /// Case 4b: new ACP session with direct `<restored_branch_context>` transcript (or fresh session).
    BootstrapDirect,
    /// Case 4c: new ACP session bootstrapped with a `handoff_model` Smart Handoff checkpoint.
    SmartHandoff,
}

/// Optional test/runtime overrides for constructing [`AgyAcpRuntime`].
#[derive(Clone, Debug, Default)]
pub struct RuntimeOptions {
    /// Working directory passed to `agy acp` and `session/new`.
    pub cwd: Option<PathBuf>,
    /// Custom directory for ephemeral `AGENT.md` profiles (defaults to `~/.gemini/config/agents`).
    pub agents_root: Option<PathBuf>,
    /// MCP bridge tuning (progress interval, tool timeout).
    pub bridge: McpBridgeOptions,
    /// Parallel tool-call coalescing window (default 30 ms).
    pub tool_batch_window: Option<Duration>,
    /// Optional explicit ACP process spec override (used by mock-ACP tests).
    pub spawn_override: Option<AcpSpawnSpec>,
}

struct RuntimeInner {
    client: Option<Arc<AcpClient>>,
    bridge: Option<Arc<McpBridge>>,
    agent_profile: Option<EphemeralAgentProfile>,
    session_map: SessionMap,
    active_turns: HashMap<String, ActiveTurn>,
    last_route: Option<RouteKind>,
}

/// Long-lived Antigravity ACP runtime (attach as `Arc<AgyAcpRuntime>` to `LlmRouter` so it
/// survives `LlmRouter::with_config` reloads without killing warm sessions).
pub struct AgyAcpRuntime {
    config: AgyProviderConfig,
    cwd: PathBuf,
    agents_root: Option<PathBuf>,
    bridge_options: McpBridgeOptions,
    tool_batch_window: Duration,
    spawn_override: Option<AcpSpawnSpec>,
    inner: AsyncMutex<RuntimeInner>,
}

impl AgyAcpRuntime {
    /// Construct a lazy runtime (spawns `agy acp` and the loopback MCP bridge on first call).
    pub fn new(config: AgyProviderConfig, options: RuntimeOptions) -> Self {
        let cwd = options.cwd.unwrap_or_else(default_cwd);
        Self {
            config,
            cwd,
            agents_root: options.agents_root,
            bridge_options: options.bridge,
            tool_batch_window: options
                .tool_batch_window
                .unwrap_or(DEFAULT_TOOL_BATCH_WINDOW),
            spawn_override: options.spawn_override,
            inner: AsyncMutex::new(RuntimeInner {
                client: None,
                bridge: None,
                agent_profile: None,
                session_map: SessionMap::default(),
                active_turns: HashMap::new(),
                last_route: None,
            }),
        }
    }

    /// Return the routing path chosen on the most recent `stream_chat` call.
    pub async fn last_route(&self) -> Option<RouteKind> {
        self.inner.lock().await.last_route
    }

    /// Return the loopback MCP bridge URL if started (mainly for diagnostics and tests).
    pub async fn bridge_url(&self) -> Option<String> {
        self.inner.lock().await.bridge.as_ref().map(|b| b.url())
    }

    /// Execute one stateless [`LlmRequest`] against `agy acp`, streaming chunks to `sink`.
    pub async fn stream_chat(
        &self,
        model_id: &str,
        req: &LlmRequest,
        cancel: &CancelFlag,
        sink: &dyn ChunkSink,
        summarizer: Option<&dyn HandoffSummarizer>,
    ) -> Result<LlmResponse> {
        if cancel.is_cancelled() {
            return Err(HrnError::Cancelled);
        }
        let hrn_sid = req
            .session_id
            .clone()
            .unwrap_or_else(|| "default".to_owned());

        let mut guard = self.inner.lock().await;
        race_cancel(cancel, self.ensure_ready(&mut guard, &req.tools)).await?;

        let client =
            guard.client.as_ref().map(Arc::clone).ok_or_else(|| {
                HrnError::Internal("ACP client unavailable after init".to_owned())
            })?;
        let bridge = guard.bridge.as_ref().map(Arc::clone);
        if let Some(b) = &bridge {
            b.set_tools(req.tools.clone());
        }

        let mut turn = match self
            .acquire_active_turn(
                &mut guard,
                &client,
                bridge.as_deref(),
                &hrn_sid,
                model_id,
                req,
                cancel,
                summarizer,
            )
            .await
        {
            Ok(t) => t,
            Err(err) => {
                if should_reset_client(&client, &err) {
                    guard.client = None;
                }
                return Err(err);
            }
        };

        let drive_res = drive_turn(
            &self.config,
            self.tool_batch_window,
            &client,
            bridge.as_deref(),
            &mut turn,
            cancel,
            sink,
        )
        .await;

        finalize_turn_step(
            &mut guard,
            &client,
            bridge.as_deref(),
            hrn_sid,
            model_id,
            req,
            turn,
            drive_res,
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "acquiring or starting a turn needs runtime state, client, bridge, session id, model, request, cancel, summarizer"
    )]
    async fn acquire_active_turn(
        &self,
        guard: &mut RuntimeInner,
        client: &AcpClient,
        bridge: Option<&McpBridge>,
        hrn_sid: &str,
        model_id: &str,
        req: &LlmRequest,
        cancel: &CancelFlag,
        summarizer: Option<&dyn HandoffSummarizer>,
    ) -> Result<ActiveTurn> {
        if let Some(turn) = guard.active_turns.remove(hrn_sid) {
            if let Some(resolved) =
                match_trailing_tool_results(&req.messages, &turn.expected_call_ids)
                && let Some(b) = bridge
            {
                for (call_id, result) in resolved {
                    let _ = b.resolve_call(&call_id, result);
                }
                guard.last_route = Some(RouteKind::ResumeToolCall);
                return Ok(turn);
            }
            abort_turn(client, bridge, &turn.agy_session_id).await;
        }

        let (agy_sid, prompt_text, route_kind) = self
            .prepare_turn(
                guard, client, bridge, hrn_sid, model_id, req, cancel, summarizer,
            )
            .await?;
        guard.last_route = Some(route_kind);

        let update_rx = client.subscribe_updates(&agy_sid);
        let tool_rx = match bridge {
            Some(b) => b.attach_call_sink(),
            None => mpsc::unbounded_channel().1,
        };
        let prompt_blocks =
            build_prompt_blocks(&prompt_text, req, client.capabilities().supports_image);
        let prompt_rx = client.start_prompt(&agy_sid, prompt_blocks).await?;

        Ok(ActiveTurn {
            agy_session_id: agy_sid,
            model_id: model_id.to_owned(),
            effort: req.thinking,
            expected_call_ids: Vec::new(),
            prompt_rx,
            update_rx,
            tool_rx,
            turn_deadline: tokio::time::Instant::now() + self.config.turn_timeout(),
        })
    }

    async fn ensure_ready(&self, inner: &mut RuntimeInner, initial_tools: &[ToolSpec]) -> Result {
        if self.config.tool_mode == AgyToolMode::Hrn && inner.bridge.is_none() {
            let bridge = Arc::new(McpBridge::start(self.bridge_options).await?);
            bridge.set_tools(initial_tools.to_vec());
            let agents_root = match &self.agents_root {
                Some(dir) => dir.clone(),
                None => EphemeralAgentProfile::default_agents_root()?,
            };
            let _ = cleanup_stale_agent_dirs(&agents_root);
            let eager_names: Vec<String> = initial_tools.iter().map(|t| t.name.clone()).collect();
            let profile = EphemeralAgentProfile::create_in(
                &agents_root,
                &bridge.url(),
                bridge.token(),
                &eager_names,
            )?;
            inner.bridge = Some(bridge);
            inner.agent_profile = Some(profile);
        }

        let need_spawn = inner.client.as_ref().is_none_or(|c| !c.is_alive());
        if need_spawn {
            let spec =
                self.build_spawn_spec(inner.bridge.as_deref(), inner.agent_profile.as_ref())?;
            let client =
                AcpClient::spawn_and_initialize(spec, self.config.tool_mode, CONTROL_RPC_TIMEOUT)
                    .await?;
            inner.client = Some(Arc::new(client));
        }
        Ok(())
    }

    fn build_spawn_spec(
        &self,
        bridge: Option<&McpBridge>,
        profile: Option<&EphemeralAgentProfile>,
    ) -> Result<AcpSpawnSpec> {
        if let Some(mut spec) = self.spawn_override.clone() {
            for (k, v) in merged_no_proxy_env() {
                spec.env.entry(k).or_insert(v);
            }
            if let Some(b) = bridge {
                spec.env.insert("HRN_BRIDGE_URL".to_owned(), b.url());
                spec.env
                    .insert("HRN_BRIDGE_TOKEN".to_owned(), b.token().to_owned());
            }
            return Ok(spec);
        }

        let binary = discover_agy_binary(self.config.binary.as_deref()).ok_or_else(|| {
            HrnError::Config(
                "`agy` CLI binary not found; install Antigravity CLI (>= 1.5.0) or set `binary`"
                    .to_owned(),
            )
        })?;
        let mut args = vec!["acp".to_owned(), "--stdio".to_owned()];
        if let Some(prof) = profile {
            args.push("--agent".to_owned());
            args.push(prof.name().to_owned());
        }
        for dir in &self.config.add_dirs {
            if !dir.trim().is_empty() {
                args.push("--add-dir".to_owned());
                args.push(dir.clone());
            }
        }
        Ok(AcpSpawnSpec {
            binary,
            args,
            cwd: self.cwd.clone(),
            env: merged_no_proxy_env(),
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "turn routing requires runtime guard, client, bridge, session id, model, request, cancel, summarizer"
    )]
    async fn prepare_turn(
        &self,
        inner: &mut RuntimeInner,
        client: &AcpClient,
        bridge: Option<&McpBridge>,
        hrn_sid: &str,
        model_id: &str,
        req: &LlmRequest,
        cancel: &CancelFlag,
        summarizer: Option<&dyn HandoffSummarizer>,
    ) -> Result<(String, String, RouteKind)> {
        let mcp_servers: Vec<Value> = bridge
            .map(|b| vec![b.acp_mcp_server_entry()])
            .unwrap_or_default();

        match inner.session_map.route(hrn_sid, &req.messages) {
            RouteDecision::ContinueLinear { entry, delta_start } => {
                race_cancel(
                    cancel,
                    sync_model_and_effort(
                        client,
                        &entry.agy_session_id,
                        Some(&entry),
                        model_id,
                        req.thinking,
                    ),
                )
                .await?;
                let delta = req.messages.get(delta_start..).unwrap_or_default();
                let text = render_user_delta(delta);
                Ok((entry.agy_session_id, text, RouteKind::Linear))
            }
            RouteDecision::ResumeBranch { entry, delta_start } => {
                if client.capabilities().supports_resume {
                    match race_cancel(
                        cancel,
                        client.session_resume(
                            &entry.agy_session_id,
                            &self.cwd,
                            &mcp_servers,
                            SESSION_INIT_RPC_TIMEOUT,
                        ),
                    )
                    .await
                    {
                        Ok(_) => {
                            race_cancel(
                                cancel,
                                sync_model_and_effort(
                                    client,
                                    &entry.agy_session_id,
                                    Some(&entry),
                                    model_id,
                                    req.thinking,
                                ),
                            )
                            .await?;
                            let delta = req.messages.get(delta_start..).unwrap_or_default();
                            let text = render_user_delta(delta);
                            return Ok((entry.agy_session_id, text, RouteKind::ResumeBranch));
                        }
                        Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
                        Err(_) => {}
                    }
                }
                inner
                    .session_map
                    .invalidate_agy_session(&entry.agy_session_id);
                self.bootstrap_new_session(
                    client,
                    &mcp_servers,
                    None,
                    model_id,
                    req,
                    cancel,
                    summarizer,
                )
                .await
            }
            RouteDecision::ForkOrBootstrap { fork_from } => {
                self.bootstrap_new_session(
                    client,
                    &mcp_servers,
                    fork_from.as_ref(),
                    model_id,
                    req,
                    cancel,
                    summarizer,
                )
                .await
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "bootstrap needs client, mcp servers, fork source, model, request, cancel, summarizer"
    )]
    async fn bootstrap_new_session(
        &self,
        client: &AcpClient,
        mcp_servers: &[Value],
        fork_from: Option<&AgySessionEntry>,
        model_id: &str,
        req: &LlmRequest,
        cancel: &CancelFlag,
        summarizer: Option<&dyn HandoffSummarizer>,
    ) -> Result<(String, String, RouteKind)> {
        if client.capabilities().supports_fork
            && let Some(src) = fork_from
        {
            match race_cancel(
                cancel,
                client.session_fork(&src.agy_session_id, &self.cwd, CONTROL_RPC_TIMEOUT),
            )
            .await
            {
                Ok(forked_sid) => {
                    race_cancel(
                        cancel,
                        sync_model_and_effort(
                            client,
                            &forked_sid,
                            Some(src),
                            model_id,
                            req.thinking,
                        ),
                    )
                    .await?;
                    let (_, tail) = split_history_and_new_turn(&req.messages);
                    let text = render_user_delta(tail);
                    return Ok((forked_sid, text, RouteKind::NativeFork));
                }
                Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
                Err(_) => {}
            }
        }

        let created = race_cancel(
            cancel,
            client.session_new(&self.cwd, mcp_servers, SESSION_INIT_RPC_TIMEOUT),
        )
        .await?;
        race_cancel(
            cancel,
            sync_model_and_effort(client, &created.session_id, None, model_id, req.thinking),
        )
        .await?;
        let (prompt_text, strategy) = prepare_bootstrap_prompt(
            &req.messages,
            self.config.handoff_threshold_tokens,
            &self.config.handoff_model,
            summarizer,
            cancel,
        )
        .await?;
        let route_kind = match strategy {
            BootstrapStrategy::SmartHandoff => RouteKind::SmartHandoff,
            BootstrapStrategy::Fresh | BootstrapStrategy::DirectTranscript => {
                RouteKind::BootstrapDirect
            }
        };
        Ok((created.session_id, prompt_text, route_kind))
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "finalizing a turn updates runtime maps, client subscriptions, and bridge state"
)]
async fn finalize_turn_step(
    guard: &mut RuntimeInner,
    client: &AcpClient,
    bridge: Option<&McpBridge>,
    hrn_sid: String,
    model_id: &str,
    req: &LlmRequest,
    mut turn: ActiveTurn,
    drive_res: Result<TurnStep>,
) -> Result<LlmResponse> {
    match drive_res {
        Ok(TurnStep::NeedTools {
            content,
            reasoning,
            tool_calls,
        }) => {
            turn.expected_call_ids = tool_calls.iter().map(|tc| tc.id.clone()).collect();
            guard.active_turns.insert(hrn_sid, turn);
            Ok(LlmResponse {
                model_used: model_id.to_owned(),
                fallback_from: None,
                content,
                reasoning,
                thinking_blocks: Vec::new(),
                tool_calls,
                usage: None,
                finish_reason: FinishReason::ToolCalls,
            })
        }
        Ok(TurnStep::Completed {
            content,
            reasoning,
            result,
        }) => {
            client.unsubscribe_updates(&turn.agy_session_id);
            if let Some(b) = bridge {
                b.detach_call_sink();
            }
            if let Some(quota_err) = detect_quota_exhaustion(&result, &content, now_epoch_secs()) {
                return Err(quota_err);
            }
            let mut synced_msgs = req.messages.clone();
            synced_msgs.push(Msg::assistant(content.clone()));
            let prefix_hash = hash_messages_prefix(&synced_msgs);
            guard.session_map.record(
                &hrn_sid,
                AgySessionEntry {
                    agy_session_id: turn.agy_session_id,
                    synced_prefix_hash: prefix_hash,
                    synced_msg_count: synced_msgs.len(),
                    model_id: turn.model_id,
                    effort: turn.effort,
                    last_used_at: Instant::now(),
                },
            );
            Ok(LlmResponse {
                model_used: model_id.to_owned(),
                fallback_from: None,
                content,
                reasoning,
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                usage: result.usage,
                finish_reason: FinishReason::from_wire(&result.stop_reason),
            })
        }
        Err(err) => {
            abort_turn(client, bridge, &turn.agy_session_id).await;
            if !client.is_alive() {
                guard.client = None;
            }
            Err(err)
        }
    }
}

/// Expose the configured working directory for callers needing to pass `cwd` to helpers.
pub fn default_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf())
}
