//! Pure-Rust `.zip` and `.tar.gz` release archive extraction for `hrn update`.
//!
//! Security invariants:
//! - Every archive entry path is inspected BEFORE extraction; any entry with `..`, leading `/` or
//!   `\`, Windows drive letters (`C:`), NUL/control bytes, or symlink/hardlink/special-device
//!   metadata causes the entire archive to be rejected (zip-slip & symlink hardening).
//! - Only the exact expected binary filenames (`hrn`/`hrn.exe` and optional `hrn-ast`/`hrn-ast.exe`)
//!   are extracted; all other archive members are ignored (after path/type validation).
//! - Decompressed output per binary is strictly capped at `max_bytes` (zip-bomb defense).

use flate2::read::{DeflateDecoder, GzDecoder};
use hrn_kernel::{HrnError, Result};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

/// Extracted release binaries ready for atomic installation.
#[derive(Debug, Default)]
pub(crate) struct ExtractedBinaries {
    /// Required main `hrn` / `hrn.exe` binary bytes.
    pub(crate) hrn: Vec<u8>,
    /// Optional sibling `hrn-ast` / `hrn-ast.exe` binary bytes.
    pub(crate) hrn_ast: Option<Vec<u8>>,
}

const EOCD_SIG: u32 = 0x0605_4b50;
const CEN_SIG: u32 = 0x0201_4b50;
const LOC_SIG: u32 = 0x0403_4b50;
const TAR_BLOCK: usize = 512;

/// Expected binary filename for the current target OS (`hrn.exe` on Windows, `hrn` elsewhere).
pub(crate) const fn hrn_exe_name() -> &'static str {
    if cfg!(windows) { "hrn.exe" } else { "hrn" }
}

/// Expected sibling `hrn-ast` binary filename for the current target OS.
pub(crate) const fn hrn_ast_exe_name() -> &'static str {
    if cfg!(windows) {
        "hrn-ast.exe"
    } else {
        "hrn-ast"
    }
}

/// Extract `hrn` (and optional `hrn-ast`) from a verified release asset.
///
/// Supports `.zip`, `.tar.gz` / `.tgz`, or a raw binary asset.
pub(crate) fn extract_release_binaries(
    asset_name: &str,
    bytes: &[u8],
    max_bytes: usize,
) -> Result<ExtractedBinaries> {
    let ext = Path::new(asset_name).extension();
    if ext.is_some_and(|e| e.eq_ignore_ascii_case("zip")) || bytes.starts_with(b"PK\x03\x04") {
        return extract_zip(bytes, max_bytes);
    }
    let lower = asset_name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz")
        || ext.is_some_and(|e| e.eq_ignore_ascii_case("tgz"))
        || bytes.starts_with(&[0x1f, 0x8b])
    {
        return extract_tar_gz(bytes, max_bytes);
    }
    if bytes.is_empty() || bytes.len() > max_bytes {
        return Err(HrnError::Tool(
            "release binary size is empty or exceeds maximum bound".to_owned(),
        ));
    }
    Ok(ExtractedBinaries {
        hrn: bytes.to_vec(),
        hrn_ast: None,
    })
}

/// Validate that an archive entry path contains no path traversal, absolute prefixes, drive
/// letters, or control characters, and return its clean slash-separated segments.
pub(crate) fn validate_archive_entry_path(raw: &str) -> Result<Vec<&str>> {
    if raw.is_empty() {
        return Err(HrnError::Tool(
            "archive contains empty entry path".to_owned(),
        ));
    }
    if raw.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(HrnError::Tool(
            "archive entry path contains control characters".to_owned(),
        ));
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(HrnError::Tool(format!(
            "rejected absolute archive entry path `{raw}`"
        )));
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2
        && bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':')
    {
        return Err(HrnError::Tool(format!(
            "rejected drive-prefixed archive entry path `{raw}`"
        )));
    }
    let trimmed = raw.trim_end_matches(['/', '\\']);
    let mut parts = Vec::new();
    for seg in trimmed.split(['/', '\\']) {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains(':') {
            return Err(HrnError::Tool(format!(
                "rejected path-traversal archive entry `{raw}`"
            )));
        }
        parts.push(seg);
    }
    Ok(parts)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetBin {
    Hrn,
    HrnAst,
}

fn match_target_binary(segments: &[&str]) -> Option<TargetBin> {
    if segments.is_empty() || segments.len() > 2 {
        return None;
    }
    let file = *segments.last()?;
    if file == hrn_exe_name() {
        Some(TargetBin::Hrn)
    } else if file == hrn_ast_exe_name() {
        Some(TargetBin::HrnAst)
    } else {
        None
    }
}

fn extract_zip(bytes: &[u8], max_bytes: usize) -> Result<ExtractedBinaries> {
    let mut cur = Cursor::new(bytes);
    let len = bytes.len();
    if len < 22 {
        return Err(HrnError::Tool("corrupt zip archive".to_owned()));
    }
    let tail_len = len.min(22 + 0xFFFF);
    let tail = bytes
        .get(len - tail_len..)
        .ok_or_else(|| HrnError::Tool("corrupt zip archive".to_owned()))?;
    let eocd_rel = (0..=tail.len().saturating_sub(22))
        .rev()
        .find(|&i| u32_le(tail, i).is_ok_and(|s| s == EOCD_SIG))
        .ok_or_else(|| HrnError::Tool("missing zip end-of-central-directory".to_owned()))?;
    let count = usize::from(u16_le(tail, eocd_rel + 10)?);
    let cd_size = usize::try_from(u32_le(tail, eocd_rel + 12)?)
        .map_err(|_| HrnError::Tool("invalid zip central directory size".to_owned()))?;
    let cd_offset = usize::try_from(u32_le(tail, eocd_rel + 16)?)
        .map_err(|_| HrnError::Tool("invalid zip central directory offset".to_owned()))?;
    let cd_end = cd_offset
        .checked_add(cd_size)
        .ok_or_else(|| HrnError::Tool("zip central directory overflow".to_owned()))?;
    let cd = bytes
        .get(cd_offset..cd_end)
        .ok_or_else(|| HrnError::Tool("zip central directory out of bounds".to_owned()))?;

    let mut out = ExtractedBinaries::default();
    let mut at = 0usize;
    for _ in 0..count {
        if u32_le(cd, at)? != CEN_SIG {
            return Err(HrnError::Tool(
                "corrupt zip central directory entry".to_owned(),
            ));
        }
        let ver_made = u16_le(cd, at + 4)?;
        let flags = u16_le(cd, at + 8)?;
        let method = u16_le(cd, at + 10)?;
        let comp_size = usize::try_from(u32_le(cd, at + 20)?)
            .map_err(|_| HrnError::Tool("zip compressed size overflow".to_owned()))?;
        let uncomp_size = usize::try_from(u32_le(cd, at + 24)?)
            .map_err(|_| HrnError::Tool("zip uncompressed size overflow".to_owned()))?;
        let name_len = usize::from(u16_le(cd, at + 28)?);
        let extra_len = usize::from(u16_le(cd, at + 30)?);
        let comment_len = usize::from(u16_le(cd, at + 32)?);
        let ext_attr = u32_le(cd, at + 38)?;
        let local_off = usize::try_from(u32_le(cd, at + 42)?)
            .map_err(|_| HrnError::Tool("zip local offset overflow".to_owned()))?;

        let name_bytes = cd
            .get(at + 46..at + 46 + name_len)
            .ok_or_else(|| HrnError::Tool("zip entry name out of bounds".to_owned()))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| HrnError::Tool("zip entry name is not valid UTF-8".to_owned()))?;
        at = at + 46 + name_len + extra_len + comment_len;

        if flags & 1 != 0 {
            return Err(HrnError::Tool(format!(
                "rejected encrypted zip entry `{name}`"
            )));
        }
        // Reject Unix symlinks (S_IFLNK = 0o120000 in upper 16 bits of external attributes).
        if (ver_made >> 8) == 3 && ((ext_attr >> 16) & 0o170_000) == 0o120_000 {
            return Err(HrnError::Tool(format!(
                "rejected symlink zip entry `{name}`"
            )));
        }
        let segments = validate_archive_entry_path(name)?;
        if name.ends_with('/') {
            continue;
        }
        let Some(target) = match_target_binary(&segments) else {
            continue;
        };
        if uncomp_size > max_bytes {
            return Err(HrnError::Tool(format!(
                "zip entry `{name}` exceeds maximum size ({max_bytes} bytes)"
            )));
        }
        let payload =
            read_zip_local_entry(&mut cur, bytes, local_off, method, comp_size, max_bytes)?;
        assign_extracted(&mut out, target, payload, name)?;
    }

    if out.hrn.is_empty() {
        return Err(HrnError::Tool(format!(
            "release zip archive did not contain `{}`",
            hrn_exe_name()
        )));
    }
    Ok(out)
}

fn read_zip_local_entry(
    cur: &mut Cursor<&[u8]>,
    bytes: &[u8],
    local_off: usize,
    method: u16,
    comp_size: usize,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    if u32_le(bytes, local_off)? != LOC_SIG {
        return Err(HrnError::Tool("corrupt zip local header".to_owned()));
    }
    let name_len = usize::from(u16_le(bytes, local_off + 26)?);
    let extra_len = usize::from(u16_le(bytes, local_off + 28)?);
    let data_start = local_off
        .checked_add(30)
        .and_then(|v| v.checked_add(name_len))
        .and_then(|v| v.checked_add(extra_len))
        .ok_or_else(|| HrnError::Tool("zip local data offset overflow".to_owned()))?;
    let data_end = data_start
        .checked_add(comp_size)
        .ok_or_else(|| HrnError::Tool("zip data length overflow".to_owned()))?;
    let slice = bytes
        .get(data_start..data_end)
        .ok_or_else(|| HrnError::Tool("zip entry data out of bounds".to_owned()))?;
    let _ = cur.seek(SeekFrom::Start(0));
    match method {
        0 => {
            if slice.len() > max_bytes {
                return Err(HrnError::Tool("zip entry exceeds maximum size".to_owned()));
            }
            Ok(slice.to_vec())
        }
        8 => read_bounded(DeflateDecoder::new(slice), max_bytes),
        other => Err(HrnError::Tool(format!(
            "unsupported zip compression method {other}"
        ))),
    }
}

fn parse_tar_entry_sizes(header: &[u8; TAR_BLOCK], max_bytes: usize) -> Result<(u64, u64, u8)> {
    let size = parse_tar_octal(header.get(124..136).unwrap_or_default())?;
    let max_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    if size > max_u64 {
        return Err(HrnError::Tool(format!(
            "tar entry exceeds maximum size ({max_bytes} bytes)"
        )));
    }
    let padded = size
        .div_ceil(TAR_BLOCK as u64)
        .checked_mul(TAR_BLOCK as u64)
        .ok_or_else(|| HrnError::Tool("tar entry padded size overflow".to_owned()))?;
    let flag = header.get(156).copied().unwrap_or(b'0');
    Ok((size, padded, flag))
}

fn extract_tar_gz(bytes: &[u8], max_bytes: usize) -> Result<ExtractedBinaries> {
    let mut gz = GzDecoder::new(bytes);
    let mut out = ExtractedBinaries::default();
    let mut pending_long_name: Option<String> = None;
    let max_total = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_mul(4);
    let mut total_bytes: u64 = 0;

    loop {
        let mut header = [0u8; TAR_BLOCK];
        match gz.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(HrnError::Tool(format!("tar.gz read failed: {e}"))),
        }
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let field = |a: usize, b: usize| header.get(a..b).unwrap_or_default();
        let (size, padded, flag) = parse_tar_entry_sizes(&header, max_bytes)?;
        total_bytes = total_bytes
            .checked_add(padded)
            .filter(|&n| n <= max_total)
            .ok_or_else(|| HrnError::Tool("tar.gz total size exceeds maximum bound".to_owned()))?;

        if matches!(flag, b'L' | b'x' | b'g') {
            if size > 1024 * 1024 {
                return Err(HrnError::Tool("tar extended header too large".to_owned()));
            }
            let buf = read_exact_u64(&mut gz, padded)?;
            let body = buf
                .get(..usize::try_from(size).unwrap_or(0))
                .unwrap_or_default();
            if flag == b'L' {
                pending_long_name = Some(c_str(body));
            } else if flag == b'x' {
                pending_long_name = pax_path(body).or(pending_long_name);
            }
            continue;
        }

        let name = pending_long_name.take().unwrap_or_else(|| {
            let prefix = c_str(field(345, 500));
            let base = c_str(field(0, 100));
            if prefix.is_empty() {
                base
            } else {
                format!("{prefix}/{base}")
            }
        });

        // Reject symlinks (b'2'), hardlinks (b'1'), devices/FIFOs (b'3', b'4', b'6'), etc.
        match flag {
            b'0' | 0 | b'5' => {}
            other => {
                return Err(HrnError::Tool(format!(
                    "rejected non-regular tar entry `{name}` (typeflag={other})"
                )));
            }
        }
        let segments = validate_archive_entry_path(&name)?;
        if flag == b'5' {
            skip_u64(&mut gz, padded)?;
            continue;
        }

        let Some(target) = match_target_binary(&segments) else {
            skip_u64(&mut gz, padded)?;
            continue;
        };
        let size_usize = usize::try_from(size)
            .map_err(|_| HrnError::Tool("tar member size overflow".to_owned()))?;
        if size_usize > max_bytes {
            return Err(HrnError::Tool(format!(
                "tar entry `{name}` exceeds maximum size ({max_bytes} bytes)"
            )));
        }
        let payload = read_exact_u64(&mut gz, size)?;
        skip_u64(&mut gz, padded - size)?;
        assign_extracted(&mut out, target, payload, &name)?;
    }

    if out.hrn.is_empty() {
        return Err(HrnError::Tool(format!(
            "release tar.gz archive did not contain `{}`",
            hrn_exe_name()
        )));
    }
    Ok(out)
}

fn assign_extracted(
    out: &mut ExtractedBinaries,
    target: TargetBin,
    payload: Vec<u8>,
    name: &str,
) -> Result {
    match target {
        TargetBin::Hrn => {
            if !out.hrn.is_empty() {
                return Err(HrnError::Tool(format!(
                    "duplicate target binary `{name}` in release archive"
                )));
            }
            out.hrn = payload;
        }
        TargetBin::HrnAst => {
            if out.hrn_ast.is_some() {
                return Err(HrnError::Tool(format!(
                    "duplicate target binary `{name}` in release archive"
                )));
            }
            out.hrn_ast = Some(payload);
        }
    }
    Ok(())
}

fn read_bounded<R: Read>(mut reader: R, max_bytes: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let cap = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    reader
        .by_ref()
        .take(cap)
        .read_to_end(&mut out)
        .map_err(|e| HrnError::Tool(format!("archive decompression failed: {e}")))?;
    if out.len() > max_bytes {
        return Err(HrnError::Tool(format!(
            "decompressed archive member exceeds maximum size ({max_bytes} bytes)"
        )));
    }
    Ok(out)
}

fn read_exact_u64<R: Read>(r: &mut R, n: u64) -> Result<Vec<u8>> {
    let len = usize::try_from(n).map_err(|_| HrnError::Tool("tar size overflow".to_owned()))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)
        .map_err(|e| HrnError::Tool(format!("tar read failed: {e}")))?;
    Ok(buf)
}

fn skip_u64<R: Read>(r: &mut R, n: u64) -> Result {
    if n == 0 {
        return Ok(());
    }
    let copied = io::copy(&mut r.by_ref().take(n), &mut io::sink())
        .map_err(|e| HrnError::Tool(format!("tar skip failed: {e}")))?;
    if copied < n {
        return Err(HrnError::Tool("truncated tar archive".to_owned()));
    }
    Ok(())
}

fn parse_tar_octal(field: &[u8]) -> Result<u64> {
    if let Some(&first) = field.first()
        && first & 0x80 != 0
    {
        if first & 0x40 != 0 {
            return Err(HrnError::Tool("negative tar base-256 size".to_owned()));
        }
        let mut acc = u64::from(first & 0x7f);
        for &b in field.get(1..).unwrap_or_default() {
            if acc > (u64::MAX >> 8) {
                return Err(HrnError::Tool("tar base-256 size overflow".to_owned()));
            }
            acc = (acc << 8) | u64::from(b);
        }
        return Ok(acc);
    }
    let mut acc = 0u64;
    for &b in field {
        if b == 0 || b == b' ' {
            if acc > 0 {
                break;
            }
            continue;
        }
        if !(b'0'..=b'7').contains(&b) {
            return Err(HrnError::Tool("corrupt tar octal size field".to_owned()));
        }
        acc = acc
            .checked_mul(8)
            .and_then(|v| v.checked_add(u64::from(b - b'0')))
            .ok_or_else(|| HrnError::Tool("tar size overflow".to_owned()))?;
    }
    Ok(acc)
}

fn c_str(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(field.get(..end).unwrap_or_default()).into_owned()
}

fn pax_path(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    text.split('\n')
        .filter_map(|rec| rec.split_once(' ').map(|(_, kv)| kv))
        .find_map(|kv| kv.strip_prefix("path=").map(str::to_owned))
}

fn u16_le(b: &[u8], at: usize) -> Result<u16> {
    let s = b
        .get(at..at + 2)
        .ok_or_else(|| HrnError::Tool("corrupt zip u16".to_owned()))?;
    Ok(u16::from_le_bytes(s.try_into().map_err(|_| {
        HrnError::Tool("corrupt zip u16".to_owned())
    })?))
}

fn u32_le(b: &[u8], at: usize) -> Result<u32> {
    let s = b
        .get(at..at + 4)
        .ok_or_else(|| HrnError::Tool("corrupt zip u32".to_owned()))?;
    Ok(u32::from_le_bytes(s.try_into().map_err(|_| {
        HrnError::Tool("corrupt zip u32".to_owned())
    })?))
}
