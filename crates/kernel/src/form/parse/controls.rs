//! The controls of a form: inputs, choice groups, selects, textareas, labels and images.

use super::{Area, Builder, Label, OptionTag, Select, Target, clean, collapse};
use crate::form::html::Attrs;
use crate::form::{Choice, Field, Item, Kind, Spec};

impl Builder {
    pub(super) fn input(&mut self, attrs: &Attrs) {
        let kind = attrs
            .get("type")
            .unwrap_or("text")
            .trim()
            .to_ascii_lowercase();
        let Some(name) = attrs.text("name") else {
            return;
        };
        if attrs.has("disabled")
            || matches!(
                kind.as_str(),
                "submit" | "button" | "reset" | "image" | "file"
            )
        {
            return;
        }
        self.flush();
        let value = attrs.get("value").unwrap_or_default().to_owned();
        let target = match kind.as_str() {
            "hidden" => {
                self.push_field(&name, Kind::Hidden(value), attrs);
                return;
            }
            "radio" | "checkbox" => self.choice(&name, kind == "radio", attrs),
            _ => {
                let spec = Spec {
                    input: kind,
                    value,
                    ..text_spec(attrs)
                };
                Target::Question(self.push_field(&name, Kind::Text(Box::new(spec)), attrs))
            }
        };
        self.register(attrs, target);
        if !matches!(target, Target::Option(..)) {
            return;
        }
        if self.label.is_none() {
            self.trail = Some((target, String::new()));
        }
    }

    /// Remember a control's `id` (for `<label for>`) and attach it to the open label.
    fn register(&mut self, attrs: &Attrs, target: Target) {
        if let Some(id) = attrs.text("id") {
            self.ids.insert(id, target);
        }
        if let Some(label) = &mut self.label {
            label.inside.push(target);
        }
    }

    /// Add one radio/checkbox to its group, creating the group on first sight.
    fn choice(&mut self, name: &str, radio: bool, attrs: &Attrs) -> Target {
        let at = if let Some(at) = self.groups.get(&(name.to_owned(), radio)) {
            *at
        } else {
            let kind = if radio { Kind::One } else { Kind::Many };
            let at = self.push_field(name, kind, attrs);
            self.groups.insert((name.to_owned(), radio), at);
            self.choice_groups.push(at);
            if !radio {
                self.checkbox_groups.insert(at);
            }
            at
        };
        let value = attrs.get("value").unwrap_or("on").to_owned();
        let required = attrs.has("required");
        let Some(field) = self.field_mut(at) else {
            return Target::Question(at);
        };
        field.required |= required;
        field.options.push(Choice {
            label: String::new(),
            value,
            checked: attrs.has("checked"),
        });
        Target::Option(at, field.options.len() - 1)
    }

    pub(super) fn image(&mut self, attrs: &Attrs) {
        let src = attrs.get("src").unwrap_or_default();
        let file = src.rsplit(['/', '\\']).next().unwrap_or_default();
        let alt = attrs
            .text("alt")
            .unwrap_or_else(|| file.split(['?', '#']).next().unwrap_or_default().to_owned());
        if alt.is_empty() {
            return;
        }
        if let Some(label) = &mut self.label {
            collapse(&mut label.text, &alt);
        } else if self.select.is_none() {
            self.flush();
            self.items.push(Item::Image { alt });
        }
    }

    pub(super) fn start_label(&mut self, attrs: &Attrs) {
        self.flush();
        self.label = Some(Label {
            for_id: attrs.text("for"),
            ..Label::default()
        });
    }

    pub(super) fn end_label(&mut self) {
        let Some(mut label) = self.label.take() else {
            return;
        };
        label.text = clean(&label.text);
        if label.inside.is_empty() && label.for_id.is_none() {
            // A caption, not a control's label.
            self.block.push_str(&label.text);
            self.flush();
        } else {
            self.labels.push(label);
        }
    }

    pub(super) fn start_select(&mut self, attrs: &Attrs) {
        self.flush();
        let name = attrs.text("name").filter(|_| !attrs.has("disabled"));
        let Some(name) = name else {
            self.skip = Some("select".to_owned());
            return;
        };
        let kind = if attrs.has("multiple") {
            Kind::Many
        } else {
            Kind::One
        };
        let question = self.context().unwrap_or_else(|| name.clone());
        let field = Field {
            name,
            question,
            required: attrs.has("required"),
            kind,
            options: Vec::new(),
            other: false,
        };
        // The select becomes the next item, so that is the index labels point at.
        self.register(attrs, Target::Question(self.items.len()));
        self.select = Some(Select {
            field,
            option: None,
        });
    }

    pub(super) fn start_option(&mut self, attrs: &Attrs) {
        self.end_option();
        if let Some(select) = &mut self.select {
            select.option = Some(OptionTag {
                value: attrs.get("value").map(str::to_owned),
                selected: attrs.has("selected"),
                disabled: attrs.has("disabled"),
                text: String::new(),
            });
        }
    }

    pub(super) fn end_option(&mut self) {
        let Some(select) = &mut self.select else {
            return;
        };
        let Some(option) = select.option.take() else {
            return;
        };
        let label = clean(&option.text);
        // A blank value is the "choose…" placeholder, not an answer.
        let value = option.value.unwrap_or_else(|| label.clone());
        if !option.disabled && !value.is_empty() {
            select.field.options.push(Choice {
                label: if label.is_empty() {
                    value.clone()
                } else {
                    label
                },
                value,
                checked: option.selected,
            });
        }
    }

    pub(super) fn end_select(&mut self) {
        self.end_option();
        if let Some(select) = self.select.take() {
            self.items.push(Item::Field(select.field));
        }
    }

    pub(super) fn start_area(&mut self, attrs: &Attrs) {
        self.flush();
        self.area = Some(Area {
            attrs: attrs.clone(),
            text: String::new(),
        });
    }

    pub(super) fn end_area(&mut self) {
        let Some(area) = self.area.take() else {
            return;
        };
        let attrs = &area.attrs;
        let Some(name) = attrs.text("name").filter(|_| !attrs.has("disabled")) else {
            return;
        };
        let spec = Spec {
            input: "textarea".to_owned(),
            multiline: true,
            value: area
                .text
                .strip_prefix('\n')
                .unwrap_or(&area.text)
                .to_owned(),
            ..text_spec(attrs)
        };
        let at = self.push_field(&name, Kind::Text(Box::new(spec)), attrs);
        self.register(attrs, Target::Question(at));
    }
}

/// The constraints every text-like control shares.
fn text_spec(attrs: &Attrs) -> Spec {
    let count = |name| attrs.get(name).and_then(|v| v.trim().parse().ok());
    Spec {
        placeholder: attrs.get("placeholder").unwrap_or_default().to_owned(),
        min: attrs.text("min"),
        max: attrs.text("max"),
        min_len: count("minlength"),
        max_len: count("maxlength"),
        pattern: attrs.text("pattern"),
        title: attrs.text("title"),
        ..Spec::default()
    }
}
