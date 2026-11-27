//! Parsing of `issue://` / `pr://` addresses: `[<host>/][<owner>/<repo>/]<N>[/diff[/<i>|/all]]`
//! plus an optional `?key=value&…` query.

use hrn_kernel::{HrnError, Result};

/// What a PR diff address selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffSel {
    /// `pr://N/diff`: the changed-file listing.
    Files,
    /// `pr://N/diff/all`: the whole unified diff.
    All,
    /// `pr://N/diff/<i>`: one file, 1-indexed.
    File(usize),
}

/// A parsed address.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    /// `[host/]owner/repo`; `None` is the checkout's own repository.
    pub(crate) repo: Option<String>,
    /// The issue / PR number; `None` browses a list.
    pub(crate) number: Option<u64>,
    pub(crate) diff: Option<DiffSel>,
    pub(crate) query: Vec<(String, String)>,
}

impl Target {
    /// The value of query key `key`.
    pub(crate) fn param(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

fn bad(scheme: &str, rest: &str, why: &str) -> HrnError {
    HrnError::Tool(format!("`{scheme}://{rest}`: {why}"))
}

/// Parse `rest` (everything after `<scheme>://`). `diffs` allows the `/diff` sub-path (PRs).
pub(crate) fn parse(scheme: &str, rest: &str, diffs: bool) -> Result<Target> {
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let number_at = segs
        .iter()
        .position(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    let repo_segs = number_at.unwrap_or(segs.len());
    let repo = match repo_segs {
        0 => None,
        2 | 3 => Some(
            segs.iter()
                .take(repo_segs)
                .copied()
                .collect::<Vec<_>>()
                .join("/"),
        ),
        _ => {
            return Err(bad(
                scheme,
                rest,
                "expected `[<host>/]<owner>/<repo>[/<N>]` or `<N>`",
            ));
        }
    };
    let mut tail = segs.iter().skip(repo_segs);
    let number = tail
        .next()
        .map(|n| {
            n.parse::<u64>()
                .map_err(|_| bad(scheme, rest, "number out of range"))
        })
        .transpose()?;
    let diff = match (tail.next().copied(), tail.next().copied(), tail.next()) {
        (None, ..) => None,
        (Some("diff"), None, None) if diffs => Some(DiffSel::Files),
        (Some("diff"), Some("all"), None) if diffs => Some(DiffSel::All),
        (Some("diff"), Some(i), None) if diffs => match i.parse::<usize>() {
            Ok(i) if i >= 1 => Some(DiffSel::File(i)),
            _ => return Err(bad(scheme, rest, "the file index is 1-based")),
        },
        _ => return Err(bad(scheme, rest, "unexpected path after the number")),
    };
    Ok(Target {
        repo,
        number,
        diff,
        query: parse_query(query),
    })
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect()
}

/// `+` and `%XX` decoding (invalid escapes stay literal).
fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let escaped = (b == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (b, escaped) {
            (_, Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b'+', None) => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(rest: &str) -> Target {
        parse("pr", rest, true).expect("parse")
    }

    #[test]
    fn forms() {
        let t = pr("");
        assert_eq!((t.repo, t.number), (None, None));
        let t = pr("12");
        assert_eq!((t.repo, t.number, t.diff), (None, Some(12), None));
        let t = pr("cli/cli");
        assert_eq!((t.repo.as_deref(), t.number), (Some("cli/cli"), None));
        let t = pr("cli/cli/7/diff/3");
        assert_eq!(t.repo.as_deref(), Some("cli/cli"));
        assert_eq!((t.number, t.diff), (Some(7), Some(DiffSel::File(3))));
        let t = pr("ghe.example.com/o/r/7/diff");
        assert_eq!(t.repo.as_deref(), Some("ghe.example.com/o/r"));
        assert_eq!(t.diff, Some(DiffSel::Files));
        assert_eq!(pr("7/diff/all").diff, Some(DiffSel::All));
    }

    #[test]
    fn rejects() {
        for rest in [
            "x",
            "a/b/c/d/1",
            "1/diff/0",
            "1/diff/x",
            "1/files",
            "1/diff/2/3",
        ] {
            assert!(parse("pr", rest, true).is_err(), "{rest}");
        }
        assert!(parse("issue", "1/diff", false).is_err());
    }

    #[test]
    fn query_is_decoded() {
        let t = pr("cli/cli?label=good+first%20issue&state=closed&limit=5");
        assert_eq!(t.param("label"), Some("good first issue"));
        assert_eq!(t.param("state"), Some("closed"));
        assert_eq!(t.param("limit"), Some("5"));
    }
}
