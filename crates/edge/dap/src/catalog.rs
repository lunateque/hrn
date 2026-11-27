//! Adapter descriptors (embedded `adapters.toml` plus the user's overrides), their detection on
//! this machine and the choice of one for a launch or an attach.

use hrn_edge_proc::{RunSpec, resolve_on_path, run_streaming};
use hrn_kernel::{CancelFlag, HrnError, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const BUILTIN: &str = include_str!("adapters.toml");
/// How long the `import <module>` probe of a python interpreter may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How the client reaches the adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Connect {
    /// The adapter's stdin/stdout.
    #[default]
    Stdio,
    /// A loopback TCP port the adapter listens on.
    Tcp,
}

/// Launch-argument overrides that depend on what the target is.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct LaunchFor {
    directory: Map<String, Value>,
    file: Map<String, Value>,
    extension: BTreeMap<String, Map<String, Value>>,
}

/// One adapter as the descriptor file states it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Spec {
    command: Vec<String>,
    args: Vec<String>,
    connect: Connect,
    languages: Vec<String>,
    file_types: Vec<String>,
    root_markers: Vec<String>,
    native_rank: Option<u32>,
    accepts_directory: bool,
    attach_port: bool,
    attach_connect: bool,
    python_module: Option<String>,
    locate_env: Option<String>,
    locate_paths: Vec<String>,
    install_hint: Option<String>,
    env: BTreeMap<String, String>,
    launch: Map<String, Value>,
    launch_for: LaunchFor,
    attach: Map<String, Value>,
}

/// What the launch target is on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramKind {
    /// An existing file.
    File,
    /// An existing directory.
    Directory,
    /// Nothing there (the adapter may create it).
    Missing,
}

/// An adapter found on this machine, ready to start.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// Descriptor id.
    pub name: String,
    /// The executable to run.
    pub program: String,
    /// Its arguments (`${port}` still to substitute for TCP adapters).
    pub args: Vec<String>,
    /// How to reach it.
    pub connect: Connect,
    /// Default DAP launch arguments.
    pub launch: Map<String, Value>,
    /// Default DAP attach arguments.
    pub attach: Map<String, Value>,
    /// Whether `launch` may take a package directory.
    pub accepts_directory: bool,
    /// Whether attach host/port travel in a `connect` object (`pid` as `processId`).
    pub attach_connect: bool,
    /// Extra environment for the adapter process.
    pub env: BTreeMap<String, String>,
    launch_for: LaunchFor,
}

impl Resolved {
    /// The default launch arguments with the overrides for this target kind applied.
    pub fn launch_arguments(&self, program: &Path, kind: ProgramKind) -> Map<String, Value> {
        let mut out = self.launch.clone();
        let extension = extension_of(program);
        let by_kind = match kind {
            ProgramKind::Directory => Some(&self.launch_for.directory),
            ProgramKind::File => Some(&self.launch_for.file),
            ProgramKind::Missing => None,
        };
        // `.go` sources debug as a package even though they are files: an extension beats the kind.
        let overrides = self
            .launch_for
            .extension
            .get(&extension)
            .or(by_kind)
            .filter(|m| !m.is_empty());
        if let Some(extra) = overrides {
            out.extend(extra.clone());
        }
        out
    }
}

/// Why no adapter could be chosen, or which one was.
#[derive(Debug)]
pub enum Selection {
    /// Use this one.
    Adapter(Box<Resolved>),
    /// A matching adapter exists in the catalog but cannot run here.
    Unavailable {
        /// Descriptor id.
        name: String,
        /// What is missing and how to get it.
        reason: String,
    },
    /// Nothing matches; carries the adapters that do run here.
    None(Vec<String>),
}

/// The attach target as the tool received it.
#[derive(Clone, Debug, Default)]
pub struct AttachTarget {
    /// Process id.
    pub pid: Option<u32>,
    /// Remote port.
    pub port: Option<u16>,
}

/// All known adapters.
pub struct Catalog {
    specs: BTreeMap<String, Spec>,
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default()
}

fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn has_marker(dir: &Path, marker: &str) -> bool {
    if let Some(suffix) = marker.strip_prefix('*') {
        return std::fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(suffix))
        });
    }
    dir.join(marker).exists()
}

/// The nearest ancestor of `program` holding one of `markers`.
fn find_root(program: &Path, kind: ProgramKind, markers: &[String]) -> Option<PathBuf> {
    if markers.is_empty() {
        return None;
    }
    let mut dir = if kind == ProgramKind::Directory {
        Some(program)
    } else {
        program.parent()
    };
    while let Some(d) = dir {
        if markers.iter().any(|m| has_marker(d, m)) {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

async fn imports(python: &Path, module: &str, cwd: &Path) -> bool {
    let spec = RunSpec {
        program: python.to_string_lossy().into_owned(),
        args: vec!["-c".to_owned(), format!("import {module}")],
        cwd: cwd.to_path_buf(),
        env: BTreeMap::new(),
        timeout: PROBE_TIMEOUT,
        max_output_bytes: 4096,
    };
    matches!(
        run_streaming(&spec, &CancelFlag::new(), |_| {}).await,
        Ok(out) if out.exit_code == Some(0)
    )
}

impl Catalog {
    /// The built-in descriptors with `overrides` (id → partial descriptor) merged over them.
    ///
    /// # Errors
    ///
    /// A descriptor that does not parse; the message names its id.
    pub fn load(overrides: &BTreeMap<String, Value>) -> Result<Self> {
        let table: toml::Table = toml::from_str(BUILTIN)
            .map_err(|e| HrnError::Internal(format!("built-in adapters.toml: {e}")))?;
        let mut all = serde_json::to_value(table)?
            .get("adapters")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        for (name, over) in overrides {
            match all.get_mut(name) {
                Some(base) => merge(base, over),
                None => {
                    if let Value::Object(map) = &mut all {
                        map.insert(name.clone(), over.clone());
                    }
                }
            }
        }
        let Value::Object(map) = all else {
            return Err(HrnError::Internal("adapters.toml has no [adapters]".into()));
        };
        let mut specs = BTreeMap::new();
        for (name, value) in map {
            let spec: Spec = serde_json::from_value(value)
                .map_err(|e| HrnError::Config(format!("[debug.adapters.{name}]: {e}")))?;
            if spec.command.is_empty() {
                return Err(HrnError::Config(format!(
                    "[debug.adapters.{name}]: `command` is required"
                )));
            }
            specs.insert(name, spec);
        }
        Ok(Self { specs })
    }

    /// Every adapter id.
    pub fn names(&self) -> Vec<&str> {
        self.specs.keys().map(String::as_str).collect()
    }

    /// Find `name` on this machine.
    ///
    /// # Errors
    ///
    /// The reason it cannot run here, ready for the model.
    pub async fn resolve(&self, name: &str, cwd: &Path) -> std::result::Result<Resolved, String> {
        let spec = self.specs.get(name).ok_or_else(|| {
            format!(
                "adapter '{name}' is unknown. Known adapters: {}",
                self.names().join(", ")
            )
        })?;
        let hint = spec
            .install_hint
            .as_deref()
            .map(|h| format!(": {h}"))
            .unwrap_or_default();
        let unavailable = |why: String| format!("adapter '{name}' is not available: {why}{hint}");

        let mut found = None;
        let mut missing_module = false;
        for candidate in &spec.command {
            let path = if candidate.contains(['/', '\\']) {
                let p = Path::new(candidate);
                Some(if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    cwd.join(p)
                })
                .filter(|p| p.exists())
            } else {
                resolve_on_path(candidate)
            };
            let Some(path) = path else { continue };
            if let Some(module) = &spec.python_module
                && !imports(&path, module, cwd).await
            {
                missing_module = true;
                continue;
            }
            found = Some(path);
            break;
        }
        let Some(program) = found else {
            let why = match (&spec.python_module, missing_module) {
                (Some(module), true) => format!("python cannot import '{module}'"),
                _ => format!("none of {} found on PATH", spec.command.join(", ")),
            };
            return Err(unavailable(why));
        };

        let mut args = spec.args.clone();
        if args.iter().any(|a| a.contains("${located}")) {
            let located = Self::locate(spec)
                .ok_or_else(|| unavailable("its server script was not found".to_owned()))?;
            for arg in &mut args {
                *arg = arg.replace("${located}", &located.to_string_lossy());
            }
        }
        Ok(Resolved {
            name: name.to_owned(),
            program: program.to_string_lossy().into_owned(),
            args,
            connect: spec.connect,
            launch: spec.launch.clone(),
            attach: spec.attach.clone(),
            accepts_directory: spec.accepts_directory,
            attach_connect: spec.attach_connect,
            env: spec.env.clone(),
            launch_for: spec.launch_for.clone(),
        })
    }

    fn locate(spec: &Spec) -> Option<PathBuf> {
        let from_env = spec
            .locate_env
            .as_deref()
            .and_then(std::env::var_os)
            .map(PathBuf::from);
        let listed = spec
            .locate_paths
            .iter()
            .filter_map(|p| match p.strip_prefix("~/") {
                Some(rest) => home().map(|h| h.join(rest)),
                None => Some(PathBuf::from(p)),
            });
        from_env.into_iter().chain(listed).find(|p| p.is_file())
    }

    /// Every adapter that can run here, in catalog order.
    pub async fn available(&self, cwd: &Path) -> Vec<Resolved> {
        let mut out = Vec::new();
        for name in self.specs.keys() {
            if let Ok(r) = self.resolve(name, cwd).await {
                out.push(r);
            }
        }
        out
    }

    async fn none(&self, cwd: &Path) -> Selection {
        let names = self
            .available(cwd)
            .await
            .into_iter()
            .map(|r| r.name)
            .collect();
        Selection::None(names)
    }

    async fn pick(&self, name: &str, cwd: &Path) -> Selection {
        match self.resolve(name, cwd).await {
            Ok(r) => Selection::Adapter(Box::new(r)),
            Err(reason) => Selection::Unavailable {
                name: name.to_owned(),
                reason,
            },
        }
    }

    fn rank_key(
        &self,
        r: &Resolved,
        program: &Path,
        kind: ProgramKind,
    ) -> (bool, bool, u32, String) {
        let spec = self.specs.get(&r.name);
        let ext = extension_of(program);
        let ext_match = !ext.is_empty() && spec.is_some_and(|s| s.file_types.contains(&ext));
        let root_match = spec.is_some_and(|s| find_root(program, kind, &s.root_markers).is_some());
        let rank = spec.and_then(|s| s.native_rank).unwrap_or(u32::MAX);
        (!ext_match, !root_match, rank, r.name.clone())
    }

    fn best(&self, mut from: Vec<Resolved>, program: &Path, kind: ProgramKind) -> Option<Resolved> {
        from.sort_by_key(|r| self.rank_key(r, program, kind));
        from.into_iter().next()
    }

    /// Choose the adapter for debugging `program`: the explicit one, else by extension, project
    /// markers and the native-debugger order.
    pub async fn select_launch(
        &self,
        program: &Path,
        cwd: &Path,
        explicit: Option<&str>,
        kind: ProgramKind,
    ) -> Selection {
        if let Some(name) = explicit {
            return self.pick(name, cwd).await;
        }
        let ext = extension_of(program);
        if !ext.is_empty() {
            let matching: Vec<&String> = self
                .specs
                .iter()
                .filter(|(_, s)| s.file_types.contains(&ext))
                .map(|(n, _)| n)
                .collect();
            let mut available = Vec::new();
            for name in &matching {
                if let Ok(r) = self.resolve(name, cwd).await {
                    available.push(r);
                }
            }
            if let Some(best) = self.best(available, program, kind) {
                return Selection::Adapter(Box::new(best));
            }
            let rooted = matching.iter().find(|n| {
                self.specs
                    .get(n.as_str())
                    .is_some_and(|s| find_root(program, kind, &s.root_markers).is_some())
            });
            if let Some(name) = rooted.or(matching.first()) {
                return self.pick(name, cwd).await;
            }
        }
        let mut available = Vec::new();
        let mut rooted: Vec<&String> = Vec::new();
        for (name, spec) in &self.specs {
            let root = find_root(program, kind, &spec.root_markers).is_some();
            if root {
                rooted.push(name);
            }
            if (root || spec.native_rank.is_some())
                && let Ok(r) = self.resolve(name, cwd).await
            {
                available.push(r);
            }
        }
        if kind == ProgramKind::Directory {
            let dir_capable: Vec<Resolved> = available
                .iter()
                .filter(|r| r.accepts_directory)
                .cloned()
                .collect();
            if let Some(best) = self.best(dir_capable, program, kind) {
                return Selection::Adapter(Box::new(best));
            }
        }
        if let Some(best) = self.best(available, program, kind) {
            return Selection::Adapter(Box::new(best));
        }
        match rooted.first() {
            Some(name) => self.pick(name, cwd).await,
            None => self.none(cwd).await,
        }
    }

    /// Choose the adapter for an attach: the explicit one; for a port the one flagged
    /// `attach_port`; then the native debuggers; then any that runs here.
    pub async fn select_attach(
        &self,
        cwd: &Path,
        explicit: Option<&str>,
        target: &AttachTarget,
    ) -> Selection {
        if let Some(name) = explicit {
            return self.pick(name, cwd).await;
        }
        let available = self.available(cwd).await;
        if target.port.is_some()
            && let Some(r) = available
                .iter()
                .find(|r| self.specs.get(&r.name).is_some_and(|s| s.attach_port))
        {
            return Selection::Adapter(Box::new(r.clone()));
        }
        let mut native: Vec<&Resolved> = available
            .iter()
            .filter(|r| {
                self.specs
                    .get(&r.name)
                    .is_some_and(|s| s.native_rank.is_some())
            })
            .collect();
        native.sort_by_key(|r| self.specs.get(&r.name).and_then(|s| s.native_rank));
        match native.first().copied().or(available.first()) {
            Some(r) => Selection::Adapter(Box::new(r.clone())),
            None => Selection::None(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_overrides_merge_into_a_builtin_and_add_new_adapters() {
        let overrides = BTreeMap::from([
            (
                "debugpy".to_owned(),
                json!({"launch": {"justMyCode": true}}),
            ),
            (
                "mine".to_owned(),
                json!({"command": ["mydbg"], "file_types": [".x"]}),
            ),
        ]);
        let catalog = Catalog::load(&overrides).expect("loads");
        let debugpy = catalog.specs.get("debugpy").expect("builtin");
        assert_eq!(debugpy.launch.get("justMyCode"), Some(&json!(true)));
        assert_eq!(debugpy.launch.get("stopOnEntry"), Some(&json!(true)));
        assert!(catalog.specs.contains_key("mine"));
    }

    #[test]
    fn a_typo_in_a_descriptor_names_the_adapter() {
        let overrides = BTreeMap::from([("debugpy".to_owned(), json!({"comand": ["x"]}))]);
        let err = Catalog::load(&overrides)
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("debugpy") && err.contains("comand"), "{err}");
    }

    #[tokio::test]
    async fn delve_debugs_go_sources_as_packages_and_binaries_as_exec() {
        let catalog = Catalog::load(&BTreeMap::new()).expect("loads");
        let spec = catalog.specs.get("dlv").expect("dlv");
        let resolved = Resolved {
            name: "dlv".into(),
            program: "dlv".into(),
            args: Vec::new(),
            connect: Connect::Tcp,
            launch: spec.launch.clone(),
            attach: spec.attach.clone(),
            accepts_directory: true,
            attach_connect: false,
            env: BTreeMap::new(),
            launch_for: spec.launch_for.clone(),
        };
        let mode = |p: &str, k| {
            resolved
                .launch_arguments(Path::new(p), k)
                .get("mode")
                .cloned()
        };
        assert_eq!(mode("main.go", ProgramKind::File), Some(json!("debug")));
        assert_eq!(mode("app.exe", ProgramKind::File), Some(json!("exec")));
        assert_eq!(mode("pkg", ProgramKind::Directory), Some(json!("debug")));
    }
}
