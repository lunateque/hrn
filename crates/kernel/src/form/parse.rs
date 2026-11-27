//! HTML tokens into a [`Form`]: context blocks, grouped choices, labels and text controls.

mod controls;

use super::html::{Attrs, Tok, tokenize};
use super::{Choice, Field, Form, Item, Kind, TextKind};
use std::collections::{HashMap, HashSet};

/// Where a label's text lands.
#[derive(Clone, Copy)]
enum Target {
    /// The question of the field at this item index.
    Question(usize),
    /// The label of option `.1` of the field at item index `.0`.
    Option(usize, usize),
}

/// A `<label>` being read, or finished.
#[derive(Default)]
struct Label {
    text: String,
    inside: Vec<Target>,
    for_id: Option<String>,
}

/// A `<select>` being read.
struct Select {
    field: Field,
    option: Option<OptionTag>,
}

struct OptionTag {
    value: Option<String>,
    selected: bool,
    disabled: bool,
    text: String,
}

/// A `<textarea>` being read.
struct Area {
    attrs: Attrs,
    text: String,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: Option<String>,
    header: bool,
}

#[derive(Default)]
struct Builder {
    items: Vec<Item>,
    block: String,
    kind: Option<TextKind>,
    prefix: String,
    pre: u32,
    lists: Vec<Option<usize>>,
    legend: Option<String>,
    table: Option<Table>,
    label: Option<Label>,
    /// The text after a radio/checkbox outside any label is its label.
    trail: Option<(Target, String)>,
    select: Option<Select>,
    area: Option<Area>,
    /// Drop everything up to the matching close tag.
    skip: Option<String>,
    groups: HashMap<(String, bool), usize>,
    choice_groups: Vec<usize>,
    checkbox_groups: HashSet<usize>,
    ids: HashMap<String, Target>,
    labels: Vec<Label>,
}

pub(super) fn parse(html: &str) -> Result<Form, String> {
    let toks = tokenize(html);
    let start = toks
        .iter()
        .position(|t| matches!(t, Tok::Open(name, _) if name == "form"));
    let slice = match start {
        Some(at) => {
            let tail = toks.get(at + 1..).unwrap_or_default();
            let end = tail
                .iter()
                .position(|t| matches!(t, Tok::Close(name) if name == "form"))
                .unwrap_or(tail.len());
            tail.get(..end).unwrap_or_default()
        }
        None => toks.as_slice(),
    };
    let mut builder = Builder::default();
    for tok in slice {
        builder.feed(tok);
    }
    builder.finish()
}

/// Append `text` with whitespace runs folded to one space.
fn collapse(dst: &mut String, text: &str) {
    let mut space = dst.ends_with(' ');
    for c in text.chars() {
        if c.is_whitespace() {
            if !space {
                dst.push(' ');
            }
            space = true;
        } else {
            dst.push(c);
            space = false;
        }
    }
}

fn clean(text: &str) -> String {
    let mut out = String::new();
    collapse(&mut out, text.trim());
    out
}

/// Tags that end the running line of text.
fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "main"
            | "nav"
            | "aside"
            | "blockquote"
            | "figure"
            | "figcaption"
            | "details"
            | "summary"
            | "address"
            | "caption"
            | "dl"
            | "dt"
            | "dd"
            | "hr"
            | "br"
            | "ul"
            | "ol"
            | "li"
            | "table"
            | "fieldset"
            | "legend"
            | "pre"
            | "tr"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
    )
}

impl Builder {
    fn feed(&mut self, tok: &Tok) {
        if let Some(skipping) = &self.skip {
            if matches!(tok, Tok::Close(name) if name == skipping) {
                self.skip = None;
            }
            return;
        }
        match tok {
            Tok::Text(text) => self.text(text),
            Tok::Open(name, attrs) => self.open(name, attrs),
            Tok::Close(name) => self.close(name),
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(select) = &mut self.select {
            if let Some(option) = &mut select.option {
                collapse(&mut option.text, text);
            }
        } else if let Some(area) = &mut self.area {
            area.text.push_str(text);
        } else if let Some(label) = &mut self.label {
            collapse(&mut label.text, text);
        } else if let Some((_, trail)) = &mut self.trail {
            collapse(trail, text);
        } else if let Some(cell) = self.table.as_mut().and_then(|t| t.cell.as_mut()) {
            collapse(cell, text);
        } else if self.pre > 0 {
            self.block.push_str(text);
        } else {
            collapse(&mut self.block, text);
        }
    }

    fn open(&mut self, name: &str, attrs: &Attrs) {
        match name {
            "input" => return self.input(attrs),
            "select" => return self.start_select(attrs),
            "option" => return self.start_option(attrs),
            "textarea" => return self.start_area(attrs),
            "img" => return self.image(attrs),
            "label" => return self.start_label(attrs),
            "button" | "svg" | "title" | "head" | "noscript" => {
                self.skip = Some(name.to_owned());
                return;
            }
            _ => {}
        }
        if is_block(name) {
            self.flush();
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.kind = Some(TextKind::Heading),
            "legend" => self.kind = Some(TextKind::Legend),
            "pre" => {
                self.pre += 1;
                self.kind = Some(TextKind::Pre);
            }
            "ul" => self.lists.push(None),
            "ol" => self.lists.push(Some(0)),
            "li" => {
                self.prefix = match self.lists.last_mut() {
                    Some(Some(count)) => {
                        *count += 1;
                        format!("{count}. ")
                    }
                    _ => "• ".to_owned(),
                };
                self.kind = Some(TextKind::List);
            }
            "table" => self.table = Some(Table::default()),
            "td" | "th" => {
                if let Some(table) = &mut self.table {
                    table.cell = Some(String::new());
                    table.header |= name == "th" && table.rows.is_empty();
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        match name {
            "select" => return self.end_select(),
            "option" => return self.end_option(),
            "textarea" => return self.end_area(),
            "label" => return self.end_label(),
            "td" | "th" => {
                if let Some(table) = &mut self.table
                    && let Some(cell) = table.cell.take()
                {
                    table.row.push(clean(&cell));
                }
                return;
            }
            "tr" => {
                if let Some(table) = &mut self.table {
                    table.rows.push(std::mem::take(&mut table.row));
                }
                return;
            }
            "table" => return self.end_table(),
            _ => {}
        }
        if is_block(name) {
            self.flush();
        }
        match name {
            "legend" => self.legend = self.items.last().and_then(legend_text),
            "fieldset" => self.legend = None,
            "pre" => self.pre = self.pre.saturating_sub(1),
            "ul" | "ol" => {
                self.lists.pop();
            }
            _ => {}
        }
    }

    /// Push the pending block text as an item, and settle a radio/checkbox's trailing label.
    fn flush(&mut self) {
        self.commit_trail();
        let kind = self.kind.take().unwrap_or(TextKind::Para);
        let prefix = std::mem::take(&mut self.prefix);
        let raw = std::mem::take(&mut self.block);
        let text = if kind == TextKind::Pre {
            raw.trim_matches('\n').trim_end().to_owned()
        } else {
            clean(&raw)
        };
        if !text.is_empty() {
            self.items.push(Item::Text {
                kind,
                text: format!("{prefix}{text}"),
            });
        }
    }

    fn commit_trail(&mut self) {
        if let Some((target, text)) = self.trail.take() {
            let text = clean(&text);
            if !text.is_empty() {
                self.apply(target, &text);
            }
        }
    }

    fn apply(&mut self, target: Target, text: &str) {
        match target {
            Target::Question(at) => {
                if let Some(field) = self.field_mut(at) {
                    text.clone_into(&mut field.question);
                }
            }
            Target::Option(at, opt) => {
                if let Some(choice) = self.field_mut(at).and_then(|f| f.options.get_mut(opt)) {
                    text.clone_into(&mut choice.label);
                }
            }
        }
    }

    fn field_mut(&mut self, at: usize) -> Option<&mut Field> {
        match self.items.get_mut(at) {
            Some(Item::Field(field)) => Some(field),
            _ => None,
        }
    }

    /// The text the user read right before a control: the last context line, else the legend.
    fn context(&self) -> Option<String> {
        match self.items.last() {
            Some(Item::Text { kind, text }) if *kind != TextKind::Pre => Some(text.clone()),
            _ => self.legend.clone(),
        }
    }

    fn push_field(&mut self, name: &str, kind: Kind, attrs: &Attrs) -> usize {
        let question = self.context().unwrap_or_else(|| name.to_owned());
        self.items.push(Item::Field(Field {
            name: name.to_owned(),
            question,
            required: attrs.has("required"),
            kind,
            options: Vec::new(),
            other: false,
        }));
        self.items.len() - 1
    }

    fn end_table(&mut self) {
        self.flush();
        if let Some(table) = self.table.take()
            && !table.rows.is_empty()
        {
            self.items.push(Item::Table {
                header: table.header,
                rows: table.rows,
            });
        }
    }

    fn finish(mut self) -> Result<Form, String> {
        self.flush();
        for label in std::mem::take(&mut self.labels) {
            let linked = label.for_id.as_ref().and_then(|id| self.ids.get(id));
            let targets: Vec<Target> = label.inside.iter().chain(linked).copied().collect();
            for target in targets {
                self.apply(target, &label.text);
            }
        }
        for at in std::mem::take(&mut self.choice_groups) {
            let lone = self.checkbox_groups.contains(&at);
            let Some(field) = self.field_mut(at) else {
                continue;
            };
            if lone && let [only] = field.options.as_slice() {
                field.question = if only.label.is_empty() {
                    field.name.clone()
                } else {
                    only.label.clone()
                };
                field.kind = Kind::Flag;
            }
            for choice in &mut field.options {
                if choice.label.is_empty() {
                    choice.label.clone_from(&choice.value);
                }
            }
            field.other = field.kind != Kind::Flag
                && field.options.len() >= 2
                && !field.options.iter().any(is_other);
        }
        let form = Form { items: self.items };
        let mut seen = HashSet::new();
        if let Some(field) = form.fields().find(|f| !seen.insert(f.name.as_str())) {
            return Err(format!(
                "the control name `{}` is used twice; every control needs its own `name` \
                 (radios and checkboxes of one group share it)",
                field.name
            ));
        }
        if seen.is_empty() {
            return Err("the form has no named controls".to_owned());
        }
        Ok(form)
    }
}

fn is_other(choice: &Choice) -> bool {
    choice
        .label
        .trim_start()
        .to_lowercase()
        .starts_with("other")
}

fn legend_text(item: &Item) -> Option<String> {
    match item {
        Item::Text {
            kind: TextKind::Legend,
            text,
        } => Some(text.clone()),
        _ => None,
    }
}
