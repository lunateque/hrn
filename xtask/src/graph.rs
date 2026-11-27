//! Star-around-kernel dependency graph validation for `hrn`: modules depend on the kernel and edge
//! crates only, never on each other, and external crates live in edge and xtask (kernel and
//! harness keep serde for the wire IR; front and bin keep their UI and CLI crates).

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    Kernel,
    Harness,
    Edge,
    Front,
    Module,
    Bin,
    Xtask,
}

#[derive(Debug)]
pub(crate) struct Package {
    pub(crate) name: String,
    pub(crate) layer: Layer,
    pub(crate) deps: Vec<String>,
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<RawPackage>,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    manifest_path: String,
    dependencies: Vec<RawDep>,
}

#[derive(Deserialize)]
struct RawDep {
    name: String,
    #[serde(default)]
    kind: Option<String>,
}

pub(crate) fn parse_metadata(root: &Path, json: &[u8]) -> Result<Vec<Package>, String> {
    let meta: CargoMetadata =
        serde_json::from_slice(json).map_err(|err| format!("parse cargo metadata: {err}"))?;
    let mut out = Vec::with_capacity(meta.packages.len());
    for raw in meta.packages {
        let layer = classify(root, Path::new(&raw.manifest_path))?;
        let deps = raw
            .dependencies
            .into_iter()
            .filter(|d| d.kind.as_deref().is_none_or(|k| k == "normal"))
            .map(|d| d.name)
            .collect();
        out.push(Package {
            name: raw.name,
            layer,
            deps,
        });
    }
    Ok(out)
}

fn classify(root: &Path, manifest: &Path) -> Result<Layer, String> {
    let rel = manifest
        .strip_prefix(root)
        .unwrap_or(manifest)
        .to_string_lossy()
        .replace('\\', "/");
    if rel.starts_with("crates/kernel/") {
        Ok(Layer::Kernel)
    } else if rel.starts_with("crates/harness/") {
        Ok(Layer::Harness)
    } else if rel.starts_with("crates/edge/") {
        Ok(Layer::Edge)
    } else if rel.starts_with("crates/front/") {
        Ok(Layer::Front)
    } else if rel.starts_with("crates/modules/") {
        Ok(Layer::Module)
    } else if rel.starts_with("crates/bins/") {
        Ok(Layer::Bin)
    } else if rel.starts_with("xtask/") {
        Ok(Layer::Xtask)
    } else {
        Err(format!("unclassified workspace crate at `{rel}`"))
    }
}

pub(crate) fn check(packages: &[Package], allow: &BTreeSet<String>, errors: &mut Vec<String>) {
    let layers: BTreeMap<&str, Layer> = packages
        .iter()
        .map(|p| (p.name.as_str(), p.layer))
        .collect();

    for pkg in packages {
        for dep in &pkg.deps {
            if let Some(&dep_layer) = layers.get(dep.as_str()) {
                check_internal_edge(pkg, dep, dep_layer, errors);
            } else {
                check_external_dep(pkg, dep, allow, errors);
            }
        }
    }
}

fn check_internal_edge(pkg: &Package, dep: &str, dep_layer: Layer, errors: &mut Vec<String>) {
    let allowed = match pkg.layer {
        Layer::Kernel | Layer::Xtask => false,
        Layer::Harness => dep_layer == Layer::Kernel,
        Layer::Edge => matches!(dep_layer, Layer::Kernel | Layer::Edge),
        Layer::Front => matches!(dep_layer, Layer::Kernel) || dep == "hrn-edge-ipc",
        Layer::Module => check_module_edge(pkg, dep, dep_layer),
        Layer::Bin => dep_layer != Layer::Xtask && dep_layer != Layer::Bin,
    };
    if !allowed {
        errors.push(format!(
            "forbidden internal dependency: `{}` ({:?}) -> `{dep}` ({dep_layer:?})",
            pkg.name, pkg.layer
        ));
    }
}

fn check_module_edge(pkg: &Package, dep: &str, dep_layer: Layer) -> bool {
    if pkg.name == "hrn-mod-agy" {
        // Hard ToS fence: agy may NEVER depend on LLM, OAuth, or HTTP crates.
        return matches!(
            dep,
            "hrn-kernel" | "hrn-edge-proc" | "hrn-edge-mcp" | "hrn-edge-rt"
        );
    }
    matches!(dep_layer, Layer::Kernel | Layer::Edge)
}

fn check_external_dep(
    pkg: &Package,
    dep: &str,
    allow: &BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    if !allow.contains(dep) {
        errors.push(format!(
            "external dependency `{dep}` in `{}` is not listed in deps.allow",
            pkg.name
        ));
    }
    let ext_ok = match pkg.layer {
        Layer::Kernel | Layer::Harness => matches!(dep, "serde" | "serde_json"),
        // Modules reach serde, serde_json and tokio through the kernel and `hrn-edge-rt`
        // re-exports; the one exception is the memory module's own SQLite file (FTS5 index).
        Layer::Module => dep == "rusqlite" && pkg.name == "hrn-mod-memory",
        Layer::Edge | Layer::Front | Layer::Bin | Layer::Xtask => true,
    };
    if !ext_ok {
        errors.push(format!(
            "external dependency `{dep}` is forbidden in `{}` ({:?})",
            pkg.name, pkg.layer
        ));
    }
}
