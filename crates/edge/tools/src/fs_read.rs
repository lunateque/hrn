//! `read_file` tool and the shared [`FileReadTracker`] that makes stale edits detectable.
//!
//! The one `path` argument carries everything: a file, a directory, or an internal URL, each
//! with an optional trailing line selector (see [`crate::selector`]).

use crate::conflicts;
use crate::dir_tree;
use crate::paths::{display_rel, resolve};
pub use crate::read_tracker::FileReadTracker;
use crate::read_tracker::key_of;
use crate::selector::{self, Selector, Span};
use hrn_kernel::{HrnError, Result, ToolContext, ToolOutput, UrlBody, UrlRouter};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// Lines returned by a read without an end (`path`, `path:50-`).
const DEFAULT_LIMIT: usize = 300;
/// Rows returned by a conflict view without an explicit range.
pub(crate) const DEFAULT_VIEW_LIMIT: usize = DEFAULT_LIMIT;
/// Body bytes per read; stays under the harness output cap so a read never needs a spill.
const BODY_BUDGET: usize = 22 * 1024;
const MAX_LINE_CHARS: usize = 2000;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const BINARY_PROBE_LEN: usize = 8192;
/// Context a single bounded range shows around itself.
const CONTEXT_BEFORE: usize = 1;
const CONTEXT_AFTER: usize = 3;

pub(crate) fn clip_line(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => format!(
            "{} … [line cut at {MAX_LINE_CHARS} chars]",
            line.get(..cut).unwrap_or_default()
        ),
        None => line.to_owned(),
    }
}

/// Whether `raw` names a web page (`read` fetches those instead of touching the disk).
#[must_use]
pub fn is_web_url(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www.")
}

/// Fetch `raw` (a web URL with an optional trailing selector) and render the text it returns.
///
/// # Errors
/// As [`crate::PageFetcher::fetch`], plus a malformed selector.
pub async fn read_url(fetcher: &crate::PageFetcher, cwd: &Path, raw: &str) -> Result<ToolOutput> {
    let (url, sel) = selector::split_url(raw)?;
    let url = if url.starts_with("www.") {
        format!("https://{url}")
    } else {
        url.to_owned()
    };
    let shown = display_rel(cwd, Path::new(&url)).replace('\\', "/");
    let text = fetcher.fetch(&url, sel.raw).await?;
    Ok(render_text(&shown, &text, &sel, None))
}

/// Resolve `raw` (URL, literal path, or path plus selector) and read it with hashline anchors
/// enabled; records the read in `tracker` so later edits can detect stale or unseen lines.
pub fn read_sync(tracker: &FileReadTracker, ctx: &ToolContext, raw: &str) -> Result<ToolOutput> {
    read_sync_with_anchors(tracker, ctx, raw, true)
}

/// Like [`read_sync`], controlling whether `[path#TAG]` hashline anchors are emitted (dropped
/// when `[edit] mode = "none"` or a non-hashline editor is active).
pub fn read_sync_with_anchors(
    tracker: &FileReadTracker,
    ctx: &ToolContext,
    raw: &str,
    hashline_anchors: bool,
) -> Result<ToolOutput> {
    if let Some((raw_scheme, _)) = UrlRouter::split(raw) {
        let (base, sel) =
            if raw_scheme.eq_ignore_ascii_case("ssh") || raw_scheme.eq_ignore_ascii_case("mcp") {
                selector::split_url(raw)?
            } else {
                selector::split(raw)?
            };
        let (scheme, rest) = UrlRouter::split(base)
            .ok_or_else(|| HrnError::Tool(format!("`{raw}` has no resource after the scheme")))?;
        if scheme.eq_ignore_ascii_case("conflict") {
            return conflicts::view(tracker, &ctx.cwd, rest, &sel);
        }
        let (url_rest, query) = if (scheme.eq_ignore_ascii_case("attachment")
            || scheme.eq_ignore_ascii_case("ssh"))
            && let Some((head, q)) = rest.split_once('?')
        {
            (head, Some(q))
        } else {
            (rest, None)
        };
        return match ctx.urls.read(ctx, scheme, url_rest)? {
            UrlBody::Text(text) => {
                if let Some(q) = query {
                    crate::read_fmt::query_json_text(base, &text, q, &sel)
                } else {
                    Ok(render_text_tracked(base, &text, &sel, None, hashline_anchors).0)
                }
            }
            UrlBody::Path(path) => {
                if let Some(out) = crate::read_fmt::read_resolved_path(&path, base, query, &sel)? {
                    return Ok(out);
                }
                read_path(None, &path, base, &sel, hashline_anchors)
            }
        };
    }
    if let Some(out) = crate::read_fmt::read(&ctx.cwd, raw)? {
        return Ok(out);
    }
    // An existing file named like a selector (`notes:2`) wins over the selector reading.
    let literal = resolve(&ctx.cwd, raw);
    let (path, sel) = if literal.exists() {
        (literal, Selector::default())
    } else {
        let (base, sel) = selector::split(raw)?;
        (resolve(&ctx.cwd, base), sel)
    };
    read_path(
        Some(tracker),
        &path,
        &display_rel(&ctx.cwd, &path),
        &sel,
        hashline_anchors,
    )
}

fn read_path(
    tracker: Option<&FileReadTracker>,
    path: &Path,
    shown: &str,
    sel: &Selector,
    hashline_anchors: bool,
) -> Result<ToolOutput> {
    let meta =
        fs::metadata(path).map_err(|e| HrnError::Tool(format!("cannot stat `{shown}`: {e}")))?;
    if meta.is_dir() {
        return directory(path, shown, sel);
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(HrnError::Tool(format!(
            "`{shown}` is {} bytes, too large to read; use `grep` or `bash` (head/tail) instead",
            meta.len()
        )));
    }
    if sel.conflicts {
        let tracker = tracker.ok_or_else(|| {
            HrnError::Tool(
                "`path:conflicts` indexes a file on disk; locate the markers in the URL text \
                 instead"
                    .to_owned(),
            )
        })?;
        return conflicts::index_of(tracker, path, shown);
    }
    // Stat before reading: a write racing the read then shows up as stale instead of hiding.
    let mtime = meta.modified().ok();
    let bytes =
        fs::read(path).map_err(|e| HrnError::Tool(format!("cannot read `{shown}`: {e}")))?;
    if let (Some(tracker), Some(mtime)) = (tracker, mtime) {
        tracker.record(path, mtime);
    }
    let svgz = lower_ext(path) == "svgz";
    if sel.img && is_svg(path) {
        return Err(HrnError::Tool(format!(
            "`{shown}` is SVG (vector); `:img` rasterization is not supported; read it as text"
        )));
    }
    if sel.img || crate::img::is_image_path(path) || crate::img::looks_like_image_bytes(&bytes) {
        return image(path, shown, &bytes);
    }
    let bytes = if svgz { gunzip(shown, &bytes)? } else { bytes };
    if bytes
        .get(..bytes.len().min(BINARY_PROBE_LEN))
        .is_some_and(|probe| probe.contains(&0))
    {
        if let Some(meta) = crate::read_fmt::exe_meta(path, bytes.len() as u64)? {
            let mut out = ToolOutput::ok(meta);
            out.read_key = Some(format!("{}:exe", key_of(path).display()));
            return Ok(out);
        }
        return Ok(ToolOutput::ok(format!(
            "[binary file `{shown}` ({} bytes); not shown]",
            bytes.len()
        )));
    }
    let decoded = String::from_utf8_lossy(&bytes);
    let text = decoded.strip_prefix('\u{feff}').unwrap_or(&decoded);
    let (mut out, shown_windows, empty_file) =
        render_text_tracked(shown, text, sel, Some(path), hashline_anchors);
    if let Some(tracker) = tracker {
        tracker.record_seen(path, &shown_windows, empty_file);
    }
    // Registering a conflicted file here is what makes `conflict://N` resolvable after a plain
    // read; the note tells the model the ids exist without a second call.
    if !sel.raw
        && text.contains("<<<<<<<")
        && let Some(tracker) = tracker
    {
        let blocks = tracker.conflicts().register(path, text);
        if !blocks.is_empty() {
            out.content.push_str(&conflicts::note(shown, &blocks));
        }
    }
    Ok(out)
}

fn lower_ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map_or_else(String::new, str::to_ascii_lowercase)
}

fn is_svg(path: &Path) -> bool {
    matches!(lower_ext(path).as_str(), "svg" | "svgz")
}

fn gunzip(shown: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::bufread::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| HrnError::Tool(format!("cannot gunzip `{shown}`: {e}")))?;
    Ok(out)
}

fn image(path: &Path, shown: &str, bytes: &[u8]) -> Result<ToolOutput> {
    let att = crate::img::load_image_from_bytes(
        bytes,
        Some(path),
        Some(shown.to_owned()),
        crate::img::DEFAULT_MAX_IMAGE_DIM,
    )?;
    let dims = match (att.width, att.height) {
        (Some(w), Some(h)) => format!("{w}x{h}"),
        _ => "unknown".to_owned(),
    };
    let kb = bytes.len().div_ceil(1024);
    let mut out = ToolOutput::ok(format!(
        "Image: {shown} ({dims}, {}, {kb} KB)",
        att.media_type
    ));
    out.images.push(att);
    Ok(out)
}

/// The tree of a directory; selectors slice its rendered lines.
fn directory(path: &Path, shown: &str, sel: &Selector) -> Result<ToolOutput> {
    let lines = dir_tree::render(path, shown)?;
    Ok(paged_lines(shown, &lines, sel, lines.len().max(1)))
}

/// Unnumbered `lines` sliced by `sel` (at most `limit` without an explicit end), with a footer
/// naming the selector that continues a cut listing.
pub(crate) fn paged_lines(
    shown: &str,
    lines: &[String],
    sel: &Selector,
    limit: usize,
) -> ToolOutput {
    let total = lines.len();
    let windows = sel.windows(total, limit);
    if windows.is_empty() {
        return ToolOutput::ok(past_the_end(shown, total, sel));
    }
    let mut rows: Vec<&str> = Vec::new();
    for &(a, b) in &windows {
        rows.extend(lines.iter().skip(a - 1).take(b + 1 - a).map(String::as_str));
    }
    let mut text = rows.join("\n");
    let last = windows.last().map_or(total, |w| w.1);
    if last < total {
        let first = windows.first().map_or(1, |w| w.0);
        let _ = write!(
            text,
            "\n[Showing lines {first}-{last} of {total}. Use `{shown}:{}-` to continue.]",
            last + 1
        );
    }
    ToolOutput::ok(text)
}

fn past_the_end(shown: &str, total: usize, sel: &Selector) -> String {
    format!(
        "[`{shown}` has {total} lines; `:{}` is past the end. Use `{shown}:{total}` for the last \
         line or `{shown}:-20` for the tail.]",
        sel.key()
    )
}

/// The snapshot header `[path#TAG]` of a file (what `edit` anchors on); URL text and non-hashline
/// reads use `File: {shown}` without a `#TAG` anchor.
fn header(shown: &str, text: &str, key_path: Option<&Path>, hashline_anchors: bool) -> String {
    match key_path {
        Some(_) if hashline_anchors => format!("[{shown}#{}]", crate::hashline::tag_of(text)),
        _ => format!("File: {shown}"),
    }
}

fn collect_rows(
    lines: &[&str],
    windows: &[(usize, usize)],
    raw: bool,
    hashline_anchors: bool,
) -> (Vec<String>, Vec<(usize, usize)>, bool) {
    let sep = if hashline_anchors { ':' } else { '|' };
    let mut rows: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut shown_windows: Vec<(usize, usize)> = Vec::new();
    let mut cut = false;
    for &(a, b) in windows {
        let mut last = None;
        for n in a..=b {
            let Some(line) = lines.get(n - 1) else { break };
            let row = if raw {
                (*line).to_owned()
            } else {
                format!("{n}{sep}{}", clip_line(line))
            };
            if !rows.is_empty() && bytes + row.len() + 1 > BODY_BUDGET {
                cut = true;
                break;
            }
            bytes += row.len() + 1;
            rows.push(row);
            last = Some(n);
        }
        if let Some(last) = last {
            shown_windows.push((a, last));
        }
        if cut {
            break;
        }
    }
    (rows, shown_windows, cut)
}

/// Render the selected lines of `text`; `key_path` names the file for compaction supersede keys.
pub(crate) fn render_text(
    shown: &str,
    text: &str,
    sel: &Selector,
    key_path: Option<&Path>,
) -> ToolOutput {
    render_text_tracked(shown, text, sel, key_path, true).0
}

/// Render the selected lines of `text`, returning `(output, shown_windows, empty_file)`.
fn render_text_tracked(
    shown: &str,
    text: &str,
    sel: &Selector,
    key_path: Option<&Path>,
    hashline_anchors: bool,
) -> (ToolOutput, Vec<(usize, usize)>, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return (
            ToolOutput::ok(format!(
                "{} (empty)",
                header(shown, text, key_path, hashline_anchors)
            )),
            Vec::new(),
            true,
        );
    }
    let mut windows = sel.windows(total, DEFAULT_LIMIT);
    if windows.is_empty() {
        return (
            ToolOutput::ok(past_the_end(shown, total, sel)),
            Vec::new(),
            false,
        );
    }
    if !sel.raw
        && sel.is_single_bounded()
        && let Some(w) = windows.first_mut()
    {
        *w = (
            w.0.saturating_sub(CONTEXT_BEFORE).max(1),
            (w.1 + CONTEXT_AFTER).min(total),
        );
    }

    let (rows, shown_windows, cut) = collect_rows(&lines, &windows, sel.raw, hashline_anchors);
    let spans = shown_windows
        .iter()
        .map(|(a, b)| format!("{a}-{b}"))
        .collect::<Vec<_>>()
        .join(",");
    let last_end = shown_windows.last().map_or(0, |w| w.1);
    let open_ended = sel.spans.is_empty()
        || sel
            .spans
            .iter()
            .any(|s| matches!(s, Span::Lines { end: None, .. }));
    let mut out = String::new();
    if !sel.raw {
        let _ = writeln!(
            out,
            "{} (lines {spans} of {total})",
            header(shown, text, key_path, hashline_anchors)
        );
    }
    out.push_str(&rows.join("\n"));
    if !sel.raw {
        out.push('\n');
    }
    if last_end < total && (cut || open_ended) {
        let first = shown_windows.first().map_or(1, |w| w.0);
        let _ = write!(
            out,
            "{}[Showing lines {first}-{last_end} of {total}. Use `{shown}:{}-` to continue.]",
            if sel.raw { "\n\n" } else { "" },
            last_end + 1
        );
    }

    let mut result = ToolOutput::ok(out);
    result.read_key = key_path.map(|p| {
        format!(
            "{}:{spans}{}",
            key_of(p).display(),
            if sel.raw { ":raw" } else { "" }
        )
    });
    (result, shown_windows, false)
}

#[cfg(test)]
#[path = "fs_read_tests.rs"]
mod tests;
