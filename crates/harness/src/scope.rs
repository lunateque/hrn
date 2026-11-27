//! Mechanical scope enforcement ([`ScopePolicy`] + [`ScopeGuardHook`]) for subagents and
//! MCP-invoked agent runs.
//!
//! Enforces `allowed_paths`, `denied_paths`, `read_only`, `allow_tools`, `deny_tools`, and
//! `max_steps` deterministically in Rust rather than relying on prose instructions.

use crate::approvals::canonicalize_with_ancestors;
use crate::runtime::AgentRuntime;
use hrn_kernel::{
    BoxFuture, HarnessHook, HookVerdict, Msg, Result, Role, ToolContext, ToolOutput, WorkspaceFs,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Tools that write or mutate workspace files and carry path arguments.
const MUTATING_FILE_TOOLS: &[&str] = &["edit", "write", "apply_patch", "ast_edit", "resolve"];

/// Declarative, mechanically enforced scope constraints for one agent run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopePolicy {
    /// Allowed workspace-relative path globs or directory prefixes for mutating file tools.
    /// Empty means unrestricted (unless `denied_paths` or `read_only` restricts writes).
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Denied workspace-relative path globs or directory prefixes (always wins over `allowed_paths`).
    #[serde(default)]
    pub denied_paths: Vec<String>,
    /// Strip all mutating tools from the agent's registry.
    #[serde(default)]
    pub read_only: bool,
    /// Explicit tool allowlist (empty = all tools permitted by the runtime).
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Explicit tool denylist (always wins over `allow_tools`).
    #[serde(default)]
    pub deny_tools: Vec<String>,
    /// Allow `bash` even when `allowed_paths` is set (default `false`: shell commands cannot be
    /// safely path-filtered, so `bash` is dropped automatically when `allowed_paths` is active).
    #[serde(default)]
    pub allow_bash_with_paths: bool,
    /// Optional hard step cap for the turn.
    #[serde(default)]
    pub max_steps: Option<usize>,
}

impl ScopePolicy {
    /// Parse scope fields (`allowed_paths`, `denied_paths`, `read_only`, `allow_tools`,
    /// `deny_tools`, `allow_bash_with_paths`, `max_steps`) from a tool call JSON object.
    pub fn from_tool_args(args: &Value) -> Self {
        let str_list = |key: &str| -> Vec<String> {
            args.get(key)
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let max_steps = args
            .get("max_steps")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|&v| v > 0);
        Self {
            allowed_paths: str_list("allowed_paths"),
            denied_paths: str_list("denied_paths"),
            read_only: args
                .get("read_only")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            allow_tools: str_list("allow_tools"),
            deny_tools: str_list("deny_tools"),
            allow_bash_with_paths: args
                .get("allow_bash_with_paths")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            max_steps,
        }
    }

    /// Whether this policy imposes any restriction at all.
    pub fn is_empty(&self) -> bool {
        self.allowed_paths.is_empty()
            && self.denied_paths.is_empty()
            && !self.read_only
            && self.allow_tools.is_empty()
            && self.deny_tools.is_empty()
            && self.max_steps.is_none()
    }

    /// Apply tool filtering, [`ScopeGuardHook`], and `max_steps` to produce a scoped runtime clone.
    pub fn apply_to_runtime(&self, runtime: &AgentRuntime, cwd: PathBuf) -> AgentRuntime {
        let mut tools = if self.read_only {
            runtime.tools().read_only_view()
        } else {
            runtime.tools().clone()
        };

        if !self.allow_tools.is_empty() {
            let allow: BTreeSet<&str> = self.allow_tools.iter().map(String::as_str).collect();
            let existing = tools.names();
            for name in existing {
                if !allow.contains(name.as_str()) {
                    tools.remove(&name);
                }
            }
        }
        for denied in &self.deny_tools {
            tools.remove(denied);
        }
        // Shell commands and code evaluators can trivially bypass file-tool path checks; drop
        // both `bash` and `eval` by default when path restrictions are active unless opted in.
        if (!self.allowed_paths.is_empty() || !self.denied_paths.is_empty())
            && !self.allow_bash_with_paths
        {
            tools.remove("bash");
            tools.remove("eval");
        }

        let mut scoped = runtime.with_tools(tools);
        if let Some(steps) = self.max_steps {
            scoped = scoped.with_max_steps(steps);
        }
        if !self.allowed_paths.is_empty() || !self.denied_paths.is_empty() {
            let hook =
                ScopeGuardHook::new(cwd, self.allowed_paths.clone(), self.denied_paths.clone())
                    .with_fs(Arc::clone(runtime.fs()));
            scoped = scoped.with_hook(Arc::new(hook));
        }
        scoped
    }
}

/// Deterministic [`HarnessHook`] blocking mutating file tool calls outside `allowed_paths` or
/// matching `denied_paths`.
pub struct ScopeGuardHook {
    cwd: PathBuf,
    allowed_paths: Vec<String>,
    denied_paths: Vec<String>,
    fs: Option<Arc<dyn WorkspaceFs>>,
}

impl ScopeGuardHook {
    /// Construct a guard rooted at `cwd`.
    pub fn new(cwd: PathBuf, allowed_paths: Vec<String>, denied_paths: Vec<String>) -> Self {
        Self {
            cwd,
            allowed_paths,
            denied_paths,
            fs: None,
        }
    }

    /// Attach a [`WorkspaceFs`] so symlink ancestors are canonicalized before scope checking.
    #[must_use]
    pub fn with_fs(mut self, fs: Arc<dyn WorkspaceFs>) -> Self {
        self.fs = Some(fs);
        self
    }

    fn check_path_for_tool(
        &self,
        tool_name: &str,
        raw_path: &str,
    ) -> std::result::Result<(), String> {
        let rel = normalize_relative(&self.cwd, raw_path, self.fs.as_deref())?;
        let is_dir_tool = tool_name == "ast_edit";
        for pat in &self.denied_paths {
            if target_may_overlap_denied(&rel, pat, is_dir_tool) {
                return Err(format!(
                    "scope violation: `{rel}` matches or overlaps denied path pattern `{pat}`"
                ));
            }
        }
        if !self.allowed_paths.is_empty()
            && !self
                .allowed_paths
                .iter()
                .any(|pat| path_matches_pattern(&rel, pat))
        {
            return Err(format!(
                "scope violation: `{rel}` is outside allowed_paths [{}]",
                self.allowed_paths.join(", ")
            ));
        }
        Ok(())
    }

    fn format_system_notice(&self) -> String {
        let mut lines = vec!["# Enforced Workspace Scope".to_owned()];
        if !self.allowed_paths.is_empty() {
            lines.push(format!(
                "- You may ONLY modify files matching: {}",
                self.allowed_paths.join(", ")
            ));
        }
        if !self.denied_paths.is_empty() {
            lines.push(format!(
                "- You MUST NOT modify files matching: {}",
                self.denied_paths.join(", ")
            ));
        }
        lines.push("- Out-of-scope file edits are mechanically blocked by the runtime.".to_owned());
        lines.join("\n")
    }
}

impl HarnessHook for ScopeGuardHook {
    fn before_llm(&self, messages: &mut Vec<Msg>) -> BoxFuture<Result> {
        let notice = self.format_system_notice();
        if let Some(first) = messages.first_mut()
            && first.role == Role::System
            && !first.content.contains("# Enforced Workspace Scope")
        {
            first.content.push_str("\n\n");
            first.content.push_str(&notice);
        }
        Box::pin(async { Ok(()) })
    }

    fn before_tool(
        &self,
        _ctx: &ToolContext,
        tool_name: &str,
        arguments: &Value,
    ) -> BoxFuture<Result<HookVerdict>> {
        let lsp_fmt = tool_name == "lsp"
            && arguments
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(|a| a.eq_ignore_ascii_case("format"));
        if !MUTATING_FILE_TOOLS.contains(&tool_name) && !lsp_fmt {
            return Box::pin(async { Ok(HookVerdict::Continue) });
        }
        let paths = collect_mutating_paths(tool_name, arguments);
        if paths.is_empty() && (!self.allowed_paths.is_empty() || !self.denied_paths.is_empty()) {
            let msg = format!(
                "scope violation: `{tool_name}` did not specify a target path while path scope restrictions are active"
            );
            return Box::pin(async move { Ok(HookVerdict::ShortCircuit(ToolOutput::err(msg))) });
        }
        let verdict = paths
            .iter()
            .find_map(|raw| self.check_path_for_tool(tool_name, raw).err())
            .map_or(HookVerdict::Continue, |msg| {
                HookVerdict::ShortCircuit(ToolOutput::err(msg))
            });
        Box::pin(async move { Ok(verdict) })
    }
}

fn collect_mutating_paths(tool_name: &str, arguments: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let split_semi = tool_name == "ast_edit";
    let mut push = |raw: &str| {
        for part in if split_semi {
            raw.split(';').collect::<Vec<_>>()
        } else {
            vec![raw]
        } {
            let t = part.trim();
            if !t.is_empty() && !out.iter().any(|p| p == t) {
                out.push(t.to_owned());
            }
        }
    };
    if tool_name == "edit" {
        if let Some(input) = arguments.get("input").and_then(Value::as_str) {
            for p in hashline_paths(input) {
                push(&p);
            }
        }
        if let Some(patch) = arguments.get("patch").and_then(Value::as_str) {
            for p in extract_patch_paths(patch) {
                push(&p);
            }
        }
    }
    if tool_name == "apply_patch" {
        for key in ["patch", "input"] {
            if let Some(patch) = arguments.get(key).and_then(Value::as_str) {
                for p in extract_patch_paths(patch) {
                    push(&p);
                }
            }
        }
    }
    for key in [
        "path",
        "file_path",
        "target_path",
        "old_path",
        "new_path",
        "file",
    ] {
        if let Some(raw) = arguments.get(key).and_then(Value::as_str) {
            push(raw);
        }
    }
    if let Some(arr) = arguments.get("paths").and_then(Value::as_array) {
        for item in arr.iter().filter_map(Value::as_str) {
            push(item);
        }
    }
    out
}

/// Files touched by an `apply_patch` unified diff payload.
pub(crate) fn extract_patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            let rest = line
                .strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Update File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
                .or_else(|| line.strip_prefix("*** Move to: "))?;
            let trimmed = rest.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
        .collect()
}

/// Files a hashline `edit` payload touches: the `[PATH#TAG]` headers and `MV DEST` lines.
pub(crate) fn hashline_paths(input: &str) -> Vec<String> {
    input
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if let Some(dest) = line.strip_prefix("MV ") {
                let dest = dest.trim();
                let dest = dest
                    .strip_prefix('"')
                    .and_then(|d| d.strip_suffix('"'))
                    .or_else(|| dest.strip_prefix('\'').and_then(|d| d.strip_suffix('\'')))
                    .unwrap_or(dest);
                return Some(dest.to_owned());
            }
            let inner = line.strip_prefix('[')?.strip_suffix(']')?;
            inner
                .rsplit_once('#')
                .map(|(path, _)| path.trim().to_owned())
        })
        .collect()
}

/// Normalize `raw` against `cwd` into a slash-separated relative path (`a/b/c`), rejecting
/// traversal or symlink escapes outside `cwd`.
fn normalize_relative(
    cwd: &Path,
    raw: &str,
    fs: Option<&dyn WorkspaceFs>,
) -> std::result::Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("scope violation: empty file path".to_owned());
    }
    let cleaned = trimmed.replace('\\', "/");
    let candidate = PathBuf::from(&cleaned);
    let combined = if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(&candidate)
    };
    let cwd_resolved = canonicalize_with_ancestors(cwd, fs);
    let target_resolved = canonicalize_with_ancestors(&combined, fs);
    let cwd_norm = lexical_normalize(&cwd_resolved);
    let target_norm = lexical_normalize(&target_resolved);

    strip_prefix_ci(&target_norm, &cwd_norm).ok_or_else(|| {
        format!(
            "scope violation: `{trimmed}` escapes workspace root `{}`",
            cwd.display()
        )
    })
}

pub(crate) fn lexical_normalize(path: &Path) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => {
                parts.push(p.as_os_str().to_string_lossy().replace('\\', "/"));
            }
            Component::RootDir => parts.push("/".to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(parts.last().map(String::as_str), Some(last) if last != "/" && !last.ends_with(':'))
                {
                    parts.pop();
                }
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
        }
    }
    parts
}

pub(crate) fn eq_seg(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn strip_prefix_ci(target: &[String], base: &[String]) -> Option<String> {
    if target.len() < base.len() {
        return None;
    }
    for (t, b) in target.iter().zip(base.iter()) {
        if !eq_seg(t, b) {
            return None;
        }
    }
    let rest = target.get(base.len()..).unwrap_or_default();
    Some(rest.join("/"))
}

/// Match a slash-normalized relative path against a glob pattern (`*`, `**`, `?`) or directory
/// prefix (`crates/front/tui` matches `crates/front/tui/src/lib.rs`).
pub fn path_matches_pattern(rel_path: &str, pattern: &str) -> bool {
    let path_norm = rel_path.replace('\\', "/");
    let path_trimmed = path_norm.trim_start_matches("./").trim_matches('/');
    let glob_norm = pattern.replace('\\', "/");
    let glob_trimmed = glob_norm.trim().trim_start_matches("./").trim_matches('/');
    if glob_trimmed.is_empty() {
        return false;
    }
    let path_segs: Vec<&str> = path_trimmed.split('/').filter(|s| !s.is_empty()).collect();
    let glob_segs: Vec<&str> = glob_trimmed.split('/').filter(|s| !s.is_empty()).collect();

    if match_segments(&path_segs, &glob_segs) {
        return true;
    }
    if !glob_trimmed.contains(['*', '?']) && path_segs.len() > glob_segs.len() {
        return glob_segs
            .iter()
            .zip(path_segs.iter())
            .all(|(p, s)| eq_seg(p, s));
    }
    false
}

fn match_segments(path: &[&str], pat: &[&str]) -> bool {
    match (pat.first(), path.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            let rest_pat = pat.get(1..).unwrap_or_default();
            if match_segments(path, rest_pat) {
                return true;
            }
            if let Some(rest_path) = path.get(1..) {
                return match_segments(rest_path, pat);
            }
            false
        }
        (Some(p), Some(s)) => {
            wildcard_match(s, p)
                && match_segments(
                    path.get(1..).unwrap_or_default(),
                    pat.get(1..).unwrap_or_default(),
                )
        }
        _ => false,
    }
}

fn wildcard_match(text: &str, pat: &str) -> bool {
    let t: Vec<char> = if cfg!(windows) {
        text.to_ascii_lowercase().chars().collect()
    } else {
        text.chars().collect()
    };
    let p: Vec<char> = if cfg!(windows) {
        pat.to_ascii_lowercase().chars().collect()
    } else {
        pat.chars().collect()
    };
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);
    while ti < t.len() {
        let tc = t.get(ti).copied();
        let pc = p.get(pi).copied();
        if pc == Some('*') {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if pc == Some('?') || (pc.is_some() && pc == tc) {
            ti += 1;
            pi += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

/// Whether a tool target `rel` (possibly a glob pattern or directory for `ast_edit`) matches or
/// could overlap a `denied_paths` entry `pat`.
fn target_may_overlap_denied(rel: &str, pat: &str, is_dir_tool: bool) -> bool {
    if path_matches_pattern(rel, pat) {
        return true;
    }
    let rel_norm = rel.replace('\\', "/");
    let rel_trimmed = rel_norm.trim().trim_start_matches("./").trim_matches('/');
    let pat_norm = pat.replace('\\', "/");
    let pat_trimmed = pat_norm.trim().trim_start_matches("./").trim_matches('/');
    if pat_trimmed.is_empty() {
        return false;
    }
    let rel_is_glob = rel_trimmed.contains(['*', '?', '[']);
    if !rel_is_glob && !is_dir_tool {
        return false;
    }
    let pat_prefix = glob_static_prefix(pat_trimmed);
    if rel_is_glob {
        let rel_glob = flatten_char_classes(rel_trimmed);
        if path_matches_pattern(pat_trimmed, &rel_glob) {
            return true;
        }
        let rel_prefix = glob_static_prefix(rel_trimmed);
        if rel_prefix.is_empty() || pat_prefix.is_empty() {
            return true;
        }
        return rel_prefix == pat_prefix
            || is_prefix_dir(rel_prefix, pat_prefix)
            || (pat_trimmed.contains(['*', '?', '[']) && is_prefix_dir(pat_prefix, rel_prefix));
    }
    // Directory-walking mutating tool (`ast_edit`) targeting a concrete directory or `.`.
    rel_trimmed.is_empty()
        || pat_prefix.is_empty()
        || rel_trimmed == pat_prefix
        || is_prefix_dir(rel_trimmed, pat_prefix)
}

/// Leading path prefix before the first segment containing glob metacharacters (`*`, `?`, `[`).
fn glob_static_prefix(path: &str) -> &str {
    let mut end = 0usize;
    for (idx, seg) in path.split('/').enumerate() {
        if seg.contains(['*', '?', '[']) {
            return path.get(..end).unwrap_or("");
        }
        if idx > 0 {
            end += 1;
        }
        end += seg.len();
    }
    path
}

fn is_prefix_dir(ancestor: &str, child: &str) -> bool {
    let a_segs: Vec<&str> = ancestor.split('/').filter(|s| !s.is_empty()).collect();
    let c_segs: Vec<&str> = child.split('/').filter(|s| !s.is_empty()).collect();
    c_segs.len() > a_segs.len() && a_segs.iter().zip(c_segs.iter()).all(|(a, c)| eq_seg(a, c))
}

/// Replace glob character classes `[...]` with `?` so `path_matches_pattern` catches patterns
/// like `src/[s]ecret.rs`.
fn flatten_char_classes(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch == '[' {
            out.push('?');
            for inner in chars.by_ref() {
                if inner == ']' {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod tests;
