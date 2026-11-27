//! `Alt+G` (`app.local.open`): strict discovery and picker overlay for session `local://`
//! scratch artifacts stored under the session tree's local artifacts directory.

use crate::theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};
use std::path::{Component, Path, PathBuf};

/// One discovered session `local://` artifact strictly contained inside the session scratch dir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalArtifact {
    /// Forward-slash relative path used in `local://<rel>`.
    pub(crate) rel: String,
    /// Canonical path inside the session's local artifacts directory.
    pub(crate) path: PathBuf,
    /// File size in bytes.
    pub(crate) bytes: u64,
}

/// Strictly resolve `rel` inside `root`, rejecting `..`, absolute/drive components, backslashes,
/// and symlinks escaping `root`.
pub(crate) fn resolve_artifact(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let canon_root = std::fs::canonicalize(root).map_err(|e| {
        format!(
            "cannot access local artifacts dir `{}`: {e}",
            root.display()
        )
    })?;
    let trimmed = rel.trim().trim_start_matches("local://");
    if trimmed.is_empty() || trimmed.contains(['\\', ':']) {
        return Err(format!("invalid local artifact path `{rel}`"));
    }
    let mut safe_rel = PathBuf::new();
    for seg in trimmed.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err(format!("path traversal is not allowed in `{rel}`")),
            part => safe_rel.push(part),
        }
    }
    if safe_rel.as_os_str().is_empty()
        || safe_rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("invalid local artifact path `{rel}`"));
    }
    let candidate = canon_root.join(&safe_rel);
    let canon_target = std::fs::canonicalize(&candidate)
        .map_err(|e| format!("cannot access `local://{trimmed}`: {e}"))?;
    if !canon_target.starts_with(&canon_root) {
        return Err(format!(
            "refusing to open `{rel}` outside the session local artifacts directory"
        ));
    }
    if !canon_target.is_file() {
        return Err(format!("`local://{trimmed}` is not a regular file"));
    }
    Ok(canon_target)
}

/// List all regular files strictly inside `root`, sorted deterministically by `rel`.
pub(crate) fn list_artifacts(root: &Path) -> Vec<LocalArtifact> {
    let Ok(canon_root) = std::fs::canonicalize(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_dir(&canon_root, &canon_root, "", &mut out);
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

fn collect_dir(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<LocalArtifact>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        let Ok(canon) = std::fs::canonicalize(&path) else {
            continue;
        };
        if !canon.starts_with(root) {
            continue;
        }
        if meta.is_dir() {
            collect_dir(root, &canon, &rel, out);
        } else if meta.is_file() {
            out.push(LocalArtifact {
                rel,
                path: canon,
                bytes: meta.len(),
            });
        }
    }
}

/// Outcome of routing a key to [`LocalPicker`].
pub(crate) enum LocalPickerAction {
    Stay,
    Close,
    Open(LocalArtifact),
}

/// Interactive picker overlay shown when `Alt+G` finds multiple `local://` artifacts.
pub(crate) struct LocalPicker {
    root: PathBuf,
    items: Vec<LocalArtifact>,
    query: String,
    selected: usize,
}

impl LocalPicker {
    pub(crate) fn new(root: PathBuf, items: Vec<LocalArtifact>) -> Self {
        Self {
            root,
            items,
            query: String::new(),
            selected: 0,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn visible(&self) -> Vec<&LocalArtifact> {
        let q = self.query.to_ascii_lowercase();
        self.items
            .iter()
            .filter(|a| q.is_empty() || a.rel.to_ascii_lowercase().contains(&q))
            .collect()
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> LocalPickerAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return LocalPickerAction::Close;
        }
        let count = self.visible().len();
        match key.code {
            KeyCode::Esc => return LocalPickerAction::Close,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(item) = self.visible().get(self.selected) {
                    return LocalPickerAction::Open((*item).clone());
                }
                return LocalPickerAction::Close;
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        LocalPickerAction::Stay
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 16 || area.height < 6 {
            return;
        }
        Clear.render(area, buf);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(
                format!(" Local artifacts ({}) ", self.items.len()),
                theme::bold(),
            ))
            .title_bottom(Span::styled(" enter open | esc close ", theme::muted()));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 2 {
            return;
        }
        let filter_line = Line::from(vec![
            Span::styled("filter: ", theme::accent()),
            if self.query.is_empty() {
                Span::styled("type to filter local:// files", theme::dim())
            } else {
                Span::raw(self.query.clone())
            },
        ]);
        buf.set_line(inner.x, inner.y, &filter_line, inner.width);

        let visible = self.visible();
        let rows = usize::from(inner.height.saturating_sub(1));
        let selected = self.selected.min(visible.len().saturating_sub(1));
        let first = (selected + 1).saturating_sub(rows);
        for (i, item) in visible.iter().enumerate().skip(first).take(rows) {
            let on = i == selected;
            let size = format!(" ({} B)", item.bytes);
            let label = format!("local://{}", item.rel);
            let room = usize::from(inner.width).saturating_sub(size.len() + 2);
            let mut line = Line::from(vec![
                Span::raw(if on { "> " } else { "  " }),
                Span::raw(wrap::truncate(&label, room).into_owned()),
                Span::styled(size, theme::dim()),
            ]);
            if on {
                line = line.style(theme::focus().add_modifier(Modifier::BOLD));
            }
            let y = inner.y + 1 + u16::try_from(i - first).unwrap_or(0);
            buf.set_line(inner.x, y, &line, inner.width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_artifact_rejects_traversal_and_accepts_contained_files() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir =
            std::env::temp_dir().join(format!("hrn-local-art-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
        std::fs::write(dir.join("1.log"), "hello").expect("write");
        std::fs::write(dir.join("sub").join("notes.md"), "notes").expect("write");

        let list = list_artifacts(&dir);
        let rels: Vec<&str> = list.iter().map(|a| a.rel.as_str()).collect();
        assert_eq!(rels, vec!["1.log", "sub/notes.md"]);

        assert!(resolve_artifact(&dir, "1.log").is_ok());
        assert!(resolve_artifact(&dir, "sub/notes.md").is_ok());
        assert!(resolve_artifact(&dir, "../escape.txt").is_err());
        assert!(resolve_artifact(&dir, "sub/../../escape.txt").is_err());
        assert!(resolve_artifact(&dir, "C:\\Windows\\win.ini").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
