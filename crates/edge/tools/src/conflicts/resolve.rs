//! Applying a conflict resolution: `write conflict://N` / `conflict://*`.

use super::{Block, parse, parse_address, shown_list, stale};
use crate::conflicts::rel;
use crate::fs_read::FileReadTracker;
use crate::hashline::tag_of;
use crate::text_codec::{self, TextFile};
use hrn_kernel::{HrnError, Result, ToolOutput};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

#[derive(Clone, Copy)]
enum Side {
    Ours,
    Theirs,
    Base,
    Both,
}

fn side_of(token: &str) -> Option<Side> {
    match token {
        "@ours" => Some(Side::Ours),
        "@theirs" => Some(Side::Theirs),
        "@base" => Some(Side::Base),
        "@both" => Some(Side::Both),
        _ => None,
    }
}

fn side_lines(block: &Block, side: Side) -> Result<Vec<String>> {
    Ok(match side {
        Side::Ours => block.ours.clone(),
        Side::Theirs => block.theirs.clone(),
        Side::Both => [block.ours.clone(), block.theirs.clone()].concat(),
        Side::Base => block.base.clone().ok_or_else(|| {
            HrnError::Tool(format!(
                "conflict {} has no `@base`: the merge was not run with diff3 style",
                block.id
            ))
        })?,
    })
}

/// `content` with every whole-line `@token` replaced by the hunk's side.
fn expand(block: &Block, content: &str) -> Result<Vec<String>> {
    if content.trim().is_empty() {
        return Err(HrnError::Tool(format!(
            "refusing to resolve conflict {} with empty content; pass the resolved text (or \
             `@ours` / `@theirs` / `@both`)",
            block.id
        )));
    }
    let normalized = text_codec::normalize_eol(content);
    let mut out = Vec::new();
    for line in normalized.lines() {
        match side_of(line.trim()) {
            Some(side) => out.extend(side_lines(block, side)?),
            None => out.push(line.to_owned()),
        }
    }
    Ok(out)
}

/// `N: @token` bulk directives; `None` when `content` is ordinary text.
fn directives(content: &str) -> Option<BTreeMap<usize, Side>> {
    let mut map = BTreeMap::new();
    let mut lines = 0usize;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        let (head, token) = line.split_once(':')?;
        let id = head.trim().parse::<usize>().ok()?;
        let side = side_of(token.trim())?;
        if map.insert(id, side).is_some() {
            return None;
        }
    }
    (lines > 0).then_some(map)
}

/// `write conflict://…`: resolve the addressed hunks, one atomic rewrite per file.
///
/// # Errors
/// No registered hunk matches `rest`, or a hunk no longer matches what was read.
pub fn write(
    tracker: &FileReadTracker,
    cwd: &Path,
    rest: &str,
    content: &str,
) -> Result<ToolOutput> {
    let addr = parse_address(rest)?;
    if addr.side.is_some() {
        return Err(HrnError::Tool(format!(
            "`conflict://{rest}` is a view; write `conflict://N` or `conflict://*`"
        )));
    }
    let entries = tracker.conflicts().snapshot();
    if entries.is_empty() {
        return Err(HrnError::Tool(
            "no merge conflicts are registered; read the conflicted file (or `path:conflicts`) \
             first"
                .into(),
        ));
    }
    let directives = directives(content);
    if addr.id.is_none() && directives.is_none() && content.trim().is_empty() {
        return Err(HrnError::Tool(
            "`conflict://*` needs content: one text, or one `N: @ours|@theirs|@base|@both` line \
             per hunk"
                .into(),
        ));
    }
    let mut receipts = Vec::new();
    let mut failures = Vec::new();
    let mut asked = 0usize;
    for (path, registered) in &entries {
        let selected: Vec<usize> = registered
            .iter()
            .filter(|b| match (directives.as_ref(), addr.id) {
                (Some(map), _) => map.contains_key(&b.id),
                (None, None) => true,
                (None, Some(id)) => b.id == id,
            })
            .map(|b| b.id)
            .collect();
        if selected.is_empty() {
            continue;
        }
        asked += selected.len();
        match resolve_file(
            tracker,
            cwd,
            path,
            registered,
            &selected,
            content,
            directives.as_ref(),
        ) {
            Ok(lines) => receipts.extend(lines),
            Err(e) => failures.push(e.to_string()),
        }
    }
    if asked == 0 {
        return Err(stale(
            addr.id.unwrap_or_default(),
            &shown_list(cwd, &entries),
        ));
    }
    let remaining = tracker
        .conflicts()
        .snapshot()
        .iter()
        .map(|(_, blocks)| blocks.len())
        .sum::<usize>();
    let mut out = String::new();
    out.push_str(&receipts.join("\n"));
    if !receipts.is_empty() && remaining > 0 {
        let _ = write!(
            out,
            "\n{remaining} conflict(s) still registered; re-read `path:conflicts` for the ids."
        );
    }
    if failures.is_empty() {
        return Ok(ToolOutput::ok(out.trim_end().to_owned()));
    }
    if !receipts.is_empty() {
        let _ = write!(out, "\nFailed:\n{}", failures.join("\n"));
        let mut partial = ToolOutput::ok(out.trim_end().to_owned());
        partial.is_error = true;
        return Ok(partial);
    }
    Err(HrnError::Tool(failures.join("\n")))
}

/// Rewrite one file: every selected hunk bottom-up, then a single atomic write.
fn resolve_file(
    tracker: &FileReadTracker,
    cwd: &Path,
    path: &Path,
    registered: &[Block],
    selected: &[usize],
    content: &str,
    directives: Option<&BTreeMap<usize, Side>>,
) -> Result<Vec<String>> {
    let shown = rel(cwd, path);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracker.conflicts().forget_file(path);
            return Err(HrnError::Tool(format!(
                "cannot read `{shown}` ({e}); its conflicts were forgotten, read it again"
            )));
        }
    };
    let mut file = TextFile::decode(bytes).map_err(|e| {
        HrnError::Tool(format!(
            "`{shown}`: {e}; only UTF-8 text conflicts can be resolved"
        ))
    })?;
    let ends_newline = file.text.ends_with('\n');
    let current = parse(&file.text);
    let mut edits: Vec<(usize, usize, Vec<String>, usize)> = Vec::new();
    let mut misses = Vec::new();
    for &id in selected {
        let Some(wanted) = registered.iter().find(|b| b.id == id) else {
            continue;
        };
        // Positions come from the file as it is now; a hunk whose content changed is stale.
        let Some(block) = current.iter().find(|b| b.same_sides(wanted)) else {
            misses.push(id);
            continue;
        };
        let replacement = match directives {
            Some(map) => side_lines(block, map[&id])?,
            None => expand(block, content)?,
        };
        edits.push((
            block.start,
            block.end,
            replacement,
            block.end - block.start + 1,
        ));
    }
    if !misses.is_empty() {
        return Err(HrnError::Tool(format!(
            "conflict(s) {} in `{shown}` no longer match what you read (the file now has {} \
             hunk(s)); re-read `{shown}:conflicts`",
            misses
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            current.len()
        )));
    }
    let mut lines: Vec<&str> = file.text.lines().collect();
    edits.sort_by_key(|(start, _, _, _)| *start);
    for (start, end, replacement, _) in edits.iter().rev() {
        lines.splice((start - 1)..*end, replacement.iter().map(String::as_str));
    }
    file.text = lines.join("\n");
    if ends_newline && !file.text.is_empty() {
        file.text.push('\n');
    }
    text_codec::write_atomic(path, &file.encode())
        .map_err(|e| HrnError::Tool(format!("cannot write `{shown}`: {e}")))?;
    tracker.record_current(path);
    tracker.conflicts().register(path, &file.text);
    Ok(edits
        .into_iter()
        .map(|(start, end, replacement, was)| {
            format!(
                "[{shown}#{}] Resolved conflict(s) at lines {start}-{end}: {was} -> {} line(s).",
                tag_of(&file.text),
                replacement.len()
            )
        })
        .collect())
}
