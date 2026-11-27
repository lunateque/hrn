//! The inline `ask` panel: the `ask` tool's HTML form, rendered between the transcript and the
//! composer. Up/Down (Tab/Shift+Tab) move between answers, Space toggles a choice, typing edits a
//! text answer, Enter submits (validation errors show under their field), Esc cancels. With an
//! `ask_timeout` the form submits its current values when nobody touches it for that long.

mod view;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hrn_kernel::{Answer, Field, Form, Kind, Submission};
use std::cell::Cell;
use std::time::{Duration, Instant};

/// What the panel decided after a key.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AskAction {
    /// Keep showing the panel.
    Stay,
    /// The form passed validation: hand it to the tool.
    Submit(Submission),
    /// The user cancelled; the tool call fails as "cancelled".
    Cancel,
}

/// One place the cursor can rest. Field numbers count [`Form::fields`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// Option `.1` of a choice field.
    Option(usize, usize),
    /// The free-text "Other" answer of a choice field.
    Other(usize),
    /// A text field.
    Text(usize),
}

impl Slot {
    fn field(self) -> usize {
        match self {
            Self::Option(field, _) | Self::Other(field) | Self::Text(field) => field,
        }
    }
}

/// One form in flight.
pub(crate) struct AskPanel {
    form: Form,
    answers: Vec<Answer>,
    /// Why each field's answer was refused at the last submit.
    errors: Vec<Option<String>>,
    slots: Vec<Slot>,
    focus: usize,
    /// Cursor (a char index) in the focused text slot.
    cursor: usize,
    /// First visible content row; kept between frames so the view does not jump.
    scroll: Cell<usize>,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
}

impl AskPanel {
    /// A panel for `form`; `None` when it has nothing to answer.
    pub(crate) fn new(form: Form, timeout: Option<Duration>, now: Instant) -> Option<Self> {
        let mut slots = Vec::new();
        for (at, field) in form.fields().enumerate() {
            match field.kind {
                Kind::Hidden(_) => {}
                Kind::Text(_) => slots.push(Slot::Text(at)),
                Kind::One | Kind::Many | Kind::Flag => {
                    slots.extend((0..field.options.len()).map(|opt| Slot::Option(at, opt)));
                    if field.other {
                        slots.push(Slot::Other(at));
                    }
                }
            }
        }
        if slots.is_empty() {
            return None;
        }
        let answers = form.defaults();
        let errors = vec![None; answers.len()];
        let mut panel = Self {
            form,
            answers,
            errors,
            slots,
            focus: 0,
            cursor: 0,
            scroll: Cell::new(0),
            timeout,
            deadline: timeout.map(|t| now + t),
        };
        panel.cursor = panel.text_len();
        Some(panel)
    }

    fn field(&self, at: usize) -> Option<&Field> {
        self.form.fields().nth(at)
    }

    fn slot(&self) -> Slot {
        self.slots.get(self.focus).copied().unwrap_or(Slot::Text(0))
    }

    /// The text behind the focused slot, when it is a text slot.
    fn text(&self) -> Option<&String> {
        match self.slot() {
            Slot::Text(at) => self.answers.get(at).map(|a| &a.text),
            Slot::Other(at) => self.answers.get(at).map(|a| &a.other),
            Slot::Option(..) => None,
        }
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        match self.slot() {
            Slot::Text(at) => self.answers.get_mut(at).map(|a| &mut a.text),
            Slot::Other(at) => self.answers.get_mut(at).map(|a| &mut a.other),
            Slot::Option(..) => None,
        }
    }

    fn text_len(&self) -> usize {
        self.text().map_or(0, |t| t.chars().count())
    }

    /// Whether the focused slot is a multi-line text area.
    fn in_area(&self) -> bool {
        matches!(self.slot(), Slot::Text(at)
            if matches!(self.field(at).map(|f| &f.kind), Some(Kind::Text(spec)) if spec.multiline))
    }

    /// When the form submits itself, counted from the last key.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// The form's current values, for the moment nobody answered in time.
    pub(crate) fn timed_out(&self) -> Submission {
        Submission {
            values: self.form.values(&self.answers),
            timed_out: true,
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent, now: Instant) -> AskAction {
        self.deadline = self.timeout.map(|t| now + t);
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => return AskAction::Cancel,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) && self.in_area() => {
                self.insert('\n');
            }
            KeyCode::Enter => return self.submit(),
            KeyCode::Char('j')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.in_area() =>
            {
                self.insert('\n');
            }
            KeyCode::Up | KeyCode::BackTab => self.step(false),
            KeyCode::Down | KeyCode::Tab => self.step(true),
            KeyCode::Char(' ') if plain && matches!(self.slot(), Slot::Option(..)) => self.toggle(),
            KeyCode::Char(c) if plain && self.text().is_some() => self.insert(c),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.remove();
            }
            KeyCode::Delete => self.remove(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text_len()),
            KeyCode::Home => self.cursor = self.line_edge(false),
            KeyCode::End => self.cursor = self.line_edge(true),
            _ => {}
        }
        AskAction::Stay
    }

    fn step(&mut self, forward: bool) {
        let count = self.slots.len();
        self.focus = if forward {
            (self.focus + 1) % count
        } else {
            (self.focus + count - 1) % count
        };
        self.cursor = self.text_len();
    }

    /// The start or end of the cursor's line in the focused text.
    fn line_edge(&self, end: bool) -> usize {
        let Some(text) = self.text() else {
            return 0;
        };
        let chars: Vec<char> = text.chars().collect();
        let before = chars.get(..self.cursor).unwrap_or_default();
        if end {
            let len = chars
                .get(self.cursor..)
                .unwrap_or_default()
                .iter()
                .take_while(|c| **c != '\n')
                .count();
            self.cursor + len
        } else {
            before
                .iter()
                .rposition(|c| *c == '\n')
                .map_or(0, |at| at + 1)
        }
    }

    fn toggle(&mut self) {
        let Slot::Option(at, opt) = self.slot() else {
            return;
        };
        let one = self.field(at).is_some_and(|f| f.kind == Kind::One);
        let Some(answer) = self.answers.get_mut(at) else {
            return;
        };
        if let Some(pos) = answer.picked.iter().position(|i| *i == opt) {
            answer.picked.remove(pos);
        } else if one {
            answer.picked = vec![opt];
            answer.other.clear();
        } else {
            answer.picked.push(opt);
        }
        self.clear_error(at);
    }

    fn insert(&mut self, c: char) {
        let cursor = self.cursor;
        let slot = self.slot();
        let Some(text) = self.text_mut() else {
            return;
        };
        let at = text
            .char_indices()
            .nth(cursor)
            .map_or(text.len(), |(byte, _)| byte);
        text.insert(at, c);
        self.cursor += 1;
        self.edited(slot);
    }

    fn remove(&mut self) {
        let cursor = self.cursor;
        let slot = self.slot();
        let Some(text) = self.text_mut() else {
            return;
        };
        if let Some((byte, _)) = text.char_indices().nth(cursor) {
            text.remove(byte);
        }
        self.edited(slot);
    }

    /// Typing an "Other" answer to a radio group unchooses the listed options.
    fn edited(&mut self, slot: Slot) {
        let at = slot.field();
        if let Slot::Other(_) = slot
            && self.field(at).is_some_and(|f| f.kind == Kind::One)
            && let Some(answer) = self.answers.get_mut(at)
            && !answer.other.trim().is_empty()
        {
            answer.picked.clear();
        }
        self.clear_error(at);
    }

    fn clear_error(&mut self, field: usize) {
        if let Some(error) = self.errors.get_mut(field) {
            *error = None;
        }
    }

    fn submit(&mut self) -> AskAction {
        self.errors = self.form.errors(&self.answers);
        let Some(bad) = self.errors.iter().position(Option::is_some) else {
            return AskAction::Submit(Submission {
                values: self.form.values(&self.answers),
                timed_out: false,
            });
        };
        if let Some(at) = self.slots.iter().position(|s| s.field() == bad) {
            self.focus = at;
            self.cursor = self.text_len();
        }
        AskAction::Stay
    }
}
