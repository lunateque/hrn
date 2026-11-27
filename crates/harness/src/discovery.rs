//! Shared auto-discovery paths and lightweight YAML front-matter parser for agents, skills,
//! and custom slash commands.

use hrn_kernel::WorkspaceFs;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// Where an agent profile, skill, or custom command was discovered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoverySource {
    /// Embedded default shipped with `hrn`.
    BuiltIn,
    /// User-global directory (e.g. `~/.harness`, `~/.omp/agent`, `~/.claude`).
    Global(String),
    /// Workspace-local directory (e.g. `.harness`, `.agent`, `.claude`).
    Project(String),
}

impl fmt::Display for DiscoverySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuiltIn => f.write_str("built-in"),
            Self::Global(label) | Self::Project(label) => f.write_str(label),
        }
    }
}

/// Ordered discovery directories for `subdir` (`"agents"`, `"skills"`, `"commands"`),
/// from lowest to highest precedence so `~/.harness` and project `.harness` always win.
///
/// Only markdown resource subdirectories are ever consulted — never auth or secret files.
pub fn discovery_dirs(
    fs: &dyn WorkspaceFs,
    cwd: &Path,
    subdir: &str,
) -> Vec<(DiscoverySource, PathBuf)> {
    hrn_kernel::resource_dirs(fs.home_dir().as_deref(), cwd, subdir)
        .into_iter()
        .map(|(layer, path)| {
            let source = match layer {
                hrn_kernel::SourceLayer::Global(label) => DiscoverySource::Global(label),
                hrn_kernel::SourceLayer::Project(label) => DiscoverySource::Project(label),
            };
            (source, path)
        })
        .collect()
}

/// Parsed `---` front matter supporting scalar `key: value` fields, inline or bullet lists,
/// and indented multi-line blocks (e.g. JSON schemas or nested YAML mappings).
/// Keys are normalized to lowercase with hyphens turned into underscores.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrontMatter {
    scalars: BTreeMap<String, String>,
    lists: BTreeMap<String, Vec<String>>,
    blocks: BTreeMap<String, String>,
    present: BTreeSet<String>,
}

impl FrontMatter {
    /// Whether `key` appeared anywhere in the front matter (even with an empty value/block).
    pub fn contains(&self, key: &str) -> bool {
        self.present.contains(&normalize_key(key))
    }

    /// Whether any of `keys` appeared in the front matter.
    pub fn has_any(&self, keys: &[&str]) -> bool {
        keys.iter().any(|k| self.contains(k))
    }

    /// Look up a normalized scalar field (`"thinking_level"` matches `thinking-level`).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.scalars.get(&normalize_key(key)).map(String::as_str)
    }

    /// Look up the raw indented multi-line block recorded under `key:`.
    pub fn get_block(&self, key: &str) -> Option<&str> {
        self.blocks.get(&normalize_key(key)).map(String::as_str)
    }

    /// Look up the first present scalar among candidate `keys`.
    pub fn get_any(&self, keys: &[&str]) -> Option<&str> {
        keys.iter().find_map(|k| self.get(k))
    }

    /// Look up a list field: supports YAML bullet lists (`- item`), `[a, b]`, or `a, b`.
    pub fn get_list(&self, keys: &[&str]) -> Vec<String> {
        for key in keys {
            let norm = normalize_key(key);
            if let Some(items) = self.lists.get(&norm) {
                return items.clone();
            }
            if let Some(raw) = self.scalars.get(&norm) {
                return parse_inline_list(raw);
            }
        }
        Vec::new()
    }
}

fn normalize_key(key: &str) -> String {
    key.trim().to_ascii_lowercase().replace('-', "_")
}

fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && ((bytes.first() == Some(&b'"') && bytes.last() == Some(&b'"'))
            || (bytes.first() == Some(&b'\'') && bytes.last() == Some(&b'\'')))
    {
        return trimmed
            .get(1..trimmed.len() - 1)
            .unwrap_or(trimmed)
            .to_owned();
    }
    trimmed.to_owned()
}

fn parse_inline_list(raw: &str) -> Vec<String> {
    let inner = raw
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw);
    inner
        .split(',')
        .map(unquote)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Split optional `---` front matter from a markdown body.
/// If an opening `---` has no closing delimiter, returns empty metadata and the original text.
pub fn parse_front_matter(raw: &str) -> (FrontMatter, &str) {
    let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (FrontMatter::default(), raw);
    };

    let mut fm = FrontMatter::default();
    let mut active_block_key: Option<String> = None;
    let mut offset = 0usize;

    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let trimmed = line.trim();
        if trimmed == "---" {
            return (fm, rest.get(offset..).unwrap_or_default());
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        if let Some(key) = active_block_key.as_ref()
            && (is_indented || trimmed.starts_with("- ") || trimmed.starts_with("-\t"))
        {
            if let Some(item) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("-\t"))
            {
                let value = unquote(item);
                if !value.is_empty() {
                    fm.lists.entry(key.clone()).or_default().push(value);
                }
            }
            let block = fm.blocks.entry(key.clone()).or_default();
            if !block.is_empty() {
                block.push('\n');
            }
            block.push_str(trimmed);
            continue;
        }
        if let Some((raw_key, raw_val)) = trimmed.split_once(':') {
            let key = normalize_key(raw_key);
            if key.is_empty() {
                active_block_key = None;
                continue;
            }
            fm.present.insert(key.clone());
            let val = raw_val.trim();
            if val.is_empty() {
                active_block_key = Some(key);
            } else {
                active_block_key = None;
                fm.scalars.insert(key, unquote(val));
            }
        }
    }

    (FrontMatter::default(), raw)
}

/// First non-empty markdown body line (stripping leading `#` heading markers).
pub fn first_summary_line<'a>(body: &'a str, fallback: &'a str) -> String {
    body.lines()
        .find(|l| !l.trim().is_empty())
        .map_or(fallback, str::trim)
        .trim_start_matches('#')
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MemFs;

    #[test]
    fn front_matter_parses_scalars_hyphens_quotes_lists_and_blocks() {
        let raw = "\
---
name: \"My Agent\"
thinking-level: high
tools:
  - read
  - grep
deny-tools: [bash, write]
prewalk:
  into: @smol
output:
  {
    \"type\": \"object\"
  }
unknown_key: ignored
---
Body line here.
";
        let (fm, body) = parse_front_matter(raw);
        assert_eq!(fm.get("name"), Some("My Agent"));
        assert_eq!(fm.get_any(&["thinking", "thinking_level"]), Some("high"));
        assert_eq!(fm.get_list(&["tools"]), vec!["read", "grep"]);
        assert_eq!(fm.get_list(&["deny_tools"]), vec!["bash", "write"]);
        assert!(fm.contains("prewalk"));
        assert!(fm.get("into").is_none(), "indented sub-keys must not leak");
        assert!(
            fm.get_block("output")
                .is_some_and(|b| b.contains("\"type\": \"object\""))
        );
        assert_eq!(body.trim(), "Body line here.");
    }

    #[test]
    fn unclosed_front_matter_is_treated_as_plain_body() {
        let raw = "---\nmodel: fast\nno closing fence";
        let (fm, body) = parse_front_matter(raw);
        assert!(fm.get("model").is_none());
        assert_eq!(body, raw);
    }

    #[test]
    fn discovery_dirs_orders_global_then_project_with_harness_highest() {
        let fs = MemFs::default().with_home("/home/u");
        let dirs = discovery_dirs(&fs, Path::new("/ws"), "skills");
        let labels: Vec<String> = dirs.iter().map(|(s, _)| s.to_string()).collect();
        assert_eq!(
            labels,
            vec![
                "~/.agents",
                "~/.agent",
                "~/.codex",
                "~/.claude",
                "~/.omp/agent",
                "~/.harness",
                ".claude",
                ".agent",
                ".omp",
                ".harness",
            ]
        );
    }
}
