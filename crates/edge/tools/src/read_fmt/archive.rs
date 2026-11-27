//! `archive.zip`, `archive.tar`, `archive.tar.gz`: list entries and read one by inner path.

use super::{tar, zip};
use hrn_kernel::{HrnError, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Largest single archive member `read_file` will extract.
pub(super) const MEMBER_CAP: u64 = 64 * 1024 * 1024;

/// Which container format a file is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    Zip,
    Tar,
}

/// Archive file extensions (lowercase, longest first) and their format.
pub(super) const EXTENSIONS: &[(&str, Format)] = &[
    (".tar.gz", Format::Tar),
    (".tgz", Format::Tar),
    (".tar", Format::Tar),
    (".zip", Format::Zip),
    (".jar", Format::Zip),
    (".war", Format::Zip),
    (".apk", Format::Zip),
    (".whl", Format::Zip),
    (".vsix", Format::Zip),
    (".nupkg", Format::Zip),
    (".cbz", Format::Zip),
    (".xpi", Format::Zip),
];

/// Whether `head` (first bytes of the file) fits `format`.
pub(super) fn matches_magic(format: Format, head: &[u8]) -> bool {
    match format {
        Format::Zip => zip::has_magic(head),
        Format::Tar => tar::has_magic(head),
    }
}

/// One archive member, path normalized without a trailing slash.
pub(super) struct Member {
    pub(super) name: String,
    pub(super) size: u64,
    pub(super) dir: bool,
}

/// Normalize an inner path: `\` to `/`, no leading `/`, `.` segments dropped, `..` rejected.
pub(super) fn normalize(inner: &str) -> Result<String> {
    let mut parts = Vec::new();
    let unified = inner.replace('\\', "/");
    for seg in unified.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                return Err(HrnError::Tool(format!(
                    "archive path `{inner}` may not contain `..`"
                )));
            }
            s => parts.push(s),
        }
    }
    Ok(parts.join("/"))
}

/// Index every member of the archive.
pub(super) fn members(path: &Path, format: Format) -> Result<Vec<Member>> {
    match format {
        Format::Zip => {
            let file = File::open(path)
                .map_err(|e| HrnError::Tool(format!("cannot open archive: {e}")))?;
            let z = zip::Zip::open(BufReader::new(file))?;
            Ok(z.entries()
                .iter()
                .filter_map(|e| {
                    let dir = e.name.ends_with('/');
                    let name = normalize(&e.name).ok()?;
                    (!name.is_empty()).then_some(Member {
                        name,
                        size: e.size,
                        dir,
                    })
                })
                .collect())
        }
        Format::Tar => Ok(tar::index(path)?
            .into_iter()
            .map(|e| Member {
                name: e.name,
                size: e.size,
                dir: e.kind == tar::Kind::Dir,
            })
            .collect()),
    }
}

/// Bytes of the file member called `name`.
pub(super) fn read_member(path: &Path, format: Format, name: &str) -> Result<Vec<u8>> {
    match format {
        Format::Zip => {
            let file = File::open(path)
                .map_err(|e| HrnError::Tool(format!("cannot open archive: {e}")))?;
            let mut z = zip::Zip::open(BufReader::new(file))?;
            // Entries may be stored with `./` or backslashes; match on the normalized name.
            let idx = z
                .entries()
                .iter()
                .position(|e| normalize(&e.name).is_ok_and(|n| n == name))
                .or_else(|| z.find(name))
                .ok_or_else(|| HrnError::Tool(format!("no entry `{name}` in the archive")))?;
            z.read(idx, MEMBER_CAP)
        }
        Format::Tar => tar::read(path, name, MEMBER_CAP),
    }
}

/// Immediate children of directory `prefix` (`""` or `dir`): directories first, then files with
/// their size. `None` when nothing lives under `prefix`.
pub(super) fn children(members: &[Member], prefix: &str) -> Option<Vec<String>> {
    let lead = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    let mut found: BTreeMap<(bool, &str), u64> = BTreeMap::new();
    let mut any = prefix.is_empty();
    for m in members {
        let Some(rest) = m.name.strip_prefix(&lead) else {
            continue;
        };
        if rest.is_empty() {
            any = true;
            continue;
        }
        any = true;
        match rest.split_once('/') {
            Some((dir, _)) => {
                found.insert((false, dir), 0);
            }
            None if m.dir => {
                found.insert((false, rest), 0);
            }
            None => {
                found.insert((true, rest), m.size);
            }
        }
    }
    any.then(|| {
        found
            .into_iter()
            .map(|((is_file, name), size)| match (is_file, size) {
                (false, _) => format!("{name}/"),
                (true, 0) => name.to_owned(),
                (true, n) => format!("{name} ({})", human(n)),
            })
            .collect()
    })
}

/// `1234` -> `1.2 KB`.
pub(super) fn human(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut unit = 0usize;
    let mut tenths = n.saturating_mul(10);
    while tenths >= 10 * 1024 && unit + 1 < UNITS.len() {
        tenths /= 1024;
        unit += 1;
    }
    if unit == 0 {
        return format!("{n} B");
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    format!("{}.{} {name}", tenths / 10, tenths % 10)
}
