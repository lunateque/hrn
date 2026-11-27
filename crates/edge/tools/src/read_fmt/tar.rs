//! Streaming tar / tar.gz reader: indexes headers (skipping payloads) and extracts one member.

use flate2::read::GzDecoder;
use hrn_kernel::{HrnError, Result};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek};
use std::path::Path;

const BLOCK: usize = 512;

/// What a tar header describes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Dir,
    /// Links, devices and other non-regular members.
    Other,
}

/// One tar member header.
pub(super) struct TarEntry {
    pub(super) name: String,
    pub(super) size: u64,
    pub(super) kind: Kind,
}

enum Source {
    Plain(BufReader<File>),
    Gz(GzDecoder<BufReader<File>>),
}

fn err(e: &io::Error) -> HrnError {
    HrnError::Tool(format!("tar read failed: {e}"))
}

impl Source {
    fn open(path: &Path) -> Result<Self> {
        let mut file = BufReader::new(
            File::open(path).map_err(|e| HrnError::Tool(format!("cannot open archive: {e}")))?,
        );
        let mut magic = [0u8; 2];
        let gz = file.read_exact(&mut magic).is_ok() && magic == [0x1f, 0x8b];
        file.rewind().map_err(|e| err(&e))?;
        Ok(if gz {
            Self::Gz(GzDecoder::new(file))
        } else {
            Self::Plain(file)
        })
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Plain(r) => r.read_exact(buf),
            Self::Gz(r) => r.read_exact(buf),
        }
    }

    fn skip(&mut self, n: u64) -> io::Result<()> {
        match self {
            Self::Plain(r) => r.seek_relative(i64::try_from(n).map_err(io::Error::other)?),
            Self::Gz(r) => io::copy(&mut r.by_ref().take(n), &mut io::sink()).map(|_| ()),
        }
    }

    fn take_vec(&mut self, n: u64) -> Result<Vec<u8>> {
        let mut out = vec![0u8; usize::try_from(n).map_err(|_| too_big())?];
        self.read_exact(&mut out).map_err(|e| err(&e))?;
        Ok(out)
    }
}

fn too_big() -> HrnError {
    HrnError::Tool("tar member is too large".into())
}

/// Octal (or GNU base-256) numeric header field.
fn number(field: &[u8]) -> u64 {
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        return field.iter().enumerate().fold(0u64, |acc, (i, b)| {
            (acc << 8) | u64::from(if i == 0 { b & 0x7f } else { *b })
        });
    }
    field
        .iter()
        .take_while(|b| **b != 0 && **b != b' ')
        .filter(|b| b.is_ascii_digit())
        .fold(0u64, |acc, b| (acc << 3) | u64::from(b - b'0'))
}

fn c_str(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(field.get(..end).unwrap_or_default()).into_owned()
}

/// `path=` record of a pax extended header.
fn pax_path(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    text.split('\n')
        .filter_map(|rec| rec.split_once(' ').map(|(_, kv)| kv))
        .find_map(|kv| kv.strip_prefix("path=").map(str::to_owned))
}

/// Walk every header of the archive at `path`, indexing it; when `want` names a regular member
/// its bytes (at most `cap`) are returned and the walk stops there.
fn walk(path: &Path, want: Option<&str>, cap: u64) -> Result<(Vec<TarEntry>, Option<Vec<u8>>)> {
    let mut src = Source::open(path)?;
    let mut entries = Vec::new();
    let mut pending: Option<String> = None;
    loop {
        let mut header = [0u8; BLOCK];
        if src.read_exact(&mut header).is_err() || header.iter().all(|b| *b == 0) {
            return Ok((entries, None));
        }
        let field = |a: usize, b: usize| header.get(a..b).unwrap_or_default();
        let size = number(field(124, 136));
        let flag = header.get(156).copied().unwrap_or(b'0');
        let padded = size.div_ceil(BLOCK as u64) * BLOCK as u64;
        if matches!(flag, b'L' | b'x' | b'g') {
            if size > 1 << 20 {
                return Err(too_big());
            }
            let data = src.take_vec(padded)?;
            let body = data.get(..usize::try_from(size).map_err(|_| too_big())?);
            match (flag, body) {
                (b'L', Some(b)) => pending = Some(c_str(b)),
                (b'x', Some(b)) => pending = pax_path(b).or(pending),
                _ => {}
            }
            continue;
        }
        let mut name = pending.take().unwrap_or_else(|| {
            let base = c_str(field(0, 100));
            let prefix = if field(257, 262) == b"ustar" {
                c_str(field(345, 500))
            } else {
                String::new()
            };
            if prefix.is_empty() {
                base
            } else {
                format!("{prefix}/{base}")
            }
        });
        let kind = match flag {
            b'0' | 0 | b'7' => Kind::File,
            b'5' => Kind::Dir,
            _ => Kind::Other,
        };
        if kind == Kind::Dir || name.ends_with('/') {
            name.truncate(name.trim_end_matches('/').len());
        }
        let name = name.trim_start_matches("./").to_owned();
        if kind == Kind::File && want == Some(name.as_str()) {
            if size > cap {
                return Err(HrnError::Tool(format!(
                    "tar member `{name}` is {size} bytes, over the {cap}-byte read limit"
                )));
            }
            let mut data = src.take_vec(padded)?;
            data.truncate(usize::try_from(size).map_err(|_| too_big())?);
            entries.push(TarEntry { name, size, kind });
            return Ok((entries, Some(data)));
        }
        let payload = if kind == Kind::File { padded } else { 0 };
        src.skip(payload).map_err(|e| err(&e))?;
        if !name.is_empty() {
            entries.push(TarEntry { name, size, kind });
        }
    }
}

/// Every member of the tar / tar.gz at `path`.
pub(super) fn index(path: &Path) -> Result<Vec<TarEntry>> {
    walk(path, None, 0).map(|(entries, _)| entries)
}

/// Bytes of regular member `name`.
pub(super) fn read(path: &Path, name: &str, cap: u64) -> Result<Vec<u8>> {
    walk(path, Some(name), cap)?
        .1
        .ok_or_else(|| HrnError::Tool(format!("no regular file `{name}` in the archive")))
}

/// Whether `head` starts like gzip or contains a tar `ustar` magic at offset 257.
pub(super) fn has_magic(head: &[u8]) -> bool {
    head.starts_with(&[0x1f, 0x8b]) || head.get(257..262) == Some(b"ustar")
}
