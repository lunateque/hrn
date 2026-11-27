//! The session-DAG effects tools ask for (`checkpoint` marks, `rewind` returns): only the
//! runtime writes the DAG, so a tool returns the request and the step loop applies it once the
//! step's tool results are stored.

use crate::runtime::AgentRuntime;
use hrn_kernel::{DagNode, DagPayload, Msg, Result, RewindTo, Role, SessionEffect, SessionMeta};

/// The node on `branch` a rewind returns to, and how many nodes after it the branch loses.
fn anchor<'a>(branch: &'a [DagNode], to: &RewindTo) -> Option<(&'a DagNode, usize)> {
    let mark = |node: &DagNode, label: &str| matches!(&node.payload, DagPayload::ForkMarker { label: l } if l == label);
    let prompt = |node: &DagNode, turn: &str| {
        matches!(&node.payload, DagPayload::Message { turn_id, msg }
            if turn_id == turn && msg.role == Role::User)
    };
    let position = match to {
        // The newest mark of that name wins: an older one is already behind it.
        RewindTo::Mark(label) => branch
            .iter()
            .rposition(|node| mark(node, label))
            .map(|index| (index, branch.len() - index - 1)),
        // The first user message of the turn is the prompt; later ones are steers.
        RewindTo::TurnStart(turn) => branch
            .iter()
            .position(|node| prompt(node, turn))
            .map(|index| (index, branch.len() - index - 1)),
    };
    position.and_then(|(index, dropped)| branch.get(index).map(|node| (node, dropped)))
}

impl AgentRuntime {
    /// Apply `effects` in order to `meta`'s active branch.
    pub(crate) fn apply_effects(
        &self,
        meta: &mut SessionMeta,
        turn_id: &str,
        effects: Vec<SessionEffect>,
    ) -> Result {
        for effect in effects {
            match effect {
                SessionEffect::Mark { label } => {
                    self.append_payload(meta, DagPayload::ForkMarker { label })?;
                }
                SessionEffect::TodoEdit { phases } => {
                    self.append_payload(meta, DagPayload::TodoBoard { phases })?;
                }
                SessionEffect::Rewind { to, report } => {
                    let branch = self.load_head_branch(meta)?;
                    let text = match anchor(&branch, &to) {
                        Some((node, dropped)) => {
                            meta.head_node_id = Some(node.id.clone());
                            format!(
                                "[rewind] {report}\n({dropped} earlier node(s) of this branch \
                                 left the context)"
                            )
                        }
                        None => format!(
                            "[rewind] {report}\n(the point rewound to is not on this branch: \
                             the files were restored but the conversation was kept)"
                        ),
                    };
                    self.append_message(meta, turn_id, Msg::user(text))?;
                }
                SessionEffect::Goal { .. } => {
                    // Applied by the step loop before the tool result is stored.
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::control::SessionControl;
    use crate::registry::ToolRegistry;
    use crate::runtime::{AgentRuntime, HarnessConfig, TurnIo};
    use crate::testutil::{MemFs, MemStore, ScriptedLlm, VecEvents, block_on};
    use hrn_kernel::{
        BoxFuture, CancelFlag, DagPayload, FinishReason, LlmResponse, PermissionMode, Result,
        RewindTo, Role, SessionEffect, SessionStore, ThinkingLevel, Tool, ToolCall, ToolContext,
        ToolOutput, ToolSpec,
    };
    use serde_json::{Value, json};
    use std::path::Path;
    use std::sync::Arc;

    /// Answers with the `effect` it was called with.
    struct EffectTool(&'static str);

    impl Tool for EffectTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.to_owned(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }

        fn execute(&self, _ctx: &ToolContext, _args: Value) -> BoxFuture<Result<ToolOutput>> {
            let effect = match self.0 {
                "mark" => Some(SessionEffect::Mark {
                    label: "checkpoint:x".into(),
                }),
                "rewind" => Some(SessionEffect::Rewind {
                    to: RewindTo::Mark("checkpoint:x".into()),
                    report: "found it".into(),
                }),
                _ => None,
            };
            let mut out = ToolOutput::ok(self.0);
            out.session_effect = effect;
            Box::pin(async move { Ok(out) })
        }
    }

    fn reply(content: &str, tool: Option<&str>) -> LlmResponse {
        LlmResponse {
            model_used: "m".into(),
            fallback_from: None,
            content: content.into(),
            reasoning: String::new(),
            thinking_blocks: Vec::new(),
            tool_calls: tool
                .map(|name| ToolCall {
                    id: format!("call-{name}"),
                    name: name.into(),
                    arguments: "{}".into(),
                    signature: None,
                })
                .into_iter()
                .collect(),
            usage: None,
            finish_reason: if tool.is_some() {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            },
        }
    }

    #[test]
    fn a_rewind_returns_the_branch_to_its_mark_and_keeps_the_report() {
        let llm = Arc::new(ScriptedLlm::default());
        llm.responses.lock().expect("lock").extend(
            [
                reply("", Some("mark")),
                reply("", Some("work")),
                reply("", Some("rewind")),
                reply("done", None),
            ]
            .into_iter()
            .map(Ok),
        );
        let store = Arc::new(MemStore::default());
        let mut tools = ToolRegistry::new();
        for name in ["mark", "work", "rewind"] {
            tools.register(Arc::new(EffectTool(name)));
        }
        let rt = AgentRuntime::new(
            llm.clone(),
            store.clone(),
            tools,
            Arc::new(MemFs::default()),
            HarnessConfig::default(),
        );
        let meta = rt
            .create_session(
                "t",
                Path::new("/ws"),
                None,
                ThinkingLevel::Off,
                PermissionMode::Yolo,
            )
            .expect("session");
        let io = TurnIo {
            cancel: CancelFlag::new(),
            events: Arc::new(VecEvents::default()),
            approval: None,
            ask: None,
        };
        block_on(rt.run_turn(&meta.id, "go", &SessionControl::default(), io)).expect("turn");

        let head = store.get_session(&meta.id).expect("get").expect("some");
        let branch = store
            .load_branch(&meta.id, head.head_node_id.as_deref().expect("head"))
            .expect("branch");
        let shape: Vec<String> = branch
            .iter()
            .map(|n| match &n.payload {
                DagPayload::Message { msg, .. } => {
                    format!(
                        "{:?}:{}",
                        msg.role,
                        msg.content.lines().next().unwrap_or("")
                    )
                }
                DagPayload::ForkMarker { label } => format!("mark:{label}"),
                DagPayload::SessionExit { reason, .. } => format!("exit:{reason:?}"),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "User:go",
                "Assistant:",
                "Tool:mark",
                "mark:checkpoint:x",
                "User:[rewind] found it",
                "Assistant:done",
                "exit:Normal"
            ]
        );
        // The abandoned exploration stays in the store as a side branch.
        let all = store.load_tree(&meta.id).expect("tree");
        assert!(all.iter().any(|n| matches!(&n.payload,
            DagPayload::Message { msg, .. } if msg.role == Role::Tool && msg.content == "work")));
    }
}
