//! Finding the user's extensions: hook executables under `<root>/hooks/`, custom tools under
//! `<root>/tools/` (a `<name>.json` schema next to a `<name>` executable or an `.mjs` module),
//! and the Claude-Code-style `hooks` map in each layer's `settings.json`.

use crate::config::EVENTS;
use hrn_kernel::serde_json::{self, Value};
use hrn_kernel::{Result, SourceLayer, resource_dirs};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// One discovered hook executable; it receives every event and self-selects on `hook_event_name`.
pub(crate) struct HookExecutable {
    pub layer: String,
    pub path: PathBuf,
}

/// One discovered custom tool: the schema JSON plus how to run it.
#[derive(Debug)]
pub(crate) struct ToolDef {
    pub layer: String,
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// `None` for a `.mjs` module (run through `node`).
    pub program: Option<PathBuf>,
    pub module: Option<PathBuf>,
}

/// A Claude-style settings file command entry: `(event, matcher, command, timeout_secs)`.
pub(crate) struct SettingsCommand {
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout_secs: Option<u64>,
}

fn layer_label(layer: &SourceLayer) -> String {
    match layer {
        SourceLayer::Global(label) | SourceLayer::Project(label) => label.clone(),
    }
}

/// Is this file something the user can drop into `hooks/` or `tools/` and have run? On Unix the
/// mode bit decides; on Windows the extension must name a form `CreateProcess` can start (`.ps1`
/// and friends are not runnable directly — they belong in a `settings.json` command line that
/// spells their interpreter).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        RUNNABLE_EXTENSIONS.iter().any(|ext| {
            path.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(*ext))
        }) && path.is_file()
    }
}

#[cfg(not(unix))]
const RUNNABLE_EXTENSIONS: [&str; 4] = ["exe", "cmd", "bat", "com"];

/// Hook executables from every layer's `hooks/` directory, in precedence order (later layers
/// win name collisions).
pub(crate) fn discover_hook_executables(home: Option<&Path>, cwd: &Path) -> Vec<HookExecutable> {
    let mut out = Vec::new();
    for (layer, dir) in resource_dirs(home, cwd, "hooks") {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_executable(&path) {
                out.push(HookExecutable {
                    layer: layer_label(&layer),
                    path,
                });
            }
        }
    }
    out
}

/// Custom tools: a `<name>.json` schema plus either a `<name>` executable or a `<name>.mjs`
/// module in each layer's `tools/` directory (later layers override same-named tools).
pub(crate) fn discover_custom_tools(
    home: Option<&Path>,
    cwd: &Path,
) -> (Vec<ToolDef>, Vec<hrn_kernel::HrnError>) {
    let mut tools: BTreeMap<String, ToolDef> = BTreeMap::new();
    let mut errors = Vec::new();
    for (layer, dir) in resource_dirs(home, cwd, "tools") {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let label = layer_label(&layer);
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match load_tool_def(&label, stem, &path, &dir) {
                Ok(def) => {
                    tools.insert(stem.to_ascii_lowercase(), def);
                }
                Err(err) => errors.push(err),
            }
        }
    }
    (tools.into_values().collect(), errors)
}

fn load_tool_def(layer: &str, name: &str, schema: &Path, dir: &Path) -> Result<ToolDef> {
    let raw = fs::read_to_string(schema)
        .map_err(|e| hrn_kernel::HrnError::Io(format!("read {}: {e}", schema.display())))?;
    let value: Value = serde_json::from_str(&raw).map_err(|e| {
        hrn_kernel::HrnError::Config(format!("invalid tool schema {}: {e}", schema.display()))
    })?;
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("(no description)")
        .to_owned();
    let parameters = value
        .get("parameters")
        .or_else(|| value.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
    let program = [".exe", ".cmd", ".bat", ".com", ""]
        .iter()
        .map(|ext| dir.join(format!("{name}{ext}")))
        .find(|candidate| candidate != schema && is_executable(candidate));
    // A `.mjs` module runs through `node`, so it only has to exist.
    let module_path = dir.join(format!("{name}.mjs"));
    let module = module_path.is_file().then_some(module_path);
    if program.is_none() && module.is_none() {
        return Err(hrn_kernel::HrnError::Config(format!(
            "tool `{name}` in {layer} has {name}.json but no runnable `{name}` (or `{name}.mjs`)"
        )));
    }
    Ok(ToolDef {
        layer: layer.to_owned(),
        name: name.to_owned(),
        description,
        parameters,
        program,
        module,
    })
}

/// Translate one Claude-Code `hooks` map (`{"PreToolUse": [{"matcher": ..., "hooks": [...]}]}`)
/// into our event commands. Unknown events and non-`command` handler types are skipped: they are
/// Claude-specific (prompt/agent handlers) rather than silently run wrong.
pub(crate) fn claude_commands(value: &Value) -> Vec<SettingsCommand> {
    let mut out = Vec::new();
    let Some(map) = value.get("hooks").and_then(Value::as_object) else {
        return out;
    };
    for (claude_event, groups) in map {
        let Some(event) = translate_event(claude_event) else {
            continue;
        };
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for group in groups {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for handler in handlers {
                if handler.get("type").and_then(Value::as_str) != Some("command") {
                    continue;
                }
                let Some(command) = handler.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let timeout_secs = handler
                    .get("timeout")
                    .and_then(Value::as_u64)
                    .or_else(|| group.get("timeout").and_then(Value::as_u64));
                out.push(SettingsCommand {
                    event: event.clone(),
                    matcher: matcher.clone(),
                    command: command.to_owned(),
                    timeout_secs,
                });
            }
        }
    }
    out
}

fn translate_event(claude_event: &str) -> Option<String> {
    let mapped = match claude_event {
        "PreToolUse" => "pre_tool",
        "PostToolUse" => "post_tool",
        "Stop" => "turn_end",
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt",
        _ => return None,
    };
    EVENTS.contains(&mapped).then(|| mapped.to_owned())
}

/// Claude-style settings files (`settings.json`, `settings.local.json`) from every discovery
/// layer that has one; later layers' entries run in addition to earlier ones.
pub(crate) fn discover_claude_settings(home: Option<&Path>, cwd: &Path) -> Vec<SettingsCommand> {
    let mut out = Vec::new();
    for (_layer, dir) in resource_dirs(home, cwd, "hooks") {
        // `resource_dirs` yields `<root>/hooks`; settings live at `<root>/settings.json`.
        let Some(root) = dir.parent() else { continue };
        for name in ["settings.json", "settings.local.json"] {
            let path = root.join(name);
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            out.extend(claude_commands(&value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_settings_translate_only_supported_events() {
        let value: Value = serde_json::from_str(
            r#"{
              "hooks": {
                "PreToolUse": [
                  {"matcher": "Bash", "hooks": [
                    {"type": "command", "command": "guard.sh", "timeout": 3},
                    {"type": "prompt", "prompt": "ignore me"}
                  ]}
                ],
                "SessionEnd": [{"hooks": [{"type": "command", "command": "nope"}]}]
              }
            }"#,
        )
        .expect("json");
        let commands = claude_commands(&value);
        assert_eq!(
            commands.len(),
            1,
            "unsupported events and handler types drop out"
        );
        assert_eq!(commands[0].event, "pre_tool");
        assert_eq!(commands[0].matcher.as_deref(), Some("Bash"));
        assert_eq!(commands[0].command, "guard.sh");
        assert_eq!(commands[0].timeout_secs, Some(3));
    }

    #[test]
    fn tool_definitions_pair_a_schema_with_its_program_or_module() {
        let root = std::env::temp_dir().join(format!("hrn-tools-test-{}", std::process::id()));
        let dir = root.join(".harness").join("tools");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("wordcount.json"),
            r#"{"description":"count words","parameters":{"type":"object"}}"#,
        )
        .expect("schema");
        std::fs::write(dir.join("wordcount.mjs"), "export const x = 1").expect("module");
        std::fs::write(
            dir.join("broken.json"),
            r#"{"description":"no program beside me"}"#,
        )
        .expect("schema");
        let home = root.join("nowhere");
        let (defs, errors) = {
            // Project layer only: pass cwd = root so `<root>/.harness/tools` is the project dir.
            discover_custom_tools(Some(&home), &root)
        };
        assert_eq!(defs.len(), 1, "{defs:?}");
        assert_eq!(defs[0].name, "wordcount");
        assert!(defs[0].program.is_none() && defs[0].module.is_some());
        assert_eq!(errors.len(), 1, "{errors:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
