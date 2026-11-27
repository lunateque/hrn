//! Normal, operator-pending, and Visual/Visual-Line mode command dispatch for [`VimEngine`].

use crate::input::Editor;
use crate::vim::{MAX_COUNT, Pending, VimAction, VimEngine, VimMode};
use crate::vim_motion::{
    CharFind, MotionResult, clamp_normal, find_char, first_non_blank, goto_line, line_bounds,
    move_left, move_right, move_vertical, normal_line_end, placeholder_at,
};
use crate::vim_ops::{self, Operator, resolve_char_motion};
use crate::vim_textobj;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl VimEngine {
    pub(crate) fn handle_normal_key(&mut self, key: KeyEvent, editor: &mut Editor) -> VimAction {
        if self.handle_nav_key(key.code, editor) {
            return VimAction::Consumed;
        }
        let KeyCode::Char(c) = key.code else {
            return VimAction::Consumed;
        };
        if self.pending != Pending::None {
            return self.finish_pending(c, key, editor);
        }
        if self.try_push_digit(c, key) {
            return VimAction::Consumed;
        }
        if let Some(op) = self.operator {
            return self.handle_operator_pending(op, c, key, editor);
        }
        self.handle_normal_command(c, key, editor)
    }

    pub(crate) fn handle_visual_key(&mut self, key: KeyEvent, editor: &mut Editor) -> VimAction {
        if self.handle_nav_key(key.code, editor) {
            return VimAction::Consumed;
        }
        let KeyCode::Char(c) = key.code else {
            return VimAction::Consumed;
        };
        if self.pending != Pending::None {
            return self.finish_pending(c, key, editor);
        }
        if self.try_push_digit(c, key) {
            return VimAction::Consumed;
        }
        if matches!(c, 'i' | 'a') {
            self.pending = Pending::TextObj { around: c == 'a' };
            return VimAction::Consumed;
        }
        if let Some(prefix) = prefix_for_char(c) {
            self.pending = prefix;
            return VimAction::Consumed;
        }
        match c {
            'v' => {
                self.mode = if self.mode == VimMode::Visual {
                    VimMode::Normal
                } else {
                    VimMode::Visual
                };
            }
            'V' => {
                self.mode = if self.mode == VimMode::VisualLine {
                    VimMode::Normal
                } else {
                    VimMode::VisualLine
                };
            }
            'o' => {
                let cur = editor.cursor();
                editor.set_cursor(clamp_normal(editor.chars(), self.anchor));
                self.anchor = cur;
            }
            'y' | 'd' | 'x' | 'c' | 's' | '>' | '<' => {
                let op = match c {
                    'y' => Operator::Yank,
                    'd' | 'x' => Operator::Delete,
                    'c' | 's' => Operator::Change,
                    '>' => Operator::Indent,
                    _ => Operator::Dedent,
                };
                let linewise = self.mode == VimMode::VisualLine;
                let (lo, hi) = self
                    .visual_selection(editor)
                    .unwrap_or((editor.cursor(), editor.cursor()));
                self.count = None;
                self.run_operator(op, lo, hi, linewise, editor);
            }
            other => {
                let explicit = self.count.is_some();
                let n = self.take_total_count();
                if let Some(m) = resolve_char_motion(
                    editor.chars(),
                    editor.cursor(),
                    other,
                    n,
                    explicit,
                    false,
                    self.last_find,
                ) {
                    editor.set_cursor(clamp_normal(editor.chars(), m.target));
                }
            }
        }
        VimAction::Consumed
    }

    fn handle_nav_key(&mut self, code: KeyCode, editor: &mut Editor) -> bool {
        let pos = match code {
            KeyCode::Left => {
                let n = self.take_total_count();
                move_left(editor.chars(), editor.cursor(), n)
            }
            KeyCode::Right => {
                let n = self.take_total_count();
                move_right(editor.chars(), editor.cursor(), n, false)
            }
            KeyCode::Up => {
                let n = self.take_total_count();
                move_vertical(editor.chars(), editor.cursor(), false, n)
            }
            KeyCode::Down => {
                let n = self.take_total_count();
                move_vertical(editor.chars(), editor.cursor(), true, n)
            }
            KeyCode::Home => line_bounds(editor.chars(), editor.cursor()).0,
            KeyCode::End => normal_line_end(editor.chars(), editor.cursor()),
            _ => return false,
        };
        self.cancel_pending();
        editor.set_cursor(pos);
        true
    }

    fn try_push_digit(&mut self, c: char, key: KeyEvent) -> bool {
        let slot = if self.operator.is_some() {
            &mut self.op_count
        } else {
            &mut self.count
        };
        if c.is_ascii_digit() && (c != '0' || slot.is_some()) {
            let digit = usize::try_from(c.to_digit(10).unwrap_or(0)).unwrap_or(0);
            *slot = Some(
                slot.unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit)
                    .min(MAX_COUNT),
            );
            if let Some(rec) = self.recording.as_mut() {
                rec.push(key);
            }
            true
        } else {
            false
        }
    }

    fn take_total_count(&mut self) -> usize {
        self.count
            .take()
            .unwrap_or(1)
            .saturating_mul(self.op_count.take().unwrap_or(1))
            .clamp(1, MAX_COUNT)
    }

    fn finish_pending(&mut self, c: char, key: KeyEvent, editor: &mut Editor) -> VimAction {
        let pending = std::mem::replace(&mut self.pending, Pending::None);
        if let Some(rec) = self.recording.as_mut() {
            rec.push(key);
        }
        match pending {
            Pending::ReplaceChar => {
                let n = self.take_total_count();
                if !self.replaying {
                    self.push_undo(editor);
                }
                let rec = self.recording.take();
                if vim_ops::replace_chars(editor, c, n)
                    && let Some(keys) = rec
                {
                    self.commit_change(keys);
                }
            }
            Pending::Find { forward, till } => {
                let find = CharFind {
                    ch: c,
                    forward,
                    till,
                };
                self.last_find = Some(find);
                let n = self.take_total_count();
                let res = find_char(editor.chars(), editor.cursor(), find, n).map(|target| {
                    MotionResult {
                        target,
                        inclusive: forward,
                        linewise: false,
                    }
                });
                self.apply_motion_or_operator(res, editor);
            }
            Pending::G if c == 'g' => {
                let line = self
                    .count
                    .take()
                    .or_else(|| self.op_count.take())
                    .unwrap_or(1);
                let target = goto_line(editor.chars(), line);
                self.apply_motion_or_operator(
                    Some(MotionResult {
                        target,
                        inclusive: true,
                        linewise: true,
                    }),
                    editor,
                );
            }
            Pending::TextObj { around } => {
                if let Some(range) =
                    vim_textobj::resolve(editor.chars(), editor.cursor(), around, c)
                {
                    if let Some(op) = self.operator.take() {
                        self.count = None;
                        self.op_count = None;
                        self.run_operator(op, range.start, range.end, range.linewise, editor);
                    } else if matches!(self.mode, VimMode::Visual | VimMode::VisualLine) {
                        self.anchor = range.start;
                        let end_pos = range.end.saturating_sub(1).max(range.start);
                        editor.set_cursor(clamp_normal(editor.chars(), end_pos));
                        if range.linewise {
                            self.mode = VimMode::VisualLine;
                        }
                    }
                } else {
                    self.cancel_pending();
                }
            }
            Pending::G | Pending::None => self.cancel_pending(),
        }
        VimAction::Consumed
    }

    fn handle_operator_pending(
        &mut self,
        op: Operator,
        c: char,
        key: KeyEvent,
        editor: &mut Editor,
    ) -> VimAction {
        if let Some(rec) = self.recording.as_mut() {
            rec.push(key);
        }
        let doubled = matches!(
            (op, c),
            (Operator::Delete, 'd')
                | (Operator::Yank, 'y')
                | (Operator::Change, 'c')
                | (Operator::Indent, '>')
                | (Operator::Dedent, '<')
        );
        if doubled {
            let n = self.take_total_count();
            self.operator = None;
            let (start, end) = vim_ops::count_lines_span(editor.chars(), editor.cursor(), n);
            self.run_operator(op, start, end, true, editor);
            return VimAction::Consumed;
        }
        if matches!(c, 'i' | 'a') {
            self.pending = Pending::TextObj { around: c == 'a' };
            return VimAction::Consumed;
        }
        if let Some(prefix) = prefix_for_char(c) {
            self.pending = prefix;
            return VimAction::Consumed;
        }
        let explicit = self.count.is_some() || self.op_count.is_some();
        let n = self.take_total_count();
        let motion_char = if op == Operator::Change
            && matches!(c, 'w' | 'W')
            && editor
                .chars()
                .get(editor.cursor())
                .is_some_and(|ch| !ch.is_whitespace())
        {
            if c == 'w' { 'e' } else { 'E' }
        } else {
            c
        };
        let motion = resolve_char_motion(
            editor.chars(),
            editor.cursor(),
            motion_char,
            n,
            explicit,
            true,
            self.last_find,
        );
        self.apply_motion_or_operator(motion, editor);
        VimAction::Consumed
    }

    fn handle_normal_command(&mut self, c: char, key: KeyEvent, editor: &mut Editor) -> VimAction {
        if let Some(op) = operator_for_char(c) {
            self.operator = Some(op);
            self.start_recording_with_count(key);
            return VimAction::Consumed;
        }
        if let Some(prefix) = prefix_for_char(c) {
            self.pending = prefix;
            return VimAction::Consumed;
        }
        if self.handle_normal_mode_switch(c, key, editor)
            || self.handle_normal_edit_cmd(c, key, editor)
        {
            return VimAction::Consumed;
        }
        let explicit = self.count.is_some();
        let n = self.take_total_count();
        let motion = resolve_char_motion(
            editor.chars(),
            editor.cursor(),
            c,
            n,
            explicit,
            false,
            self.last_find,
        );
        self.apply_motion_or_operator(motion, editor);
        VimAction::Consumed
    }

    fn handle_normal_mode_switch(&mut self, c: char, key: KeyEvent, editor: &mut Editor) -> bool {
        match c {
            'i' => self.enter_insert_mode(editor, key, editor.cursor()),
            'a' => {
                let pos = editor.cursor();
                let (_, line_end) = line_bounds(editor.chars(), pos);
                let next = if pos >= line_end {
                    line_end
                } else if let Some(p) = placeholder_at(editor.chars(), pos) {
                    p.end
                } else {
                    (pos + 1).min(line_end)
                };
                self.enter_insert_mode(editor, key, next);
            }
            'I' => {
                let target = first_non_blank(editor.chars(), editor.cursor());
                self.enter_insert_mode(editor, key, target);
            }
            'A' => {
                let (_, end) = line_bounds(editor.chars(), editor.cursor());
                self.enter_insert_mode(editor, key, end);
            }
            'o' | 'O' => {
                self.count = None;
                self.prepare_mutation(editor, key);
                vim_ops::open_line(editor, c == 'o');
                self.mode = VimMode::Insert;
                self.insert_undo_saved = true;
            }
            'R' => {
                self.count = None;
                self.prepare_mutation(editor, key);
                self.mode = VimMode::Replace;
                self.insert_undo_saved = true;
                self.replace_history.clear();
            }
            'v' | 'V' => {
                self.cancel_pending();
                self.anchor = editor.cursor();
                self.mode = if c == 'v' {
                    VimMode::Visual
                } else {
                    VimMode::VisualLine
                };
            }
            _ => return false,
        }
        true
    }

    fn handle_normal_edit_cmd(&mut self, c: char, key: KeyEvent, editor: &mut Editor) -> bool {
        match c {
            'r' => {
                self.pending = Pending::ReplaceChar;
                self.start_recording_with_count(key);
            }
            'x' => {
                let n = self.take_total_count();
                let rec = keys_with_count(n, key);
                if !self.replaying {
                    self.push_undo(editor);
                }
                if vim_ops::delete_chars(editor, n, &mut self.register) {
                    self.commit_change(rec);
                }
            }
            'D' | 'C' => {
                self.count = None;
                self.start_recording_with_count(key);
                let pos = editor.cursor();
                let (_, end) = line_bounds(editor.chars(), pos);
                let op = if c == 'D' {
                    Operator::Delete
                } else {
                    Operator::Change
                };
                self.run_operator(op, pos, end, false, editor);
            }
            'J' => {
                let n = self.take_total_count();
                let rec = keys_with_count(n, key);
                if !self.replaying {
                    self.push_undo(editor);
                }
                if vim_ops::join_lines(editor, n) {
                    self.commit_change(rec);
                }
            }
            'p' | 'P' => {
                let n = self.take_total_count();
                let rec = keys_with_count(n, key);
                if !self.replaying {
                    self.push_undo(editor);
                }
                if vim_ops::paste(editor, &self.register, c == 'p', n) {
                    self.commit_change(rec);
                }
            }
            'u' => {
                self.cancel_pending();
                self.undo(editor);
            }
            '.' => {
                let override_count = self.count.take();
                self.cancel_pending();
                self.replay_last_change(override_count, editor);
            }
            _ => return false,
        }
        true
    }

    fn apply_motion_or_operator(&mut self, motion: Option<MotionResult>, editor: &mut Editor) {
        let Some(motion) = motion else {
            self.cancel_pending();
            return;
        };
        if let Some(op) = self.operator.take() {
            let start = editor.cursor();
            let (lo, hi) = if motion.linewise {
                if start <= motion.target {
                    (start, motion.target)
                } else {
                    (motion.target, start)
                }
            } else if start <= motion.target {
                let end = if motion.inclusive {
                    (motion.target + 1).min(editor.chars().len())
                } else {
                    motion.target
                };
                (start, end)
            } else {
                (motion.target, start)
            };
            self.run_operator(op, lo, hi, motion.linewise, editor);
        } else {
            editor.set_cursor(clamp_normal(editor.chars(), motion.target));
        }
    }

    fn run_operator(
        &mut self,
        op: Operator,
        start: usize,
        end: usize,
        linewise: bool,
        editor: &mut Editor,
    ) {
        if !self.replaying && op != Operator::Yank {
            self.push_undo(editor);
        }
        let (next_mode, mutated) = vim_ops::apply_operator(
            editor,
            op,
            start,
            end,
            linewise,
            &mut self.register,
            self.clipboard.as_mut(),
        );
        self.mode = next_mode;
        if next_mode == VimMode::Insert {
            self.insert_undo_saved = true;
        } else {
            let rec = self.recording.take();
            if mutated && let Some(keys) = rec {
                self.commit_change(keys);
            }
        }
    }

    fn enter_insert_mode(&mut self, editor: &mut Editor, key: KeyEvent, cursor: usize) {
        self.count = None;
        self.prepare_mutation(editor, key);
        editor.set_cursor(cursor.min(editor.chars().len()));
        self.mode = VimMode::Insert;
        self.insert_undo_saved = true;
    }

    fn prepare_mutation(&mut self, editor: &Editor, key: KeyEvent) {
        if !self.replaying {
            self.push_undo(editor);
            self.recording = Some(vec![key]);
        }
    }

    fn start_recording_with_count(&mut self, key: KeyEvent) {
        if self.replaying {
            return;
        }
        self.recording = Some(keys_with_count(self.count.unwrap_or(1), key));
    }
}

fn keys_with_count(count: usize, key: KeyEvent) -> Vec<KeyEvent> {
    let mut keys = Vec::new();
    if count > 1 {
        for ch in count.to_string().chars() {
            keys.push(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
    }
    keys.push(key);
    keys
}

fn operator_for_char(c: char) -> Option<Operator> {
    match c {
        'd' => Some(Operator::Delete),
        'y' => Some(Operator::Yank),
        'c' => Some(Operator::Change),
        '>' => Some(Operator::Indent),
        '<' => Some(Operator::Dedent),
        _ => None,
    }
}

fn prefix_for_char(c: char) -> Option<Pending> {
    match c {
        'g' => Some(Pending::G),
        'f' => Some(Pending::Find {
            forward: true,
            till: false,
        }),
        'F' => Some(Pending::Find {
            forward: false,
            till: false,
        }),
        't' => Some(Pending::Find {
            forward: true,
            till: true,
        }),
        'T' => Some(Pending::Find {
            forward: false,
            till: true,
        }),
        _ => None,
    }
}
