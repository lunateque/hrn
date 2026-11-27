//! Filesystem-backed internal URL schemes: `local://` (per-session-tree scratch files and spilled tool
//! output) and `skill://` (skills found by the harness discovery, plus their sibling files).

use hrn_kernel::{HrnError, Result, ToolContext, UrlBody, UrlScheme};
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A relative path from URL text: no `..`, no drive or backslash tricks.
fn safe_rel(rest: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                return Err(HrnError::Tool(format!("`..` is not allowed in `{rest}`")));
            }
            s if s.contains(['\\', ':']) => {
                return Err(HrnError::Tool(format!(
                    "`{rest}` must be a plain relative path"
                )));
            }
            s => out.push(s),
        }
    }
    Ok(out)
}

fn io_err(action: &str, path: &Path, err: &io::Error) -> HrnError {
    HrnError::Io(format!("cannot {action} `{}`: {err}", path.display()))
}

/// `local://<name>`: scratch files shared by a session and its subagents, stored under
/// `root/<root session id>/`.
pub struct LocalScheme {
    root: PathBuf,
}

impl LocalScheme {
    /// Keep every session tree's scratch directory under `root` (created on first write).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn session_dir(&self, ctx: &ToolContext) -> PathBuf {
        let id: String = ctx
            .root_session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(id)
    }

    fn file(&self, ctx: &ToolContext, rest: &str) -> Result<PathBuf> {
        let rel = safe_rel(rest)?;
        if rel.as_os_str().is_empty() {
            return Err(HrnError::Tool("`local://` needs a file name".into()));
        }
        Ok(self.session_dir(ctx).join(rel))
    }

    /// The next free `N` for `N.log` in `dir` (one past the highest existing).
    fn next_spill_number(dir: &Path) -> Result<u64> {
        let mut next = 1;
        match fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| io_err("list", dir, &e))?;
                    let name = entry.file_name();
                    if let Some(n) = name
                        .to_str()
                        .and_then(|n| n.strip_suffix(".log"))
                        .and_then(|n| n.parse::<u64>().ok())
                    {
                        next = next.max(n + 1);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("list", dir, &e)),
        }
        Ok(next)
    }
}

impl UrlScheme for LocalScheme {
    fn scheme(&self) -> &'static str {
        "local"
    }

    fn about(&self) -> &'static str {
        "this session's scratch files; `local://<name>` is readable and writable (`write_file`), \
         and tool output cut for length is saved whole as `local://N.log`"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        if safe_rel(rest)?.as_os_str().is_empty() {
            let dir = self.session_dir(ctx);
            return Ok(if dir.is_dir() {
                UrlBody::Path(dir)
            } else {
                UrlBody::Text("(no local files yet)".to_owned())
            });
        }
        let path = self.file(ctx, rest)?;
        if !path.exists() {
            return Err(HrnError::Tool(format!(
                "`local://{rest}` does not exist in this session"
            )));
        }
        Ok(UrlBody::Path(path))
    }

    fn write(&self, ctx: &ToolContext, rest: &str, content: &str) -> Result {
        let path = self.file(ctx, rest)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err("create", parent, &e))?;
        }
        fs::write(&path, content).map_err(|e| io_err("write", &path, &e))
    }

    fn spill(&self, ctx: &ToolContext, content: &str) -> Result<String> {
        let dir = self.session_dir(ctx);
        fs::create_dir_all(&dir).map_err(|e| io_err("create", &dir, &e))?;
        let mut n = Self::next_spill_number(&dir)?;
        loop {
            let path = dir.join(format!("{n}.log"));
            // `create_new` keeps parallel tool calls from overwriting each other's spill.
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(content.as_bytes())
                        .map_err(|e| io_err("write", &path, &e))?;
                    return Ok(format!("local://{n}.log"));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(io_err("create", &path, &e)),
            }
        }
    }
}

/// Lists the discovered skills for a working directory as `(name, SKILL.md path)`.
pub type SkillResolver = Arc<dyn Fn(&Path) -> Result<Vec<(String, PathBuf)>> + Send + Sync>;

/// `skill://<name>[/path]`: a skill's `SKILL.md`, or a file beside it.
pub struct SkillScheme {
    resolver: SkillResolver,
}

impl SkillScheme {
    /// Resolve skills through `resolver` (the harness skill discovery, wired by the binary).
    pub fn new(resolver: SkillResolver) -> Self {
        Self { resolver }
    }
}

impl UrlScheme for SkillScheme {
    fn scheme(&self) -> &'static str {
        "skill"
    }

    fn about(&self) -> &'static str {
        "discovered skills; `skill://<name>` is its SKILL.md, `skill://<name>/<path>` a file \
         beside it, bare `skill://` lists them"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let skills = (self.resolver)(&ctx.cwd)?;
        let rest = rest.trim_matches('/');
        if rest.is_empty() {
            let listing = skills
                .iter()
                .map(|(name, path)| format!("{name}  ({})", path.display()))
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(UrlBody::Text(if listing.is_empty() {
                "(no skills discovered)".to_owned()
            } else {
                listing
            }));
        }
        let (name, sub) = rest.split_once('/').unwrap_or((rest, ""));
        let Some((_, skill_file)) = skills.iter().find(|(n, _)| n.eq_ignore_ascii_case(name))
        else {
            let known: Vec<&str> = skills.iter().map(|(n, _)| n.as_str()).collect();
            return Err(HrnError::Tool(format!(
                "unknown skill `{name}` (known: {})",
                known.join(", ")
            )));
        };
        let rel = safe_rel(sub)?;
        if rel.as_os_str().is_empty() {
            return Ok(UrlBody::Path(skill_file.clone()));
        }
        let path = skill_file.parent().unwrap_or(Path::new("")).join(rel);
        if !path.exists() {
            return Err(HrnError::Tool(format!("`skill://{rest}` does not exist")));
        }
        Ok(UrlBody::Path(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, test_ctx};

    #[test]
    fn local_writes_reads_and_spills_per_session_tree() {
        let root = TempDir::new("local");
        let scheme = LocalScheme::new(root.path().to_path_buf());
        let ctx = test_ctx(root.path().to_path_buf());
        scheme.write(&ctx, "notes/a.txt", "hi").unwrap();
        let UrlBody::Path(path) = scheme.read(&ctx, "notes/a.txt").unwrap() else {
            panic!("expected a path");
        };
        assert_eq!(fs::read_to_string(path).unwrap(), "hi");
        assert_eq!(scheme.spill(&ctx, "one").unwrap(), "local://1.log");
        assert_eq!(scheme.spill(&ctx, "two").unwrap(), "local://2.log");
        let mut other = ctx.clone();
        other.session_id = "other".to_owned();
        other.root_session_id = "other".to_owned();
        assert_eq!(scheme.spill(&other, "x").unwrap(), "local://1.log");
        assert!(scheme.read(&other, "notes/a.txt").is_err());
        let mut child = ctx.clone();
        child.session_id = "child".to_owned();
        assert!(scheme.read(&child, "notes/a.txt").is_ok());
        assert!(scheme.read(&ctx, "../escape").is_err());
        assert!(scheme.write(&ctx, "a/../../b", "x").is_err());
    }

    #[test]
    fn skill_resolves_by_name_and_sibling_path() {
        let dir = TempDir::new("skill");
        let md = dir.write("s/demo/SKILL.md", "# demo");
        dir.write("s/demo/ref/notes.md", "n");
        let md2 = md.clone();
        let scheme = SkillScheme::new(Arc::new(move |_| {
            Ok(vec![("demo".to_owned(), md2.clone())])
        }));
        let ctx = test_ctx(dir.path().to_path_buf());
        assert_eq!(scheme.read(&ctx, "demo").unwrap(), UrlBody::Path(md));
        let UrlBody::Path(p) = scheme.read(&ctx, "DEMO/ref/notes.md").unwrap() else {
            panic!("expected a path");
        };
        assert!(p.ends_with("ref/notes.md"));
        assert!(scheme.read(&ctx, "nope").is_err());
        assert!(scheme.read(&ctx, "demo/../x").is_err());
    }
}
