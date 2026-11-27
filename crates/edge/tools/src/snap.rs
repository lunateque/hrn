//! Deterministic PNG transcript page renderer (`PngSnapRenderer`) implementing
//! [`hrn_kernel::SnapRenderer`] for Wave 11i `snapcompact` archival.

use crate::snap_font::{GLYPH_H, GLYPH_W, glyph_for};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use hrn_kernel::{HrnError, ImageAttachment, Result, SnapRenderer};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ColorType, ImageEncoder, Rgb, RgbImage};

/// Horizontal/vertical pixel scale factor for each 5x7 font pixel.
const SCALE: u32 = 2;
/// Horizontal cell advance in output pixels (`5 * 2 + 2 = 12`).
const CELL_W: u32 = GLYPH_W * SCALE + 2;
/// Vertical line advance in output pixels (`7 * 2 + 4 = 18`).
const LINE_H: u32 = GLYPH_H * SCALE + 4;
/// Outer horizontal margin in pixels.
const MARGIN_X: u32 = 24;
/// Outer vertical margin in pixels.
const MARGIN_Y: u32 = 16;
/// Height of the top page header bar in pixels.
const HEADER_H: u32 = 30;
/// Spacing between the header divider and first text row.
const HEADER_GAP: u32 = 10;
/// Maximum wrapped characters per line (`24 + 120*12 + 24 = 1488` px <= 1600 px).
const MAX_COLS: usize = 120;
/// Maximum rendered rows per page (`16 + 30 + 10 + 80*18 + 16 = 1512` px <= 1600 px).
const MAX_ROWS: usize = 80;

const BG_COLOR: Rgb<u8> = Rgb([252, 252, 253]);
const HEADER_BG: Rgb<u8> = Rgb([235, 240, 247]);
const BORDER_COLOR: Rgb<u8> = Rgb([203, 213, 225]);
const HEADER_FG: Rgb<u8> = Rgb([15, 23, 42]);
const BODY_FG: Rgb<u8> = Rgb([17, 24, 39]);
const USER_FG: Rgb<u8> = Rgb([29, 78, 216]);
const ASSISTANT_FG: Rgb<u8> = Rgb([4, 120, 87]);
const TOOL_FG: Rgb<u8> = Rgb([124, 45, 18]);

/// Pure-Rust deterministic PNG renderer for `snapcompact` transcript archives.
///
/// Renders each page with an embedded 5x7 bitmap font (2x scaled, crisp dark-on-light layout) and
/// deterministic PNG compression so identical pages always produce byte-for-byte identical bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct PngSnapRenderer;

impl PngSnapRenderer {
    /// Render one transcript page into a deterministic PNG [`ImageAttachment`].
    ///
    /// # Errors
    /// Returns [`HrnError::Tool`] if PNG encoding fails.
    pub fn render_page(
        &self,
        page_text: &str,
        page_num: usize,
        total_pages: usize,
    ) -> Result<ImageAttachment> {
        let lines = wrap_page_lines(page_text, MAX_COLS, MAX_ROWS);
        let row_count = u32::try_from(lines.len().max(1)).unwrap_or(1);
        let cols = u32::try_from(
            lines
                .iter()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(40)
                .clamp(48, MAX_COLS),
        )
        .unwrap_or(48);

        let width = MARGIN_X * 2 + cols * CELL_W;
        let height = MARGIN_Y * 2 + HEADER_H + HEADER_GAP + row_count * LINE_H;
        let mut img = RgbImage::from_pixel(width, height, BG_COLOR);

        draw_header(&mut img, width, page_num, total_pages);

        let text_top = MARGIN_Y + HEADER_H + HEADER_GAP;
        for (idx, line) in lines.iter().enumerate() {
            let y = text_top + u32::try_from(idx).unwrap_or(0) * LINE_H;
            let fg = line_color(line);
            draw_text(&mut img, MARGIN_X, y, line, fg);
        }

        let mut png_bytes = Vec::new();
        let encoder =
            PngEncoder::new_with_quality(&mut png_bytes, CompressionType::Default, FilterType::Sub);
        encoder
            .write_image(img.as_raw(), width, height, ColorType::Rgb8.into())
            .map_err(|e| HrnError::Tool(format!("snapcompact PNG encode failed: {e}")))?;

        Ok(ImageAttachment {
            media_type: "image/png".to_owned(),
            data_base64: B64.encode(&png_bytes),
            label: Some(format!("snapcompact-page-{page_num:02}.png")),
            width: Some(width),
            height: Some(height),
        })
    }
}

impl SnapRenderer for PngSnapRenderer {
    fn render_pages(&self, pages: &[String]) -> Result<Vec<ImageAttachment>> {
        let total = pages.len();
        let mut out = Vec::with_capacity(total);
        for (idx, page) in pages.iter().enumerate() {
            out.push(self.render_page(page, idx + 1, total)?);
        }
        Ok(out)
    }
}

fn draw_header(img: &mut RgbImage, width: u32, page_num: usize, total_pages: usize) {
    let header_bottom = MARGIN_Y + HEADER_H;
    for y in MARGIN_Y..header_bottom {
        for x in 0..width {
            img.put_pixel(x, y, HEADER_BG);
        }
    }
    for x in 0..width {
        img.put_pixel(x, header_bottom, BORDER_COLOR);
    }
    let title = format!("SNAPCOMPACT ARCHIVE - PAGE {page_num}/{total_pages}");
    let title_y = MARGIN_Y + (HEADER_H.saturating_sub(GLYPH_H * SCALE)) / 2;
    draw_text(img, MARGIN_X, title_y, &title, HEADER_FG);
}

fn line_color(line: &str) -> Rgb<u8> {
    if line.starts_with("[USER]") {
        USER_FG
    } else if line.starts_with("[ASSISTANT") {
        ASSISTANT_FG
    } else if line.starts_with("[TOOL") {
        TOOL_FG
    } else {
        BODY_FG
    }
}

fn draw_text(img: &mut RgbImage, x0: u32, y0: u32, text: &str, color: Rgb<u8>) {
    let img_w = img.width();
    let img_h = img.height();
    for (col, ch) in text.chars().enumerate() {
        let gx = x0 + u32::try_from(col).unwrap_or(0) * CELL_W;
        if gx + GLYPH_W * SCALE > img_w {
            break;
        }
        let bitmap = glyph_for(ch);
        for (row, &bits) in bitmap.iter().enumerate() {
            let py0 = y0 + u32::try_from(row).unwrap_or(0) * SCALE;
            if py0 + SCALE > img_h {
                break;
            }
            for bit in 0..GLYPH_W {
                if (bits & (1 << (GLYPH_W - 1 - bit))) != 0 {
                    let px0 = gx + bit * SCALE;
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            img.put_pixel(px0 + dx, py0 + dy, color);
                        }
                    }
                }
            }
        }
    }
}

fn wrap_page_lines(text: &str, max_cols: usize, max_rows: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let expanded = raw_line.replace('\t', "    ");
        if expanded.is_empty() {
            out.push(String::new());
            if out.len() >= max_rows {
                break;
            }
            continue;
        }
        let mut current = String::new();
        let mut count = 0usize;
        for ch in expanded.chars() {
            if ch.is_control() {
                continue;
            }
            current.push(ch);
            count += 1;
            if count >= max_cols {
                out.push(std::mem::take(&mut current));
                count = 0;
                if out.len() >= max_rows {
                    return out;
                }
            }
        }
        if !current.is_empty() {
            out.push(current);
            if out.len() >= max_rows {
                break;
            }
        }
    }
    if out.is_empty() {
        out.push("(empty page)".to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageFormat;

    #[test]
    fn snapcompact_png_deterministic() {
        let renderer = PngSnapRenderer;
        let pages = vec![
            "[USER]: Refactor auth token refresh\n[ASSISTANT]: Checking oauth.rs\n[TOOL read]: fn refresh() -> Result<()> { Ok(()) }".to_owned(),
            "[USER]: Add deterministic PNG tests — verify UTF-8 fallback: ✓\n[ASSISTANT]: Done.".to_owned(),
        ];
        let first = renderer.render_pages(&pages).expect("first render");
        let second = renderer.render_pages(&pages).expect("second render");

        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.media_type, "image/png");
            assert_eq!(a.data_base64, b.data_base64, "PNG bytes must be bit-exact");
            assert_eq!(a.width, b.width);
            assert_eq!(a.height, b.height);
            let raw = B64.decode(&a.data_base64).expect("valid base64");
            assert!(
                raw.starts_with(b"\x89PNG\r\n\x1a\n"),
                "must start with PNG signature"
            );
            let decoded =
                image::load_from_memory_with_format(&raw, ImageFormat::Png).expect("valid PNG");
            assert_eq!(Some(decoded.width()), a.width);
            assert_eq!(Some(decoded.height()), a.height);
        }
    }
}
