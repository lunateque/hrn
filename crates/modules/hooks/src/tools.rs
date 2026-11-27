//! The user-extensibility tools: custom tools discovered as `<name>.json` + `<name>` under a
//! `tools/` directory of any discovery layer (run as a subprocess with the JSON arguments on
//! stdin), and `manage_skill`, which writes skills into the user's own `~/.harness/skills`.

use crate::discover::ToolDef;
use crate::run;
use hrn_kernel::serde_json::{self, Value, json};
use hrn_kernel::{
    BoxFuture, HrnError, Result, Tool, ToolApproval, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The `manage_skill` description, as data.
const DESCRIPTION_MANAGE_SKILL: &str = include_str!("../prompts/manage_skill.md");

/// One custom tool: a JSON schema the user wrote, executed by running the file beside it.
pub(crate) struct CustomTool {
    spec: ToolSpec,
    program: PathBuf,
    module: Option<PathBuf>,
    timeout: Duration,
}

impl CustomTool {
    pub(crate) fn new(def: ToolDef, timeout: Duration) -> Self {
        let spec = ToolSpec {
            name: def.name.clone(),
            // Say where the tool came from: the model and the user must be able to tell a
            // project-local tool from a global one.
            description: format!("{} (from {}/tools)", def.description, def.layer),
            parameters: def.parameters,
        };
        Self {
            spec,
            program: def.program.unwrap_or_default(),
            module: def.module,
            timeout,
        }
    }

    /// What to spawn: the executable itself, or `node` for an `.mjs` module.
    fn invocation(&self) -> (PathBuf, Vec<String>) {
        match (&self.module, self.program.as_os_str().is_empty()) {
            (Some(module), true) => (
                PathBuf::from("node"),
                vec![module.to_string_lossy().into_owned()],
            ),
            _ => (self.program.clone(), Vec::new()),
        }
    }
}

impl Tool for CustomTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    /// A user program we know nothing about: assume it writes, and let the user approve it.
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    fn summarize_call(&self, args: &Value) -> String {
        format!("{} {}", self.spec.name, args)
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let (program, program_args) = self.invocation();
        let cwd = ctx.cwd.clone();
        let timeout = self.timeout;
        let cancel = ctx.cancel.clone();
        let name = self.spec.name.clone();
        let session_id = ctx.session_id.clone();
        Box::pin(async move {
            let payload = json!({
                "tool": name,
                "session_id": session_id,
                "cwd": cwd.to_string_lossy(),
                "arguments": args,
            });
            let result = run::run_json(
                &program,
                &program_args,
                &cwd,
                &payload.to_string(),
                timeout,
                Some(&cancel),
            )
            .await?;
            if result.timed_out {
                return Ok(ToolOutput::err(format!(
                    "tool `{name}` timed out after {}s",
                    timeout.as_secs()
                )));
            }
            let stdout = result.stdout.trim();
            // A tool answers with JSON (delivered verbatim) or plain text; stderr is an error
            // unless the run failed outright, in which case the exit code leads.
            let text = match serde_json::from_str::<Value>(stdout) {
                Ok(value) => value.to_string(),
                Err(_) => stdout.to_owned(),
            };
            let failed = result.code.is_none_or(|code| code != 0);
            if failed {
                let reason = if text.is_empty() {
                    result.stderr.trim().to_owned()
                } else if result.stderr.trim().is_empty() {
                    text
                } else {
                    format!("{text}\n{}", result.stderr.trim())
                };
                return Ok(ToolOutput::err(format!(
                    "tool `{name}` failed (exit {}): {reason}",
                    result.code.unwrap_or(-1)
                )));
            }
            if text.is_empty() {
                return Ok(ToolOutput::ok(format!("`{name}` completed with no output")));
            }
            Ok(ToolOutput::ok(text))
        })
    }
}

/// `manage_skill`: create, replace, delete or list skills in the user's own skills directory
/// (`~/.harness/skills/<name>/SKILL.md`), the layer every session discovers last (so it wins).
pub(crate) struct ManageSkillTool {
    home: Option<PathBuf>,
}

impl ManageSkillTool {
    pub(crate) fn new(home: Option<PathBuf>) -> Self {
        Self { home }
    }

    fn root(&self) -> Result<PathBuf> {
        self.home
            .as_deref()
            .map(|home| home.join(".harness").join("skills"))
            .ok_or_else(|| HrnError::Config("no home directory: cannot manage skills".to_owned()))
    }
}

impl Tool for ManageSkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "manage_skill".to_owned(),
            description: DESCRIPTION_MANAGE_SKILL.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "update", "delete", "list"],
                        "description": "What to do."
                    },
                    "name": {
                        "type": "string",
                        "description": "Skill name (one path segment, lowercase letters, digits, \
                                        '-' and '_'); required except for `list`."
                    },
                    "description": {
                        "type": "string",
                        "description": "One line saying when to use the skill (create/update)."
                    },
                    "content": {
                        "type": "string",
                        "description": "The SKILL.md body, Markdown (create/update)."
                    },
                    "autoload": {
                        "type": "boolean",
                        "description": "Inject the body into every session's system prompt \
                                        instead of only listing the skill (default false)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::exec()
    }

    fn execute(&self, _ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let this = ManageSkillTool {
            home: self.home.clone(),
        };
        Box::pin(async move { execute(&this, &args) })
    }
}

/// Longest skill name accepted (a filesystem segment, so keep it boring).
const MAX_NAME: usize = 64;

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('-')
        && !name.contains('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn execute(tool: &ManageSkillTool, args: &Value) -> Result<ToolOutput> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let root = tool.root()?;
    match action {
        "list" => return list(&root),
        "create" | "update" | "delete" => {}
        other => {
            return Ok(ToolOutput::err(format!(
                "unknown action `{other}` (create, update, delete, list)"
            )));
        }
    }
    let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
    if !valid_name(name) {
        return Ok(ToolOutput::err(format!(
            "`{name}` is not a skill name: use up to {MAX_NAME} lowercase letters, digits, '-' or '_'"
        )));
    }
    let dir = root.join(name);
    let file = dir.join("SKILL.md");
    match action {
        "delete" => {
            if !file.is_file() {
                return Ok(ToolOutput::err(format!(
                    "no skill `{name}` in {}",
                    root.display()
                )));
            }
            std::fs::remove_dir_all(&dir)
                .map_err(|e| HrnError::Io(format!("delete {}: {e}", dir.display())))?;
            Ok(ToolOutput::ok(format!("deleted skill `{name}`")))
        }
        "create" if file.is_file() => Ok(ToolOutput::err(format!(
            "skill `{name}` already exists at {}; use update",
            file.display()
        ))),
        _ => {
            let description = args
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let body = args.get("content").and_then(Value::as_str).unwrap_or("");
            if description.trim().is_empty() {
                return Ok(ToolOutput::err(
                    "a skill needs a one-line `description` (it is what the model sees)".to_owned(),
                ));
            }
            if body.trim().is_empty() {
                return Ok(ToolOutput::err(
                    "`content` (the SKILL.md body) is required".to_owned(),
                ));
            }
            let autoload = args
                .get("autoload")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let document = render(name, description, autoload, body);
            std::fs::create_dir_all(&dir)
                .map_err(|e| HrnError::Io(format!("create {}: {e}", dir.display())))?;
            std::fs::write(&file, document)
                .map_err(|e| HrnError::Io(format!("write {}: {e}", file.display())))?;
            let verb = if action == "create" {
                "created"
            } else {
                "updated"
            };
            Ok(ToolOutput::ok(format!(
                "{verb} skill `{name}` at {} (it applies to new sessions)",
                file.display()
            )))
        }
    }
}

fn render(name: &str, description: &str, autoload: bool, body: &str) -> String {
    let description = description.replace('\n', " ").trim().to_owned();
    let mut out = format!("---\nname: {name}\ndescription: {description}\n");
    if autoload {
        out.push_str("autoload: true\n");
    }
    out.push_str("---\n\n");
    out.push_str(body.trim());
    out.push('\n');
    out
}

fn list(root: &Path) -> Result<ToolOutput> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ToolOutput::ok(format!(
                "no skills yet in {}",
                root.display()
            )));
        }
        Err(err) => return Err(HrnError::Io(format!("read {}: {err}", root.display()))),
    };
    let mut lines = vec![format!("skills in {}:", root.display())];
    for entry in entries {
        let entry = entry.map_err(|e| HrnError::Io(format!("read {}: {e}", root.display())))?;
        let file = entry.path().join("SKILL.md");
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        let description = raw
            .lines()
            .find_map(|line| line.trim().strip_prefix("description:"))
            .unwrap_or("(no description)")
            .trim();
        let autoload = raw.lines().take(20).any(|l| l.trim() == "autoload: true");
        lines.push(format!(
            "- {}{}: {description}",
            entry.file_name().to_string_lossy(),
            if autoload { " [autoload]" } else { "" }
        ));
    }
    Ok(ToolOutput::ok(lines.join("\n")))
}
