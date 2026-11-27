//! The inline renderer: no alternate screen. Finished transcript lines are written once into the
//! terminal's own scrollback and never touched again; only the *live region* below them (the
//! streaming message, running cards, the status band and the prompt) is repainted, row by row,
//! and only the rows whose bytes changed. The renderer tracks nothing but its own region: how
//! many rows it painted and where inside it the cursor rests, so it never needs the cursor's
//! absolute position and the terminal keeps its native scrollback, selection and copy.

use crate::theme;
use crate::vim::CursorShape;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use std::io::{self, Write};
use unicode_width::UnicodeWidthStr;

const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
/// Erase the screen and the scrollback, then home the cursor.
const CLEAR_ALL: &[u8] = b"\x1b[2J\x1b[3J\x1b[H";
const ERASE_BELOW: &[u8] = b"\x1b[J";
const ERASE_LINE_END: &[u8] = b"\x1b[K";
const RESET_PEN: &[u8] = b"\x1b[0m";
const MODIFIERS: [(Modifier, u8); 9] = [
    (Modifier::BOLD, 1),
    (Modifier::DIM, 2),
    (Modifier::ITALIC, 3),
    (Modifier::UNDERLINED, 4),
    (Modifier::SLOW_BLINK, 5),
    (Modifier::RAPID_BLINK, 6),
    (Modifier::REVERSED, 7),
    (Modifier::HIDDEN, 8),
    (Modifier::CROSSED_OUT, 9),
];

/// One frame: what to append to the scrollback and what the live region looks like now.
pub(crate) struct Frame<'a> {
    /// Erase the screen and scrollback first; everything is then written afresh.
    pub(crate) reset: bool,
    /// Lines that became final, written above the live region once.
    pub(crate) scrollback: &'a [Line<'static>],
    /// The live region, one buffer row per terminal row.
    pub(crate) live: &'a Buffer,
    /// Where the text cursor is inside the live region; `None` hides it.
    pub(crate) cursor: Option<Position>,
    /// DECSCUSR terminal cursor shape for this frame.
    pub(crate) cursor_shape: CursorShape,
}

/// What the terminal is showing of our region.
pub(crate) struct Inline<W: Write> {
    out: W,
    truecolor: bool,
    /// The bytes painted on each row of the live region.
    rows: Vec<Vec<u8>>,
    /// The row of the region the cursor rests on.
    at: usize,
    /// Where the text cursor was left, to skip frames that change nothing.
    cursor: Option<Position>,
    /// Last DECSCUSR cursor shape sent to the terminal.
    cursor_shape: CursorShape,
    /// Reused frame assembly buffer: a frame reaches the terminal in one write.
    frame: Vec<u8>,
}

impl<W: Write> Inline<W> {
    pub(crate) fn new(out: W, truecolor: bool) -> Self {
        Self {
            out,
            truecolor,
            rows: Vec::new(),
            at: 0,
            cursor: None,
            cursor_shape: CursorShape::Default,
            frame: Vec::new(),
        }
    }

    /// Bring the terminal to `frame`; `true` when bytes were written.
    pub(crate) fn paint(&mut self, frame: &Frame<'_>) -> io::Result<bool> {
        let rows = (0..frame.live.area.height)
            .map(|y| row_bytes(frame.live, y, self.truecolor))
            .collect::<io::Result<Vec<_>>>()?;
        if !frame.reset
            && frame.scrollback.is_empty()
            && rows == self.rows
            && frame.cursor == self.cursor
            && frame.cursor_shape == self.cursor_shape
        {
            return Ok(false);
        }
        let mut out = std::mem::take(&mut self.frame);
        out.clear();
        let result = self
            .assemble(frame, rows, &mut out)
            .and_then(|()| self.out.write_all(&out))
            .and_then(|()| self.out.flush());
        self.frame = out;
        result.map(|()| true)
    }

    fn assemble(
        &mut self,
        frame: &Frame<'_>,
        rows: Vec<Vec<u8>>,
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        out.extend_from_slice(SYNC_BEGIN);
        out.extend_from_slice(HIDE_CURSOR);
        if frame.reset {
            out.extend_from_slice(CLEAR_ALL);
            self.rows.clear();
            self.at = 0;
        } else if !frame.scrollback.is_empty() {
            self.goto_row(0, out)?;
        }
        if !frame.scrollback.is_empty() {
            let width = frame.live.area.width;
            let mut one = Buffer::empty(ratatui::layout::Rect::new(0, 0, width, 1));
            for line in frame.scrollback {
                one.reset();
                one.set_line(0, 0, line, width);
                out.extend_from_slice(&row_bytes(&one, 0, self.truecolor)?);
                out.extend_from_slice(b"\r\n");
            }
            if !frame.reset {
                // The old live region's leftover rows sit below the lines just written.
                out.extend_from_slice(ERASE_BELOW);
            }
            self.rows.clear();
            self.at = 0;
        }
        self.paint_region(&rows, out)?;
        if frame.cursor_shape != self.cursor_shape {
            out.extend_from_slice(frame.cursor_shape.decscusr());
            self.cursor_shape = frame.cursor_shape;
        }
        self.place_cursor(frame.cursor, out)?;
        out.extend_from_slice(SYNC_END);
        self.rows = rows;
        self.cursor = frame.cursor;
        Ok(())
    }

    /// Grow the region to `rows.len()` and repaint the rows that differ.
    fn paint_region(&mut self, rows: &[Vec<u8>], out: &mut Vec<u8>) -> io::Result<()> {
        // The row under the cursor exists even when the region painted nothing yet.
        let existing = self.rows.len().max(1);
        if rows.len() > existing {
            self.goto_row(existing - 1, out)?;
            // A newline moves down, or scrolls the terminal when the region reaches its bottom.
            for _ in existing..rows.len() {
                out.extend_from_slice(b"\r\n");
            }
            self.at = rows.len() - 1;
        }
        for (i, row) in rows.iter().enumerate() {
            if self.rows.get(i) != Some(row) {
                self.goto_row(i, out)?;
                out.extend_from_slice(row);
            }
        }
        if rows.len() < self.rows.len() {
            self.goto_row(rows.len(), out)?;
            out.extend_from_slice(ERASE_BELOW);
        }
        Ok(())
    }

    fn place_cursor(&mut self, cursor: Option<Position>, out: &mut Vec<u8>) -> io::Result<()> {
        match cursor {
            Some(position) => {
                self.goto_row(usize::from(position.y), out)?;
                if position.x > 0 {
                    write!(out, "\x1b[{}C", position.x)?;
                }
                out.extend_from_slice(SHOW_CURSOR);
            }
            None => out.extend_from_slice(HIDE_CURSOR),
        }
        Ok(())
    }

    /// Write the whole live region once more, byte for byte unchanged. Windows Terminal behind
    /// the pseudo console sometimes keeps painting rows of an earlier frame after a burst,
    /// although the bytes that describe them were written; the loop calls this once the output
    /// has been quiet, so a stale row is refreshed a moment later instead of staying on screen.
    pub(crate) fn nudge(&mut self) -> io::Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let mut out = Vec::new();
        out.extend_from_slice(HIDE_CURSOR);
        self.goto_row(0, &mut out)?;
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b"\r\n");
                self.at = i;
            }
            out.extend_from_slice(row);
        }
        self.place_cursor(self.cursor, &mut out)?;
        self.out.write_all(&out)?;
        self.out.flush()
    }

    /// Move the cursor to the start of row `row` of the region.
    fn goto_row(&mut self, row: usize, out: &mut Vec<u8>) -> io::Result<()> {
        match row.cmp(&self.at) {
            std::cmp::Ordering::Greater => write!(out, "\x1b[{}B", row - self.at)?,
            std::cmp::Ordering::Less => write!(out, "\x1b[{}A", self.at - row)?,
            std::cmp::Ordering::Equal => {}
        }
        out.push(b'\r');
        self.at = row;
        Ok(())
    }

    /// Erase the live region and leave the cursor where it began, restoring the default cursor
    /// shape via DECSCUSR (`\x1b[0 q`).
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        let mut out = Vec::new();
        self.goto_row(0, &mut out)?;
        out.extend_from_slice(ERASE_BELOW);
        if self.cursor_shape != CursorShape::Default {
            out.extend_from_slice(CursorShape::Default.decscusr());
            self.cursor_shape = CursorShape::Default;
        }
        out.extend_from_slice(SHOW_CURSOR);
        self.rows.clear();
        self.cursor = None;
        self.out.write_all(&out)?;
        self.out.flush()
    }
}

impl<W: Write> Drop for Inline<W> {
    fn drop(&mut self) {
        if self.cursor_shape != CursorShape::Default {
            let _ = self.out.write_all(CursorShape::Default.decscusr());
            let _ = self.out.flush();
            self.cursor_shape = CursorShape::Default;
        }
    }
}

/// A cell that paints nothing a cleared row would not.
fn is_blank(cell: &Cell) -> bool {
    cell.symbol() == " "
        && cell.bg == Color::Reset
        && !cell
            .modifier
            .intersects(Modifier::REVERSED | Modifier::UNDERLINED | Modifier::CROSSED_OUT)
}

/// The terminal's current text attributes while a row is written.
struct Pen {
    fg: Color,
    bg: Color,
    modifier: Modifier,
}

impl Pen {
    const RESET: Self = Self {
        fg: Color::Reset,
        bg: Color::Reset,
        modifier: Modifier::empty(),
    };

    /// Emit the SGR sequence that turns the pen into `cell`'s attributes.
    fn set(&mut self, cell: &Cell, truecolor: bool, out: &mut Vec<u8>) -> io::Result<()> {
        let fg = theme::paint(cell.fg, truecolor);
        let bg = theme::paint(cell.bg, truecolor);
        if (fg, bg, cell.modifier) == (self.fg, self.bg, self.modifier) {
            return Ok(());
        }
        out.extend_from_slice(b"\x1b[0");
        for (flag, code) in MODIFIERS {
            if cell.modifier.contains(flag) {
                write!(out, ";{code}")?;
            }
        }
        color_params(out, fg, 30)?;
        color_params(out, bg, 40)?;
        out.push(b'm');
        *self = Self {
            fg,
            bg,
            modifier: cell.modifier,
        };
        Ok(())
    }
}

/// The SGR parameters selecting `color`; `base` is 30 for the foreground and 40 for the
/// background. `Reset` needs none: the sequence starts from a reset pen.
fn color_params(out: &mut Vec<u8>, color: Color, base: u8) -> io::Result<()> {
    let bright = |n: u8| base + 60 + n;
    match color {
        Color::Reset => Ok(()),
        Color::Black => write!(out, ";{base}"),
        Color::Red => write!(out, ";{}", base + 1),
        Color::Green => write!(out, ";{}", base + 2),
        Color::Yellow => write!(out, ";{}", base + 3),
        Color::Blue => write!(out, ";{}", base + 4),
        Color::Magenta => write!(out, ";{}", base + 5),
        Color::Cyan => write!(out, ";{}", base + 6),
        Color::Gray => write!(out, ";{}", base + 7),
        Color::DarkGray => write!(out, ";{}", bright(0)),
        Color::LightRed => write!(out, ";{}", bright(1)),
        Color::LightGreen => write!(out, ";{}", bright(2)),
        Color::LightYellow => write!(out, ";{}", bright(3)),
        Color::LightBlue => write!(out, ";{}", bright(4)),
        Color::LightMagenta => write!(out, ";{}", bright(5)),
        Color::LightCyan => write!(out, ";{}", bright(6)),
        Color::White => write!(out, ";{}", bright(7)),
        Color::Rgb(r, g, b) => write!(out, ";{};2;{r};{g};{b}", base + 8),
        Color::Indexed(n) => write!(out, ";{};5;{n}", base + 8),
    }
}

/// Row `y` of `buf` as terminal bytes: no cursor movement, the tail erased when it is blank.
fn row_bytes(buf: &Buffer, y: u16, truecolor: bool) -> io::Result<Vec<u8>> {
    let width = buf.area.width;
    let end = (0..width)
        .rev()
        .find(|x| buf.cell((*x, y)).is_some_and(|cell| !is_blank(cell)))
        .map_or(0, |x| x + 1);
    let mut out = Vec::with_capacity(usize::from(width) * 2);
    let mut pen = Pen::RESET;
    let mut column = 0u16;
    let mut x = 0;
    while x < end {
        let Some(cell) = buf.cell((x, y)) else { break };
        pen.set(cell, truecolor, &mut out)?;
        out.extend_from_slice(cell.symbol().as_bytes());
        // A wide symbol covers the cells after it.
        let cells = u16::try_from(cell.symbol().width().max(1)).unwrap_or(1);
        column += cells;
        x += cells;
    }
    if pen.fg != Color::Reset || pen.bg != Color::Reset || !pen.modifier.is_empty() {
        out.extend_from_slice(RESET_PEN);
    }
    if column < width {
        out.extend_from_slice(ERASE_LINE_END);
    } else {
        // A row that filled the last column leaves the console waiting to wrap: the next relative
        // cursor move resolves that wrap and shoves every live row below onto the wrong line.
        // Returning to the column start settles it; the following row write repositions anyway.
        out.extend_from_slice(b"\r");
    }
    Ok(out)
}
