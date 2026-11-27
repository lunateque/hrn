//! Numbered image attachments (`[Image #1]` / `фото №1`), clipboard capture,
//! path extraction, and truecolor Unicode half-block (`▀`) in-console previews.

use crate::theme;
use crate::wrap;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use crossterm::event::{KeyCode, KeyEvent};
use hrn_kernel::ImageAttachment;
use image::{DynamicImage, GenericImageView, ImageFormat, imageops::FilterType};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};
use std::collections::HashSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};

const MAX_ATTACH_DIM: u32 = 1600;
const MAX_ASSISTANT_PREVIEWS: usize = 4;
const BG_RGB: [u32; 3] = [24, 26, 32];

/// Session-scoped 1-indexed image registry (`[Image #1]`, `[Image #2]`, ...) and staging tray.
#[derive(Default)]
pub(crate) struct ImageTray {
    registered: Vec<ImageAttachment>,
    pending: Vec<usize>,
}

impl ImageTray {
    pub(crate) fn clear(&mut self) {
        self.registered.clear();
        self.pending.clear();
    }

    pub(crate) fn clear_pending(&mut self) {
        self.pending.clear();
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn register(&mut self, img: ImageAttachment) -> usize {
        self.registered.push(img);
        self.registered.len()
    }

    pub(crate) fn stage(&mut self, img: ImageAttachment) -> usize {
        let id = self.register(img);
        if !self.pending.contains(&id) {
            self.pending.push(id);
        }
        id
    }

    pub(crate) fn get(&self, id: usize) -> Option<&ImageAttachment> {
        id.checked_sub(1).and_then(|i| self.registered.get(i))
    }

    pub(crate) fn pending_labels(&self) -> Vec<String> {
        self.pending
            .iter()
            .filter_map(|&id| {
                let att = self.get(id)?;
                let name = att.label.as_deref().unwrap_or("image");
                Some(format!("[Image #{id}] {name}"))
            })
            .collect()
    }

    /// Drain staged images and resolve any `@path/to/img.png` or explicit `[Image #N]` references
    /// in `prompt`, returning `(Some(id), attachment)` pairs in order.
    pub(crate) fn take_for_prompt(
        &mut self,
        prompt: &str,
        cwd: &Path,
    ) -> Vec<(Option<usize>, ImageAttachment)> {
        let mut ids: Vec<usize> = std::mem::take(&mut self.pending);
        let mut seen: HashSet<usize> = ids.iter().copied().collect();

        for path in extract_at_image_paths(prompt, cwd) {
            if let Ok(att) = load_image_file(&path) {
                let id = self.register(att);
                if seen.insert(id) {
                    ids.push(id);
                }
            }
        }
        for id in extract_image_refs(prompt) {
            if self.get(id).is_some() && seen.insert(id) {
                ids.push(id);
            }
        }
        ids.into_iter()
            .filter_map(|id| Some((Some(id), self.get(id)?.clone())))
            .collect()
    }

    /// Resolve images referenced by an assistant reply via `[Image #N]`, `фото №N`, or existing
    /// local image paths in `cwd`.
    pub(crate) fn resolve_assistant_previews(
        &mut self,
        text: &str,
        cwd: &Path,
    ) -> Vec<(Option<usize>, ImageAttachment)> {
        let mut out: Vec<(Option<usize>, ImageAttachment)> = Vec::new();
        let mut seen_ids: HashSet<usize> = HashSet::new();
        let mut seen_paths: HashSet<PathBuf> = HashSet::new();

        for id in extract_image_refs(text) {
            if out.len() >= MAX_ASSISTANT_PREVIEWS {
                break;
            }
            if seen_ids.insert(id)
                && let Some(att) = self.get(id)
            {
                out.push((Some(id), att.clone()));
            }
        }
        for path in extract_mentioned_image_paths(text, cwd) {
            if out.len() >= MAX_ASSISTANT_PREVIEWS {
                break;
            }
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !seen_paths.insert(key) {
                continue;
            }
            if let Ok(att) = load_image_file(&path) {
                let id = self.register(att.clone());
                if seen_ids.insert(id) {
                    out.push((Some(id), att));
                }
            }
        }
        out
    }
}

/// Centered larger preview overlay opened by `/image <N>`.
pub(crate) struct ImageModal {
    id: usize,
    attachment: ImageAttachment,
}

impl ImageModal {
    pub(crate) fn new(id: usize, attachment: ImageAttachment) -> Self {
        Self { id, attachment }
    }

    pub(crate) fn closes_on(key: KeyEvent) -> bool {
        matches!(
            key.code,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'Q')
        )
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 12 || area.height < 6 {
            return;
        }
        Clear.render(area, buf);
        let label = self.attachment.label.as_deref().unwrap_or("image");
        let dims = dims_text(&self.attachment);
        let title = Line::from(vec![
            Span::styled(
                format!(" [Image #{}] ", self.id),
                theme::accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{label}{dims} "), theme::muted()),
        ]);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::accent())
            .title(title)
            .title_bottom(Line::from(Span::styled(" esc / q close ", theme::muted())));
        let inner = block.inner(area);
        block.render(area, buf);
        let cols = usize::from(inner.width.saturating_sub(2)).max(4);
        let rows = usize::from(inner.height).max(2);
        let preview = render_preview(&self.attachment, cols, rows);
        for (y, line) in (inner.y..inner.bottom()).zip(preview) {
            buf.set_line(inner.x, y, &line, inner.width);
        }
    }
}

/// Returns `true` when `path` has a supported raster image extension.
pub(crate) fn is_image_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
            )
        })
}

/// If `raw` (pasted text or drag-and-drop path) is a single existing image file, return its path.
pub(crate) fn unquote_image_path(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("& ").unwrap_or(trimmed).trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    let cleaned = unquoted
        .strip_prefix("file:///")
        .or_else(|| unquoted.strip_prefix("file://"))
        .unwrap_or(unquoted);
    let candidate = Path::new(cleaned);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    (resolved.is_file() && is_image_ext(&resolved)).then_some(resolved)
}

/// Read an image file from disk and encode it (downscaling if larger than `MAX_ATTACH_DIM`).
pub(crate) fn load_image_file(path: &Path) -> Result<ImageAttachment, String> {
    let label = path.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    load_image_file_labeled(path, label)
}

/// Read an image file from disk with an explicit display label.
pub(crate) fn load_image_file_labeled(
    path: &Path,
    label: Option<String>,
) -> Result<ImageAttachment, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
    let format = image::guess_format(&bytes)
        .or_else(|_| ImageFormat::from_path(path))
        .map_err(|e| format!("unsupported image `{}`: {e}", path.display()))?;
    let decoded = image::load_from_memory_with_format(&bytes, format)
        .or_else(|_| image::load_from_memory(&bytes))
        .map_err(|e| format!("failed to decode `{}`: {e}", path.display()))?;
    let (w, h) = decoded.dimensions();
    if w <= MAX_ATTACH_DIM
        && h <= MAX_ATTACH_DIM
        && matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
        )
    {
        let media_type = match format {
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::WebP => "image/webp",
            ImageFormat::Gif => "image/gif",
            _ => "image/png",
        };
        return Ok(ImageAttachment {
            media_type: media_type.to_owned(),
            data_base64: B64.encode(&bytes),
            label,
            width: Some(w),
            height: Some(h),
        });
    }
    let img = if w > MAX_ATTACH_DIM || h > MAX_ATTACH_DIM {
        decoded.resize(MAX_ATTACH_DIM, MAX_ATTACH_DIM, FilterType::Triangle)
    } else {
        decoded
    };
    encode_dynamic(&img, format == ImageFormat::Jpeg, label)
}

/// Encode raw RGBA bytes (e.g. from `arboard`) as a downscaled PNG attachment.
pub(crate) fn encode_rgba(
    w: u32,
    h: u32,
    rgba: Vec<u8>,
    label: Option<String>,
) -> Result<ImageAttachment, String> {
    let Some(buf) = image::RgbaImage::from_raw(w, h, rgba) else {
        return Err("invalid clipboard RGBA buffer".to_owned());
    };
    let mut dyn_img = DynamicImage::ImageRgba8(buf);
    if w > MAX_ATTACH_DIM || h > MAX_ATTACH_DIM {
        dyn_img = dyn_img.resize(MAX_ATTACH_DIM, MAX_ATTACH_DIM, FilterType::Triangle);
    }
    encode_dynamic(&dyn_img, false, label)
}

/// Read an image from the OS clipboard, returning `None` when the clipboard has no image.
pub(crate) fn read_clipboard_image() -> Option<Result<ImageAttachment, String>> {
    crate::clipboard::read_image()
}

/// Read plain text from the OS clipboard, if available.
pub(crate) fn read_clipboard_text() -> Option<String> {
    crate::clipboard::read_text()
}

fn encode_dynamic(
    img: &DynamicImage,
    as_jpeg: bool,
    label: Option<String>,
) -> Result<ImageAttachment, String> {
    let (w, h) = img.dimensions();
    let mut out = Cursor::new(Vec::new());
    let media_type = if as_jpeg {
        img.to_rgb8()
            .write_to(&mut out, ImageFormat::Jpeg)
            .map_err(|e| format!("failed to encode JPEG: {e}"))?;
        "image/jpeg"
    } else {
        img.write_to(&mut out, ImageFormat::Png)
            .map_err(|e| format!("failed to encode PNG: {e}"))?;
        "image/png"
    };
    Ok(ImageAttachment {
        media_type: media_type.to_owned(),
        data_base64: B64.encode(out.into_inner()),
        label,
        width: Some(w),
        height: Some(h),
    })
}

/// Extract 1-based image numbers referenced in text (`[Image #1]`, `Image #2`, `фото №1`, `фото #3`).
pub(crate) fn extract_image_refs(text: &str) -> Vec<usize> {
    let lower = text.to_lowercase();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for marker in ["image #", "image №", "фото №", "фото #", "img #"] {
        let mut offset = 0usize;
        while let Some(pos) = lower.get(offset..).and_then(|s| s.find(marker)) {
            let abs = offset + pos;
            let after = abs + marker.len();
            let digits: String = lower
                .get(after..)
                .unwrap_or_default()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(n) = digits.parse::<usize>()
                && n > 0
            {
                hits.push((abs, n));
            }
            offset = after;
        }
    }
    hits.sort_by_key(|&(pos, _)| pos);
    let mut seen = HashSet::new();
    hits.into_iter()
        .filter_map(|(_, n)| seen.insert(n).then_some(n))
        .collect()
}

fn extract_at_image_paths(text: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for tok in text.split_whitespace() {
        let Some(rest) = tok.strip_prefix('@') else {
            continue;
        };
        let cleaned =
            rest.trim_matches(|c: char| matches!(c, ',' | '.' | ';' | ')' | ']' | '"' | '\''));
        if let Some(p) = unquote_image_path(cleaned, cwd) {
            out.push(p);
        }
    }
    out
}

fn extract_mentioned_image_paths(text: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| {
        c.is_whitespace() || matches!(c, '`' | '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\'')
    }) {
        let cleaned = tok.trim_end_matches([',', '.', ';', ':']);
        if cleaned.is_empty() {
            continue;
        }
        let p = Path::new(cleaned);
        if !is_image_ext(p) {
            continue;
        }
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        if resolved.is_file() {
            out.push(resolved);
        }
    }
    out
}

fn dims_text(att: &ImageAttachment) -> String {
    match (att.width, att.height) {
        (Some(w), Some(h)) => format!(" · {w}×{h}"),
        _ => String::new(),
    }
}

/// Render captions and compact truecolor half-block previews for a list of attached/referenced images.
pub(crate) fn render_gallery(
    images: &[(Option<usize>, ImageAttachment)],
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let max_cols = width.saturating_sub(4).clamp(12, 48);
    for (id, att) in images {
        let tag = id.map_or_else(|| "Image".to_owned(), |n| format!("[Image #{n}]"));
        let label = att.label.as_deref().unwrap_or(&att.media_type);
        let dims = dims_text(att);
        let room = width.saturating_sub(tag.len() + dims.len() + 8);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("▣ ", theme::accent()),
            Span::styled(tag, theme::accent().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(wrap::truncate(label, room).into_owned(), theme::muted()),
            Span::styled(dims, theme::dim()),
        ]));
        lines.extend(render_preview(att, max_cols, max_rows));
    }
    lines
}

fn composite(px: [u8; 4]) -> Color {
    let a = u32::from(px[3]);
    let inv = 255 - a;
    let blend = |fg: u8, bg: u32| -> u8 {
        u8::try_from((u32::from(fg) * a + bg * inv + 127) / 255).unwrap_or(255)
    };
    theme::pixel(
        blend(px[0], BG_RGB[0]),
        blend(px[1], BG_RGB[1]),
        blend(px[2], BG_RGB[2]),
    )
}

/// Decode `att` and paint a truecolor Unicode upper half-block (`▀`) preview.
pub(crate) fn render_preview(
    att: &ImageAttachment,
    max_cols: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if max_cols < 2 || max_rows == 0 {
        return Vec::new();
    }
    let Ok(bytes) = B64.decode(&att.data_base64) else {
        return Vec::new();
    };
    let Ok(decoded) = image::load_from_memory(&bytes) else {
        return Vec::new();
    };
    let (src_w, src_h) = decoded.dimensions();
    if src_w == 0 || src_h == 0 {
        return Vec::new();
    }
    let target_max_w = u32::try_from(max_cols).unwrap_or(40);
    let target_max_h = u32::try_from(max_rows.saturating_mul(2)).unwrap_or(16);
    let resized = decoded.resize(target_max_w, target_max_h, FilterType::Triangle);
    let rgba = resized.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut lines = Vec::with_capacity(usize::try_from(h.div_ceil(2)).unwrap_or(0));
    let mut y = 0u32;
    while y < h {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(usize::try_from(w).unwrap_or(0) + 1);
        spans.push(Span::raw("  "));
        for x in 0..w {
            let top = composite(rgba.get_pixel(x, y).0);
            let bot = if y + 1 < h {
                composite(rgba.get_pixel(x, y + 1).0)
            } else {
                Color::Rgb(24, 26, 32)
            };
            spans.push(Span::styled("▀", Style::new().fg(top).bg(bot)));
        }
        lines.push(Line::from(spans));
        y += 2;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png() -> ImageAttachment {
        let rgba = image::RgbaImage::from_fn(8, 8, |x, y| {
            image::Rgba([
                u8::try_from(x * 30).unwrap_or(255),
                u8::try_from(y * 30).unwrap_or(255),
                200,
                255,
            ])
        });
        encode_dynamic(
            &DynamicImage::ImageRgba8(rgba),
            false,
            Some("demo.png".into()),
        )
        .expect("encode")
    }

    #[test]
    fn extracts_english_and_russian_image_references() {
        let refs =
            extract_image_refs("Look at [Image #2], фото №1, and also Image #2 and фото #3!");
        assert_eq!(refs, vec![2, 1, 3]);
    }

    #[test]
    fn renders_half_block_truecolor_preview() {
        let att = sample_png();
        let lines = render_preview(&att, 16, 4);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].to_string().contains('▀'));
    }

    #[test]
    fn tray_stages_and_resolves_assistant_references() {
        let mut tray = ImageTray::default();
        let id = tray.stage(sample_png());
        assert_eq!(id, 1);
        assert!(tray.has_pending());
        let attached = tray.take_for_prompt("check [Image #1]", Path::new("."));
        assert_eq!(attached.len(), 1);
        assert!(!tray.has_pending());

        let previews = tray.resolve_assistant_previews("На фото №1 видна кнопка.", Path::new("."));
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].0, Some(1));
    }
}
