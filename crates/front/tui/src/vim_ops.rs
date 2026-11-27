//! Buffer editing operations for [`crate::vim`]: operators (`d`, `y`, `c`, `>`, `<`),
//! linewise forms (`dd`, `yy`, `cc`, `>>`, `<<`), single commands (`x`, `D`, `C`, `r`, `J`,
//! `p`, `P`, `o`, `O`), and Visual selections.

use crate::input::Editor;
use crate::vim::{ClipboardIo, Register, VimMode};
use crate::vim_motion::{
    CharFind, MotionResult, clamp_normal, expand_range_for_placeholders, find_char,
    first_non_blank, goto_last_line, goto_line, line_bounds, match_bracket, move_left,
    move_paragraph, move_right, move_vertical, normal_line_end, placeholder_at, word_backward,
    word_end, word_forward,
};

/// Operator awaiting a motion, doubled key, or text object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operator {
    Delete,
    Yank,
    Change,
    Indent,
    Dedent,
}

/// Apply `op` over `[start, end)` (`linewise` deletes/yanks full lines). Returns the resulting
/// [`VimMode`] and whether the buffer was modified.
pub(crate) fn apply_operator(
    editor: &mut Editor,
    op: Operator,
    start: usize,
    end: usize,
    linewise: bool,
    register: &mut Register,
    clipboard: &mut dyn ClipboardIo,
) -> (VimMode, bool) {
    if linewise {
        return apply_linewise(editor, op, start, end, register, clipboard);
    }
    let chars = editor.chars();
    let (lo, hi) = expand_range_for_placeholders(chars, start, end);
    let text: String = chars.get(lo..hi).unwrap_or_default().iter().collect();
    match op {
        Operator::Yank => {
            *register = Register {
                text: text.clone(),
                linewise: false,
            };
            let _ = clipboard.write_text(&text);
            let cur = clamp_normal(editor.chars(), lo);
            editor.set_cursor(cur);
            (VimMode::Normal, false)
        }
        Operator::Delete => {
            *register = Register {
                text,
                linewise: false,
            };
            if lo < hi {
                editor.splice_chars(lo..hi, &[]);
            }
            let cur = clamp_normal(editor.chars(), lo);
            editor.set_cursor(cur);
            (VimMode::Normal, lo < hi)
        }
        Operator::Change => {
            *register = Register {
                text,
                linewise: false,
            };
            if lo < hi {
                editor.splice_chars(lo..hi, &[]);
            }
            editor.set_cursor(lo.min(editor.chars().len()));
            (VimMode::Insert, true)
        }
        Operator::Indent => {
            indent_lines(editor, lo, hi.saturating_sub(1).max(lo), true);
            (VimMode::Normal, true)
        }
        Operator::Dedent => {
            indent_lines(editor, lo, hi.saturating_sub(1).max(lo), false);
            (VimMode::Normal, true)
        }
    }
}

/// Compute the `[start, end]` bounds spanning `count` lines starting at `cursor`.
pub(crate) fn count_lines_span(chars: &[char], cursor: usize, count: usize) -> (usize, usize) {
    let (first_start, mut last_end) = line_bounds(chars, cursor);
    for _ in 1..count.max(1) {
        if last_end >= chars.len() {
            break;
        }
        let (_, next_end) = line_bounds(chars, last_end + 1);
        last_end = next_end;
    }
    (first_start, last_end)
}

fn apply_linewise(
    editor: &mut Editor,
    op: Operator,
    raw_start: usize,
    raw_end: usize,
    register: &mut Register,
    clipboard: &mut dyn ClipboardIo,
) -> (VimMode, bool) {
    let (a, b) = if raw_start <= raw_end {
        (raw_start, raw_end)
    } else {
        (raw_end, raw_start)
    };
    let chars = editor.chars();
    let (first_start, _) = line_bounds(chars, a);
    let (_, last_end) = line_bounds(chars, b.min(chars.len()));
    let yanked: String = chars
        .get(first_start..last_end)
        .unwrap_or_default()
        .iter()
        .collect();
    match op {
        Operator::Yank => {
            *register = Register {
                text: yanked.clone(),
                linewise: true,
            };
            let _ = clipboard.write_text(&yanked);
            let cur = first_non_blank(editor.chars(), first_start);
            editor.set_cursor(cur);
            (VimMode::Normal, false)
        }
        Operator::Delete => {
            *register = Register {
                text: yanked,
                linewise: true,
            };
            let (del_start, del_end) = line_delete_range(editor.chars(), first_start, last_end);
            editor.splice_chars(del_start..del_end, &[]);
            let cur = if editor.chars().is_empty() {
                0
            } else {
                first_non_blank(
                    editor.chars(),
                    del_start.min(editor.chars().len().saturating_sub(1)),
                )
            };
            editor.set_cursor(cur);
            (VimMode::Normal, true)
        }
        Operator::Change => {
            *register = Register {
                text: yanked,
                linewise: true,
            };
            editor.splice_chars(first_start..last_end, &[]);
            editor.set_cursor(first_start.min(editor.chars().len()));
            (VimMode::Insert, true)
        }
        Operator::Indent => {
            indent_lines(editor, first_start, last_end, true);
            (VimMode::Normal, true)
        }
        Operator::Dedent => {
            indent_lines(editor, first_start, last_end, false);
            (VimMode::Normal, true)
        }
    }
}

fn line_delete_range(chars: &[char], first_start: usize, last_end: usize) -> (usize, usize) {
    if last_end < chars.len() {
        // Consume the trailing '\n'.
        (first_start, last_end + 1)
    } else if first_start > 0 {
        // Last line(s) of a multi-line buffer: consume the leading '\n'.
        (first_start - 1, last_end)
    } else {
        (first_start, last_end)
    }
}

fn indent_lines(editor: &mut Editor, first_pos: usize, last_pos: usize, indent: bool) {
    let chars = editor.chars();
    let (first_start, _) = line_bounds(chars, first_pos);
    let (_, last_end) = line_bounds(chars, last_pos.min(chars.len()));
    let mut line_starts = Vec::new();
    let mut cur = first_start;
    loop {
        line_starts.push(cur);
        let (_, end) = line_bounds(chars, cur);
        if end >= last_end || end >= chars.len() {
            break;
        }
        cur = end + 1;
    }
    // Mutate from bottom to top so earlier indices stay valid.
    for start in line_starts.into_iter().rev() {
        if indent {
            editor.splice_chars(start..start, &[' ', ' ']);
        } else {
            let mut drop_n = 0usize;
            for offset in 0..2 {
                if editor.chars().get(start + offset) == Some(&' ') {
                    drop_n += 1;
                } else {
                    break;
                }
            }
            if drop_n > 0 {
                editor.splice_chars(start..start + drop_n, &[]);
            }
        }
    }
    let landed = first_non_blank(editor.chars(), first_start.min(editor.chars().len()));
    editor.set_cursor(landed);
}

/// `x`: delete `count` characters under cursor on the current line (atomic over placeholders).
pub(crate) fn delete_chars(editor: &mut Editor, count: usize, register: &mut Register) -> bool {
    let chars = editor.chars();
    if chars.is_empty() {
        return false;
    }
    let pos = editor.cursor();
    let (_, line_end) = line_bounds(chars, pos);
    if pos >= line_end {
        return false;
    }
    let mut end = pos;
    for _ in 0..count.max(1) {
        if end >= line_end {
            break;
        }
        if let Some(p) = placeholder_at(chars, end) {
            end = p.end;
        } else {
            end += 1;
        }
    }
    let (lo, hi) = expand_range_for_placeholders(chars, pos, end);
    if lo >= hi {
        return false;
    }
    let text: String = chars.get(lo..hi).unwrap_or_default().iter().collect();
    *register = Register {
        text,
        linewise: false,
    };
    editor.splice_chars(lo..hi, &[]);
    let next = clamp_normal(editor.chars(), lo);
    editor.set_cursor(next);
    true
}

/// `r<ch>`: replace `count` chars under cursor with `ch`; refuses to split atomic placeholders.
pub(crate) fn replace_chars(editor: &mut Editor, ch: char, count: usize) -> bool {
    let chars = editor.chars();
    let pos = editor.cursor();
    let (_, line_end) = line_bounds(chars, pos);
    let n = count.max(1);
    if pos + n > line_end {
        return false;
    }
    for i in pos..pos + n {
        if placeholder_at(chars, i).is_some() {
            return false;
        }
    }
    let replacement = vec![ch; n];
    editor.splice_chars(pos..pos + n, &replacement);
    editor.set_cursor(pos + n - 1);
    true
}

/// `J`: join `count` following lines onto the current line.
pub(crate) fn join_lines(editor: &mut Editor, count: usize) -> bool {
    let joins = if count <= 1 { 1 } else { count - 1 };
    let mut changed = false;
    for _ in 0..joins {
        let chars = editor.chars();
        let (_, end) = line_bounds(chars, editor.cursor());
        if end >= chars.len() {
            break;
        }
        let next_start = end + 1;
        let mut trim_end = next_start;
        while chars
            .get(trim_end)
            .is_some_and(|c| *c != '\n' && c.is_whitespace())
        {
            trim_end += 1;
        }
        let need_space = end > 0
            && chars
                .get(end - 1)
                .is_some_and(|c| !c.is_whitespace() && *c != '\n')
            && chars.get(trim_end).is_some_and(|c| *c != '\n');
        let sep: &[char] = if need_space { &[' '] } else { &[] };
        editor.splice_chars(end..trim_end, sep);
        editor.set_cursor(clamp_normal(editor.chars(), end));
        changed = true;
    }
    changed
}

/// `p` (`after = true`) / `P` (`after = false`): paste register contents `count` times.
pub(crate) fn paste(editor: &mut Editor, register: &Register, after: bool, count: usize) -> bool {
    if register.text.is_empty() && !register.linewise {
        return false;
    }
    let reps = count.max(1);
    if register.linewise {
        return paste_linewise(editor, &register.text, after, reps);
    }
    let mut payload: Vec<char> = Vec::new();
    for _ in 0..reps {
        payload.extend(register.text.chars());
    }
    let chars = editor.chars();
    let pos = editor.cursor();
    let (_, line_end) = line_bounds(chars, pos);
    let insert_at = if after {
        if pos >= line_end {
            line_end
        } else if let Some(p) = placeholder_at(chars, pos) {
            p.end
        } else {
            (pos + 1).min(line_end)
        }
    } else {
        pos.min(chars.len())
    };
    let added = payload.len();
    editor.splice_chars(insert_at..insert_at, &payload);
    let cur = clamp_normal(editor.chars(), insert_at + added.saturating_sub(1));
    editor.set_cursor(cur);
    true
}

fn paste_linewise(editor: &mut Editor, text: &str, after: bool, reps: usize) -> bool {
    let mut lines = Vec::with_capacity(reps);
    for _ in 0..reps {
        lines.push(text);
    }
    let block = lines.join("\n");
    let block_chars: Vec<char> = block.chars().collect();
    if editor.chars().is_empty() {
        editor.splice_chars(0..0, &block_chars);
        let cur = first_non_blank(editor.chars(), 0);
        editor.set_cursor(cur);
        return true;
    }
    let (start, end) = line_bounds(editor.chars(), editor.cursor());
    if after {
        let mut insert = Vec::with_capacity(block_chars.len() + 1);
        insert.push('\n');
        insert.extend(block_chars);
        editor.splice_chars(end..end, &insert);
        let cur = first_non_blank(editor.chars(), end + 1);
        editor.set_cursor(cur);
    } else {
        let mut insert = Vec::with_capacity(block_chars.len() + 1);
        insert.extend(block_chars);
        insert.push('\n');
        editor.splice_chars(start..start, &insert);
        let cur = first_non_blank(editor.chars(), start);
        editor.set_cursor(cur);
    }
    true
}

/// `o` (`below = true`) / `O` (`below = false`): open a new line and enter Insert mode.
pub(crate) fn open_line(editor: &mut Editor, below: bool) {
    let (start, end) = line_bounds(editor.chars(), editor.cursor());
    if below {
        editor.splice_chars(end..end, &['\n']);
        editor.set_cursor(end + 1);
    } else {
        editor.splice_chars(start..start, &['\n']);
        editor.set_cursor(start);
    }
}

/// Resolve a single-character Vim motion (`h j k l 0 ^ $ w b e W B E ; , G % { }`).
pub(crate) fn resolve_char_motion(
    chars: &[char],
    pos: usize,
    c: char,
    count: usize,
    explicit_count: bool,
    for_operator: bool,
    last_find: Option<CharFind>,
) -> Option<MotionResult> {
    let m = |target, inclusive, linewise| {
        Some(MotionResult {
            target,
            inclusive,
            linewise,
        })
    };
    match c {
        'h' => m(move_left(chars, pos, count), false, false),
        'l' => m(move_right(chars, pos, count, for_operator), false, false),
        'j' => m(move_vertical(chars, pos, true, count), true, for_operator),
        'k' => m(move_vertical(chars, pos, false, count), true, for_operator),
        '0' => m(line_bounds(chars, pos).0, false, false),
        '^' => m(first_non_blank(chars, pos), false, false),
        '$' => {
            let base = if count > 1 {
                move_vertical(chars, pos, true, count - 1)
            } else {
                pos
            };
            let target = if for_operator {
                line_bounds(chars, base).1
            } else {
                normal_line_end(chars, base)
            };
            m(target, !for_operator, false)
        }
        'w' | 'W' => {
            let mut target = word_forward(chars, pos, count, c == 'W');
            if for_operator {
                let (_, line_end) = line_bounds(chars, pos);
                if target > line_end && pos < line_end {
                    target = line_end;
                }
            }
            m(target, false, false)
        }
        'b' | 'B' => m(word_backward(chars, pos, count, c == 'B'), false, false),
        'e' | 'E' => m(word_end(chars, pos, count, c == 'E'), true, false),
        ';' | ',' => {
            let mut find = last_find?;
            if c == ',' {
                find.forward = !find.forward;
            }
            m(find_char(chars, pos, find, count)?, find.forward, false)
        }
        'G' => m(
            if explicit_count {
                goto_line(chars, count)
            } else {
                goto_last_line(chars)
            },
            true,
            true,
        ),
        '%' => m(match_bracket(chars, pos)?, true, false),
        '{' => m(move_paragraph(chars, pos, false, count), false, false),
        '}' => m(move_paragraph(chars, pos, true, count), false, false),
        _ => None,
    }
}
