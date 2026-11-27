//! Writing one member back into a zip / tar archive: read every existing entry, replace or add
//! the addressed one, serialize to a temporary file beside the archive and rename over it. An
//! archive that does not exist yet is created.
//!
//! Existing members keep their bytes but not their mode or mtime (both are reset, zip entries
//! are always re-deflated); a tar holding links or special members is refused rather than
//! silently dropping them.

use super::{archive, tar, zip};
use crate::paths::display_rel;
use crate::time::{dos_stamp, unix_now};
use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder};
use hrn_kernel::{HrnError, Result, ToolOutput};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

const ZIP_SIG_LOCAL: u32 = 0x0403_4b50;
const ZIP_SIG_CENTRAL: u32 = 0x0201_4b50;
const ZIP_SIG_EOCD: u32 = 0x0605_4b50;
const ZIP_VERSION: u16 = 20;
const ZIP_UTF8: u16 = 0x0800;
const UNIX_DIR_MODE: u32 = 0o040_755;
const UNIX_FILE_MODE: u32 = 0o100_644;
const BLOCK: usize = 512;

fn err(what: impl Into<String>) -> HrnError {
    HrnError::Tool(what.into())
}

/// One entry of the rewritten archive.
struct Item {
    name: String,
    dir: bool,
    bytes: Vec<u8>,
}

/// Replace (or add) member `inner` of the archive at `path` with `content`, creating `path` when
/// it does not exist.
///
/// # Errors
/// The archive cannot be read or rewritten, `inner` names a directory, or a tar member is a link.
pub(super) fn write_member(
    cwd: &Path,
    path: &Path,
    format: archive::Format,
    inner: &str,
    content: &str,
) -> Result<ToolOutput> {
    let shown = display_rel(cwd, path);
    let target = archive::normalize(inner)?;
    if target.is_empty() {
        return Err(err(format!(
            "`{shown}:{inner}` names no member; address a file inside the archive"
        )));
    }
    let created = !path.exists();
    let gzip = is_gzip(&shown, path, format);
    let mut items = if created {
        Vec::new()
    } else {
        existing(path, format, &shown)?
    };
    let bytes = content.as_bytes().to_vec();
    match items.iter_mut().find(|i| i.name == target) {
        Some(found) => found.bytes.clone_from(&bytes),
        None => items.push(Item {
            name: target.clone(),
            dir: false,
            bytes,
        }),
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    let temp = Temp::beside(path);
    match match format {
        archive::Format::Zip => write_zip(temp.path(), &items),
        archive::Format::Tar => write_tar(temp.path(), &items, gzip),
    } {
        Ok(()) => {
            fs::rename(temp.path(), path).map_err(|e| err(format!("cannot replace `{shown}`: {e}")))
        }
        Err(e) => Err(e),
    }?;
    Ok(ToolOutput::ok(format!(
        "Successfully wrote {} bytes to {shown}:{target} ({} entries{}, {}).",
        content.len(),
        items.len(),
        if created {
            " created".to_owned()
        } else {
            String::new()
        },
        archive::human(fs::metadata(path)?.len())
    )))
}

/// A temporary file in the archive's directory, deleted on drop unless renamed away.
struct Temp(PathBuf);

impl Temp {
    fn beside(path: &Path) -> Self {
        let name = path.file_name().map_or_else(
            || "archive".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let dir = path.parent().unwrap_or(Path::new("."));
        Self(dir.join(format!(".{name}.hrn-{}.tmp", std::process::id())))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = fs::remove_file(&self.0);
        }
    }
}

/// Whether the tar at `path` is gzip-compressed (by name, then by content).
fn is_gzip(shown: &str, path: &Path, format: archive::Format) -> bool {
    if format != archive::Format::Tar {
        return false;
    }
    let lower = shown.to_ascii_lowercase();
    if [".tar.gz", ".tgz"].iter().any(|ext| lower.ends_with(ext)) {
        return true;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut head = [0u8; 2];
    file.read_exact(&mut head).is_ok() && head == [0x1f, 0x8b]
}

/// Every readable member of `path`, bytes included.
fn existing(path: &Path, format: archive::Format, shown: &str) -> Result<Vec<Item>> {
    match format {
        archive::Format::Zip => {
            let file = File::open(path).map_err(|e| err(format!("cannot open `{shown}`: {e}")))?;
            let mut z = zip::Zip::open(BufReader::new(file))?;
            let named: Vec<(usize, String, bool)> = z
                .entries()
                .iter()
                .enumerate()
                .filter_map(|(idx, e)| {
                    let dir = e.name.ends_with('/');
                    archive::normalize(&e.name)
                        .ok()
                        .filter(|n| !n.is_empty())
                        .map(|n| (idx, n, dir))
                })
                .collect();
            let mut items = Vec::with_capacity(named.len());
            for (idx, name, dir) in named {
                let bytes = if dir {
                    Vec::new()
                } else {
                    z.read(idx, archive::MEMBER_CAP)?
                };
                items.push(Item { name, dir, bytes });
            }
            Ok(items)
        }
        archive::Format::Tar => {
            let entries = tar::index(path)?;
            let mut items = Vec::with_capacity(entries.len());
            for entry in entries {
                let bytes = match entry.kind {
                    tar::Kind::File => tar::read(path, &entry.name, archive::MEMBER_CAP)?,
                    tar::Kind::Dir => Vec::new(),
                    tar::Kind::Other => {
                        return Err(err(format!(
                            "`{shown}` holds the link or special member `{}`; only regular files \
                             and directories can be rewritten",
                            entry.name
                        )));
                    }
                };
                items.push(Item {
                    name: entry.name,
                    dir: entry.kind == tar::Kind::Dir,
                    bytes,
                });
            }
            Ok(items)
        }
    }
}

// ---------------------------------------------------------------------------- zip

/// Local headers + payloads, then the central directory and end record.
fn write_zip(path: &Path, items: &[Item]) -> Result {
    let (time, date) = dos_stamp();
    let file = File::create(path).map_err(|e| io_err(path, &e))?;
    let mut out = BufWriter::new(file);
    let mut central: Vec<u8> = Vec::new();
    let mut count = 0u16;
    for item in items {
        let name = entry_name(item);
        let raw = name.as_bytes();
        let (method, payload) = if item.dir || item.bytes.is_empty() {
            (0u16, Vec::new())
        } else {
            (8u16, deflate(&item.bytes)?)
        };
        let u32_of = |v: usize| {
            u32::try_from(v).map_err(|_| err(format!("`{name}` is past the 4 GiB zip limit")))
        };
        let mut crc = flate2::Crc::new();
        crc.update(&item.bytes);
        let (crc32, usize_, csize) = (crc.sum(), u32_of(item.bytes.len())?, u32_of(payload.len())?);
        let offset = u32::try_from(out.stream_position().map_err(|e| io_err(path, &e))?)
            .map_err(|_| err("the archive passed 4 GiB; zip64 rewriting is unsupported"))?;
        let mut head: Vec<u8> = Vec::with_capacity(30 + raw.len());
        head.extend_from_slice(&ZIP_SIG_LOCAL.to_le_bytes());
        head.extend_from_slice(&ZIP_VERSION.to_le_bytes());
        head.extend_from_slice(&ZIP_UTF8.to_le_bytes());
        head.extend_from_slice(&method.to_le_bytes());
        head.extend_from_slice(&time.to_le_bytes());
        head.extend_from_slice(&date.to_le_bytes());
        head.extend_from_slice(&crc32.to_le_bytes());
        head.extend_from_slice(&csize.to_le_bytes());
        head.extend_from_slice(&usize_.to_le_bytes());
        head.extend_from_slice(&name_len(raw)?.to_le_bytes());
        head.extend_from_slice(&0u16.to_le_bytes());
        head.extend_from_slice(raw);
        out.write_all(&head)
            .and_then(|()| out.write_all(&payload))
            .map_err(|e| io_err(path, &e))?;

        let attrs = if item.dir {
            UNIX_DIR_MODE
        } else {
            UNIX_FILE_MODE
        };
        central.extend_from_slice(&ZIP_SIG_CENTRAL.to_le_bytes());
        central.extend_from_slice(&0x0314u16.to_le_bytes());
        central.extend_from_slice(&ZIP_VERSION.to_le_bytes());
        central.extend_from_slice(&ZIP_UTF8.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&time.to_le_bytes());
        central.extend_from_slice(&date.to_le_bytes());
        central.extend_from_slice(&crc32.to_le_bytes());
        central.extend_from_slice(&csize.to_le_bytes());
        central.extend_from_slice(&usize_.to_le_bytes());
        central.extend_from_slice(&name_len(raw)?.to_le_bytes());
        central.extend_from_slice(&[0u8; 6]);
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&(attrs << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(raw);
        count = count
            .checked_add(1)
            .ok_or_else(|| err("more than 65535 entries: zip64 rewriting is unsupported"))?;
    }
    let cd_at = u32::try_from(out.stream_position().map_err(|e| io_err(path, &e))?)
        .map_err(|_| err("the archive passed 4 GiB"))?;
    let mut end: Vec<u8> = Vec::with_capacity(22);
    end.extend_from_slice(&ZIP_SIG_EOCD.to_le_bytes());
    end.extend_from_slice(&[0u8; 4]);
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&cd_len(&central)?.to_le_bytes());
    end.extend_from_slice(&cd_at.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    out.write_all(&central)
        .and_then(|()| out.write_all(&end))
        .and_then(|()| out.flush())
        .map_err(|e| io_err(path, &e))
}

/// The 2-byte name length a zip entry header carries.
fn name_len(raw: &[u8]) -> Result<u16> {
    u16::try_from(raw.len()).map_err(|_| {
        err(format!(
            "entry name is {} bytes; zip allows 65535",
            raw.len()
        ))
    })
}

/// The 4-byte central-directory size.
fn cd_len(central: &[u8]) -> Result<u32> {
    u32::try_from(central.len()).map_err(|_| err("the central directory passed 4 GiB"))
}

fn entry_name(item: &Item) -> String {
    if item.dir {
        format!("{}/", item.name.trim_end_matches('/'))
    } else {
        item.name.clone()
    }
}

fn io_err(path: &Path, e: &std::io::Error) -> HrnError {
    err(format!("cannot write `{}`: {e}", path.display()))
}

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)
        .map_err(|e| err(format!("zip deflate failed: {e}")))?;
    enc.finish()
        .map_err(|e| err(format!("zip deflate failed: {e}")))
}

// ---------------------------------------------------------------------------- tar

/// A tar (optionally gzipped) stream holding `items`; pax extended headers carry long names.
fn write_tar(path: &Path, items: &[Item], gzip: bool) -> Result {
    let file = File::create(path).map_err(|e| io_err(path, &e))?;
    let mut sink: Box<dyn Write> = if gzip {
        Box::new(GzEncoder::new(BufWriter::new(file), Compression::default()))
    } else {
        Box::new(BufWriter::new(file))
    };
    let mtime = unix_now();
    for item in items {
        let name = entry_name(item);
        let (shown, prefix) = match split_prefix(&name) {
            Some((prefix, base)) => (base, prefix),
            None if name.len() <= 100 => (name.clone(), String::new()),
            None => {
                return Err(err(format!(
                    "member `{name}` is longer than the 255 bytes a ustar header can hold"
                )));
            }
        };
        let (mode, flag) = if item.dir {
            (UNIX_DIR_MODE & 0o7777, '5')
        } else {
            (UNIX_FILE_MODE & 0o7777, '0')
        };
        header(
            &mut *sink,
            &shown,
            &prefix,
            item.bytes.len() as u64,
            flag,
            mode,
            mtime,
        )?;
        if !item.dir {
            sink.write_all(&item.bytes)
                .and_then(|()| pad_to(&mut *sink, item.bytes.len()))
                .map_err(|e| io_err(path, &e))?;
        }
    }
    sink.write_all(&[0u8; BLOCK * 2])
        .and_then(|()| sink.flush())
        .map_err(|e| io_err(path, &e))
}

/// `prefix`/`base` split for the ustar fields when the path exceeds 100 bytes.
fn split_prefix(name: &str) -> Option<(String, String)> {
    let bytes = name.as_bytes();
    if bytes.len() <= 100 {
        return None;
    }
    let cut = (1..bytes.len().saturating_sub(1))
        .filter(|i| bytes.get(*i) == Some(&b'/'))
        .find(|i| bytes.get(i + 1..).unwrap_or_default().len() <= 100 && *i <= 155)?;
    Some((name[..cut].to_owned(), name[cut + 1..].to_owned()))
}

/// One 512-byte ustar header with a computed checksum.
fn header(
    sink: &mut dyn Write,
    name: &str,
    prefix: &str,
    size: u64,
    flag: char,
    mode: u32,
    mtime: u64,
) -> Result {
    let mut h = [b' '; BLOCK];
    h[156] = flag as u8;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    text(&mut h, 0, 100, name)?;
    text(&mut h, 345, 155, prefix)?;
    for (at, width, value) in [
        (100usize, 8usize, format!("{mode:07o}")),
        (108, 8, "0".to_owned()),
        (116, 8, "0".to_owned()),
        (124, 12, format!("{size:011o}")),
        (136, 12, format!("{mtime:011o}")),
    ] {
        text(&mut h, at, width, &value)?;
    }
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().fold(0, |acc, b| acc + u32::from(*b));
    h[148..154].copy_from_slice(format!("{sum:06o}").as_bytes());
    h[154] = 0;
    h[155] = b' ';
    sink.write_all(&h)
        .map_err(|e| err(format!("cannot write the archive: {e}")))
}

/// Write `value` into the header field, NUL-padding the rest.
fn text(header: &mut [u8], at: usize, width: usize, value: &str) -> Result {
    let raw = value.as_bytes();
    if raw.len() >= width {
        return Err(err(format!(
            "`{value}` does not fit the {width}-byte tar header field"
        )));
    }
    let Some(field) = header.get_mut(at..at + width) else {
        return Err(err(format!("the tar header has no field at {at}")));
    };
    let (body, pad) = field.split_at_mut(raw.len());
    body.copy_from_slice(raw);
    pad.fill(0);
    Ok(())
}

fn pad_to(sink: &mut dyn Write, written: usize) -> std::io::Result<()> {
    let rest = written.next_multiple_of(BLOCK) - written;
    sink.write_all(&vec![0u8; rest])
}
