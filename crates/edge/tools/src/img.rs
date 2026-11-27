//! Image decoding, smart downscaling, and base64 encoding for multimodal tool outputs and inputs.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use hrn_kernel::{HrnError, ImageAttachment, Result};
use image::{DynamicImage, GenericImageView, ImageFormat, imageops::FilterType};
use std::io::Cursor;
use std::path::Path;

/// Default maximum width/height for images sent to multimodal LLMs.
pub const DEFAULT_MAX_IMAGE_DIM: u32 = 1600;

/// Returns `true` when `path` has a supported raster image file extension.
#[must_use]
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
            )
        })
}

/// Returns `true` when `bytes` start with a recognized PNG/JPEG/GIF/WebP/BMP signature.
#[must_use]
pub fn looks_like_image_bytes(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok()
}

/// Decode a base64-encoded raster image (e.g. from CDP or desktop capture) and downscale to
/// `max_dim`.
pub fn load_image_from_base64(
    b64: &str,
    label: Option<String>,
    max_dim: u32,
) -> Result<ImageAttachment> {
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = B64
        .decode(cleaned.as_bytes())
        .map_err(|e| HrnError::Tool(format!("invalid base64 image data: {e}")))?;
    load_image_from_bytes(&bytes, None, label, max_dim)
}

/// Decode the base64 text a tool output carries, without touching the pixels.
///
/// # Errors
///
/// When the text is not standard base64.
pub fn decode_base64(b64: &str) -> Result<Vec<u8>> {
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    B64.decode(cleaned.as_bytes())
        .map_err(|e| HrnError::Tool(format!("invalid base64 data: {e}")))
}

/// Decode, optionally downscale to `max_dim`, and encode an image from raw bytes.
pub fn load_image_from_bytes(
    bytes: &[u8],
    path: Option<&Path>,
    label: Option<String>,
    max_dim: u32,
) -> Result<ImageAttachment> {
    let format = image::guess_format(bytes)
        .or_else(|_| {
            path.ok_or_else(|| {
                image::ImageError::Unsupported(
                    image::error::UnsupportedError::from_format_and_kind(
                        image::error::ImageFormatHint::Unknown,
                        image::error::UnsupportedErrorKind::Format(
                            image::error::ImageFormatHint::Unknown,
                        ),
                    ),
                )
            })
            .and_then(ImageFormat::from_path)
        })
        .map_err(|e| HrnError::Tool(format!("unsupported image format: {e}")))?;

    let decoded = image::load_from_memory_with_format(bytes, format)
        .or_else(|_| image::load_from_memory(bytes))
        .map_err(|e| HrnError::Tool(format!("failed to decode image: {e}")))?;

    let (orig_w, orig_h) = decoded.dimensions();
    let resized = max_dim > 0 && (orig_w > max_dim || orig_h > max_dim);

    // Keep original bytes untouched when already within bounds and in a widely supported format.
    if !resized
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
            data_base64: B64.encode(bytes),
            label,
            width: Some(orig_w),
            height: Some(orig_h),
        });
    }

    let img = if resized {
        decoded.resize(max_dim, max_dim, FilterType::Triangle)
    } else {
        decoded
    };
    encode_dynamic_image(&img, format == ImageFormat::Jpeg, label)
}

/// Read an image file from disk, downscaling to `max_dim` if needed.
pub fn load_image_file(
    path: &Path,
    label: Option<String>,
    max_dim: u32,
) -> Result<ImageAttachment> {
    let bytes = std::fs::read(path)
        .map_err(|e| HrnError::Tool(format!("cannot read `{}`: {e}", path.display())))?;
    let label = label.or_else(|| path.file_name().and_then(|n| n.to_str()).map(str::to_owned));
    load_image_from_bytes(&bytes, Some(path), label, max_dim)
}

/// Encode raw RGBA pixel buffers (e.g. from screenshots or clipboard) into a PNG [`ImageAttachment`].
pub fn encode_rgba_png(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    label: Option<String>,
    max_dim: u32,
) -> Result<ImageAttachment> {
    let buf = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| HrnError::Tool("invalid RGBA image buffer dimensions".to_owned()))?;
    let mut img = DynamicImage::ImageRgba8(buf);
    if max_dim > 0 && (width > max_dim || height > max_dim) {
        img = img.resize(max_dim, max_dim, FilterType::Triangle);
    }
    encode_dynamic_image(&img, false, label)
}

fn encode_dynamic_image(
    img: &DynamicImage,
    as_jpeg: bool,
    label: Option<String>,
) -> Result<ImageAttachment> {
    let (w, h) = img.dimensions();
    let mut out = Cursor::new(Vec::new());
    let media_type = if as_jpeg {
        img.to_rgb8()
            .write_to(&mut out, ImageFormat::Jpeg)
            .map_err(|e| HrnError::Tool(format!("failed to encode JPEG: {e}")))?;
        "image/jpeg"
    } else {
        img.write_to(&mut out, ImageFormat::Png)
            .map_err(|e| HrnError::Tool(format!("failed to encode PNG: {e}")))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_downscales_rgba_png() {
        let rgba = vec![255u8; 40 * 20 * 4];
        let att = encode_rgba_png(40, 20, rgba, Some("test.png".into()), 20).expect("encode");
        assert_eq!(att.media_type, "image/png");
        assert_eq!((att.width, att.height), (Some(20), Some(10)));
        assert!(!att.data_base64.is_empty());
        let raw = B64.decode(&att.data_base64).expect("b64");
        assert!(looks_like_image_bytes(&raw));
    }

    #[test]
    fn read_file_returns_image_attachment_for_png() {
        let dir = crate::testutil::TempDir::new("read-img");
        let rgba = vec![120u8; 16 * 12 * 4];
        let att = encode_rgba_png(16, 12, rgba, None, 1600).expect("encode");
        let raw = B64.decode(&att.data_base64).expect("b64");
        let path = dir.path().join("shot.png");
        std::fs::write(&path, raw).expect("write");

        let tracker = crate::FileReadTracker::new();
        let out = crate::fs_read::read_sync(
            &tracker,
            &crate::testutil::test_ctx(dir.path().to_path_buf()),
            path.to_str().unwrap(),
        )
        .expect("read");
        assert!(
            out.content.contains("Image: shot.png (16x12, image/png"),
            "{}",
            out.content
        );
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
    }
}
