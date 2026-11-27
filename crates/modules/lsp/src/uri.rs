//! RFC 8089 `file://` URI helpers and file-extension to LSP `languageId` mapping.

use std::path::{Path, PathBuf};

/// Convert an absolute filesystem path into a canonical `file://` URI.
pub(crate) fn path_to_uri(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    let normalized = abs.to_string_lossy().replace('\\', "/");
    let stripped = normalized.strip_prefix("//?/").unwrap_or(&normalized);
    let mut out = String::with_capacity(stripped.len() + 12);
    out.push_str("file://");
    if !stripped.starts_with('/') {
        out.push('/');
    }
    for (idx, b) in stripped.bytes().enumerate() {
        // Keep drive colon (`C:`) unencoded as expected by rust-analyzer / VS Code LSP servers.
        let is_drive_colon = idx == 1
            && b == b':'
            && stripped
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic);
        if is_drive_colon || is_unreserved_uri_byte(b) || b == b'/' {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0F));
        }
    }
    out
}

/// Convert a `file://` URI back into a local filesystem [`PathBuf`].
pub(crate) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(rest);
    // On Windows `file:///C:/foo` has a leading `/` before the drive letter.
    let bytes = decoded.as_bytes();
    let trimmed = if bytes.len() >= 3
        && bytes.first() == Some(&b'/')
        && bytes.get(1).is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(2) == Some(&b':')
    {
        decoded.get(1..).unwrap_or(&decoded)
    } else {
        decoded.as_str()
    };
    #[cfg(windows)]
    {
        Some(PathBuf::from(trimmed.replace('/', "\\")))
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from(trimmed))
    }
}

/// Normalize a URI key for cache lookups (case-insensitive drive letter on Windows).
pub(crate) fn canonical_uri_key(uri: &str) -> String {
    let decoded = percent_decode(uri);
    #[cfg(windows)]
    {
        if let Some(rest) = decoded.strip_prefix("file:///") {
            let mut chars = rest.chars();
            if let (Some(drive), Some(':')) = (chars.next(), chars.next())
                && drive.is_ascii_alphabetic()
            {
                return format!(
                    "file:///{}:{}{}",
                    drive.to_ascii_lowercase(),
                    "",
                    chars.as_str()
                );
            }
        }
    }
    decoded
}

/// Map a file extension to its standard LSP `languageId`.
pub(crate) fn language_id_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Some("rust"),
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("typescriptreact"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "jsx" => Some("javascriptreact"),
        "py" | "pyi" => Some("python"),
        "go" => Some("go"),
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" => Some("cpp"),
        "json" | "jsonc" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "java" => Some("java"),
        "kt" | "kts" => Some("kotlin"),
        "cs" => Some("csharp"),
        "lua" => Some("lua"),
        "zig" => Some("zig"),
        _ => None,
    }
}

fn is_unreserved_uri_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => char::from(b'0' + n),
        10..=15 => char::from(b'A' + (n - 10)),
        _ => '0',
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'%')
            && let (Some(&hi), Some(&lo)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        if let Some(&b) = bytes.get(i) {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_uris_with_spaces() {
        let p = PathBuf::from(if cfg!(windows) {
            r"C:\Users\Test User\my file.rs"
        } else {
            "/home/test user/my file.rs"
        });
        let uri = path_to_uri(&p);
        assert!(uri.starts_with("file:///"), "{uri}");
        assert!(
            uri.contains("Test%20User") || uri.contains("test%20user"),
            "{uri}"
        );
        let back = uri_to_path(&uri).expect("decode");
        assert_eq!(back, p);
    }

    #[test]
    fn maps_common_language_ids() {
        assert_eq!(language_id_for_path(Path::new("src/main.rs")), Some("rust"));
        assert_eq!(
            language_id_for_path(Path::new("app/page.tsx")),
            Some("typescriptreact")
        );
        assert_eq!(language_id_for_path(Path::new("script.py")), Some("python"));
        assert_eq!(language_id_for_path(Path::new("cmd/main.go")), Some("go"));
        assert_eq!(language_id_for_path(Path::new("README.md")), None);
    }
}
