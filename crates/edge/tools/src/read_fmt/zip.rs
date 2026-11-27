//! Minimal ZIP reader: central directory index plus stored/deflated member extraction. Shared by
//! zip archives, Office documents and EPUB. Encryption and multi-disk archives are not supported.

use flate2::read::DeflateDecoder;
use hrn_kernel::{HrnError, Result};
use std::io::{Read, Seek, SeekFrom};

const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD64_LOC_SIG: u32 = 0x0706_4b50;
const EOCD64_SIG: u32 = 0x0606_4b50;
const CEN_SIG: u32 = 0x0201_4b50;
const MAX_COMMENT: u64 = 0xFFFF;
const EOCD_LEN: u64 = 22;

/// One central-directory record.
pub(super) struct ZipEntry {
    pub(super) name: String,
    /// Uncompressed size.
    pub(super) size: u64,
    compressed: u64,
    method: u16,
    flags: u16,
    offset: u64,
}

/// An opened zip: the entry index over a seekable source.
pub(super) struct Zip<R> {
    src: R,
    entries: Vec<ZipEntry>,
}

fn corrupt() -> HrnError {
    HrnError::Tool("corrupt or unsupported zip archive".into())
}

fn io(e: &std::io::Error) -> HrnError {
    HrnError::Tool(format!("zip read failed: {e}"))
}

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    let bytes = b.get(at..at + 2).ok_or_else(corrupt)?;
    Ok(u16::from_le_bytes(bytes.try_into().map_err(|_| corrupt())?))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    let bytes = b.get(at..at + 4).ok_or_else(corrupt)?;
    Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| corrupt())?))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64> {
    let bytes = b.get(at..at + 8).ok_or_else(corrupt)?;
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| corrupt())?))
}

/// Whether `head` starts like a zip file (local header or empty archive).
pub(super) fn has_magic(head: &[u8]) -> bool {
    head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06")
}

impl<R: Read + Seek> Zip<R> {
    /// Index the central directory of `src`.
    pub(super) fn open(mut src: R) -> Result<Self> {
        let len = src.seek(SeekFrom::End(0)).map_err(|e| io(&e))?;
        let tail_len = len.min(EOCD_LEN + MAX_COMMENT);
        let tail_start = len - tail_len;
        src.seek(SeekFrom::Start(tail_start)).map_err(|e| io(&e))?;
        let mut tail = vec![0u8; usize::try_from(tail_len).map_err(|_| corrupt())?];
        src.read_exact(&mut tail).map_err(|e| io(&e))?;
        let eocd = (0..tail.len().saturating_sub(21))
            .rev()
            .find(|&i| u32_at(&tail, i).is_ok_and(|s| s == EOCD_SIG))
            .ok_or_else(corrupt)?;
        let mut count = u64::from(u16_at(&tail, eocd + 10)?);
        let mut cd_size = u64::from(u32_at(&tail, eocd + 12)?);
        let mut cd_offset = u64::from(u32_at(&tail, eocd + 16)?);
        if (count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF)
            && eocd >= 20
            && u32_at(&tail, eocd - 20)? == EOCD64_LOC_SIG
        {
            let at = u64_at(&tail, eocd - 20 + 8)?;
            src.seek(SeekFrom::Start(at)).map_err(|e| io(&e))?;
            let mut rec = [0u8; 56];
            src.read_exact(&mut rec).map_err(|e| io(&e))?;
            if u32_at(&rec, 0)? != EOCD64_SIG {
                return Err(corrupt());
            }
            count = u64_at(&rec, 32)?;
            cd_size = u64_at(&rec, 40)?;
            cd_offset = u64_at(&rec, 48)?;
        }
        if cd_size > 256 * 1024 * 1024 {
            return Err(corrupt());
        }
        src.seek(SeekFrom::Start(cd_offset)).map_err(|e| io(&e))?;
        let mut cd = vec![0u8; usize::try_from(cd_size).map_err(|_| corrupt())?];
        src.read_exact(&mut cd).map_err(|e| io(&e))?;
        let mut entries = Vec::new();
        let mut at = 0usize;
        for _ in 0..count {
            if u32_at(&cd, at)? != CEN_SIG {
                return Err(corrupt());
            }
            let (entry, next) = parse_entry(&cd, at)?;
            entries.push(entry);
            at = next;
        }
        Ok(Self { src, entries })
    }

    /// The central-directory records in archive order.
    pub(super) fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    /// Index of the entry called `name`.
    pub(super) fn find(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.name == name)
    }

    /// Decompress entry `idx`, refusing more than `cap` bytes.
    pub(super) fn read(&mut self, idx: usize, cap: u64) -> Result<Vec<u8>> {
        let e = self.entries.get(idx).ok_or_else(corrupt)?;
        if e.flags & 1 != 0 {
            return Err(HrnError::Tool(format!(
                "zip entry `{}` is encrypted",
                e.name
            )));
        }
        if e.size > cap {
            return Err(HrnError::Tool(format!(
                "zip entry `{}` is {} bytes, over the {cap}-byte read limit",
                e.name, e.size
            )));
        }
        let (method, compressed, offset) = (e.method, e.compressed, e.offset);
        self.src.seek(SeekFrom::Start(offset)).map_err(|e| io(&e))?;
        let mut local = [0u8; 30];
        self.src.read_exact(&mut local).map_err(|e| io(&e))?;
        let skip = u64::from(u16_at(&local, 26)?) + u64::from(u16_at(&local, 28)?);
        self.src
            .seek(SeekFrom::Current(
                i64::try_from(skip).map_err(|_| corrupt())?,
            ))
            .map_err(|e| io(&e))?;
        let raw = (&mut self.src).take(compressed);
        let mut out = Vec::new();
        match method {
            0 => raw.take(cap + 1).read_to_end(&mut out),
            8 => DeflateDecoder::new(raw).take(cap + 1).read_to_end(&mut out),
            m => {
                return Err(HrnError::Tool(format!(
                    "zip compression method {m} is not supported"
                )));
            }
        }
        .map_err(|e| io(&e))?;
        if out.len() as u64 > cap {
            return Err(HrnError::Tool(format!(
                "zip entry expands past the {cap}-byte read limit"
            )));
        }
        Ok(out)
    }
}

/// Parse the central-directory record at `at`; returns it and the offset of the next record.
fn parse_entry(cd: &[u8], at: usize) -> Result<(ZipEntry, usize)> {
    let flags = u16_at(cd, at + 8)?;
    let method = u16_at(cd, at + 10)?;
    let mut compressed = u64::from(u32_at(cd, at + 20)?);
    let mut size = u64::from(u32_at(cd, at + 24)?);
    let name_len = usize::from(u16_at(cd, at + 28)?);
    let extra_len = usize::from(u16_at(cd, at + 30)?);
    let comment_len = usize::from(u16_at(cd, at + 32)?);
    let mut offset = u64::from(u32_at(cd, at + 42)?);
    let name_at = at + 46;
    let name = String::from_utf8_lossy(cd.get(name_at..name_at + name_len).ok_or_else(corrupt)?)
        .into_owned();
    let extra_at = name_at + name_len;
    let extra = cd.get(extra_at..extra_at + extra_len).ok_or_else(corrupt)?;
    if size == 0xFFFF_FFFF || compressed == 0xFFFF_FFFF || offset == 0xFFFF_FFFF {
        zip64_extra(extra, &mut size, &mut compressed, &mut offset)?;
    }
    let entry = ZipEntry {
        name,
        size,
        compressed,
        method,
        flags,
        offset,
    };
    Ok((entry, extra_at + extra_len + comment_len))
}

/// Replace the `0xFFFFFFFF` placeholders from the zip64 extra field (id 0x0001).
fn zip64_extra(extra: &[u8], size: &mut u64, compressed: &mut u64, offset: &mut u64) -> Result {
    let mut at = 0usize;
    while at + 4 <= extra.len() {
        let id = u16_at(extra, at)?;
        let len = usize::from(u16_at(extra, at + 2)?);
        if id == 1 {
            let mut pos = at + 4;
            for field in [&mut *size, &mut *compressed, &mut *offset] {
                if *field == 0xFFFF_FFFF {
                    *field = u64_at(extra, pos)?;
                    pos += 8;
                }
            }
            return Ok(());
        }
        at += 4 + len;
    }
    Err(corrupt())
}
