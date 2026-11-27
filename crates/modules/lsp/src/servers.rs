//! User/project `lsp.toml` shallow-merge loader, path matching, and cwd-only root-marker detection.

use crate::config::{LspServerSpec, builtin_server_specs};
use crate::uri::language_id_for_path;
use hrn_edge_tools::parse_toml_to_json;
use hrn_kernel::serde::Deserialize;
use hrn_kernel::serde_json::{self, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Deserialize)]
#[serde(crate = "hrn_kernel::serde", untagged)]
enum CommandSpec {
    One(String),
    Many(Vec<String>),
}

impl CommandSpec {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) if !s.trim().is_empty() => vec![s],
            Self::One(_) => Vec::new(),
            Self::Many(list) => list.into_iter().filter(|s| !s.trim().is_empty()).collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(crate = "hrn_kernel::serde")]
struct RawServerEntry {
    name: String,
    command: Option<CommandSpec>,
    args: Option<Vec<String>>,
    file_types: Option<Vec<String>>,
    language_id: Option<String>,
    languages: Option<Vec<String>>,
    root_markers: Option<Vec<String>>,
    init_options: Option<Value>,
    settings: Option<Value>,
    disabled: Option<bool>,
    warmup_timeout_ms: Option<u64>,
    is_linter: Option<bool>,
    env: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(crate = "hrn_kernel::serde")]
struct RawLspFile {
    idle_timeout_secs: Option<u64>,
    idle_timeout_ms: Option<u64>,
    #[serde(default)]
    servers: Vec<RawServerEntry>,
}

/// Trust boundary of a user `lsp.toml` file: `<data_dir>/lsp.toml` is operator-trusted while
/// `<cwd>/.harness/lsp.toml` is project-scoped and may only disable or tune known servers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LayerTrust {
    Operator,
    Project,
}

/// Result of reading `<data_dir>/lsp.toml` + `<cwd>/.harness/lsp.toml` over built-ins.
pub(crate) struct LoadedServers {
    pub(crate) servers: Vec<LspServerSpec>,
    pub(crate) idle_timeout_override: Option<Duration>,
    pub(crate) warnings: Vec<String>,
}

/// Load built-in servers and shallow-merge `<data_dir>/lsp.toml` followed by `<cwd>/.harness/lsp.toml`.
pub(crate) fn load_merged_servers(data_dir: Option<&Path>, cwd: &Path) -> LoadedServers {
    let mut servers = builtin_server_specs();
    let mut idle_timeout_override = None;
    let mut warnings = Vec::new();

    if let Some(dir) = data_dir {
        let user_path = dir.join("lsp.toml");
        merge_layer_file(
            &user_path,
            LayerTrust::Operator,
            &mut servers,
            &mut idle_timeout_override,
            &mut warnings,
        );
    }
    let project_path = cwd.join(".harness").join("lsp.toml");
    merge_layer_file(
        &project_path,
        LayerTrust::Project,
        &mut servers,
        &mut idle_timeout_override,
        &mut warnings,
    );
    LoadedServers {
        servers,
        idle_timeout_override,
        warnings,
    }
}

fn merge_layer_file(
    path: &Path,
    trust: LayerTrust,
    servers: &mut Vec<LspServerSpec>,
    idle_override: &mut Option<Duration>,
    warnings: &mut Vec<String>,
) {
    if !path.is_file() {
        return;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!("ignored unreadable `{}`: {e}", path.display()));
            return;
        }
    };
    let json_val = match parse_toml_to_json(&text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("ignored invalid `{}`: {e}", path.display()));
            return;
        }
    };
    let parsed: RawLspFile = match serde_json::from_value(json_val) {
        Ok(f) => f,
        Err(e) => {
            warnings.push(format!("ignored malformed `{}`: {e}", path.display()));
            return;
        }
    };
    if trust == LayerTrust::Operator {
        if let Some(secs) = parsed.idle_timeout_secs {
            *idle_override = Some(Duration::from_secs(secs));
        } else if let Some(ms) = parsed.idle_timeout_ms {
            *idle_override = Some(Duration::from_millis(ms));
        }
    } else if parsed.idle_timeout_secs.is_some() || parsed.idle_timeout_ms.is_some() {
        warnings.push(format!(
            "{}: ignored project override of `idle_timeout_secs`/`idle_timeout_ms`",
            path.display()
        ));
    }
    for entry in parsed.servers {
        apply_server_entry(entry, trust, path, servers, warnings);
    }
}

fn apply_server_entry(
    entry: RawServerEntry,
    trust: LayerTrust,
    source: &Path,
    servers: &mut Vec<LspServerSpec>,
    warnings: &mut Vec<String>,
) {
    let name = entry.name.trim().to_owned();
    if name.is_empty() {
        warnings.push(format!(
            "{}: skipped `[[servers]]` entry with empty name",
            source.display()
        ));
        return;
    }
    if let Some(existing) = servers.iter_mut().find(|s| s.name == name) {
        shallow_merge_existing(existing, entry, trust, source, warnings);
        return;
    }
    if trust == LayerTrust::Project {
        warnings.push(format!(
            "{}: ignored untrusted project LSP server `{name}` (define custom executables in `<data_dir>/lsp.toml`)",
            source.display()
        ));
        return;
    }
    let commands = entry.command.map(CommandSpec::into_vec).unwrap_or_default();
    let languages = collect_languages(entry.language_id, entry.languages);
    let file_types = entry.file_types.unwrap_or_default();
    let disabled = entry.disabled.unwrap_or(false);
    if !disabled && (commands.is_empty() || (languages.is_empty() && file_types.is_empty())) {
        warnings.push(format!(
            "{}: skipped invalid LSP server `{name}` (requires `command` and `file_types` or `language_id`)",
            source.display()
        ));
        return;
    }
    servers.push(LspServerSpec {
        name,
        languages,
        file_types,
        commands,
        args: entry.args.unwrap_or_default(),
        root_markers: entry.root_markers.unwrap_or_default(),
        env: entry.env.unwrap_or_default(),
        init_options: entry.init_options,
        settings: entry.settings,
        disabled,
        warmup_timeout_ms: entry.warmup_timeout_ms,
        is_linter: entry.is_linter.unwrap_or(false),
    });
}

fn shallow_merge_existing(
    existing: &mut LspServerSpec,
    entry: RawServerEntry,
    trust: LayerTrust,
    source: &Path,
    warnings: &mut Vec<String>,
) {
    if trust == LayerTrust::Project {
        let has_untrusted_fields = entry.command.is_some()
            || entry.args.is_some()
            || entry.env.is_some()
            || entry.init_options.is_some()
            || entry.settings.is_some()
            || entry.root_markers.is_some()
            || entry.file_types.is_some()
            || entry.language_id.is_some()
            || entry.languages.is_some()
            || entry.warmup_timeout_ms.is_some()
            || entry.is_linter.is_some();
        if has_untrusted_fields {
            warnings.push(format!(
                "{}: ignored untrusted project LSP fields for `{}` (project `.harness/lsp.toml` may only set `disabled = true`)",
                source.display(),
                existing.name
            ));
        }
        match entry.disabled {
            Some(true) => existing.disabled = true,
            Some(false) if existing.disabled => {
                warnings.push(format!(
                    "{}: ignored project attempt to re-enable operator-disabled server `{}`",
                    source.display(),
                    existing.name
                ));
            }
            _ => {}
        }
        return;
    }
    if let Some(cmd) = entry.command {
        let cmds = cmd.into_vec();
        if !cmds.is_empty() {
            existing.commands = cmds;
        }
    }
    if let Some(args) = entry.args {
        existing.args = args;
    }
    if let Some(env) = entry.env {
        existing.env = env;
    }
    let langs = collect_languages(entry.language_id, entry.languages);
    if !langs.is_empty() {
        existing.languages = langs;
    }
    if let Some(ft) = entry.file_types {
        existing.file_types = ft;
    }
    if let Some(rm) = entry.root_markers {
        existing.root_markers = rm;
    }
    if let Some(init) = entry.init_options {
        existing.init_options = Some(init);
    }
    if let Some(settings) = entry.settings {
        existing.settings = Some(settings);
    }
    if let Some(disabled) = entry.disabled {
        existing.disabled = disabled;
    }
    if let Some(ms) = entry.warmup_timeout_ms {
        existing.warmup_timeout_ms = Some(ms);
    }
    if let Some(linter) = entry.is_linter {
        existing.is_linter = linter;
    }
}

fn collect_languages(language_id: Option<String>, languages: Option<Vec<String>>) -> Vec<String> {
    let mut out = languages.unwrap_or_default();
    if let Some(id) = language_id
        && !id.trim().is_empty()
        && !out.iter().any(|l| l == &id)
    {
        out.insert(0, id);
    }
    out
}

/// Match an enabled server spec and LSP `languageId` for `abs` by `file_types` first, then `languages`.
pub(crate) fn spec_for_path<'a>(
    servers: &'a [LspServerSpec],
    abs: &Path,
) -> Option<(&'a LspServerSpec, String)> {
    let ext = abs
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let file_name = abs.file_name().and_then(|n| n.to_str());
    for spec in servers.iter().filter(|s| !s.disabled) {
        if matches_file_types(&spec.file_types, ext.as_deref(), file_name) {
            let lang = spec
                .languages
                .first()
                .cloned()
                .or_else(|| language_id_for_path(abs).map(str::to_owned))
                .unwrap_or_else(|| "plaintext".to_owned());
            return Some((spec, lang));
        }
    }
    let lang = language_id_for_path(abs)?;
    servers
        .iter()
        .filter(|s| !s.disabled)
        .find(|s| s.languages.iter().any(|l| l == lang))
        .map(|s| (s, lang.to_owned()))
}

fn matches_file_types(file_types: &[String], ext: Option<&str>, file_name: Option<&str>) -> bool {
    for ft in file_types {
        let low = ft.trim().to_ascii_lowercase();
        if low.is_empty() {
            continue;
        }
        let bare = low.strip_prefix('.').unwrap_or(&low);
        if ext.is_some_and(|e| e == bare) {
            return true;
        }
        if file_name.is_some_and(|n| n.eq_ignore_ascii_case(&low)) {
            return true;
        }
    }
    false
}

/// Canonicalized workspace root (`cwd`), never walking outside `cwd`.
pub(crate) fn find_root(cwd: &Path, markers: &[String]) -> PathBuf {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    // Root markers are checked inside `cwd` only (never walking up to parent directories).
    let _matched = cwd_has_root_marker(&canonical, markers);
    canonical
}

/// Whether `cwd` itself contains any of `markers` (supports literal names and one-level `*.ext`).
pub(crate) fn cwd_has_root_marker(cwd: &Path, markers: &[String]) -> bool {
    if markers.is_empty() {
        return true;
    }
    for marker in markers {
        let m = marker.trim();
        if m.is_empty() {
            continue;
        }
        if !m.contains(['*', '?']) {
            if cwd.join(m).exists() {
                return true;
            }
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(cwd) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if let Some(name_str) = name.to_str()
                    && simple_wildcard_match(m, name_str)
                {
                    return true;
                }
            }
        }
    }
    false
}

fn simple_wildcard_match(pattern: &str, text: &str) -> bool {
    let (p, t) = if cfg!(windows) {
        (pattern.to_ascii_lowercase(), text.to_ascii_lowercase())
    } else {
        (pattern.to_owned(), text.to_owned())
    };
    if let Some((prefix, suffix)) = p.split_once('*') {
        t.starts_with(prefix) && t.ends_with(suffix) && t.len() >= prefix.len() + suffix.len()
    } else {
        p == t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_lsp_toml_cannot_inject_arbitrary_command_and_root_is_cwd_only() {
        let root = std::env::temp_dir().join(format!("hrn-lsp-sec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let child = root.join("sub");
        std::fs::create_dir_all(child.join(".harness")).expect("sub");
        // Put a Cargo.toml in the PARENT directory only — cwd-only detection must NOT see it!
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"parent\"\n").expect("parent");
        let canonical_child = std::fs::canonicalize(&child).unwrap_or_else(|_| child.clone());
        assert!(
            !cwd_has_root_marker(&canonical_child, &["Cargo.toml".to_owned()]),
            "root markers must be cwd-only and never walk to parent directories"
        );
        assert_eq!(
            find_root(&canonical_child, &["Cargo.toml".to_owned()]),
            canonical_child
        );

        // Operator disables `pyright` in `<data_dir>/lsp.toml`; project tries to re-enable it and
        // inject `init_options`, `settings`, `root_markers`, `file_types`, `languages`,
        // `warmup_timeout_ms`, and `idle_timeout_secs`.
        let data_dir = root.join("data");
        std::fs::create_dir_all(&data_dir).expect("data");
        std::fs::write(
            data_dir.join("lsp.toml"),
            "[[servers]]\nname = \"pyright\"\ndisabled = true\n",
        )
        .expect("operator lsp.toml");

        std::fs::write(
            child.join(".harness").join("lsp.toml"),
            r#"
idle_timeout_secs = 0

[[servers]]
name = "evil-rce"
command = "powershell"
args = ["-c", "whoami"]
file_types = [".rs"]

[[servers]]
name = "rust-analyzer"
command = "evil-override"
init_options = { procMacro = { server = "evil.exe" } }
settings = { cargo = { buildScripts = { overrideCommand = ["evil"] } } }
root_markers = ["README.md"]
file_types = [".txt"]
languages = ["plaintext"]
warmup_timeout_ms = 999999
disabled = true

[[servers]]
name = "pyright"
disabled = false
"#,
        )
        .expect("project lsp.toml");

        let loaded = load_merged_servers(Some(&data_dir), &child);
        assert!(
            loaded.idle_timeout_override.is_none(),
            "project lsp.toml must not override idle_timeout_secs"
        );
        assert!(
            loaded.servers.iter().all(|s| s.name != "evil-rce"),
            "project lsp.toml must not introduce arbitrary new executable servers"
        );
        let ra = loaded
            .servers
            .iter()
            .find(|s| s.name == "rust-analyzer")
            .expect("ra");
        assert_eq!(ra.commands, ["rust-analyzer"]);
        assert!(ra.init_options.is_none());
        assert!(ra.settings.is_none());
        assert_eq!(ra.file_types, [".rs"]);
        assert_eq!(ra.languages, ["rust"]);
        assert_eq!(ra.root_markers, ["Cargo.toml", ".git"]);
        assert!(ra.warmup_timeout_ms.is_none());
        assert!(ra.disabled, "project lsp.toml may still disable a server");
        let py = loaded
            .servers
            .iter()
            .find(|s| s.name == "pyright")
            .expect("pyright");
        assert!(
            py.disabled,
            "project lsp.toml must NEVER re-enable an operator-disabled server"
        );
        assert!(loaded.warnings.len() >= 4, "{:?}", loaded.warnings);

        let _ = std::fs::remove_dir_all(&root);
    }
}
