//! Disk-backed session attachment store (`<data_dir>/sessions/<root_session_id>/attachments/`).
//!
//! Stores user-attached images and files under monotonically numbered, sanitized filenames
//! (`<n>-<sanitized-name>`) and enforces strict canonical containment on resolution.

use base64::Engine as _;
use hrn_kernel::{HrnError, ImageAttachment, Result};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Maximum size of one stored attachment (32 MiB).
pub const MAX_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum encoded base64 length accepted before decoding an attached image.
const MAX_BASE64_LEN: usize = (32usize * 1024 * 1024).div_ceil(3) * 4 + 4;
/// Maximum number of attachments stored in a single session.
pub const MAX_SESSION_ATTACHMENTS: usize = 64;
/// Maximum combined byte total of all attachments stored in a single session (128 MiB).
pub const MAX_SESSION_ATTACHMENT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_FILENAME_LEN: usize = 120;
const MAX_KEY_LEN: usize = 160;
const MAX_SESSION_ID_LEN: usize = 128;

/// Disk-backed storage for session-scoped attachments under `<data_dir>/sessions/<root>/attachments`.
pub struct DiskAttachmentStore {
    data_dir: PathBuf,
    alloc_lock: Mutex<()>,
    max_session_files: usize,
    max_session_bytes: u64,
}

impl DiskAttachmentStore {
    /// Create a store rooted at `data_dir`.
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            alloc_lock: Mutex::new(()),
            max_session_files: MAX_SESSION_ATTACHMENTS,
            max_session_bytes: MAX_SESSION_ATTACHMENT_BYTES,
        }
    }

    #[cfg(test)]
    fn with_quotas(data_dir: PathBuf, max_session_files: usize, max_session_bytes: u64) -> Self {
        Self {
            data_dir,
            alloc_lock: Mutex::new(()),
            max_session_files,
            max_session_bytes,
        }
    }

    /// Root directory for `root_session_id`'s attachments.
    ///
    /// # Errors
    /// `root_session_id` is empty or contains unsafe path characters.
    pub fn attachments_dir(&self, root_session_id: &str) -> Result<PathBuf> {
        let session = validate_session_segment(root_session_id)?;
        Ok(self
            .data_dir
            .join("sessions")
            .join(session)
            .join("attachments"))
    }

    /// Store raw `bytes` under `<n>-<sanitized-name>` for `root_session_id` and return the stored key.
    ///
    /// # Errors
    /// Size exceeds [`MAX_ATTACHMENT_BYTES`], `root_session_id` is invalid, or disk I/O fails.
    pub fn store_bytes(
        &self,
        root_session_id: &str,
        raw_name: &str,
        bytes: &[u8],
    ) -> Result<String> {
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(HrnError::Tool(format!(
                "attachment `{raw_name}` is {} bytes, over the {MAX_ATTACHMENT_BYTES}-byte limit",
                bytes.len()
            )));
        }
        let dir = self.attachments_dir(root_session_id)?;
        let safe = sanitize_name(raw_name);
        let _guard = self
            .alloc_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fs::create_dir_all(&dir).map_err(|e| {
            HrnError::Io(format!(
                "cannot create attachments dir `{}`: {e}",
                dir.display()
            ))
        })?;
        let (existing_files, existing_bytes, mut next) = scan_session_usage(&dir);
        if existing_files >= self.max_session_files {
            return Err(HrnError::Tool(format!(
                "session attachment limit reached ({} files max)",
                self.max_session_files
            )));
        }
        if existing_bytes.saturating_add(bytes.len() as u64) > self.max_session_bytes {
            return Err(HrnError::Tool(format!(
                "session attachment byte quota exceeded ({} bytes max)",
                self.max_session_bytes
            )));
        }
        for _ in 0..1024 {
            let key = format!("{next}-{safe}");
            let target = dir.join(&key);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
            {
                Ok(mut file) => {
                    file.write_all(bytes).map_err(|e| {
                        HrnError::Io(format!("cannot write attachment `{key}`: {e}"))
                    })?;
                    return Ok(key);
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    next = next.saturating_add(1);
                }
                Err(err) => {
                    return Err(HrnError::Io(format!(
                        "cannot create attachment `{key}`: {err}"
                    )));
                }
            }
        }
        Err(HrnError::Io(
            "failed to allocate unique attachment filename".to_owned(),
        ))
    }

    /// Decode and materialize one user-submitted [`ImageAttachment`] into the session store.
    ///
    /// # Errors
    /// Base64 decoding fails, size exceeds the cap, or disk I/O fails.
    pub fn store_image(&self, root_session_id: &str, image: &ImageAttachment) -> Result<String> {
        let encoded = image.data_base64.trim();
        if encoded.len() > MAX_BASE64_LEN {
            return Err(HrnError::Tool(format!(
                "attached image base64 payload is {} bytes, over the {MAX_BASE64_LEN}-byte encoded limit",
                encoded.len()
            )));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| HrnError::Tool(format!("invalid base64 in attached image: {e}")))?;
        let raw_name = image
            .label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(
                || format!("image.{}", ext_for_media_type(&image.media_type)),
                ToOwned::to_owned,
            );
        self.store_bytes(root_session_id, &raw_name, &bytes)
    }

    /// Copy an existing file from `source` into `root_session_id`'s attachment directory.
    ///
    /// # Errors
    /// `source` is not a regular file, exceeds [`MAX_ATTACHMENT_BYTES`], or cannot be read/written.
    pub fn copy_file(&self, root_session_id: &str, source: &Path) -> Result<String> {
        let meta = fs::metadata(source).map_err(|e| {
            HrnError::Tool(format!(
                "cannot stat attachment `{}`: {e}",
                source.display()
            ))
        })?;
        if !meta.is_file() {
            return Err(HrnError::Tool(format!(
                "`{}` is not a regular file",
                source.display()
            )));
        }
        if meta.len() > MAX_ATTACHMENT_BYTES {
            return Err(HrnError::Tool(format!(
                "`{}` is {} bytes, over the {MAX_ATTACHMENT_BYTES}-byte limit",
                source.display(),
                meta.len()
            )));
        }
        let bytes = fs::read(source).map_err(|e| {
            HrnError::Tool(format!(
                "cannot read attachment `{}`: {e}",
                source.display()
            ))
        })?;
        let name = source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment");
        self.store_bytes(root_session_id, name, &bytes)
    }

    /// List stored attachments for `root_session_id` as `(name, size_bytes)` pairs.
    ///
    /// # Errors
    /// `root_session_id` is invalid or the attachments directory cannot be read.
    pub fn list(&self, root_session_id: &str) -> Result<Vec<(String, u64)>> {
        let dir = self.attachments_dir(root_session_id)?;
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let canon_dir = fs::canonicalize(&dir).map_err(|e| {
            HrnError::Io(format!(
                "cannot canonicalize attachments dir `{}`: {e}",
                dir.display()
            ))
        })?;
        let mut entries: Vec<(u64, String, u64)> = Vec::new();
        let read_dir = fs::read_dir(&canon_dir).map_err(|e| {
            HrnError::Io(format!(
                "cannot list attachments dir `{}`: {e}",
                canon_dir.display()
            ))
        })?;
        for entry in read_dir.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            if validate_key(&name).is_err() {
                continue;
            }
            let Ok(canon_path) = fs::canonicalize(entry.path()) else {
                continue;
            };
            if !canon_path.starts_with(&canon_dir) || canon_path == canon_dir {
                continue;
            }
            let size = entry.metadata().map_or(0, |m| m.len());
            entries.push((leading_number(&name).unwrap_or(u64::MAX), name, size));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        Ok(entries
            .into_iter()
            .map(|(_, name, size)| (name, size))
            .collect())
    }

    /// Resolve `key` inside `root_session_id`'s attachments directory with strict canonical
    /// containment verification.
    ///
    /// # Errors
    /// `root_session_id` or `key` is invalid, the file does not exist, or canonical resolution
    /// escapes the session attachments directory.
    pub fn resolve(&self, root_session_id: &str, key: &str) -> Result<PathBuf> {
        let validated = validate_key(key)?;
        let dir = self.attachments_dir(root_session_id)?;
        if !dir.is_dir() {
            return Err(HrnError::Tool(format!(
                "attachment `{validated}` not found (no attachments in this session)"
            )));
        }
        let canon_dir = fs::canonicalize(&dir).map_err(|e| {
            HrnError::Io(format!(
                "cannot canonicalize attachments dir `{}`: {e}",
                dir.display()
            ))
        })?;
        let candidate = canon_dir.join(validated);
        let canon_path = fs::canonicalize(&candidate).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HrnError::Tool(format!(
                    "attachment `{validated}` not found; read `attachment://` to list attachments"
                ))
            } else {
                HrnError::Tool(format!("cannot open attachment `{validated}`: {e}"))
            }
        })?;
        if !canon_path.starts_with(&canon_dir) || canon_path == canon_dir || !canon_path.is_file() {
            return Err(HrnError::Tool(format!(
                "attachment `{validated}` escapes the session attachments directory"
            )));
        }
        Ok(canon_path)
    }
}

fn validate_session_segment(raw: &str) -> Result<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_SESSION_ID_LEN
        || trimmed == "."
        || trimmed == ".."
        || trimmed.starts_with('.')
        || trimmed.contains("..")
        || trimmed.chars().any(|c| {
            c == '\0'
                || c.is_control()
                || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
        || is_windows_reserved(trimmed)
    {
        return Err(HrnError::Tool(
            "invalid session id for attachment storage".to_owned(),
        ));
    }
    Ok(trimmed)
}

fn validate_key(raw: &str) -> Result<&str> {
    if raw.is_empty() || raw.len() > MAX_KEY_LEN {
        return Err(HrnError::Tool(
            "invalid attachment name: must be 1..=160 characters".to_owned(),
        ));
    }
    if raw == "." || raw == ".." || raw.starts_with('.') || raw.contains("..") {
        return Err(HrnError::Tool(format!(
            "invalid attachment name `{raw}`: path traversal is not allowed"
        )));
    }
    if raw.chars().any(|c| {
        c == '\0'
            || c.is_control()
            || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
    }) {
        return Err(HrnError::Tool(format!(
            "invalid attachment name `{raw}`: contains forbidden path or control characters"
        )));
    }
    if is_windows_reserved(raw) {
        return Err(HrnError::Tool(format!(
            "invalid attachment name `{raw}`: reserved device name"
        )));
    }
    Ok(raw)
}

fn sanitize_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| c.is_whitespace() || c.is_control());
    let mut cleaned = String::with_capacity(base.len().min(MAX_FILENAME_LEN));
    for ch in base.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            cleaned.push(ch);
        } else {
            cleaned.push('_');
        }
    }
    while cleaned.contains("..") {
        cleaned = cleaned.replace("..", "_");
    }
    let trimmed = cleaned.trim_start_matches('.').trim_end_matches('.');
    let mut safe = if trimmed.is_empty() || !trimmed.chars().any(|c| c.is_ascii_alphanumeric()) {
        "attachment".to_owned()
    } else {
        trimmed.to_owned()
    };
    if is_windows_reserved(&safe) {
        safe.insert(0, '_');
    }
    if safe.len() > MAX_FILENAME_LEN {
        safe = truncate_name(&safe, MAX_FILENAME_LEN);
    }
    safe
}

fn truncate_name(name: &str, max_len: usize) -> String {
    if let Some((stem, ext)) = name.rsplit_once('.')
        && !ext.is_empty()
        && ext.len() + 2 <= max_len
    {
        let keep = max_len - ext.len() - 1;
        let raw_prefix = stem.get(..keep.min(stem.len())).unwrap_or("attachment");
        let prefix = raw_prefix.trim_end_matches('.');
        let prefix = if prefix.is_empty() {
            "attachment"
        } else {
            prefix
        };
        return format!("{prefix}.{ext}");
    }
    let clipped = name
        .get(..max_len)
        .unwrap_or("attachment")
        .trim_end_matches('.');
    if clipped.is_empty() {
        "attachment".to_owned()
    } else {
        clipped.to_owned()
    }
}

fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn leading_number(name: &str) -> Option<u64> {
    name.split_once('-')?.0.parse::<u64>().ok()
}

fn scan_session_usage(dir: &Path) -> (usize, u64, u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0, 1);
    };
    let mut count = 0usize;
    let mut total_bytes = 0u64;
    let mut max_seen = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        count = count.saturating_add(1);
        total_bytes = total_bytes.saturating_add(meta.len());
        if let Some(n) = entry
            .file_name()
            .into_string()
            .ok()
            .and_then(|name| leading_number(&name))
        {
            max_seen = max_seen.max(n);
        }
    }
    (count, total_bytes, max_seen.saturating_add(1))
}

fn ext_for_media_type(media_type: &str) -> &'static str {
    match media_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod tests;
