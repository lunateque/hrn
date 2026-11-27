//! `check`: the architectural gate. Reads the tracked state and the cargo metadata and reports
//! violations instead of fixing them.

use crate::{Outcome, graph, process, report, workspace};
use std::collections::BTreeSet;
use std::path::Path;

/// The one file that may name module features: `cargo xtask module new|remove` generates it.
const MODULES_RS: &str = "crates/bins/harness/src/modules.rs";

pub(crate) fn run() -> Outcome<Vec<String>> {
    let root = workspace::root()?;
    let files = workspace::tracked_files(&root)?;
    let mut errors = Vec::new();
    check_dependencies(&root, &mut errors)?;
    check_feature_references(&root, &files, &mut errors)?;
    if errors.is_empty() {
        report::out("xtask check: ok");
    }
    Ok(errors)
}

fn check_dependencies(root: &Path, errors: &mut Vec<String>) -> Outcome {
    let allow = read_deps_allow(&workspace::read(root, "deps.allow")?);
    let json = process::output(
        "cargo",
        &["metadata", "--no-deps", "--format-version", "1"],
        root,
    )?;
    let packages = graph::parse_metadata(root, &json)?;
    graph::check(&packages, &allow, errors);
    Ok(())
}

fn read_deps_allow(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

fn rust_files(files: &[String]) -> impl Iterator<Item = &String> {
    files.iter().filter(|path| {
        Path::new(path.as_str())
            .extension()
            .is_some_and(|ext| ext == "rs")
            && (path.starts_with("crates/") || path.starts_with("xtask/src/"))
    })
}

/// `feature = "<module id>"` outside the generated `modules.rs` would dangle the moment the module
/// is removed through `cargo xtask module remove`. Comment lines are skipped.
fn check_feature_references(root: &Path, files: &[String], errors: &mut Vec<String>) -> Outcome {
    let ids = workspace::module_ids(files);
    for path in rust_files(files).filter(|path| path.as_str() != MODULES_RS) {
        let text = workspace::read(root, path)?;
        for (number, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let squeezed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            if let Some(id) = ids
                .iter()
                .find(|id| squeezed.contains(&format!("feature=\"{id}\"")))
            {
                errors.push(format!(
                    "{path}:{}: `feature = \"{id}\"` belongs only in {MODULES_RS}",
                    number + 1
                ));
            }
        }
    }
    Ok(())
}
