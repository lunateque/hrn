//! 7-layer TOML configuration merge engine, path-scoped array resolution, and key provenance.
//!
//! Precedence (low -> high):
//! 1. Embedded `data/config.defaults.toml`
//! 2. OMP compatibility `~/.omp/agent/config.yml` (when present)
//! 3. Global `~/.harness/config.toml` (or profile directory)
//! 4. Project `<cwd>/.harness/config.toml`
//! 5. Repeatable `--config <file>` overlays (later wins; missing overlay is a hard error)
//! 6. `HRN_*` environment variables declared in `settings.schema.toml`
//! 7. Runtime overrides (CLI flags and session-only `cfg://` writes)

use crate::settings_schema::{DEFAULTS_TOML, MergeRule, parse_env_value, schema};
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Origin of a resolved configuration value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LayerSource {
    Default,
    Omp,
    Global,
    Project,
    Overlay(PathBuf),
    Env(String),
    Runtime,
}

impl LayerSource {
    /// Human-readable source label shown by `hrn config get` and `read cfg://`.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Default => "default".to_owned(),
            Self::Omp => "omp config (~/.omp/agent/config.yml)".to_owned(),
            Self::Global => "global config".to_owned(),
            Self::Project => "project config".to_owned(),
            Self::Overlay(p) => format!("--config overlay ({})", p.display()),
            Self::Env(var) => format!("environment variable ({var})"),
            Self::Runtime => "session override".to_owned(),
        }
    }

    /// Short tag used in JSON `overridden_by` responses.
    pub(crate) const fn kind_tag(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Omp => "omp",
            Self::Global => "global",
            Self::Project => "project",
            Self::Overlay(_) => "overlay",
            Self::Env(_) => "env",
            Self::Runtime => "runtime",
        }
    }

    /// Precedence rank (higher wins).
    pub(crate) const fn rank(&self) -> u8 {
        match self {
            Self::Default => 0,
            Self::Omp => 1,
            Self::Global => 2,
            Self::Project => 3,
            Self::Overlay(_) => 4,
            Self::Env(_) => 5,
            Self::Runtime => 6,
        }
    }
}

/// Inputs to the 7-layer merge engine.
#[derive(Clone, Debug, Default)]
pub(crate) struct LayerInput {
    pub(crate) global_path: Option<PathBuf>,
    pub(crate) project_path: Option<PathBuf>,
    pub(crate) overlays: Vec<PathBuf>,
    pub(crate) cwd: PathBuf,
    pub(crate) home: Option<PathBuf>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) runtime: toml::Table,
}

/// Merged TOML document together with per-key provenance and raw per-layer tables.
#[derive(Clone, Debug)]
pub(crate) struct MergedConfig {
    pub(crate) root: toml::Table,
    pub(crate) sources: BTreeMap<String, LayerSource>,
    pub(crate) global_raw: Option<toml::Table>,
    pub(crate) project_raw: Option<toml::Table>,
}

impl MergedConfig {
    /// Look up a dotted key (e.g. `tools.premium_defer`) in the merged table.
    pub(crate) fn get(&self, dotted_key: &str) -> Option<&toml::Value> {
        lookup_dotted(&self.root, dotted_key)
    }

    /// Winning source for `dotted_key` (`Default` when unset everywhere).
    pub(crate) fn source_of(&self, dotted_key: &str) -> LayerSource {
        self.sources
            .get(dotted_key)
            .cloned()
            .unwrap_or(LayerSource::Default)
    }

    /// If a layer higher than `Global` currently shadows `dotted_key`, return that source.
    pub(crate) fn overridden_above_global(&self, dotted_key: &str) -> Option<&LayerSource> {
        self.sources
            .get(dotted_key)
            .filter(|src| src.rank() > LayerSource::Global.rank())
    }
}

/// Read a single TOML table from disk. Returns `Ok(None)` if `required == false` and the file is missing.
pub(crate) fn read_layer_file(path: &Path, required: bool) -> Result<Option<toml::Table>> {
    if !path.is_file() {
        if required {
            return Err(HrnError::Config(format!(
                "config overlay `{}` does not exist",
                path.display()
            )));
        }
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| HrnError::Config(format!("cannot read config `{}`: {e}", path.display())))?;
    let table: toml::Table =
        toml::from_str(&text).map_err(|e| HrnError::Config(format!("{}: {e}", path.display())))?;
    Ok(Some(table))
}

/// Execute the 7-layer merge and post-merge path-scoped array resolution.
pub(crate) fn load_layers(input: &LayerInput) -> Result<MergedConfig> {
    let defaults: toml::Table = toml::from_str(DEFAULTS_TOML)
        .map_err(|e| HrnError::Internal(format!("invalid embedded defaults: {e}")))?;
    let omp_raw = crate::omp_config::load_omp_config_table(input.home.as_deref());
    let global_raw = match input.global_path.as_deref() {
        Some(p) => read_layer_file(p, false)?,
        None => None,
    };
    let project_raw = match input.project_path.as_deref() {
        Some(p) => read_layer_file(p, false)?,
        None => None,
    };
    let mut overlays_raw = Vec::with_capacity(input.overlays.len());
    for path in &input.overlays {
        if let Some(tbl) = read_layer_file(path, true)? {
            overlays_raw.push((path.clone(), tbl));
        }
    }
    let env_layers = collect_env_layers(&input.env);
    let mut layers: Vec<(LayerSource, toml::Table)> = vec![(LayerSource::Default, defaults)];
    if let Some(tbl) = omp_raw {
        layers.push((LayerSource::Omp, tbl));
    }
    if let Some(tbl) = &global_raw {
        layers.push((LayerSource::Global, tbl.clone()));
    }
    if let Some(tbl) = &project_raw {
        layers.push((LayerSource::Project, tbl.clone()));
    }
    for (path, tbl) in overlays_raw {
        layers.push((LayerSource::Overlay(path), tbl));
    }
    for (var, tbl) in env_layers {
        layers.push((LayerSource::Env(var), tbl));
    }
    if !input.runtime.is_empty() {
        layers.push((LayerSource::Runtime, input.runtime.clone()));
    }
    let mut merged = merge_layer_list(&layers, &input.cwd, input.home.as_deref());
    merged.global_raw = global_raw;
    merged.project_raw = project_raw;
    Ok(merged)
}

/// Merge an explicit list of `(LayerSource, toml::Table)` layers and resolve path-scoped arrays.
pub(crate) fn merge_layer_list(
    layers: &[(LayerSource, toml::Table)],
    cwd: &Path,
    home: Option<&Path>,
) -> MergedConfig {
    let mut root = toml::Table::new();
    let mut sources = BTreeMap::new();
    for (source, tbl) in layers {
        if matches!(source, LayerSource::Project) {
            let clean = policy::sanitize_project_layer(tbl);
            merge_table("", &mut root, &clean, source, &mut sources);
        } else {
            merge_table("", &mut root, tbl, source, &mut sources);
        }
    }
    resolve_all_path_scoped(&mut root, cwd, home);
    policy::enforce_operator_policies(&mut root, layers, cwd, home);
    MergedConfig {
        root,
        sources,
        global_raw: None,
        project_raw: None,
    }
}

fn collect_env_layers(env: &BTreeMap<String, String>) -> Vec<(String, toml::Table)> {
    let mut out = Vec::new();
    for (var, spec) in schema().env_bindings() {
        let Some(raw) = env.get(var) else {
            continue;
        };
        let Some(val) = parse_env_value(spec, raw) else {
            continue;
        };
        let mut tbl = toml::Table::new();
        insert_dotted(&mut tbl, &spec.key, val);
        out.push((var.to_owned(), tbl));
    }
    out
}

fn merge_table(
    prefix: &str,
    dst: &mut toml::Table,
    src: &toml::Table,
    source: &LayerSource,
    sources: &mut BTreeMap<String, LayerSource>,
) {
    for (key, val) in src {
        let dotted = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match (dst.get_mut(key), val) {
            (Some(toml::Value::Table(dst_sub)), toml::Value::Table(src_sub)) => {
                merge_table(&dotted, dst_sub, src_sub, source, sources);
            }
            (Some(toml::Value::Array(dst_arr)), toml::Value::Array(src_arr))
                if is_prepend_key(&dotted) =>
            {
                let mut combined = src_arr.clone();
                combined.append(dst_arr);
                *dst_arr = combined;
                sources.insert(dotted, source.clone());
            }
            (_, toml::Value::Table(src_sub)) => {
                let mut fresh = toml::Table::new();
                merge_table(&dotted, &mut fresh, src_sub, source, sources);
                dst.insert(key.clone(), toml::Value::Table(fresh));
            }
            _ => {
                dst.insert(key.clone(), val.clone());
                sources.insert(dotted, source.clone());
            }
        }
    }
}

fn is_prepend_key(dotted_key: &str) -> bool {
    schema()
        .get(dotted_key)
        .is_some_and(|s| s.merge == MergeRule::Prepend)
}

/// Resolve path-scoped array entries (`{ path | paths | path_prefix | path_prefixes, values | ... }`)
/// against `cwd` (`~` expanded, case-insensitive on Windows).
fn resolve_all_path_scoped(root: &mut toml::Table, cwd: &Path, home: Option<&Path>) {
    for spec in schema().all().iter().filter(|s| s.path_scoped) {
        if let Some(toml::Value::Array(arr)) = lookup_dotted_mut(root, &spec.key) {
            *arr = resolve_path_scoped_array(arr, cwd, home);
        }
    }
}

fn resolve_path_scoped_array(
    items: &[toml::Value],
    cwd: &Path,
    home: Option<&Path>,
) -> Vec<toml::Value> {
    let mut out = Vec::new();
    for item in items {
        let Some(tbl) = item.as_table() else {
            out.push(item.clone());
            continue;
        };
        let Some(scopes) = extract_scope_paths(tbl) else {
            out.push(item.clone());
            continue;
        };
        if !scopes.iter().any(|s| path_matches_cwd(s, cwd, home)) {
            continue;
        }
        if let Some(values) = extract_scoped_values(tbl) {
            out.extend(values.iter().cloned());
        }
    }
    out
}

fn extract_scope_paths(tbl: &toml::Table) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    for key in ["path", "path_prefix", "pathPrefix"] {
        if let Some(s) = tbl.get(key).and_then(toml::Value::as_str) {
            paths.push(s.to_owned());
        }
    }
    for key in ["paths", "path_prefixes", "pathPrefixes"] {
        if let Some(arr) = tbl.get(key).and_then(toml::Value::as_array) {
            for v in arr {
                if let Some(s) = v.as_str() {
                    paths.push(s.to_owned());
                }
            }
        }
    }
    (!paths.is_empty()).then_some(paths)
}

fn extract_scoped_values(tbl: &toml::Table) -> Option<&Vec<toml::Value>> {
    for key in ["values", "models", "providers", "patterns", "items"] {
        if let Some(arr) = tbl.get(key).and_then(toml::Value::as_array) {
            return Some(arr);
        }
    }
    None
}

fn path_matches_cwd(scope: &str, cwd: &Path, home: Option<&Path>) -> bool {
    let expanded = expand_tilde(scope.trim(), home);
    let scope_norm = normalize_path(&expanded);
    let cwd_norm = normalize_path(cwd);
    if scope_norm.is_empty() {
        return false;
    }
    cwd_norm == scope_norm
        || cwd_norm
            .strip_prefix(&scope_norm)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn expand_tilde(raw: &str, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if raw == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn normalize_path(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

#[path = "config_layers_policy.rs"]
mod policy;

/// Look up a dotted path inside `table`.
pub(crate) fn lookup_dotted<'a>(table: &'a toml::Table, dotted: &str) -> Option<&'a toml::Value> {
    let mut parts = dotted.split('.');
    let first = parts.next()?;
    let mut cur = table.get(first)?;
    for part in parts {
        cur = cur.as_table()?.get(part)?;
    }
    Some(cur)
}

fn lookup_dotted_mut<'a>(table: &'a mut toml::Table, dotted: &str) -> Option<&'a mut toml::Value> {
    let mut parts = dotted.split('.');
    let first = parts.next()?;
    let mut cur = table.get_mut(first)?;
    for part in parts {
        cur = cur.as_table_mut()?.get_mut(part)?;
    }
    Some(cur)
}

/// Insert `val` at dotted path `dotted` inside `table`, creating intermediate tables.
pub(crate) fn insert_dotted(table: &mut toml::Table, dotted: &str, val: toml::Value) {
    let parts: Vec<&str> = dotted.split('.').collect();
    insert_parts(table, &parts, val);
}

fn insert_parts(table: &mut toml::Table, parts: &[&str], val: toml::Value) {
    let Some((&head, tail)) = parts.split_first() else {
        return;
    };
    if tail.is_empty() {
        table.insert(head.to_owned(), val);
        return;
    }
    let entry = table
        .entry(head.to_owned())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::Table::new());
    }
    if let Some(sub) = entry.as_table_mut() {
        insert_parts(sub, tail, val);
    }
}

#[cfg(test)]
#[path = "config_layers_tests.rs"]
mod tests;
