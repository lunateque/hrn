//! Branch navigation helpers, shared leaf-selection rules, node labels, and abandoned-branch
//! summaries (`DagPayload::Label` and `DagPayload::BranchSummary`).

use crate::compaction::project_branch;
use crate::runtime::AgentRuntime;
use crate::summary::{build_summary_prompt, extract_modified_files, extract_todos};
use hrn_kernel::{
    CancelFlag, DagNode, DagPayload, HrnError, Msg, NodeId, Result, Role, SessionMeta,
};
use std::collections::{HashMap, HashSet};

/// Result of applying the shared `/tree` + `/branch` leaf-selection rule to a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafSelection {
    /// New session head node (`None` when rewinding past the root user message).
    pub target_head: Option<NodeId>,
    /// Draft text to prefill into an empty editor when selecting a user message.
    pub prefill: Option<String>,
    /// True when the selected node is already the current leaf ("Already at this point").
    pub already_at_leaf: bool,
}

/// Resolve all effective node labels across a session DAG (`DagPayload::Label`, last write wins).
///
/// An entry with `label: None` (or whitespace-only) removes any earlier label on that target node.
#[must_use]
pub fn resolve_labels(nodes: &[DagNode]) -> HashMap<NodeId, String> {
    let mut labels = HashMap::new();
    for node in nodes {
        if let DagPayload::Label {
            target_node_id,
            label,
        } = &node.payload
        {
            match label.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(text) => {
                    labels.insert(target_node_id.clone(), text.to_owned());
                }
                None => {
                    labels.remove(target_node_id);
                }
            }
        }
    }
    labels
}

/// Apply the shared `/tree` & `/branch` leaf rule to `selected_id`:
/// - Selecting the current leaf is a no-op (`already_at_leaf = true`).
/// - Selecting a `Role::User` message sets the leaf to its parent (`None` if root) and returns
///   its text as `prefill` for an empty editor.
/// - Selecting any other node sets the leaf to that node directly.
#[must_use]
pub fn resolve_leaf_selection(
    nodes: &[DagNode],
    current_head: Option<&str>,
    selected_id: &str,
) -> Option<LeafSelection> {
    if current_head == Some(selected_id) {
        return Some(LeafSelection {
            target_head: current_head.map(str::to_owned),
            prefill: None,
            already_at_leaf: true,
        });
    }
    let node = nodes.iter().find(|n| n.id == selected_id)?;
    match &node.payload {
        DagPayload::Message { msg, .. } if msg.role == Role::User => Some(LeafSelection {
            target_head: node.parent_id.clone(),
            prefill: Some(msg.content.clone()),
            already_at_leaf: false,
        }),
        _ => Some(LeafSelection {
            target_head: Some(node.id.clone()),
            prefill: None,
            already_at_leaf: false,
        }),
    }
}

/// Nodes on the current branch that are NOT ancestors of `new_head` (i.e. abandoned by switching).
#[must_use]
pub fn compute_abandoned_nodes(
    all_nodes: &[DagNode],
    current_head: Option<&str>,
    new_head: Option<&str>,
) -> Vec<DagNode> {
    let Some(cur) = current_head else {
        return Vec::new();
    };
    let by_id: HashMap<&str, &DagNode> = all_nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut new_ancestors: HashSet<&str> = HashSet::new();
    let mut walk = new_head;
    while let Some(id) = walk {
        if !new_ancestors.insert(id) {
            break;
        }
        walk = by_id.get(id).and_then(|n| n.parent_id.as_deref());
    }
    let mut abandoned = Vec::new();
    let mut cur_walk = Some(cur);
    let mut seen: HashSet<&str> = HashSet::new();
    while let Some(id) = cur_walk {
        if !seen.insert(id) || new_ancestors.contains(id) {
            break;
        }
        let Some(node) = by_id.get(id) else {
            break;
        };
        abandoned.push((*node).clone());
        cur_walk = node.parent_id.as_deref();
    }
    abandoned.reverse();
    abandoned
}

impl AgentRuntime {
    /// Move `session_id`'s head to `target_node_id` (or `None` to rewind before the first turn),
    /// returning updated metadata and active branch.
    ///
    /// # Errors
    /// Fails if the session or target node does not exist or persistence fails.
    pub fn switch_head_optional(
        &self,
        session_id: &str,
        target_node_id: Option<&str>,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let branch = match target_node_id {
            Some(id) => self.store.load_branch(session_id, id)?,
            None => Vec::new(),
        };
        meta.head_node_id = target_node_id.map(str::to_owned);
        meta.updated_at_ms = self.clock.now_ms();
        self.store.update_session(&meta)?;
        Ok((meta, branch))
    }

    /// Append a [`DagPayload::Label`] node for `target_node_id` (`None` / blank clears the label;
    /// last write wins).
    ///
    /// # Errors
    /// Fails if the session does not exist or persistence fails.
    pub fn set_label(
        &self,
        session_id: &str,
        target_node_id: &str,
        label: Option<String>,
    ) -> Result<NodeId> {
        let mut meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let cleaned = label.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        let id = self.append_payload(
            &mut meta,
            DagPayload::Label {
                target_node_id: target_node_id.to_owned(),
                label: cleaned,
            },
        )?;
        meta.updated_at_ms = self.clock.now_ms();
        self.store.update_session(&meta)?;
        Ok(id)
    }

    /// Switch `session_id`'s head to `new_head`, optionally summarizing any abandoned nodes and
    /// attaching a [`DagPayload::BranchSummary`] at the new leaf.
    ///
    /// # Errors
    /// Fails if the session/node is missing, summarization fails, or persistence fails.
    pub async fn switch_with_branch_summary(
        &self,
        session_id: &str,
        new_head: Option<&str>,
        summarize: bool,
        cancel: &CancelFlag,
    ) -> Result<(SessionMeta, Vec<DagNode>)> {
        let before = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
        let old_head = before.head_node_id.clone();
        let all_nodes = self.store.load_tree(session_id)?;
        let abandoned = compute_abandoned_nodes(&all_nodes, old_head.as_deref(), new_head);
        let has_abandoned_msgs = abandoned
            .iter()
            .any(|n| matches!(n.payload, DagPayload::Message { .. }));

        let summary_text = if summarize && has_abandoned_msgs {
            let policy = self.policy_for(before.model.as_deref());
            let projected = project_branch(&abandoned, &policy);
            let mut prompt = build_summary_prompt(
                &projected.messages,
                &extract_todos(&abandoned),
                &extract_modified_files(&abandoned),
            );
            if let Some(first) = prompt.first_mut() {
                *first = Msg::system(
                    "Summarize the abandoned branch concisely (under 250 words) so the active branch retains key findings, files touched, and why the branch was left.",
                );
            }
            Some(self.summarize(&before, prompt, cancel).await?)
        } else {
            None
        };

        let (mut meta, mut branch) = self.switch_head_optional(session_id, new_head)?;
        if let (Some(text), Some(from_node_id)) = (summary_text, old_head) {
            self.append_payload(&mut meta, DagPayload::BranchSummary { from_node_id, text })?;
            meta.updated_at_ms = self.clock.now_ms();
            self.store.update_session(&meta)?;
            branch = self.load_head_branch(&meta)?;
        }
        Ok((meta, branch))
    }
}
