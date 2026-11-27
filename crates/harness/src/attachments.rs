//! `attachment://`: read-only access to files and images attached in the current session.
//!
//! Pure port + scheme implementation so `hrn-harness` stays free of filesystem I/O: callers supply
//! an [`AttachmentStore`] (disk-backed in production, in-memory stub in tests).

use hrn_kernel::{HrnError, Result, ToolContext, UrlBody, UrlScheme};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

/// Maximum sanitized attachment filename length.
const MAX_FILENAME_LEN: usize = 120;
/// Maximum validated attachment key length (prefix `<n>-` + sanitized name).
const MAX_KEY_LEN: usize = 160;

/// Metadata for one materialized session attachment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentEntry {
    /// Stored attachment filename (`<n>-<sanitized-name>`).
    pub name: String,
    /// Size in bytes on disk.
    pub size_bytes: u64,
}

/// Port backing `attachment://` listing and path resolution per root session.
pub trait AttachmentStore: Send + Sync {
    /// List attachments stored for `root_session_id` in stable order.
    ///
    /// # Errors
    /// Storage cannot be read or `root_session_id` is invalid.
    fn list(&self, root_session_id: &str) -> Result<Vec<AttachmentEntry>>;

    /// Resolve validated attachment `name` inside `root_session_id`'s attachment directory.
    ///
    /// # Errors
    /// The attachment does not exist or fails containment checks.
    fn resolve(&self, root_session_id: &str, name: &str) -> Result<PathBuf>;
}

/// Sanitize a raw user-supplied filename for safe storage under `<n>-<name>`.
///
/// Strips directory components, control characters, traversal sequences (`..`), reserved shell
/// or filesystem characters, and Windows device names.
#[must_use]
pub fn sanitize_attachment_name(raw: &str) -> String {
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
    if is_windows_reserved_name(&safe) {
        safe.insert(0, '_');
    }
    if safe.len() > MAX_FILENAME_LEN {
        safe = truncate_preserving_ext(&safe, MAX_FILENAME_LEN);
    }
    safe
}

/// Validate an attachment key passed after `attachment://` before resolving on disk.
///
/// Strictly rejects path traversal (`..`), directory separators, drive colons, NUL/control
/// characters, hidden dot-prefixes, wildcard/reserved filesystem chars, and Windows device names.
///
/// # Errors
/// Returns `HrnError::Tool` when `raw` is malformed or unsafe.
pub fn validate_attachment_key(raw: &str) -> Result<&str> {
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
    if is_windows_reserved_name(raw) {
        return Err(HrnError::Tool(format!(
            "invalid attachment name `{raw}`: reserved device name"
        )));
    }
    Ok(raw)
}

fn is_windows_reserved_name(name: &str) -> bool {
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

fn truncate_preserving_ext(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        return name.to_owned();
    }
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

/// `attachment://`: read-only scheme resolving session-scoped user attachments.
pub struct AttachmentScheme {
    store: Arc<dyn AttachmentStore>,
}

impl AttachmentScheme {
    /// Build an `attachment://` scheme backed by `store`.
    pub fn new(store: Arc<dyn AttachmentStore>) -> Self {
        Self { store }
    }
}

impl UrlScheme for AttachmentScheme {
    fn scheme(&self) -> &'static str {
        "attachment"
    }

    fn about(&self) -> &'static str {
        "user-attached files for this session (read-only): `attachment://` lists attachments, `attachment://<n>-<name>` reads one"
    }

    fn read(&self, ctx: &ToolContext, rest: &str) -> Result<UrlBody> {
        let key = rest.trim().trim_start_matches('/');
        if key.is_empty() {
            let entries = self.store.list(&ctx.root_session_id)?;
            if entries.is_empty() {
                return Ok(UrlBody::Text("No attachments in this session.".to_owned()));
            }
            let mut out = String::new();
            for entry in entries {
                let _ = writeln!(
                    out,
                    "attachment://{}  ({} bytes)",
                    entry.name, entry.size_bytes
                );
            }
            return Ok(UrlBody::Text(out));
        }
        let validated = validate_attachment_key(key)?;
        let path = self.store.resolve(&ctx.root_session_id, validated)?;
        Ok(UrlBody::Path(path))
    }

    fn write(&self, _ctx: &ToolContext, _rest: &str, _content: &str) -> Result {
        Err(HrnError::Tool("`attachment://` is read-only".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::hook_ctx;
    use std::collections::BTreeMap;

    struct StubStore {
        entries: BTreeMap<String, Vec<(String, u64, PathBuf)>>,
    }

    impl AttachmentStore for StubStore {
        fn list(&self, root_session_id: &str) -> Result<Vec<AttachmentEntry>> {
            Ok(self
                .entries
                .get(root_session_id)
                .into_iter()
                .flatten()
                .map(|(name, size, _)| AttachmentEntry {
                    name: name.clone(),
                    size_bytes: *size,
                })
                .collect())
        }

        fn resolve(&self, root_session_id: &str, name: &str) -> Result<PathBuf> {
            self.entries
                .get(root_session_id)
                .and_then(|rows| rows.iter().find(|(n, _, _)| n == name))
                .map(|(_, _, p)| p.clone())
                .ok_or_else(|| HrnError::Tool(format!("attachment `{name}` not found")))
        }
    }

    #[test]
    fn attachment_scheme_listing() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "root-a".to_owned(),
            vec![
                (
                    "1-screenshot.png".to_owned(),
                    2048,
                    PathBuf::from("/tmp/root-a/1-screenshot.png"),
                ),
                (
                    "2-report.json".to_owned(),
                    512,
                    PathBuf::from("/tmp/root-a/2-report.json"),
                ),
            ],
        );
        entries.insert(
            "root-b".to_owned(),
            vec![(
                "1-other.txt".to_owned(),
                64,
                PathBuf::from("/tmp/root-b/1-other.txt"),
            )],
        );
        let scheme = AttachmentScheme::new(Arc::new(StubStore { entries }));

        let mut ctx = hook_ctx();
        ctx.root_session_id = "root-a".to_owned();
        ctx.session_id = "child-of-root-a".to_owned();

        let listed = scheme.read(&ctx, "").expect("list attachments");
        let UrlBody::Text(body) = listed else {
            panic!("expected text listing");
        };
        assert!(body.contains("attachment://1-screenshot.png  (2048 bytes)"));
        assert!(body.contains("attachment://2-report.json  (512 bytes)"));
        assert!(!body.contains("1-other.txt"));

        let resolved = scheme
            .read(&ctx, "1-screenshot.png")
            .expect("resolve attachment");
        assert_eq!(
            resolved,
            UrlBody::Path(PathBuf::from("/tmp/root-a/1-screenshot.png"))
        );

        let err = scheme
            .write(&ctx, "1-screenshot.png", "overwrite")
            .expect_err("read-only")
            .to_string();
        assert!(err.contains("read-only"), "{err}");
    }

    #[test]
    fn sanitize_and_validate_reject_traversal_and_reserved_names() {
        assert_eq!(sanitize_attachment_name("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_attachment_name("a\\b\\my report (1).png"),
            "my_report__1_.png"
        );
        assert_eq!(sanitize_attachment_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_attachment_name("..."), "attachment");

        // Long filename whose `.` lands right on the truncation boundary must never produce `..`.
        let boundary = format!("{}.bbbb.png", "a".repeat(115));
        let truncated = sanitize_attachment_name(&boundary);
        assert!(!truncated.contains(".."), "{truncated}");
        assert!(validate_attachment_key(&format!("1-{truncated}")).is_ok());

        for bad in [
            "..",
            ".",
            "../secret.txt",
            "..\\secret.txt",
            "sub/file.txt",
            "C:drive.txt",
            "bad\0name.txt",
            "bad\nname.txt",
            ".hidden",
            "NUL.png",
        ] {
            assert!(validate_attachment_key(bad).is_err(), "must reject {bad:?}");
        }
    }
}
