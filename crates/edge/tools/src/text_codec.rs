//! UTF-8 text file codec that preserves BOM and line endings, plus atomic file replacement.

use hrn_kernel::{HrnError, Result};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const BOM: &str = "\u{feff}";
/// Attempts at the final rename (Windows indexers/AV briefly hold files open).
const RENAME_ATTEMPTS: u32 = 4;
const RENAME_BACKOFF: Duration = Duration::from_millis(40);

/// A decoded text file: LF-normalized `text` plus the encoding facts needed to write it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFile {
    /// File started with a UTF-8 byte order mark.
    pub bom: bool,
    /// Dominant line ending is CRLF (written back for every `\n`).
    pub crlf: bool,
    /// File mixed CRLF and bare LF; re-encoding normalizes to the dominant style.
    pub mixed_eol: bool,
    /// Content with every CRLF folded to LF and the BOM removed.
    pub text: String,
}

/// Fold CRLF to LF.
#[must_use]
pub fn normalize_eol(text: &str) -> String {
    text.replace("\r\n", "\n")
}

impl TextFile {
    /// Decode `bytes`; non-UTF-8 content is refused rather than mangled.
    ///
    /// # Errors
    /// Returns `HrnError::Tool` when `bytes` is not valid UTF-8.
    pub fn decode(bytes: Vec<u8>) -> Result<Self> {
        let raw = String::from_utf8(bytes).map_err(|_| {
            HrnError::Tool("file is not valid UTF-8; only UTF-8 text can be edited".into())
        })?;
        let (bom, body) = match raw.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, raw.as_str()),
        };
        let crlf_count = body.matches("\r\n").count();
        let bare_lf = body.matches('\n').count() - crlf_count;
        let text = if crlf_count > 0 {
            normalize_eol(body)
        } else {
            body.to_owned()
        };
        Ok(Self {
            bom,
            crlf: crlf_count > bare_lf,
            mixed_eol: crlf_count > 0 && bare_lf > 0,
            text,
        })
    }

    /// Serialize back with the original BOM and line-ending style.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.text.len() + self.text.len() / 16 + BOM.len());
        if self.bom {
            out.extend_from_slice(BOM.as_bytes());
        }
        if self.crlf {
            out.extend_from_slice(self.text.replace('\n', "\r\n").as_bytes());
        } else {
            out.extend_from_slice(self.text.as_bytes());
        }
        out
    }
}

fn temp_sibling(target: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".{name}.hrn-{}-{n}.tmp", std::process::id()))
}

fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && attempt < RENAME_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(RENAME_BACKOFF);
            }
            other => return other,
        }
    }
}

/// Replace `path` with `bytes` atomically: write a temp file next to it, then rename over it.
///
/// A symlink is followed so the link itself survives; parent directories are created; a
/// read-only target is refused instead of being silently replaced.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let existing = fs::metadata(&target).ok();
    if let Some(meta) = &existing
        && meta.permissions().readonly()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "file is read-only",
        ));
    }
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(&target);
    let result = (|| {
        let mut file = fs::File::create_new(&tmp)?;
        file.write_all(bytes)?;
        if let Some(meta) = &existing {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        rename_with_retry(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_and_crlf_round_trip_byte_exact() {
        let original = "\u{feff}one\r\ntwo\r\n\r\nthree".as_bytes().to_vec();
        let file = TextFile::decode(original.clone()).unwrap();
        assert!(file.bom && file.crlf && !file.mixed_eol);
        assert_eq!(file.text, "one\ntwo\n\nthree");
        assert_eq!(file.encode(), original);
    }

    #[test]
    fn lf_file_stays_lf_and_mixed_is_reported() {
        let lf = TextFile::decode(b"a\nb\n".to_vec()).unwrap();
        assert!(!lf.bom && !lf.crlf);
        assert_eq!(lf.encode(), b"a\nb\n");

        let mixed = TextFile::decode(b"a\r\nb\r\nc\nd".to_vec()).unwrap();
        assert!(mixed.crlf && mixed.mixed_eol);
        assert_eq!(mixed.encode(), b"a\r\nb\r\nc\r\nd");
    }

    #[test]
    fn invalid_utf8_is_refused() {
        assert!(TextFile::decode(vec![0xff, 0xfe, b'a']).is_err());
    }
}
