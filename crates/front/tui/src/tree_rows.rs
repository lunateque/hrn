//! Tree-row construction, filter modes, label resolution, shared leaf-selection rules, and
//! abandoned-branch detection backing [`crate::tree::TreeOverlay`].

use crate::toolfmt;
use crate::wrap;
use hrn_kernel::{DagNode, DagPayload, Role};
use std::collections::{HashMap, HashSet};

/// Maximum trailing nodes shown when `/branch` (`/rewind`) opens in user-turns-first mode.
pub(crate) const BRANCH_NODE_CAP: usize = 600;

/// Filter preset cycled by `Ctrl+O` inside the tree overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeFilter {
    /// Standard messages and structural milestones (hides low-level tool rows & metadata).
    Default,
    /// Conversation messages without tool-result noise (keeps assistant calls & summaries).
    NoTools,
    /// User prompt turns only (default when opened via `/branch` or `/rewind`).
    UserOnly,
    /// Only nodes that carry a non-empty user label (`Shift+L`).
    LabeledOnly,
    /// Every node in the DAG (including tool results, exit records, labels, and todo boards).
    All,
}

impl TreeFilter {
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::Default => Self::NoTools,
            Self::NoTools => Self::UserOnly,
            Self::UserOnly => Self::LabeledOnly,
            Self::LabeledOnly => Self::All,
            Self::All => Self::Default,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NoTools => "no-tools",
            Self::UserOnly => "user-only",
            Self::LabeledOnly => "labeled-only",
            Self::All => "all",
        }
    }
}

/// Shared leaf-selection outcome for `/tree` and `/branch`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeafSelection {
    pub(crate) target_head: Option<String>,
    pub(crate) prefill: Option<String>,
    pub(crate) already_at_leaf: bool,
}

pub(crate) struct TreeRow {
    pub(crate) node_id: String,
    pub(crate) prefix: String,
    pub(crate) role: &'static str,
    pub(crate) preview: String,
    pub(crate) label: Option<String>,
    pub(crate) on_active: bool,
    pub(crate) is_head: bool,
}

/// Resolve all effective node labels across `nodes` (`DagPayload::Label`, last write wins).
pub(crate) fn resolve_labels(nodes: &[DagNode]) -> HashMap<String, String> {
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

/// Apply the shared `/tree` + `/branch` leaf rule to `selected_id`.
pub(crate) fn resolve_leaf_selection(
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

/// True when switching from `current_head` to `new_head` abandons at least one message node.
pub(crate) fn switch_abandons_nodes(
    nodes: &[DagNode],
    current_head: Option<&str>,
    new_head: Option<&str>,
) -> bool {
    let Some(cur) = current_head else {
        return false;
    };
    if current_head == new_head {
        return false;
    }
    let by_id: HashMap<&str, &DagNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut new_ancestors: HashSet<&str> = HashSet::new();
    let mut walk = new_head;
    while let Some(id) = walk {
        if !new_ancestors.insert(id) {
            break;
        }
        walk = by_id.get(id).and_then(|n| n.parent_id.as_deref());
    }
    let mut cur_walk = Some(cur);
    let mut seen: HashSet<&str> = HashSet::new();
    while let Some(id) = cur_walk {
        if !seen.insert(id) || new_ancestors.contains(id) {
            break;
        }
        let Some(node) = by_id.get(id) else {
            break;
        };
        if matches!(node.payload, DagPayload::Message { .. }) {
            return true;
        }
        cur_walk = node.parent_id.as_deref();
    }
    false
}

/// Cap `nodes` to the latest `cap` entries for `/branch`.
pub(crate) fn latest_nodes(nodes: Vec<DagNode>, cap: usize) -> Vec<DagNode> {
    if nodes.len() <= cap {
        nodes
    } else {
        let drop_n = nodes.len() - cap;
        nodes.into_iter().skip(drop_n).collect()
    }
}

fn is_visible_node(node: &DagNode, filter: TreeFilter, labels: &HashMap<String, String>) -> bool {
    match filter {
        TreeFilter::All => true,
        TreeFilter::LabeledOnly => labels.contains_key(&node.id),
        TreeFilter::UserOnly => matches!(
            &node.payload,
            DagPayload::Message { msg, .. } if msg.role == Role::User
        ),
        TreeFilter::NoTools => match &node.payload {
            DagPayload::Message { msg, .. } => {
                msg.role != Role::Tool
                    && !(msg.role == Role::Assistant
                        && msg.content.trim().is_empty()
                        && !msg.tool_calls.is_empty())
            }
            DagPayload::Summary { .. }
            | DagPayload::ForkMarker { .. }
            | DagPayload::BranchSummary { .. }
            | DagPayload::ModelChange { .. } => true,
            DagPayload::ToolsActivated { .. }
            | DagPayload::TodoBoard { .. }
            | DagPayload::SessionExit { .. }
            | DagPayload::Label { .. } => false,
        },
        TreeFilter::Default => match &node.payload {
            DagPayload::Message { msg, .. } => msg.role != Role::Tool,
            DagPayload::Summary { .. }
            | DagPayload::ForkMarker { .. }
            | DagPayload::BranchSummary { .. }
            | DagPayload::ModelChange { .. } => true,
            DagPayload::ToolsActivated { .. }
            | DagPayload::TodoBoard { .. }
            | DagPayload::SessionExit { .. }
            | DagPayload::Label { .. } => false,
        },
    }
}

fn summarize_node(node: &DagNode) -> (&'static str, String) {
    match &node.payload {
        DagPayload::Message { msg, .. } => match msg.role {
            Role::User => ("user", toolfmt::one_line(&wrap::sanitize(&msg.content))),
            Role::Assistant => {
                let text = msg.content.trim();
                if !text.is_empty() {
                    ("asst", toolfmt::one_line(&wrap::sanitize(text)))
                } else if !msg.tool_calls.is_empty() {
                    let names: Vec<&str> = msg.tool_calls.iter().map(|c| c.name.as_str()).collect();
                    ("asst", format!("calls: {}", names.join(", ")))
                } else {
                    ("asst", "(empty)".to_owned())
                }
            }
            Role::Tool => {
                let name = msg.tool_name.as_deref().unwrap_or("tool");
                let status = if msg.is_error { "err" } else { "ok" };
                ("tool", format!("{name} ({status})"))
            }
            Role::System => ("sys", toolfmt::one_line(&wrap::sanitize(&msg.content))),
        },
        DagPayload::Summary { text, method, .. } => {
            let tag = if method.as_deref() == Some("handoff") {
                "hndf"
            } else {
                "sum"
            };
            (tag, toolfmt::one_line(&wrap::sanitize(text)))
        }
        DagPayload::ForkMarker { label } => ("fork", toolfmt::one_line(&wrap::sanitize(label))),
        DagPayload::ToolsActivated { names } => ("tools", names.join(", ")),
        DagPayload::TodoBoard { phases } => {
            let open: usize = phases
                .iter()
                .flat_map(|p| &p.tasks)
                .filter(|t| !t.status.closed())
                .count();
            ("todo", format!("{open} open task(s)"))
        }
        DagPayload::SessionExit { reason, .. } => ("exit", format!("{reason:?}").to_lowercase()),
        DagPayload::Label {
            target_node_id,
            label,
        } => (
            "label",
            format!(
                "{target_node_id}: {}",
                label.as_deref().unwrap_or("(clear)")
            ),
        ),
        DagPayload::BranchSummary { text, .. } => {
            ("bsum", toolfmt::one_line(&wrap::sanitize(text)))
        }
        DagPayload::ModelChange { selector, .. } => ("model", selector.clone()),
    }
}

pub(crate) fn build_rows(
    nodes: &[DagNode],
    head_id: Option<&str>,
    filter: TreeFilter,
    labels: &HashMap<String, String>,
) -> Vec<TreeRow> {
    let by_id: HashMap<&str, &DagNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut active_set: HashSet<&str> = HashSet::new();
    let mut cursor = head_id;
    while let Some(id) = cursor {
        if !active_set.insert(id) {
            break;
        }
        cursor = by_id.get(id).and_then(|n| n.parent_id.as_deref());
    }

    // Map each visible node to its nearest visible ancestor so filtering keeps the tree connected.
    let mut children: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
    for (idx, node) in nodes.iter().enumerate() {
        if !is_visible_node(node, filter, labels) {
            continue;
        }
        let mut parent = node.parent_id.as_deref();
        let mut steps = 0usize;
        while let Some(pid) = parent {
            steps += 1;
            if steps > nodes.len() {
                parent = None;
                break;
            }
            match by_id.get(pid) {
                Some(pnode) if is_visible_node(pnode, filter, labels) => break,
                Some(pnode) => parent = pnode.parent_id.as_deref(),
                None => {
                    parent = None;
                    break;
                }
            }
        }
        children.entry(parent).or_default().push(idx);
    }

    let mut walker = TreeWalker {
        nodes,
        children: &children,
        active_set: &active_set,
        labels,
        head_id,
        guides: Vec::new(),
        out: Vec::new(),
    };
    if let Some(roots) = children.get(&None) {
        let branched = roots.len() > 1;
        for (pos, &root_idx) in roots.iter().enumerate() {
            walker.visit(root_idx, branched, pos + 1 == roots.len());
        }
    }
    walker.out
}

struct TreeWalker<'a> {
    nodes: &'a [DagNode],
    children: &'a HashMap<Option<&'a str>, Vec<usize>>,
    active_set: &'a HashSet<&'a str>,
    labels: &'a HashMap<String, String>,
    head_id: Option<&'a str>,
    guides: Vec<bool>,
    out: Vec<TreeRow>,
}

impl TreeWalker<'_> {
    fn visit(&mut self, idx: usize, draw_branch: bool, is_last: bool) {
        let Some(node) = self.nodes.get(idx) else {
            return;
        };
        let mut prefix = String::new();
        for &has_more in &self.guides {
            prefix.push_str(if has_more { "│  " } else { "   " });
        }
        if draw_branch {
            prefix.push_str(if is_last { "└─ " } else { "├─ " });
        }
        let (role, preview) = summarize_node(node);
        self.out.push(TreeRow {
            node_id: node.id.clone(),
            prefix,
            role,
            preview,
            label: self.labels.get(&node.id).cloned(),
            on_active: self.active_set.contains(node.id.as_str()),
            is_head: self.head_id == Some(node.id.as_str()),
        });

        if let Some(kids) = self.children.get(&Some(node.id.as_str())) {
            let branched = kids.len() > 1;
            if draw_branch {
                self.guides.push(!is_last);
            }
            for (pos, &kid_idx) in kids.iter().enumerate() {
                self.visit(kid_idx, branched, pos + 1 == kids.len());
            }
            if draw_branch {
                self.guides.pop();
            }
        }
    }
}
