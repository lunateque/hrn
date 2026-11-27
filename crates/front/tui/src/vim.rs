//! Modal Vim editor engine for the prompt composer (`[tui] vim_mode`).
//!
//! Supports `Insert`, `Normal`, `Visual`, `VisualLine`, and `Replace` modes, numeric counts,
//! operators (`d`, `y`, `c`, `>`, `<` + linewise doubled forms), motions, text objects,
//! multi-level undo/redo (`u`, `Ctrl+R`), `.` dot repeat, atomic attachment placeholders,
//! DECSCUSR cursor shapes, and injectable clipboard I/O for deterministic tests.

use crate::input::Editor;
use crate::vim_motion::{
    CharFind, clamp_normal, expand_range_for_placeholders, line_bounds, placeholder_at,
    snap_backward,
};
use crate::vim_ops::Operator;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const MAX_UNDO: usize = 200;
pub(crate) const MAX_COUNT: usize = 10_000;

/// Active modal Vim editing mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum VimMode {
    /// Text insertion (default startup mode when `vim_mode = true`).
    #[default]
    Insert,
    /// Motion and operator command mode.
    Normal,
    /// Characterwise visual selection.
    Visual,
    /// Linewise visual selection.
    VisualLine,
    /// Character-by-character overwrite mode (`R`).
    Replace,
}

impl VimMode {
    /// Uppercase ASCII badge label for `[tui] vim_mode_display = "text"`.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Normal => "NORMAL",
            Self::Visual => "VISUAL",
            Self::VisualLine => "V-LINE",
            Self::Replace => "REPLACE",
        }
    }
}

/// Terminal cursor shape emitted via DECSCUSR (`\x1b[<n> q`) and restored on exit/drop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    /// Terminal default cursor (`\x1b[0 q`).
    #[default]
    Default,
    /// Steady block (`\x1b[2 q`) for Normal and Visual modes.
    Block,
    /// Steady underline (`\x1b[4 q`) for Replace mode.
    Underline,
    /// Steady bar (`\x1b[6 q`) for Insert mode.
    Bar,
}

impl CursorShape {
    /// DECSCUSR escape bytes selecting this cursor shape.
    pub(crate) const fn decscusr(self) -> &'static [u8] {
        match self {
            Self::Default => b"\x1b[0 q",
            Self::Block => b"\x1b[2 q",
            Self::Underline => b"\x1b[4 q",
            Self::Bar => b"\x1b[6 q",
        }
    }
}

/// Injectable clipboard adapter so unit tests verify visual yank/paste deterministically.
pub(crate) trait ClipboardIo: Send {
    fn write_text(&mut self, text: &str) -> bool;
    fn read_text(&mut self) -> Option<String>;
}

/// Default OS clipboard backed by [`crate::clipboard`] (`arboard`).
pub(crate) struct SystemClipboard;

impl ClipboardIo for SystemClipboard {
    fn write_text(&mut self, text: &str) -> bool {
        crate::clipboard::write_text(text)
    }

    fn read_text(&mut self) -> Option<String> {
        crate::clipboard::read_text()
    }
}

/// Register contents stored by `d`, `c`, `y`, `x`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Register {
    pub(crate) text: String,
    pub(crate) linewise: bool,
}

/// Outcome of routing one key event through [`VimEngine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VimAction {
    /// Vim handled the key; do not run default readline or app Escape handling.
    Consumed,
    /// Vim did not claim the key (`Enter`, `Tab`, `Ctrl+*`, Insert typing, or Normal idle `Esc`).
    PassThrough,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pending {
    None,
    G,
    Find { forward: bool, till: bool },
    ReplaceChar,
    TextObj { around: bool },
}

#[derive(Clone, Debug)]
struct Snapshot {
    chars: Vec<char>,
    cursor: usize,
}

/// Full modal Vim editor state machine operating over [`Editor`].
pub(crate) struct VimEngine {
    enabled: bool,
    pub(crate) mode: VimMode,
    pub(crate) anchor: usize,
    pub(crate) count: Option<usize>,
    pub(crate) op_count: Option<usize>,
    pub(crate) operator: Option<Operator>,
    pub(crate) pending: Pending,
    pub(crate) last_find: Option<CharFind>,
    pub(crate) register: Register,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_change: Option<Vec<KeyEvent>>,
    pub(crate) recording: Option<Vec<KeyEvent>>,
    pub(crate) replaying: bool,
    pub(crate) insert_undo_saved: bool,
    pub(crate) replace_history: Vec<(usize, Option<char>)>,
    pub(crate) clipboard: Box<dyn ClipboardIo>,
}

impl Default for VimEngine {
    fn default() -> Self {
        Self::new(false)
    }
}

impl VimEngine {
    /// Create a fresh Vim engine (`enabled = true` starts in `Insert` mode).
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            mode: VimMode::Insert,
            anchor: 0,
            count: None,
            op_count: None,
            operator: None,
            pending: Pending::None,
            last_find: None,
            register: Register::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            last_change: None,
            recording: None,
            replaying: false,
            insert_undo_saved: false,
            replace_history: Vec::new(),
            clipboard: Box::new(SystemClipboard),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.mode = VimMode::Insert;
            self.cancel_pending();
        }
    }

    pub(crate) fn mode(&self) -> VimMode {
        self.mode
    }

    /// Cursor shape requested by the current Vim mode (`Default` when disabled).
    pub(crate) fn cursor_shape(&self) -> CursorShape {
        if !self.enabled {
            return CursorShape::Default;
        }
        match self.mode {
            VimMode::Insert => CursorShape::Bar,
            VimMode::Normal | VimMode::Visual | VimMode::VisualLine => CursorShape::Block,
            VimMode::Replace => CursorShape::Underline,
        }
    }

    /// True when an operator, count, or multi-key prefix is half-typed.
    pub(crate) fn has_pending(&self) -> bool {
        self.operator.is_some()
            || self.count.is_some()
            || self.op_count.is_some()
            || self.pending != Pending::None
    }

    /// Cancel any half-typed operator, count, or prefix (called before app chords, `Enter`, `Tab`).
    pub(crate) fn cancel_pending(&mut self) {
        self.count = None;
        self.op_count = None;
        self.operator = None;
        self.pending = Pending::None;
        if self.mode == VimMode::Normal {
            self.recording = None;
        }
    }

    /// Reset to Insert mode after submitting or clearing the prompt.
    pub(crate) fn reset_for_new_draft(&mut self) {
        self.mode = VimMode::Insert;
        self.cancel_pending();
        self.insert_undo_saved = false;
        self.replace_history.clear();
    }

    /// Active visual highlight char range `[start, end)` for prompt rendering, if in Visual mode.
    pub(crate) fn visual_selection(&self, editor: &Editor) -> Option<(usize, usize)> {
        if !self.enabled || editor.chars().is_empty() {
            return None;
        }
        let chars = editor.chars();
        match self.mode {
            VimMode::Visual => {
                let a = self.anchor.min(chars.len().saturating_sub(1));
                let b = editor.cursor().min(chars.len().saturating_sub(1));
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                Some(expand_range_for_placeholders(
                    chars,
                    lo,
                    (hi + 1).min(chars.len()),
                ))
            }
            VimMode::VisualLine => {
                let a = self.anchor.min(chars.len());
                let b = editor.cursor().min(chars.len());
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                Some((line_bounds(chars, lo).0, line_bounds(chars, hi).1))
            }
            _ => None,
        }
    }

    /// Paste raw text into `editor` respecting current Vim mode and recording undo.
    pub(crate) fn paste_text_raw(&mut self, editor: &mut Editor, text: &str) {
        self.cancel_pending();
        if self.enabled {
            self.push_undo(editor);
        }
        match self.mode {
            VimMode::Visual | VimMode::VisualLine if self.enabled => {
                let (lo, hi) = self
                    .visual_selection(editor)
                    .unwrap_or((editor.cursor(), editor.cursor()));
                let added: Vec<char> = text.chars().collect();
                let len = added.len();
                editor.splice_chars(lo..hi, &added);
                let cur = clamp_normal(editor.chars(), lo + len.saturating_sub(1));
                editor.set_cursor(cur);
                self.mode = VimMode::Normal;
            }
            VimMode::Normal if self.enabled => {
                editor.insert(text);
                let cur = clamp_normal(editor.chars(), editor.cursor().saturating_sub(1));
                editor.set_cursor(cur);
            }
            _ => editor.insert(text),
        }
    }

    /// Route one key press through Vim. `Ctrl+*` (except `Ctrl+R` redo in Normal), `Enter`,
    /// `Tab`, and `Alt+*` always cancel pending state and pass through.
    pub(crate) fn handle_key(&mut self, key: KeyEvent, editor: &mut Editor) -> VimAction {
        if !self.enabled {
            return VimAction::PassThrough;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl
            && !alt
            && self.mode == VimMode::Normal
            && matches!(key.code, KeyCode::Char('r' | 'R'))
        {
            self.cancel_pending();
            self.redo(editor);
            return VimAction::Consumed;
        }
        if (ctrl && !alt)
            || (alt && !ctrl)
            || matches!(key.code, KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab)
        {
            self.cancel_pending();
            return VimAction::PassThrough;
        }
        if key.code == KeyCode::Esc {
            return self.handle_escape(editor);
        }
        match self.mode {
            VimMode::Insert => self.handle_insert_key(key, editor),
            VimMode::Replace => self.handle_replace_key(key, editor),
            VimMode::Normal => self.handle_normal_key(key, editor),
            VimMode::Visual | VimMode::VisualLine => self.handle_visual_key(key, editor),
        }
    }

    fn handle_escape(&mut self, editor: &mut Editor) -> VimAction {
        match self.mode {
            VimMode::Insert | VimMode::Replace => {
                if let Some(mut rec) = self.recording.take() {
                    rec.push(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                    if !self.replaying {
                        self.last_change = Some(rec);
                    }
                }
                let from_insert = self.mode == VimMode::Insert;
                self.mode = VimMode::Normal;
                self.insert_undo_saved = false;
                self.replace_history.clear();
                self.cancel_pending();
                let pos = editor.cursor();
                let (line_start, _) = line_bounds(editor.chars(), pos);
                let landed = if from_insert && pos > line_start {
                    clamp_normal(editor.chars(), snap_backward(editor.chars(), pos - 1))
                } else {
                    clamp_normal(editor.chars(), pos)
                };
                editor.set_cursor(landed);
                VimAction::Consumed
            }
            VimMode::Visual | VimMode::VisualLine => {
                self.mode = VimMode::Normal;
                self.cancel_pending();
                editor.set_cursor(clamp_normal(editor.chars(), editor.cursor()));
                VimAction::Consumed
            }
            VimMode::Normal if self.has_pending() => {
                self.cancel_pending();
                VimAction::Consumed
            }
            VimMode::Normal => VimAction::PassThrough,
        }
    }

    fn handle_insert_key(&mut self, key: KeyEvent, editor: &mut Editor) -> VimAction {
        if !self.replaying
            && !self.insert_undo_saved
            && matches!(
                key.code,
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
            )
        {
            self.push_undo(editor);
            self.insert_undo_saved = true;
        }
        if let Some(rec) = self.recording.as_mut() {
            rec.push(key);
        }
        if self.replaying {
            match key.code {
                KeyCode::Char(c) => editor.insert_char(c),
                KeyCode::Backspace => editor.backspace(),
                KeyCode::Delete => editor.delete(),
                _ => {}
            }
            return VimAction::Consumed;
        }
        VimAction::PassThrough
    }

    fn handle_replace_key(&mut self, key: KeyEvent, editor: &mut Editor) -> VimAction {
        if let Some(rec) = self.recording.as_mut() {
            rec.push(key);
        }
        match key.code {
            KeyCode::Char(c) => {
                let pos = editor.cursor();
                let (_, line_end) = line_bounds(editor.chars(), pos);
                if placeholder_at(editor.chars(), pos).is_some() || pos >= line_end {
                    editor.insert_char(c);
                    self.replace_history.push((pos, None));
                } else {
                    let old = editor.chars().get(pos).copied();
                    editor.splice_chars(pos..pos + 1, &[c]);
                    editor.set_cursor(pos + 1);
                    self.replace_history.push((pos, old));
                }
            }
            KeyCode::Backspace => {
                if let Some((pos, Some(ch))) = self.replace_history.pop()
                    && pos < editor.chars().len()
                {
                    editor.splice_chars(pos..pos + 1, &[ch]);
                    editor.set_cursor(pos);
                } else {
                    editor.backspace();
                }
            }
            KeyCode::Left => editor.left(),
            KeyCode::Right => editor.right(),
            _ => {}
        }
        VimAction::Consumed
    }

    pub(crate) fn commit_change(&mut self, keys: Vec<KeyEvent>) {
        if !self.replaying {
            self.last_change = Some(keys);
        }
    }

    pub(crate) fn push_undo(&mut self, editor: &Editor) {
        let snap = Snapshot {
            chars: editor.chars().to_vec(),
            cursor: editor.cursor(),
        };
        if self
            .undo
            .last()
            .is_some_and(|prev| prev.chars == snap.chars && prev.cursor == snap.cursor)
        {
            return;
        }
        self.undo.push(snap);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub(crate) fn undo(&mut self, editor: &mut Editor) {
        let Some(prev) = self.undo.pop() else {
            return;
        };
        self.redo.push(Snapshot {
            chars: editor.chars().to_vec(),
            cursor: editor.cursor(),
        });
        let cur = clamp_normal(&prev.chars, prev.cursor);
        editor.replace_state(prev.chars, cur);
    }

    pub(crate) fn redo(&mut self, editor: &mut Editor) {
        let Some(next) = self.redo.pop() else {
            return;
        };
        self.undo.push(Snapshot {
            chars: editor.chars().to_vec(),
            cursor: editor.cursor(),
        });
        let cur = clamp_normal(&next.chars, next.cursor);
        editor.replace_state(next.chars, cur);
    }

    pub(crate) fn replay_last_change(
        &mut self,
        override_count: Option<usize>,
        editor: &mut Editor,
    ) {
        let Some(mut keys) = self.last_change.clone() else {
            return;
        };
        if let Some(n) = override_count {
            let non_digit = keys
                .iter()
                .position(|k| !matches!(k.code, KeyCode::Char(c) if c.is_ascii_digit()))
                .unwrap_or(keys.len());
            let mut replaced: Vec<KeyEvent> = n
                .to_string()
                .chars()
                .map(|ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .collect();
            replaced.extend(keys.into_iter().skip(non_digit));
            keys = replaced;
        }
        self.push_undo(editor);
        self.replaying = true;
        for key in keys {
            self.handle_key(key, editor);
        }
        self.replaying = false;
        self.recording = None;
    }
}
