//! Parser of the hashline patch language: `[PATH#TAG]` sections holding `PUT` / `CUT` / `REM` /
//! `MV` operations and `+TEXT` body rows.

/// The lines an operation addresses (1-based, original snapshot numbering).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    /// `N.=M`: inclusive range.
    Range(usize, usize),
    /// `N*`: the syntactic block opening on line `N`.
    Block(usize),
}

/// A gap between lines where rows are inserted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gap {
    /// `<N`: immediately before line `N`.
    Before(usize),
    /// `>N`: immediately after line `N`.
    After(usize),
    /// `>N*`: after the block opening on line `N`.
    AfterBlock(usize),
    /// `>$`: at the end of the file.
    End,
}

/// Where the new rows of a `PUT` come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// `+TEXT` rows (marker stripped).
    Body(Vec<String>),
    /// A register paste: `None` is the anonymous register.
    Register(Option<String>),
}

/// One edit operation of a section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Op {
    /// `PUT N.=M:` / `PUT N*:` / `PUT N.=M @name` / `PUT N* @name`.
    Replace { target: Target, src: Source },
    /// `PUT <N:` / `PUT >N:` / `PUT >N*:` / `PUT >$:` and their paste forms.
    Insert { gap: Gap, src: Source },
    /// `CUT N.=M [@name]` / `CUT N* [@name]`.
    Cut {
        target: Target,
        name: Option<String>,
    },
}

impl Op {
    /// Line numbers this operation points at (for stale-tag context).
    pub(super) fn anchors(&self) -> Vec<usize> {
        match self {
            Self::Replace { target, .. } | Self::Cut { target, .. } => match *target {
                Target::Range(a, b) => vec![a, b],
                Target::Block(n) => vec![n],
            },
            Self::Insert { gap, .. } => match *gap {
                Gap::Before(n) | Gap::After(n) | Gap::AfterBlock(n) => vec![n],
                Gap::End => Vec::new(),
            },
        }
    }
}

/// One `[PATH#TAG]` section.
#[derive(Debug)]
pub(super) struct Section {
    pub(super) path: String,
    /// The tag as written (may be a placeholder).
    pub(super) tag: String,
    pub(super) ops: Vec<Op>,
    /// `REM`: delete the file.
    pub(super) remove: bool,
    /// `MV DEST`.
    pub(super) mv: Option<String>,
}

fn is_register_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn number(raw: &str, what: &str) -> Result<usize, String> {
    match raw.trim().parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!(
            "{what} must be a line number >= 1, got `{}`",
            raw.trim()
        )),
    }
}

fn parse_target(spec: &str) -> Result<Target, String> {
    let spec = spec.trim();
    if let Some(n) = spec.strip_suffix('*') {
        return Ok(Target::Block(number(n, "block anchor")?));
    }
    if let Some((a, b)) = spec.split_once(".=") {
        let (a, b) = (number(a, "range start")?, number(b, "range end")?);
        if b < a {
            return Err(format!("range `{spec}` is reversed; write `{b}.={a}`"));
        }
        return Ok(Target::Range(a, b));
    }
    Err(format!(
        "expected a range `N.=M` (use `N.=N` for one line) or a block `N*`, got `{spec}`"
    ))
}

fn parse_gap(spec: &str) -> Result<Option<Gap>, String> {
    let spec = spec.trim();
    if spec == ">$" {
        return Ok(Some(Gap::End));
    }
    if let Some(n) = spec.strip_prefix('<') {
        return Ok(Some(Gap::Before(number(n, "`<N`")?)));
    }
    if let Some(n) = spec.strip_prefix('>') {
        return Ok(Some(match n.strip_suffix('*') {
            Some(n) => Gap::AfterBlock(number(n, "`>N*`")?),
            None => Gap::After(number(n, "`>N`")?),
        }));
    }
    Ok(None)
}

/// Split a trailing ` @name` register reference off `rest`.
fn split_register(rest: &str) -> Result<(&str, Option<String>), String> {
    match rest.rsplit_once('@') {
        Some((spec, name)) if spec.is_empty() || spec.ends_with(' ') => {
            let name = name.trim();
            if !is_register_name(name) {
                return Err(format!(
                    "register name `{name}` must be ASCII letters, digits, `_` or `-`"
                ));
            }
            Ok((spec.trim_end(), Some(name.to_owned())))
        }
        _ => Ok((rest, None)),
    }
}

/// What a `PUT` header asks for, before its body is attached.
enum Put {
    /// Body-bearing: the rows follow.
    Body(Op),
    /// Register paste: complete.
    Paste(Op),
}

fn parse_put(rest: &str) -> Result<Put, String> {
    let rest = rest.trim();
    let (spec, has_body) = match rest.strip_suffix(':') {
        Some(spec) => (spec, true),
        None => (rest, false),
    };
    let (spec, register) = if has_body {
        (spec, None)
    } else {
        split_register(spec)?
    };
    let src = if has_body {
        Source::Body(Vec::new())
    } else {
        Source::Register(register.clone())
    };
    let op = if let Some(gap) = parse_gap(spec)? {
        Op::Insert { gap, src }
    } else {
        if !has_body && register.is_none() {
            return Err(format!(
                "`PUT {spec}` replaces a range or block with a register; name it (`PUT {spec} @name`) \
                 or give body rows (`PUT {spec}:`)"
            ));
        }
        Op::Replace {
            target: parse_target(spec)?,
            src,
        }
    };
    Ok(if has_body {
        Put::Body(op)
    } else {
        Put::Paste(op)
    })
}

fn parse_cut(rest: &str) -> Result<Op, String> {
    let (spec, name) = split_register(rest.trim())?;
    Ok(Op::Cut {
        target: parse_target(spec)?,
        name,
    })
}

fn parse_header(line: &str) -> Option<(String, String)> {
    let inner = line.trim().strip_prefix('[')?.strip_suffix(']')?;
    let (path, tag) = inner.rsplit_once('#')?;
    let path = path.trim();
    (!path.is_empty()).then(|| (path.to_owned(), tag.trim().to_owned()))
}

/// Paths of the `[PATH#TAG]` headers of a payload, in order (no validation).
pub(super) fn headers(input: &str) -> Vec<String> {
    input
        .lines()
        .filter_map(|l| parse_header(l).map(|(path, _)| path))
        .collect()
}

fn unquote(raw: &str) -> &str {
    let raw = raw.trim();
    for q in ['"', '\''] {
        if let Some(inner) = raw.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    raw
}

fn err_at(line_no: usize, line: &str, msg: &str) -> String {
    format!("line {line_no} `{}`: {msg}", line.trim_end())
}

/// Attach the finished body-bearing op to the section it was written under.
fn flush(pending: &mut Option<Op>, sections: &mut [Section]) {
    if let (Some(op), Some(section)) = (pending.take(), sections.last_mut()) {
        section.ops.push(op);
    }
}

/// Parse a whole payload (optionally wrapped in `*** Begin Patch` / `*** End Patch`).
pub(super) fn parse(input: &str) -> Result<Vec<Section>, String> {
    let mut sections: Vec<Section> = Vec::new();
    // The `PUT` header whose `+` rows are being collected.
    let mut pending: Option<Op> = None;
    for (idx, raw) in input.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r');
        if let Some(text) = line.strip_prefix('+') {
            match pending.as_mut() {
                Some(
                    Op::Replace {
                        src: Source::Body(rows),
                        ..
                    }
                    | Op::Insert {
                        src: Source::Body(rows),
                        ..
                    },
                ) => rows.push(text.to_owned()),
                _ => {
                    return Err(err_at(
                        line_no,
                        line,
                        "body row without a body-bearing `PUT ...:` header above it",
                    ));
                }
            }
            continue;
        }
        flush(&mut pending, &mut sections);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "*** Begin Patch" || trimmed == "*** End Patch" {
            continue;
        }
        if let Some((path, tag)) = parse_header(trimmed) {
            sections.push(Section {
                path,
                tag,
                ops: Vec::new(),
                remove: false,
                mv: None,
            });
            continue;
        }
        let Some(section) = sections.last_mut() else {
            return Err(err_at(
                line_no,
                line,
                "expected a `[PATH#TAG]` section header first",
            ));
        };
        let (word, rest) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
        match word {
            "PUT" => match parse_put(rest).map_err(|e| err_at(line_no, line, &e))? {
                Put::Body(op) => pending = Some(op),
                Put::Paste(op) => section.ops.push(op),
            },
            "CUT" => section
                .ops
                .push(parse_cut(rest).map_err(|e| err_at(line_no, line, &e))?),
            "REM" if rest.trim().is_empty() => section.remove = true,
            "MV" if !rest.trim().is_empty() => section.mv = Some(unquote(rest).to_owned()),
            _ => {
                let hint = if trimmed.starts_with('-') || trimmed.starts_with("@@") {
                    "unified-diff rows are not hashline: write the final content as `+TEXT` \
                     rows under `PUT N.=M:`"
                } else {
                    "expected `PUT`, `CUT`, `REM`, `MV DEST` or a `[PATH#TAG]` header; every \
                     body row starts with `+`"
                };
                return Err(err_at(
                    line_no,
                    line,
                    &format!("unrecognized line ({hint})"),
                ));
            }
        }
    }
    flush(&mut pending, &mut sections);
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(rows: &[&str]) -> Source {
        Source::Body(rows.iter().map(|r| (*r).to_owned()).collect())
    }

    #[test]
    fn parses_every_operation_form() {
        let input = "*** Begin Patch\n[a.rs#1A2B]\nPUT 2.=3:\n+x\n+\n++y\nPUT >$:\n+tail\n\
                     PUT <1:\n+head\nPUT >4*:\n+z\nCUT 5* @fn\nCUT 1.=2\nPUT <9 @fn\nPUT >9\n\
                     PUT 7* @fn\nPUT 1.=2 @fn\nMV \"new dir/b.rs\"\n[c.rs#FFFF]\nREM\n*** End Patch";
        let sections = parse(input).unwrap();
        assert_eq!(sections.len(), 2);
        let a = sections.first().unwrap();
        assert_eq!((a.path.as_str(), a.tag.as_str()), ("a.rs", "1A2B"));
        assert_eq!(a.mv.as_deref(), Some("new dir/b.rs"));
        assert_eq!(
            a.ops,
            vec![
                Op::Replace {
                    target: Target::Range(2, 3),
                    src: body(&["x", "", "+y"])
                },
                Op::Insert {
                    gap: Gap::End,
                    src: body(&["tail"])
                },
                Op::Insert {
                    gap: Gap::Before(1),
                    src: body(&["head"])
                },
                Op::Insert {
                    gap: Gap::AfterBlock(4),
                    src: body(&["z"])
                },
                Op::Cut {
                    target: Target::Block(5),
                    name: Some("fn".into())
                },
                Op::Cut {
                    target: Target::Range(1, 2),
                    name: None
                },
                Op::Insert {
                    gap: Gap::Before(9),
                    src: Source::Register(Some("fn".into()))
                },
                Op::Insert {
                    gap: Gap::After(9),
                    src: Source::Register(None)
                },
                Op::Replace {
                    target: Target::Block(7),
                    src: Source::Register(Some("fn".into()))
                },
                Op::Replace {
                    target: Target::Range(1, 2),
                    src: Source::Register(Some("fn".into()))
                },
            ]
        );
        assert!(sections.get(1).unwrap().remove);
    }

    #[test]
    fn rejects_malformed_input_with_the_line_number() {
        for (input, needle) in [
            ("PUT 1.=1:\n+x", "section header"),
            ("[a#1]\n+x", "body row"),
            ("[a#1]\n-old\n+new", "unified-diff"),
            ("[a#1]\nPUT 3.=2:\n+x", "reversed"),
            ("[a#1]\nPUT 0.=2:\n+x", ">= 1"),
            ("[a#1]\nPUT 4.=4", "name it"),
            ("[a#1]\nCUT 4", "expected a range"),
            ("[a#1]\nCUT 1* @bad name", "register name"),
        ] {
            let err = parse(input).unwrap_err();
            assert!(err.contains(needle), "{input:?}: {err}");
        }
    }
}
