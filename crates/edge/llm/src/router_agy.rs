//! `LlmRouter` integration for `ProviderKind::AgyAcp` (lazy warm runtime cache, thinking-loop
//! guard, recursion-safe Smart Handoff summarizer, and failover classification).

use crate::agy_acp::{AgyAcpRuntime, AgyProviderConfig, HandoffSummarizer, RuntimeOptions};
use crate::config::ProviderEntry;
use crate::router::{GuardSink, LlmRouter};
use crate::router_pool::PoolOutcome;
use hrn_kernel::{
    BoxFuture, CancelFlag, ChunkSink, HrnError, LlmChunk, LlmProvider, LlmRequest, Msg, Result,
    ScopedFuture, ThinkingLevel,
};
use std::sync::Arc;

tokio::task_local! {
    /// Guards against infinite recursion when `handoff_model` resolves back to an `agy_acp` preset.
    static IN_AGY_HANDOFF: bool;
}

struct NullSink;

impl ChunkSink for NullSink {
    fn on_chunk(&self, _chunk: LlmChunk) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// Bridges `AgyAcpRuntime`'s Smart Handoff summarization onto the router's `handoff_model`
/// (default `"smol"`), with a task-local guard so a misconfigured `handoff_model` pointing back
/// to an `agy_acp` preset cannot recurse infinitely.
struct RouterHandoffSummarizer<'a> {
    router: &'a LlmRouter,
}

impl HandoffSummarizer for RouterHandoffSummarizer<'_> {
    fn summarize<'a>(
        &'a self,
        model: &'a str,
        messages: &'a [Msg],
        cancel: &'a CancelFlag,
    ) -> ScopedFuture<'a, Result<String>> {
        Box::pin(async move {
            let req = LlmRequest {
                model: Some(model.to_owned()),
                messages: messages.to_vec(),
                tools: Vec::new(),
                thinking: ThinkingLevel::Off,
                thinking_budgets: None,
                temperature: Some(0.0),
                max_tokens: Some(1500),
                session_id: None,
                service_tier: None,
            };
            let sink = NullSink;
            let resp = IN_AGY_HANDOFF
                .scope(true, self.router.stream_chat(&req, cancel, &sink))
                .await?;
            Ok(resp.content)
        })
    }
}

pub(crate) type AgyRuntimeEntry = (AgyProviderConfig, Arc<AgyAcpRuntime>);

impl LlmRouter {
    /// Look up or lazily create the warm [`AgyAcpRuntime`] for `provider_name`.
    ///
    /// Preserved across [`LlmRouter::with_config`] reloads whenever `prov.agy` is unchanged.
    fn agy_runtime(&self, provider_name: &str, prov: &ProviderEntry) -> Arc<AgyAcpRuntime> {
        let mut map = self
            .agy_runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_cfg, rt)) = map.get(provider_name)
            && cached_cfg == &prov.agy
        {
            return Arc::clone(rt);
        }
        let rt = Arc::new(AgyAcpRuntime::new(
            prov.agy.clone(),
            RuntimeOptions::default(),
        ));
        map.insert(
            provider_name.to_owned(),
            (prov.agy.clone(), Arc::clone(&rt)),
        );
        rt
    }

    /// Execute one `agy_acp` candidate turn and map its outcome onto [`PoolOutcome`].
    #[expect(
        clippy::too_many_arguments,
        reason = "candidate attempt passes alias, provider name, provider entry, model id, request, cancel, sink"
    )]
    pub(crate) async fn attempt_agy_candidate(
        &self,
        alias: &str,
        provider_name: &str,
        prov: &ProviderEntry,
        effective_id: &str,
        effective_req: &LlmRequest,
        cancel: &CancelFlag,
        sink: &dyn ChunkSink,
    ) -> PoolOutcome {
        let runtime = self.agy_runtime(provider_name, prov);
        let attempt_cancel = cancel.child();
        let guard_sink = GuardSink::new(sink, attempt_cancel.clone());
        let in_handoff = IN_AGY_HANDOFF.try_with(|v| *v).unwrap_or(false);
        let summarizer = RouterHandoffSummarizer { router: self };
        let summarizer_opt: Option<&dyn HandoffSummarizer> =
            if in_handoff { None } else { Some(&summarizer) };

        let res = runtime
            .stream_chat(
                effective_id,
                effective_req,
                &attempt_cancel,
                &guard_sink,
                summarizer_opt,
            )
            .await;

        if attempt_cancel.is_cancelled() && !cancel.is_cancelled() {
            return PoolOutcome::Exhausted(HrnError::Provider {
                message: format!("thinking loop detected on {alias}"),
                retryable: true,
                retry_after_secs: None,
            });
        }

        match res {
            Ok(resp) => PoolOutcome::Succeeded(resp),
            Err(err)
                if matches!(err, HrnError::Cancelled)
                    || crate::overflow::is_context_overflow(&err)
                    || guard_sink.emitted_any() =>
            {
                PoolOutcome::Fatal(err)
            }
            Err(err @ HrnError::Auth(_)) => PoolOutcome::Exhausted(err),
            Err(err) if err.is_retryable() => PoolOutcome::Exhausted(err),
            Err(err) => PoolOutcome::Fatal(err),
        }
    }
}
