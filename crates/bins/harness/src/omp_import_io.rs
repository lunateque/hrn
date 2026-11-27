//! Confinement, dotenv, secret-file atomic write, TOML path, YAML emitter, and report helpers
//! for [`super`].

use super::ImportReport;
use crate::toml_write::{resolve_write_target, to_edit_item};
use hrn_edge_llm::yaml::Yaml;
use hrn_kernel::{HrnError, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use toml_edit::{Item, Table};

const MAX_IMPORT_FILE_BYTES: u64 = 8 * 1024 * 1024;
static SECRET_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

pub(super) fn canonicalize_omp_dir(omp_dir: &Path) -> Result<PathBuf> {
    if has_forbidden_root(omp_dir) {
        return Err(HrnError::Config(format!(
            "refusing forbidden source directory `{}`",
            omp_dir.display()
        )));
    }
    let meta = std::fs::symlink_metadata(omp_dir).map_err(|e| {
        HrnError::Config(format!(
            "OMP directory `{}` is not accessible: {e}",
            omp_dir.display()
        ))
    })?;
    if meta.file_type().is_symlink() {
        return Err(HrnError::Config(format!(
            "refusing symlink OMP directory `{}`",
            omp_dir.display()
        )));
    }
    if !meta.is_dir() {
        return Err(HrnError::Config(format!(
            "OMP path `{}` is not a directory",
            omp_dir.display()
        )));
    }
    let canon = std::fs::canonicalize(omp_dir)
        .map_err(|e| HrnError::Config(format!("cannot resolve `{}`: {e}", omp_dir.display())))?;
    if has_forbidden_root(&canon) {
        return Err(HrnError::Config(format!(
            "refusing forbidden resolved directory `{}`",
            canon.display()
        )));
    }
    Ok(canon)
}

fn has_forbidden_root(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(s) => s.to_str().is_some_and(|n| {
            n.eq_ignore_ascii_case(".gemini")
                || n.eq_ignore_ascii_case(".claude")
                || n.eq_ignore_ascii_case(".codex")
        }),
        _ => false,
    })
}

pub(super) fn read_confined_file(omp_canon: &Path, rel: &Path) -> Result<Option<String>> {
    let mut cur = omp_canon.to_path_buf();
    for comp in rel.components() {
        let Component::Normal(part) = comp else {
            return Err(HrnError::Config(format!(
                "invalid relative path `{}`",
                rel.display()
            )));
        };
        cur.push(part);
        let meta = match std::fs::symlink_metadata(&cur) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(HrnError::Io(format!(
                    "cannot inspect `{}`: {e}",
                    cur.display()
                )));
            }
        };
        if meta.file_type().is_symlink() {
            return Err(HrnError::Config(format!(
                "refusing symlink `{}` inside OMP directory",
                cur.display()
            )));
        }
    }
    let meta = std::fs::symlink_metadata(&cur)
        .map_err(|e| HrnError::Io(format!("cannot stat `{}`: {e}", cur.display())))?;
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() > MAX_IMPORT_FILE_BYTES {
        return Err(HrnError::Config(format!(
            "refusing oversized file `{}` ({} bytes)",
            cur.display(),
            meta.len()
        )));
    }
    let canon = std::fs::canonicalize(&cur)
        .map_err(|e| HrnError::Io(format!("cannot canonicalize `{}`: {e}", cur.display())))?;
    if !canon.starts_with(omp_canon) {
        return Err(HrnError::Config(format!(
            "path `{}` escapes OMP directory",
            cur.display()
        )));
    }
    let text = std::fs::read_to_string(&canon)
        .map_err(|e| HrnError::Io(format!("cannot read `{}`: {e}", canon.display())))?;
    Ok(Some(text))
}

pub(super) fn extract_gateway_token_file(command: &str) -> Option<String> {
    let after = command.split_once("auth-gateway-tokens/")?.1;
    let end = after.find(['\'', '"', ' ', ')']).unwrap_or(after.len());
    let name = after.get(..end)?.trim();
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.contains("..");
    safe.then(|| name.to_owned())
}

pub(super) fn parse_env_var_name(raw: &str) -> Option<&str> {
    let key = raw
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(raw);
    let ok = key.starts_with(|c: char| c.is_ascii_uppercase())
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    ok.then_some(key)
}

pub(super) fn derive_api_key_env_name(provider: &str) -> String {
    let mut out = String::new();
    let mut prev_us = false;
    for c in provider.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
            prev_us = false;
        } else if !prev_us && !out.is_empty() {
            out.push('_');
            prev_us = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "PROVIDER_API_KEY".to_owned()
    } else if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        format!("P_{trimmed}_API_KEY")
    } else {
        format!("{trimmed}_API_KEY")
    }
}

pub(super) fn parse_dotenv_map(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let body = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((key, raw_val)) = body.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let val = raw_val.trim();
        let unquoted = ['"', '\'']
            .iter()
            .find_map(|q| val.strip_prefix(*q)?.strip_suffix(*q))
            .unwrap_or(val);
        out.insert(key.to_owned(), unquoted.to_owned());
    }
    out
}

pub(super) fn render_merged_dotenv(existing: &str, updates: &BTreeMap<String, String>) -> String {
    let mut remaining: BTreeSet<&str> = updates.keys().map(String::as_str).collect();
    let mut lines: Vec<String> = Vec::new();
    for line in existing.lines() {
        let trimmed = line.trim();
        let body = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if !trimmed.starts_with('#')
            && let Some((raw_key, _)) = body.split_once('=')
        {
            let key = raw_key.trim();
            if let Some(new_val) = updates.get(key) {
                lines.push(format_dotenv_assignment(key, new_val));
                remaining.remove(key);
                continue;
            }
        }
        lines.push(line.to_owned());
    }
    for key in remaining {
        if let Some(val) = updates.get(key) {
            lines.push(format_dotenv_assignment(key, val));
        }
    }
    let mut out = lines.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn format_dotenv_assignment(key: &str, value: &str) -> String {
    let safe_plain = !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '+' | '=')
        });
    if safe_plain {
        format!("{key}={value}")
    } else {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        format!("{key}=\"{escaped}\"")
    }
}

pub(super) fn atomic_write_secret(path: &Path, content: &str) -> Result {
    let target = resolve_write_target(path);
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| HrnError::Io(format!("create `{}`: {e}", parent.display())))?;
    }
    let seq = SECRET_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".{}.tmp-{}-{seq}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("secret"),
        std::process::id()
    );
    let tmp_path = target.with_file_name(tmp_name);
    write_secret_file(&tmp_path, content)?;
    if let Err(rename_err) = std::fs::rename(&tmp_path, &target) {
        let _ = std::fs::remove_file(&tmp_path);
        write_secret_file(&target, content).map_err(|write_err| {
            HrnError::Io(format!(
                "write `{}` failed (rename: {rename_err}; fallback: {write_err})",
                target.display()
            ))
        })?;
    }
    Ok(())
}

fn write_secret_file(path: &Path, content: &str) -> Result {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| HrnError::Io(format!("open `{}`: {e}", path.display())))?;
    file.write_all(content.as_bytes())
        .map_err(|e| HrnError::Io(format!("write `{}`: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub(super) fn collect_toml_leaves(
    table: &toml::Table,
    prefix: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, toml::Value)>,
) {
    for (k, v) in table {
        prefix.push(k.clone());
        match v {
            toml::Value::Table(child) => collect_toml_leaves(child, prefix, out),
            leaf => out.push((prefix.clone(), leaf.clone())),
        }
        prefix.pop();
    }
}

pub(super) fn has_toml_path(table: &Table, path: &[String]) -> bool {
    let Some((head, tail)) = path.split_first() else {
        return false;
    };
    if tail.is_empty() {
        return table.contains_key(head);
    }
    table
        .get(head)
        .and_then(Item::as_table)
        .is_some_and(|child| has_toml_path(child, tail))
}

pub(super) fn set_toml_path(table: &mut Table, path: &[String], value: &toml::Value) -> Result {
    let Some((head, tail)) = path.split_first() else {
        return Ok(());
    };
    if tail.is_empty() {
        let mut item = to_edit_item(value)?;
        if let Some(existing) = table.get_mut(head) {
            if let (Some(old_val), Some(new_val)) = (existing.as_value(), item.as_value_mut()) {
                *new_val.decor_mut() = old_val.decor().clone();
            }
            *existing = item;
        } else {
            table.insert(head, item);
        }
        return Ok(());
    }
    if !table.contains_key(head) {
        let mut child = Table::new();
        child.set_implicit(tail.len() > 1);
        table.insert(head, Item::Table(child));
    }
    let Some(next) = table.get_mut(head).and_then(Item::as_table_mut) else {
        return Err(HrnError::Config(format!(
            "`{head}` is not a table in config.toml"
        )));
    };
    set_toml_path(next, tail, value)
}

pub(super) fn map_entries(node: Option<&Yaml>) -> Vec<(String, Yaml)> {
    node.map_or(&[][..], Yaml::entries).to_vec()
}

pub(super) fn emit_yaml(node: &Yaml, indent: usize) -> String {
    let mut out = String::new();
    write_yaml(node, indent, &mut out);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn write_yaml(node: &Yaml, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match node {
        Yaml::Scalar(s) => {
            let _ = writeln!(out, "{pad}{}", format_yaml_scalar(s));
        }
        Yaml::Map(entries) if entries.is_empty() => {
            let _ = writeln!(out, "{pad}{{}}");
        }
        Yaml::Map(entries) => {
            for (k, v) in entries {
                write_yaml_kv(k, v, indent, true, out);
            }
        }
        Yaml::Seq(items) if items.is_empty() => {
            let _ = writeln!(out, "{pad}[]");
        }
        Yaml::Seq(items) => {
            for item in items {
                match item {
                    Yaml::Scalar(s) => {
                        let _ = writeln!(out, "{pad}- {}", format_yaml_scalar(s));
                    }
                    Yaml::Map(entries) if entries.is_empty() => {
                        let _ = writeln!(out, "{pad}- {{}}");
                    }
                    Yaml::Map(entries) => {
                        if let Some(((first_k, first_v), rest)) = entries.split_first() {
                            out.push_str(&pad);
                            out.push_str("- ");
                            write_yaml_kv(first_k, first_v, indent + 2, false, out);
                            for (k, v) in rest {
                                write_yaml_kv(k, v, indent + 2, true, out);
                            }
                        }
                    }
                    Yaml::Seq(nested) if nested.is_empty() => {
                        let _ = writeln!(out, "{pad}- []");
                    }
                    Yaml::Seq(_) => {
                        let _ = writeln!(out, "{pad}-");
                        write_yaml(item, indent + 2, out);
                    }
                }
            }
        }
    }
}

fn write_yaml_kv(key: &str, value: &Yaml, indent: usize, lead_pad: bool, out: &mut String) {
    if lead_pad {
        out.push_str(&" ".repeat(indent));
    }
    out.push_str(&format_yaml_key(key));
    match value {
        Yaml::Scalar(s) => {
            let _ = writeln!(out, ": {}", format_yaml_scalar(s));
        }
        Yaml::Map(entries) if entries.is_empty() => out.push_str(": {}\n"),
        Yaml::Seq(items) if items.is_empty() => out.push_str(": []\n"),
        Yaml::Map(_) | Yaml::Seq(_) => {
            out.push_str(":\n");
            write_yaml(value, indent + 2, out);
        }
    }
}

fn format_yaml_key(key: &str) -> String {
    let plain = !key.is_empty()
        && !key.starts_with(['\'', '"', '[', '{', '-', ' '])
        && !key.ends_with(' ')
        && !key.contains(": ")
        && !key.contains(" #")
        && !key.contains(['\n', '\r', '\t']);
    if plain {
        key.to_owned()
    } else {
        double_quote_yaml(key)
    }
}

fn format_yaml_scalar(value: &str) -> String {
    let plain = !value.is_empty()
        && !value.starts_with(['\'', '"', '[', '{', '!', '#', '-', ' '])
        && !value.ends_with(' ')
        && !value.contains(": ")
        && !value.contains(" #")
        && !value.contains(['\n', '\r', '\t']);
    if plain {
        value.to_owned()
    } else {
        double_quote_yaml(value)
    }
}

fn double_quote_yaml(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

pub(super) fn render_report(report: &ImportReport) -> String {
    let mut out = String::new();
    let banner = if report.dry_run {
        "[dry-run] OMP import preview"
    } else {
        "OMP import complete"
    };
    let _ = writeln!(out, "{banner} from {}", report.source_dir.display());
    push_list(&mut out, "providers imported", &report.providers_imported);
    if !report.providers_skipped.is_empty() {
        let _ = writeln!(
            out,
            "  providers skipped ({}):",
            report.providers_skipped.len()
        );
        for (name, reason) in &report.providers_skipped {
            let _ = writeln!(out, "    - {name}: {reason}");
        }
    }
    push_list(&mut out, "gateway tokens copied", &report.tokens_imported);
    push_list(
        &mut out,
        "gateway tokens kept existing",
        &report.tokens_skipped,
    );
    push_list(&mut out, ".env keys imported", &report.env_imported);
    push_list(&mut out, ".env keys kept existing", &report.env_skipped);
    push_list(&mut out, "modelTags imported", &report.tags_imported);
    push_list(
        &mut out,
        "config.toml keys imported",
        &report.config_imported,
    );
    push_list(
        &mut out,
        "config.toml keys kept existing",
        &report.config_skipped,
    );
    if !report.oauth_providers.is_empty() {
        let _ = writeln!(
            out,
            "  note: OAuth providers ({}) require `hrn login claude|chatgpt|kimi-code` to activate.",
            report.oauth_providers.join(", ")
        );
    }
    out
}

fn push_list(out: &mut String, label: &str, items: &[String]) {
    if !items.is_empty() {
        let _ = writeln!(out, "  {label} ({}): {}", items.len(), items.join(", "));
    }
}
