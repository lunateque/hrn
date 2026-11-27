//! Native-executable metadata for bare `read`: PE (Windows), ELF and Mach-O headers decoded by
//! hand, so a read of a binary says what the file is instead of "binary file, not shown".
//! The id → name tables come from `exe-formats.tsv` (file-format constants, embedded).

use crate::time::unix_utc;
use hrn_kernel::{HrnError, Result};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::LazyLock;

const TABLES: &str = include_str!("../exe-formats.tsv");
const HEADER_WINDOW: usize = 4096;
/// Below this an `MZ` prefix is a text file, not a DOS stub.
const MIN_EXE_BYTES: u64 = 1024;
const MAX_TABLE_BYTES: usize = 32 * 1024;
const MAX_LISTED: usize = 12;

/// One `group<TAB>id<TAB>name` row of the embedded tables.
static TABLES_BY_GROUP: LazyLock<HashMap<&'static str, Vec<(u64, &'static str)>>> =
    LazyLock::new(|| {
        let mut map: HashMap<&str, Vec<(u64, &str)>> = HashMap::new();
        for line in TABLES.lines() {
            if line.starts_with('#') {
                continue;
            }
            let mut fields = line.split('\t');
            let (Some(group), Some(id), Some(name)) = (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Ok(id) = u64::from_str_radix(id.trim_start_matches("0x"), 16) else {
                continue;
            };
            map.entry(group).or_default().push((id, name));
        }
        map
    });

fn name_of(group: &str, id: u64) -> String {
    TABLES_BY_GROUP
        .get(group)
        .and_then(|rows| rows.iter().find(|(v, _)| *v == id))
        .map_or_else(|| format!("unknown ({id:#x})"), |(_, n)| (*n).to_owned())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Pe,
    Elf,
    MachO,
}

fn detect(head: &[u8]) -> Option<Format> {
    if head.starts_with(b"MZ") {
        Some(Format::Pe)
    } else if head.starts_with(b"\x7fELF") {
        Some(Format::Elf)
    } else if u32_at(head, 0, true)
        .is_some_and(|m| matches!(m, 0xfeed_face | 0xfeed_facf | 0xcefa_edfe | 0xcffa_edfe))
    {
        Some(Format::MachO)
    } else {
        None
    }
}

fn u32_at(bytes: &[u8], at: usize, le: bool) -> Option<u32> {
    let w = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(if le {
        u32::from_le_bytes(w)
    } else {
        u32::from_be_bytes(w)
    })
}

/// Random-access, bounds-checked reads (a short read is `None`, never a panic).
struct Slice(File);

impl Slice {
    fn at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        if len == 0 || len > MAX_TABLE_BYTES {
            return None;
        }
        self.0.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0u8; len];
        self.0.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    fn u16(&mut self, offset: u64, le: bool) -> Option<u16> {
        let two: [u8; 2] = self.at(offset, 2)?.as_slice().try_into().ok()?;
        Some(if le {
            u16::from_le_bytes(two)
        } else {
            u16::from_be_bytes(two)
        })
    }

    fn u32(&mut self, offset: u64, le: bool) -> Option<u32> {
        u32_at(&self.at(offset, 4)?, 0, le)
    }

    fn u64(&mut self, offset: u64, le: bool) -> Option<u64> {
        let w = self.at(offset, 8)?.as_slice().try_into().ok()?;
        Some(if le {
            u64::from_le_bytes(w)
        } else {
            u64::from_be_bytes(w)
        })
    }

    /// NUL-terminated Latin-1 string at `offset` (`None` when it starts NUL or runs past EOF).
    fn cstr(&mut self, offset: u64) -> Option<String> {
        let chunk = self.at(offset, 96)?;
        let end = chunk.iter().position(|b| *b == 0)?;
        let text: String = chunk
            .get(..end)
            .unwrap_or_default()
            .iter()
            .map(|b| char::from(*b))
            .collect();
        (!text.is_empty()).then_some(text)
    }
}

/// Metadata of the executable `path`, or `None` when it is not one. A file whose `MZ` / `ELF`
/// prefix does not hold up as a header is reported as `None` (it reads as plain bytes): the
/// signature alone is not enough to claim an executable.
///
/// # Errors
/// The file cannot be opened or read.
pub(super) fn describe(path: &Path, size: u64) -> Result<Option<String>> {
    if size < MIN_EXE_BYTES {
        return Ok(None);
    }
    let mut head = vec![0u8; HEADER_WINDOW];
    let read = {
        let mut file = File::open(path).map_err(|e| io_err(&e))?;
        file.read(&mut head).map_err(|e| io_err(&e))?
    };
    head.truncate(read);
    let body = match detect(&head) {
        Some(Format::Pe) => pe(&mut open(path)?, &head),
        Some(Format::Elf) => elf(&mut open(path)?, &head),
        Some(Format::MachO) => macho(&mut open(path)?, &head),
        None => return Ok(None),
    };
    Ok(body.map(|body| format!("{body}\nsize: {} bytes", fmt_int(size))))
}

fn open(path: &Path) -> Result<Slice> {
    File::open(path)
        .map(Slice)
        .map_err(|e| HrnError::Tool(format!("cannot read the executable: {e}")))
}

fn io_err(e: &std::io::Error) -> HrnError {
    HrnError::Tool(format!("cannot read the executable: {e}"))
}

fn fmt_int(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(ch);
    }
    out
}

fn list(items: &[String]) -> String {
    let shown = items.iter().take(MAX_LISTED).cloned().collect::<Vec<_>>();
    if items.len() > MAX_LISTED {
        format!("{}, … ({} total)", shown.join(", "), items.len())
    } else {
        shown.join(", ")
    }
}

// --------------------------------------------------------------------------- PE

/// `e_lfanew` → `PE\0\0` → COFF header → optional header → data directories → section table.
fn pe(src: &mut Slice, head: &[u8]) -> Option<String> {
    let lfanew = u32_at(head, 0x3c, true)?;
    let coff = u64::from(lfanew).checked_add(4)?;
    if u32_at(head, usize::try_from(lfanew).ok()?, true) != Some(0x0000_4550) {
        return None;
    }
    let machine = src.u16(coff, true)?;
    let sections = src.u16(coff + 2, true)?;
    let timestamp = src.u32(coff + 4, true).unwrap_or(0);
    let opt_size = src.u16(coff + 16, true)?;
    let characteristics = src.u16(coff + 18, true)?;
    let opt = coff + 20;
    let magic = u64::from(src.u32(opt, true)?) & 0xffff;
    let pe32 = magic == 0x10b;
    if !pe32 && magic != 0x20b {
        return None;
    }
    let entry = src.u32(opt + 16, true)?;
    let image_base = if pe32 {
        u64::from(src.u32(opt + 28, true)?)
    } else {
        src.u64(opt + 24, true)?
    };
    let (dd_off, dd_count) = if pe32 {
        (opt + 96, src.u32(opt + 92, true).unwrap_or(0))
    } else {
        (opt + 112, src.u32(opt + 108, true).unwrap_or(0))
    };
    let subsystem = src.u16(opt + if pe32 { 68 } else { 64 }, true).unwrap_or(0);
    let table = pe_sections(src, opt + u64::from(opt_size), u64::from(sections))?;

    let kind = if characteristics & 0x2000 != 0 {
        "DLL"
    } else if characteristics & 0x1 != 0 {
        "object file"
    } else {
        "executable"
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Windows PE {kind}: {} ({machine:#06x}), {} section(s)",
        name_of("pe-machine", u64::from(machine)),
        sections
    );
    let _ = writeln!(
        out,
        "subsystem: {}",
        name_of("pe-subsystem", u64::from(subsystem))
    );
    let _ = writeln!(
        out,
        "entry point: {:#x} (image base {image_base:#x})",
        image_base + u64::from(entry)
    );
    let _ = writeln!(
        out,
        "compiled: {}",
        if timestamp == 0 {
            "not stamped".to_owned()
        } else {
            unix_utc(timestamp)
        }
    );
    if !table.names.is_empty() {
        let _ = writeln!(out, "sections: {}", list(&table.names));
    }
    if let Some(rva) = pe_dir(src, dd_off, 1, dd_count)
        && let Some(imports) = pe_imports(src, &table, rva, pe32)
    {
        let _ = writeln!(out, "imports: {imports}");
    }
    if let Some(rva) = pe_dir(src, dd_off, 0, dd_count)
        && let Some(exports) = pe_exports(src, &table, rva)
    {
        let _ = writeln!(out, "exports: {exports}");
    }
    Some(out.trim_end().to_owned())
}

/// Import (1) / export (0) data directory RVA, `None` when absent.
fn pe_dir(src: &mut Slice, dd: u64, index: u32, count: u32) -> Option<u32> {
    if index >= count {
        return None;
    }
    let at = dd + u64::from(index) * 8;
    let rva = src.u32(at, true)?;
    (rva != 0 && src.u32(at + 4, true)? != 0).then_some(rva)
}

struct SecTable {
    names: Vec<String>,
    /// `(virtual address, virtual size, raw pointer, raw size)` per section.
    entries: Vec<(u32, u32, u32, u32)>,
}

fn pe_sections(src: &mut Slice, at: u64, count: u64) -> Option<SecTable> {
    let mut names = Vec::new();
    let mut entries = Vec::new();
    for i in 0..count.min(128) {
        let raw = src.at(at + i * 40, 40)?;
        let le = |o: usize| {
            raw.get(o..o + 4)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .map(u32::from_le_bytes)
        };
        let name = raw.get(..8).unwrap_or_default();
        let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
        names.push(String::from_utf8_lossy(name.get(..end).unwrap_or_default()).into_owned());
        entries.push((le(12)?, le(8)?, le(20)?, le(16)?));
    }
    (!entries.is_empty()).then_some(SecTable { names, entries })
}

/// Map an RVA to a file offset through the section table.
fn rva_to_offset(table: &SecTable, rva: u32) -> Option<u64> {
    table
        .entries
        .iter()
        .find_map(|(va, virt, raw_ptr, raw_size)| {
            let span = (*virt).max(*raw_size).max(1);
            (rva >= *va && rva < va.checked_add(span)?)
                .then_some(u64::from(*raw_ptr) + u64::from(rva - va))
        })
}

fn pe_imports(src: &mut Slice, table: &SecTable, rva: u32, pe32: bool) -> Option<String> {
    let mut dlls = Vec::new();
    let mut named = 0usize;
    for i in 0..256 {
        let base = rva_to_offset(table, rva)?.checked_add(i * 20)?;
        let original = src.u32(base, true)?;
        let name_rva = src.u32(base + 12, true)?;
        let first = src.u32(base + 16, true)?;
        if original == 0 && name_rva == 0 && first == 0 {
            break;
        }
        dlls.push(src.cstr(rva_to_offset(table, name_rva)?)?);
        let mut thunk = if original != 0 { original } else { first };
        if thunk == 0 {
            continue;
        }
        let (stride, flag) = if pe32 {
            (4u32, 0x8000_0000u64)
        } else {
            (8, 0x8000_0000_0000_0000)
        };
        for _ in 0..4096 {
            let at = rva_to_offset(table, thunk)?;
            let entry = if pe32 {
                u64::from(src.u32(at, true)?)
            } else {
                src.u64(at, true)?
            };
            if entry == 0 {
                break;
            }
            if entry & flag == 0 {
                named += 1;
            }
            thunk = thunk.checked_add(stride)?;
        }
    }
    (!dlls.is_empty()).then(|| {
        format!(
            "{named} function(s) from {} DLL(s): {}",
            dlls.len(),
            list(&dlls)
        )
    })
}

fn pe_exports(src: &mut Slice, table: &SecTable, rva: u32) -> Option<String> {
    let at = rva_to_offset(table, rva)?;
    let functions = src.u32(at + 20, true)?;
    let names = src.u32(at + 24, true)?;
    Some(format!("{functions} function(s), {names} exported by name"))
}

// -------------------------------------------------------------------------- ELF

fn elf(src: &mut Slice, head: &[u8]) -> Option<String> {
    let class64 = *head.get(4)? == 2;
    let le = *head.get(5)? == 1;
    let ty = src.u16(16, le)?;
    let machine = src.u16(18, le)?;
    let (entry, ph, sh, ph_num, sh_num, shstrx) = if class64 {
        (
            src.u64(24, le)?,
            src.u64(32, le)?,
            src.u64(40, le)?,
            u64::from(src.u16(56, le)?),
            u64::from(src.u16(60, le)?),
            src.u16(62, le)?,
        )
    } else {
        (
            u64::from(src.u32(24, le)?),
            u64::from(src.u32(28, le)?),
            u64::from(src.u32(32, le)?),
            u64::from(src.u16(44, le)?),
            u64::from(src.u16(48, le)?),
            src.u16(50, le)?,
        )
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ELF {}: {}, {}, {}",
        if class64 { "64-bit" } else { "32-bit" },
        name_of("elf-type", u64::from(ty)),
        name_of("elf-machine", u64::from(machine)),
        if le { "little-endian" } else { "big-endian" }
    );
    let _ = writeln!(out, "entry point: {entry:#x}");
    let _ = writeln!(
        out,
        "program headers: {ph_num} at {ph:#x}, section headers: {sh_num} at {sh:#x}"
    );
    let names = elf_section_names(src, class64, le, sh, sh_num, shstrx);
    if !names.is_empty() {
        let _ = write!(out, "sections: {}", list(&names));
    }
    Some(out.trim_end().to_owned())
}

/// Section names resolved through the section-header string table.
fn elf_section_names(
    src: &mut Slice,
    class64: bool,
    le: bool,
    at: u64,
    count: u64,
    string_index: u16,
) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let entry = if class64 { 64 } else { 40 };
    let link_at = if class64 { 40 } else { 26 };
    let Some(strtab) = src.u32(at + u64::from(string_index) * entry + link_at, le) else {
        return Vec::new();
    };
    (1..count.min(128))
        .map_while(|i| {
            let base = at + i * entry;
            let name_off = src.u32(base, le)?;
            Some(
                src.cstr(u64::from(strtab) + u64::from(name_off))
                    .unwrap_or_else(|| "(unnamed)".to_owned()),
            )
        })
        .collect()
}

// ------------------------------------------------------------------------ Mach-O

fn macho(src: &mut Slice, head: &[u8]) -> Option<String> {
    let magic = u32_at(head, 0, true)?;
    let (bits, le) = match magic {
        0xfeed_face => (32, true),
        0xfeed_facf => (64, true),
        0xcefa_edfe => (32, false),
        0xcffa_edfe => (64, false),
        _ => return None,
    };
    let cputype = src.u32(4, le)?;
    let filetype = src.u32(12, le)?;
    let ncmds = src.u32(16, le)?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Mach-O {bits}-bit {}: {} ({cputype:#x})",
        name_of("macho-type", u64::from(filetype)),
        name_of("macho-cpu", u64::from(cputype))
    );
    let _ = write!(out, "load commands: {ncmds}");
    Some(out)
}
