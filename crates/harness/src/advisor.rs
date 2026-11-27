//! Passive post-turn advisor: watches a completed turn and injects concise, high-signal review
//! notes into the next turn when it spots bugs, missed edge cases, or unfulfilled requirements.

use crate::control::SessionControl;
use crate::runtime::{AgentRuntime, TurnIo, emit};
use hrn_kernel::{
    AgentEvent, BoxFuture, ChunkSink, DagNode, DagPayload, LlmChunk, LlmRequest, Msg, Role,
    SessionMeta, ThinkingLevel,
};

const ADVISOR_SYSTEM_PROMPT: &str = "\
You are a concise senior-engineer passive reviewer watching a coding agent's completed turn.\n\
Inspect the user's request, the tool actions taken in this turn, and the agent's final reply.\n\
Look only for high-signal issues: real bugs, missed edge cases, unverified assumptions, security \
risks, or parts of the user's request that were missed.\n\
If everything looks solid or nothing important needs flagging, reply with EXACTLY `OK` and nothing else.\n\
Otherwise, reply with 1-4 short, concrete bullet points (under 80 words total) of actionable advice \
for the next turn. No preamble, no praise, no repetition of what already succeeded.";

/// Maximum characters of turn transcript sent to the advisor model.
const MAX_DIGEST_CHARS: usize = 12_000;
/// Minimum assistant response length to trigger review when no tools ran in the turn.
const MIN_TEXT_ONLY_CHARS: usize = 80;

/// Configuration for the passive post-turn advisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvisorConfig {
    /// Whether the advisor runs after completed turns by default.
    pub enabled: bool,
    /// Optional model alias or role (default: `"reviewer"` role if configured, else session model).
    pub model: Option<String>,
    /// Thinking effort for the advisor call (default: `Low` for fast reviews).
    pub thinking: ThinkingLevel,
    /// Output token budget for each advisor note (default: `384`).
    pub max_tokens: u32,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            thinking: ThinkingLevel::Low,
            max_tokens: 384,
        }
    }
}

/// Live advisor setting resolved for a single turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedAdvisor {
    pub(crate) enabled: bool,
    pub(crate) model: Option<String>,
}

struct SilentSink;

impl ChunkSink for SilentSink {
    fn on_chunk(&self, _chunk: LlmChunk) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

impl AgentRuntime {
    /// Clone this runtime with an updated default [`AdvisorConfig`].
    #[must_use]
    pub fn with_advisor(&self, advisor: AdvisorConfig) -> Self {
        let mut config = self.config.clone();
        config.advisor = advisor;
        Self {
            config,
            ..self.clone()
        }
    }

    /// Clone this runtime with the passive advisor disabled (used for subagent children).
    #[must_use]
    pub fn without_advisor(&self) -> Self {
        let mut config = self.config.clone();
        config.advisor.enabled = false;
        Self {
            config,
            ..self.clone()
        }
    }

    pub(crate) fn resolve_advisor(&self, control: &SessionControl) -> ResolvedAdvisor {
        match control.advisor_override() {
            Some((enabled, model_override)) => ResolvedAdvisor {
                enabled,
                model: model_override.or_else(|| self.config.advisor.model.clone()),
            },
            None => ResolvedAdvisor {
                enabled: self.config.advisor.enabled,
                model: self.config.advisor.model.clone(),
            },
        }
    }

    /// Run a passive advisor review of `turn_id` when enabled. Errors are swallowed softly so the
    /// advisor can never fail a completed turn.
    pub(crate) async fn maybe_run_advisor(
        &self,
        meta: &SessionMeta,
        turn_id: &str,
        final_text: &str,
        control: &SessionControl,
        io: &TurnIo,
    ) {
        if io.cancel.is_cancelled() {
            return;
        }
        let resolved = self.resolve_advisor(control);
        if !resolved.enabled {
            return;
        }
        let Ok(branch) = self.load_head_branch(meta) else {
            return;
        };
        let Some(digest) = build_turn_digest(&branch, turn_id, final_text) else {
            return;
        };

        let explicit_model = resolved.model;
        let req = |model: Option<String>| LlmRequest {
            model,
            messages: vec![
                Msg::system(ADVISOR_SYSTEM_PROMPT),
                Msg::user(digest.clone()),
            ],
            tools: Vec::new(),
            thinking: self.config.advisor.thinking,
            thinking_budgets: None,
            temperature: None,
            max_tokens: Some(self.config.advisor.max_tokens),
            session_id: None,
            service_tier: None,
        };

        // Resolution order: explicit override -> `advisor` role -> `slow` role -> session model.
        let candidates: Vec<Option<String>> = if explicit_model.is_some() {
            vec![explicit_model]
        } else {
            vec![
                Some("advisor".to_owned()),
                Some("slow".to_owned()),
                meta.model.clone(),
            ]
        };

        let mut response = None;
        for candidate in candidates {
            if io.cancel.is_cancelled() {
                return;
            }
            if let Ok(resp) = self
                .llm
                .stream_chat(&req(candidate), &io.cancel, &SilentSink)
                .await
            {
                response = Some(resp);
                break;
            }
        }
        let Some(response) = response else {
            return;
        };
        // Best effort like the rest of the advisor: a store failure only loses this call's cost.
        self.record_usage(&meta.id, meta.model.as_deref(), &response)
            .ok();

        if let Some(note) = parse_advisor_reply(&response.content) {
            let model = response.model_used;
            control.push_advisor_note(format!("[{model}] {note}"));
            emit(&io.events, AgentEvent::AdvisorNote { model, note }).await;
        }
    }
}

/// Build a compact review digest of a single turn, or `None` when the turn is too trivial to review.
pub(crate) fn build_turn_digest(
    branch: &[DagNode],
    turn_id: &str,
    final_text: &str,
) -> Option<String> {
    let mut user_prompt = String::new();
    let mut actions = Vec::new();
    let mut tool_count = 0usize;

    for node in branch {
        let DagPayload::Message {
            turn_id: node_turn,
            msg,
        } = &node.payload
        else {
            continue;
        };
        if node_turn != turn_id {
            continue;
        }
        match msg.role {
            Role::User if user_prompt.is_empty() && !msg.content.starts_with("[steer]") => {
                user_prompt = truncate_chars(&msg.content, 2000);
            }
            Role::Assistant if !msg.tool_calls.is_empty() => {
                for call in &msg.tool_calls {
                    tool_count += 1;
                    actions.push(format!(
                        "- call {}({})",
                        call.name,
                        truncate_chars(&call.arguments, 240)
                    ));
                }
            }
            Role::Tool => {
                let status = if msg.is_error { "ERR" } else { "ok" };
                let name = msg.tool_name.as_deref().unwrap_or("tool");
                actions.push(format!(
                    "  -> {name} [{status}]: {}",
                    truncate_chars(&msg.content, 320)
                ));
            }
            _ => {}
        }
    }

    if tool_count == 0 && final_text.trim().chars().count() < MIN_TEXT_ONLY_CHARS {
        return None;
    }

    let mut out = String::new();
    if !user_prompt.is_empty() {
        out.push_str("## User request\n");
        out.push_str(&user_prompt);
        out.push_str("\n\n");
    }
    if !actions.is_empty() {
        out.push_str("## Tool activity\n");
        for line in actions {
            out.push_str(&line);
            out.push('\n');
            if out.len() > MAX_DIGEST_CHARS * 2 / 3 {
                out.push_str("... [earlier tool steps truncated]\n");
                break;
            }
        }
        out.push('\n');
    }
    out.push_str("## Final assistant reply\n");
    out.push_str(&truncate_chars(final_text, 4000));
    Some(truncate_chars(&out, MAX_DIGEST_CHARS))
}

/// Returns `None` when the advisor signed off (`OK` / `LGTM` / empty), else the trimmed note.
pub(crate) fn parse_advisor_reply(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed
        .trim_matches(|c: char| matches!(c, '`' | '*' | '_' | '.' | '!' | ' '))
        .to_ascii_uppercase();
    if matches!(
        normalized.as_str(),
        "OK" | "LGTM" | "NO ISSUES" | "NONE" | "ALL GOOD"
    ) {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Format accumulated advisor notes for injection into the next turn's system prompt.
pub(crate) fn format_pending_notes(notes: &[String]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let body = notes.join("\n\n");
    Some(format!(
        "[Advisor review of previous turn]\n\
         Consider these concise notes from the passive reviewer if relevant to the current task:\n\
         {body}"
    ))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let kept: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{kept}…")
    } else {
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::ToolCall;

    fn node(id: &str, turn_id: &str, msg: Msg) -> DagNode {
        DagNode {
            id: id.to_owned(),
            session_id: "s1".to_owned(),
            parent_id: None,
            created_at_ms: 0,
            payload: DagPayload::Message {
                turn_id: turn_id.to_owned(),
                msg,
            },
        }
    }

    #[test]
    fn parse_reply_suppresses_ok_variants_and_keeps_notes() {
        assert_eq!(parse_advisor_reply(""), None);
        assert_eq!(parse_advisor_reply("OK"), None);
        assert_eq!(parse_advisor_reply("  `ok.`  "), None);
        assert_eq!(parse_advisor_reply("LGTM!"), None);
        assert_eq!(
            parse_advisor_reply("- Missing null check on `foo`."),
            Some("- Missing null check on `foo`.".to_owned())
        );
    }

    #[test]
    fn digest_skips_trivial_text_only_turns_and_keeps_tool_turns() {
        let trivial = vec![node("n1", "t1", Msg::user("hi"))];
        assert!(build_turn_digest(&trivial, "t1", "Hello!").is_none());

        let with_tool = vec![
            node("n1", "t1", Msg::user("fix bug")),
            node(
                "n2",
                "t1",
                Msg::assistant_tool_calls(
                    String::new(),
                    None,
                    vec![ToolCall {
                        id: "c1".into(),
                        name: "edit".into(),
                        arguments: "{\"path\":\"src/lib.rs\"}".into(),
                        signature: None,
                    }],
                ),
            ),
            node("n3", "t1", Msg::tool_result("c1", "edit", "updated")),
        ];
        let digest = build_turn_digest(&with_tool, "t1", "Done.").expect("digest");
        assert!(digest.contains("fix bug"));
        assert!(digest.contains("edit"));
        assert!(digest.contains("Done."));
    }

    #[test]
    fn advisor_emits_note_and_injects_into_next_turn_while_ok_stays_silent() {
        use crate::registry::ToolRegistry;
        use crate::runtime::HarnessConfig;
        use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
        use hrn_kernel::{CancelFlag, FinishReason, LlmResponse, PermissionMode};
        use std::collections::VecDeque;
        use std::path::Path;
        use std::sync::{Arc, Mutex};

        let mk_resp = |text: &str, model: &str| LlmResponse {
            content: text.to_owned(),
            reasoning: String::new(),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
            model_used: model.to_owned(),
            fallback_from: None,
        };

        let long_reply = "Implemented the parser and wired the endpoints cleanly across all modules so tests pass now.";
        let llm = Arc::new(ScriptedLlm {
            responses: Mutex::new(VecDeque::from([
                Ok(mk_resp(long_reply, "default")),
                Ok(mk_resp("- Check empty input handling.", "reviewer")),
                Ok(mk_resp(long_reply, "default")),
                Ok(mk_resp("OK", "reviewer")),
            ])),
            requests: Mutex::new(Vec::new()),
            models: Vec::new(),
        });
        let store = Arc::new(MemStore::default());
        let fs = Arc::new(MemFs::default());
        let rt = AgentRuntime::new(
            llm.clone(),
            store,
            ToolRegistry::new(),
            fs,
            HarnessConfig::default(),
        )
        .with_advisor(AdvisorConfig {
            enabled: true,
            model: Some("reviewer".into()),
            ..AdvisorConfig::default()
        });
        let session = rt
            .create_session(
                "adv test",
                Path::new("/repo"),
                Some("default".into()),
                ThinkingLevel::Low,
                PermissionMode::Default,
            )
            .expect("session");
        let control = SessionControl::default();
        let events = Arc::new(VecEvents::default());
        let io = || TurnIo {
            cancel: CancelFlag::new(),
            events: events.clone(),
            approval: None,
            ask: None,
        };

        block_on(rt.run_turn(&session.id, "build feature", &control, io())).expect("turn 1");
        let turn1_events: Vec<AgentEvent> = events.0.lock().expect("lock").drain(..).collect();
        let notes: Vec<String> = turn1_events
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::AdvisorNote { note, .. } => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(notes, vec!["- Check empty input handling."]);

        block_on(rt.run_turn(&session.id, "add edge test", &control, io())).expect("turn 2");
        let turn2_events: Vec<AgentEvent> = events.0.lock().expect("lock").drain(..).collect();
        assert!(
            !turn2_events
                .into_iter()
                .any(|e| matches!(e, AgentEvent::AdvisorNote { .. }))
        );
        let reqs = llm.requests.lock().expect("reqs");
        assert_eq!(reqs.len(), 4);
        // The advisor note left the system string: it arrives as a `[system]` user message at
        // the turn boundary, after the (unchanged) system prompt.
        assert!(
            !reqs[2].messages[0]
                .content
                .contains("[Advisor review of previous turn]")
        );
        let advisor_msg = reqs[2]
            .messages
            .iter()
            .find(|m| {
                m.content
                    .contains("[system] [Advisor review of previous turn]")
            })
            .expect("the note rides as a user-role [system] message");
        assert!(advisor_msg.content.contains("Check empty input handling."));
    }
}
