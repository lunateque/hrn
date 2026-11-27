//! How the chat/exec launch flags land on a loaded `Config` (split out of `config.rs`):
//! instruction-source switches and `--hook` paths merged into the `[hooks]` module table.

use super::Config;
use hrn_harness::ContextSources;
use std::path::Path;

/// A `--system-prompt` / `--append-system-prompt` value: literal text, or the contents of the
/// named file when the value is an existing readable file (omp's `resolvePromptInput`).
#[must_use]
pub(crate) fn resolve_prompt_text(value: &str) -> String {
    let candidate = Path::new(value);
    if candidate.is_file()
        && let Ok(text) = std::fs::read_to_string(candidate)
    {
        return text.replace("\r\n", "\n").trim().to_owned();
    }
    value.to_owned()
}

/// Override `[lsp] enabled = false` when `--no-lsp` is passed on the CLI (no-op when `false`
/// so removing the `lsp` module never sees an unclaimed `[lsp]` section).
pub(crate) fn apply_no_lsp(
    cfg: &mut Config,
    input: Option<&mut crate::config_layers::LayerInput>,
    no_lsp: bool,
) {
    if !no_lsp {
        return;
    }
    let table = cfg
        .module_tables
        .entry("lsp".to_owned())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(map) = table.as_object_mut() {
        map.insert("enabled".to_owned(), serde_json::Value::Bool(false));
    }
    if let Some(layer_input) = input {
        crate::config_layers::insert_dotted(
            &mut layer_input.runtime,
            "lsp.enabled",
            toml::Value::Boolean(false),
        );
    }
}

/// Apply the rpc-mode prompt flags (`--system-prompt` / `--append-system-prompt` / `--no-title`) to `cfg`.
pub(crate) fn apply_prompt_flags(cfg: &mut Config, args: &crate::cli::ServeArgs) {
    cfg.system_prompt_override = args.system_prompt.as_deref().map(resolve_prompt_text);
    cfg.append_system_override = args
        .append_system_prompt
        .as_deref()
        .map(resolve_prompt_text);
    if args.no_title {
        cfg.auto_title = false;
        cfg.title_refresh_on_replan = false;
    }
}

/// Apply the chat/exec instruction-source flags to `cfg`: the source switches themselves, the
/// `--system-prompt` / `--append-system-prompt` overrides (text, or the contents of the named
/// file) and the `--hook` paths, injected into the `[hooks]` module table so the module sees
/// them.
pub(crate) fn apply_source_flags(cfg: &mut Config, flags: &crate::cli::SourceArgs) {
    let skill_patterns = flags
        .skills
        .as_deref()
        .map(hrn_harness::parse_skill_patterns)
        .filter(|p| !p.is_empty());
    cfg.sources = ContextSources {
        skill_patterns,
        skills: !flags.no_skills,
        rules: !flags.no_rules,
        context_files: !flags.no_context_files,
    };
    cfg.system_prompt_override = flags.system_prompt.as_deref().map(resolve_prompt_text);
    cfg.append_system_override = flags
        .append_system_prompt
        .as_deref()
        .map(resolve_prompt_text);
    if flags.hook.is_empty() {
        return;
    }
    let table = cfg
        .module_tables
        .entry("hooks".to_owned())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let extra = table
        .as_object_mut()
        .map(|map| {
            map.entry("extra")
                .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        })
        .and_then(serde_json::Value::as_array_mut);
    let Some(extra) = extra else {
        return;
    };
    extra.extend(
        flags
            .hook
            .iter()
            .map(|path| serde_json::Value::String(path.display().to_string())),
    );
}
