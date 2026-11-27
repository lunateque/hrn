//! The `ask` tool's HTML forms: parsed once into a [`Form`], then answered by a front-end
//! ([`Answer`] per field) and turned into the JSON the model reads ([`Form::values`]).
//!
//! The accepted HTML is the plain semantic subset a model writes for a questionnaire: `fieldset`
//! / `legend`, `label`, headings, `p`, `ul`/`ol`/`li`, `table`, `pre`, `img` (its alt text),
//! radio and checkbox groups, `select`, `textarea` and text-like `input`s.

mod data;
mod html;
mod parse;
mod pattern;

pub use data::{Answer, Submission};

use serde::{Deserialize, Serialize};

/// One parsed `<form>`: context and controls in document order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Form {
    /// What to show, top to bottom.
    pub items: Vec<Item>,
}

/// One block of a form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Item {
    /// Context text.
    Text {
        /// How it is styled.
        kind: TextKind,
        /// The text; `Pre` keeps its line breaks, everything else is one wrapped line.
        text: String,
    },
    /// A `<table>`: cell texts by row.
    Table {
        /// The first row is made of `<th>` cells.
        header: bool,
        /// Rows of cell texts.
        rows: Vec<Vec<String>>,
    },
    /// An `<img>`, shown as its alt text.
    Image {
        /// Alternative text (the file name when the tag has none).
        alt: String,
    },
    /// A control the user answers.
    Field(Field),
}

/// How a [`Item::Text`] is styled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextKind {
    /// `<h1>`..`<h6>`.
    Heading,
    /// `<legend>`.
    Legend,
    /// `<p>`, `<div>` and loose text.
    Para,
    /// `<li>`, with its bullet or number already in the text.
    List,
    /// `<pre>`.
    Pre,
}

/// What a [`Field`] collects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// One of [`Field::options`]: a radio group or a single `<select>`.
    One,
    /// Any of [`Field::options`]: a checkbox group or a `<select multiple>`.
    Many,
    /// A lone checkbox: its value when checked, absent otherwise.
    Flag,
    /// Free text.
    Text(Box<Spec>),
    /// A hidden input: passed through.
    Hidden(String),
}

/// One named control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    /// The `name` the answer is keyed by.
    pub name: String,
    /// The label, else the text right before it, else the legend, else the name.
    pub question: String,
    /// The user must answer.
    pub required: bool,
    /// What it collects.
    pub kind: Kind,
    /// Choices of a `One`, `Many` or `Flag` field.
    pub options: Vec<Choice>,
    /// The front-end adds a free-text "Other" answer.
    pub other: bool,
}

/// One choice of a radio group, checkbox group or select.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    /// What the user sees.
    pub label: String,
    /// What the answer carries.
    pub value: String,
    /// Preselected.
    pub checked: bool,
}

/// The constraints and starting text of a text control.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    /// The input `type` (`text`, `number`, `date`, …); `textarea` for a textarea.
    pub input: String,
    /// A `<textarea>`: the text may span lines.
    pub multiline: bool,
    /// The starting text.
    pub value: String,
    /// Shown while the text is empty.
    pub placeholder: String,
    /// `min` (a number, or an ISO date).
    pub min: Option<String>,
    /// `max`.
    pub max: Option<String>,
    /// `minlength`.
    pub min_len: Option<usize>,
    /// `maxlength`.
    pub max_len: Option<usize>,
    /// `pattern`: the whole text must match it.
    pub pattern: Option<String>,
    /// `title`: the explanation shown when the pattern fails.
    pub title: Option<String>,
}

impl Form {
    /// Parse the first `<form>` of `html` (the whole text when it has none).
    ///
    /// # Errors
    /// The form has no named control.
    pub fn parse(html: &str) -> Result<Self, String> {
        parse::parse(html)
    }

    /// The controls in order; an [`Answer`] list is indexed the same way.
    pub fn fields(&self) -> impl Iterator<Item = &Field> {
        self.items.iter().filter_map(|item| match item {
            Item::Field(field) => Some(field),
            _ => None,
        })
    }

    /// Whether the user has anything to answer (a form of hidden inputs alone has not).
    pub fn is_interactive(&self) -> bool {
        self.fields()
            .any(|field| !matches!(field.kind, Kind::Hidden(_)))
    }
}

#[cfg(test)]
mod tests;
