//! Comment-preserving TOML editor (`toml_edit`) with symlink-safe atomic writes.
//!
//! Used by `hrn config set/reset`, `cfg://<key>/save`, `/modelpreset save`, and `/login` setup
//! so user comments and formatting in `config.toml` survive edits.

use hrn_kernel::{HrnError, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value as EditValue};

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Follow symlink target if `path` is a symlink so writes update the real file rather than
/// replacing the symlink itself.
pub(crate) fn resolve_write_target(path: &Path) -> PathBuf {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
        && let Ok(real) = std::fs::canonicalize(path)
    {
        return real;
    }
    path.to_path_buf()
}

/// Atomically write `content` to `path` (creating parent directories and preserving symlink target).
pub(crate) fn atomic_write(path: &Path, content: &str) -> Result {
    let target = resolve_write_target(path);
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| HrnError::Io(format!("create `{}`: {e}", parent.display())))?;
    }
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".{}.tmp-{}-{seq}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml"),
        std::process::id()
    );
    let tmp_path = target.with_file_name(tmp_name);
    std::fs::write(&tmp_path, content)
        .map_err(|e| HrnError::Io(format!("write temp `{}`: {e}", tmp_path.display())))?;
    if let Err(rename_err) = std::fs::rename(&tmp_path, &target) {
        let _ = std::fs::remove_file(&tmp_path);
        std::fs::write(&target, content).map_err(|write_err| {
            HrnError::Io(format!(
                "write `{}` failed (rename: {rename_err}; fallback: {write_err})",
                target.display()
            ))
        })?;
    }
    Ok(())
}

/// Load `path` into a comment-preserving [`DocumentMut`] (empty document when the file does not
/// exist yet).
pub(crate) fn load_doc(path: &Path) -> Result<DocumentMut> {
    let target = resolve_write_target(path);
    if !target.is_file() {
        return Ok(DocumentMut::new());
    }
    let raw = std::fs::read_to_string(&target)
        .map_err(|e| HrnError::Config(format!("cannot read `{}`: {e}", target.display())))?;
    raw.parse::<DocumentMut>()
        .map_err(|e| HrnError::Config(format!("cannot parse `{}`: {e}", target.display())))
}

/// Load, mutate, and atomically save `path`, returning the serialized document text.
pub(crate) fn edit_document(
    path: &Path,
    mutate: impl FnOnce(&mut DocumentMut) -> Result,
) -> Result<String> {
    let mut doc = load_doc(path)?;
    mutate(&mut doc)?;
    let serialized = doc.to_string();
    atomic_write(path, &serialized)?;
    Ok(serialized)
}

/// Set a dotted key (e.g. `tools.premium_defer`) in `path`, preserving existing comments.
pub(crate) fn set_key(path: &Path, dotted_key: &str, value: &toml::Value) -> Result {
    let parts: Vec<&str> = dotted_key.split('.').collect();
    if parts.is_empty() || parts.iter().any(|p| p.trim().is_empty()) {
        return Err(HrnError::Config(format!("invalid key `{dotted_key}`")));
    }
    let _ = edit_document(path, |doc| set_in_table(doc.as_table_mut(), &parts, value))?;
    Ok(())
}

/// Remove a dotted key from `path`, preserving surrounding comments. Returns `true` if removed.
pub(crate) fn delete_key(path: &Path, dotted_key: &str) -> Result<bool> {
    let parts: Vec<&str> = dotted_key.split('.').collect();
    if parts.is_empty() || parts.iter().any(|p| p.trim().is_empty()) {
        return Err(HrnError::Config(format!("invalid key `{dotted_key}`")));
    }
    let target = resolve_write_target(path);
    if !target.is_file() {
        return Ok(false);
    }
    let mut doc = load_doc(&target)?;
    let removed = remove_from_table(doc.as_table_mut(), &parts);
    if removed {
        atomic_write(&target, &doc.to_string())?;
    }
    Ok(removed)
}

/// Convert a [`toml::Value`] to a [`toml_edit::Item`].
pub(crate) fn to_edit_item(value: &toml::Value) -> Result<Item> {
    match value {
        toml::Value::Table(tbl) => {
            let mut out = Table::new();
            for (k, v) in tbl {
                out.insert(k, to_edit_item(v)?);
            }
            Ok(Item::Table(out))
        }
        other => Ok(Item::Value(to_edit_value(other)?)),
    }
}

/// Convert a scalar, array, or inline table [`toml::Value`] to a [`toml_edit::Value`].
pub(crate) fn to_edit_value(value: &toml::Value) -> Result<EditValue> {
    match value {
        toml::Value::String(s) => Ok(EditValue::from(s.as_str())),
        toml::Value::Integer(n) => Ok(EditValue::from(*n)),
        toml::Value::Float(f) => Ok(EditValue::from(*f)),
        toml::Value::Boolean(b) => Ok(EditValue::from(*b)),
        toml::Value::Datetime(dt) => dt
            .to_string()
            .parse::<toml_edit::Datetime>()
            .map(EditValue::from)
            .map_err(|e| HrnError::Config(format!("invalid datetime `{dt}`: {e}"))),
        toml::Value::Array(items) => {
            let mut arr = Array::new();
            for item in items {
                arr.push(to_edit_value(item)?);
            }
            Ok(EditValue::Array(arr))
        }
        toml::Value::Table(tbl) => {
            let mut inline = InlineTable::new();
            for (k, v) in tbl {
                inline.insert(k, to_edit_value(v)?);
            }
            Ok(EditValue::InlineTable(inline))
        }
    }
}

fn set_in_table(table: &mut Table, parts: &[&str], value: &toml::Value) -> Result {
    let Some((&head, tail)) = parts.split_first() else {
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
            "`{head}` is not a table in config file"
        )));
    };
    set_in_table(next, tail, value)
}

fn remove_from_table(table: &mut Table, parts: &[&str]) -> bool {
    let Some((&head, tail)) = parts.split_first() else {
        return false;
    };
    if tail.is_empty() {
        return table.remove(head).is_some();
    }
    let Some(next) = table.get_mut(head).and_then(Item::as_table_mut) else {
        return false;
    };
    remove_from_table(next, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_delete_preserve_comments_and_siblings() {
        let dir = std::env::temp_dir().join(format!("hrn-toml-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let original = "\
# Top-level user comment
thinking = \"medium\" # keep inline comment

[tui]
# Theme preference
theme = \"titanium\"
notify = true
";
        atomic_write(&path, original).expect("seed");
        set_key(&path, "thinking", &toml::Value::String("high".into())).expect("set thinking");
        set_key(&path, "tools.premium_defer", &toml::Value::Boolean(false)).expect("set nested");
        let after_set = std::fs::read_to_string(&path).expect("read");
        assert!(
            after_set.contains("# Top-level user comment"),
            "{after_set}"
        );
        assert!(after_set.contains("# keep inline comment"), "{after_set}");
        assert!(after_set.contains("# Theme preference"), "{after_set}");
        assert!(after_set.contains("thinking = \"high\""), "{after_set}");
        assert!(after_set.contains("premium_defer = false"), "{after_set}");

        assert!(delete_key(&path, "tui.theme").expect("delete"));
        assert!(!delete_key(&path, "tui.nonexistent").expect("delete missing"));
        let after_del = std::fs::read_to_string(&path).expect("read");
        assert!(
            after_del.contains("# Top-level user comment"),
            "{after_del}"
        );
        assert!(after_del.contains("notify = true"), "{after_del}");
        assert!(!after_del.contains("titanium"), "{after_del}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
