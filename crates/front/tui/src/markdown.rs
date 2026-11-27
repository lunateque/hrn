//! Markdown to styled terminal lines on top of `pulldown-cmark`. A streaming message is simply
//! re-rendered from its full text: unfinished constructs (an unclosed fence, a half table) render
//! as what they are so far and settle once complete.

use crate::highlight;
use crate::table::Table;
use crate::theme;
use crate::wrap::{self, Seg};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

const BULLET: &str = "- ";
/// Horizontal rules stop here however wide the column is.
const RULE_MAX: usize = 80;

/// Render `text` into lines at most `width` columns wide; plain text takes `base` (reasoning is
/// muted italic), other constructs paint over it.
pub(crate) fn render(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    let mut r = Renderer {
        width: width.max(8),
        base,
        out: Vec::new(),
        inline: Vec::new(),
        styles: Vec::new(),
        containers: Vec::new(),
        lists: Vec::new(),
        links: Vec::new(),
        blank_pending: false,
        code: None,
        table: None,
        skipping: false,
    };
    for event in Parser::new_ext(text, options) {
        r.event(event);
    }
    r.flush();
    r.out
}

enum Container {
    Quote,
    Item { marker: String, fresh: bool },
}

struct Renderer {
    width: usize,
    base: Style,
    out: Vec<Line<'static>>,
    inline: Vec<Seg>,
    styles: Vec<Style>,
    containers: Vec<Container>,
    /// Next number of each open list; `None` for bullet lists.
    lists: Vec<Option<u64>>,
    /// Open links: destination and the text seen so far.
    links: Vec<(String, String)>,
    blank_pending: bool,
    /// An open code block: its fence info (`None` when indented) and the text so far.
    code: Option<(Option<String>, String)>,
    table: Option<Table>,
    skipping: bool,
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

impl Renderer {
    fn style(&self) -> Style {
        self.styles.iter().fold(self.base, |acc, s| acc.patch(*s))
    }

    /// Line prefix from the open containers. `consume` marks fresh list markers as printed, so
    /// only the first line of an item shows its bullet.
    fn prefix(&mut self, consume: bool) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for container in &mut self.containers {
            match container {
                Container::Quote => spans.push(Span::styled("▏ ", theme::border())),
                Container::Item { marker, fresh } => {
                    if *fresh {
                        spans.push(Span::styled(marker.clone(), theme::accent()));
                        *fresh = !consume;
                    } else {
                        spans.push(Span::raw(" ".repeat(wrap::str_width(marker))));
                    }
                }
            }
        }
        spans
    }

    /// Emit the blank separator owed by the previous block, if any. It carries the quote bars of
    /// the open containers but no trailing indentation.
    fn open_block(&mut self) {
        if self.blank_pending && !self.out.is_empty() {
            let bars = self
                .containers
                .iter()
                .rposition(|c| matches!(c, Container::Quote))
                .map_or(0, |i| i + 1);
            let spans: Vec<Span<'static>> = self
                .containers
                .iter()
                .take(bars)
                .map(|c| match c {
                    Container::Quote => Span::styled("▏", theme::border()),
                    Container::Item { marker, .. } => {
                        Span::raw(" ".repeat(wrap::str_width(marker)))
                    }
                })
                .collect();
            self.out.push(Line::from(spans));
        }
        self.blank_pending = false;
    }

    fn flush(&mut self) {
        if self.inline.iter().all(|(t, _)| t.trim().is_empty()) {
            self.inline.clear();
            return;
        }
        let segs = std::mem::take(&mut self.inline);
        self.open_block();
        let first = self.prefix(true);
        let rest = self.prefix(false);
        let lines = wrap::wrap(&segs, self.width, &first, &rest);
        self.out.extend(lines);
    }

    fn push(&mut self, text: &str, style: Style) {
        let text = wrap::sanitize(text);
        for (_, seen) in &mut self.links {
            seen.push_str(&text);
        }
        if let Some(table) = &mut self.table {
            table.push(&text, style);
            return;
        }
        match self.inline.last_mut() {
            Some((t, s)) if *s == style => t.push_str(&text),
            _ => self.inline.push((text.into_owned(), style)),
        }
    }

    fn push_styled(&mut self, text: &str, extra: Style) {
        let style = self.style().patch(extra);
        self.push(text, style);
    }

    fn event(&mut self, event: Event<'_>) {
        if self.skipping {
            self.skipping = !matches!(event, Event::End(TagEnd::MetadataBlock(_)));
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, code)) = &mut self.code {
                    code.push_str(&text);
                } else {
                    self.push_styled(&text, Style::new());
                }
            }
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push_styled(&text, theme::inline_code());
            }
            Event::Html(text) | Event::InlineHtml(text) => self.push(&text, theme::dim()),
            Event::FootnoteReference(label) => self.push(&format!("[^{label}]"), theme::dim()),
            Event::SoftBreak => self.push_styled(" ", Style::new()),
            Event::HardBreak => self.push_styled("\n", Style::new()),
            Event::TaskListMarker(done) => {
                let (mark, style) = if done {
                    ("[x] ", theme::fg(theme::SUCCESS))
                } else {
                    ("[ ] ", theme::muted())
                };
                self.push(mark, style);
            }
            Event::Rule => {
                self.flush();
                self.open_block();
                let mut spans = self.prefix(true);
                let used: usize = spans.iter().map(Span::width).sum();
                let rule = "─".repeat(self.width.saturating_sub(used).min(RULE_MAX));
                spans.push(Span::styled(rule, theme::border()));
                self.out.push(Line::from(spans));
                self.blank_pending = true;
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock | Tag::DefinitionList => self.flush(),
            Tag::Heading { level, .. } => {
                self.flush();
                let level = heading_level(level);
                self.styles.push(theme::heading(level));
                if level >= 3 {
                    let marks = format!("{} ", "#".repeat(usize::from(level)));
                    self.push_styled(&marks, Style::new());
                }
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.open_block();
                self.containers.push(Container::Quote);
                self.styles
                    .push(theme::muted().add_modifier(Modifier::ITALIC));
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let info = match kind {
                    CodeBlockKind::Fenced(info) => Some(info.into_string()),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((info, String::new()));
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => BULLET.to_owned(),
                };
                self.containers.push(Container::Item {
                    marker,
                    fresh: true,
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.flush();
                self.containers.push(Container::Item {
                    marker: format!("[^{label}] "),
                    fresh: true,
                });
            }
            Tag::DefinitionListTitle => {
                self.flush();
                self.styles.push(theme::bold());
            }
            Tag::DefinitionListDefinition => {
                self.flush();
                self.containers.push(Container::Item {
                    marker: "  ".to_owned(),
                    fresh: true,
                });
            }
            Tag::Table(aligns) => {
                self.flush();
                self.table = Some(Table::new(aligns));
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Superscript | Tag::Subscript => self.styles.push(Style::new()),
            Tag::Link { dest_url, .. } => {
                self.styles.push(theme::link());
                self.links.push((dest_url.into_string(), String::new()));
            }
            Tag::Image { .. } => self.push("[image: ", theme::dim()),
            Tag::MetadataBlock(_) => self.skipping = true,
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock | TagEnd::DefinitionList => {
                self.flush();
                self.blank_pending = true;
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
                self.blank_pending = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.styles.pop();
                self.containers.pop();
                self.blank_pending = true;
            }
            TagEnd::CodeBlock => {
                self.emit_code();
                self.blank_pending = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                let nested = self
                    .containers
                    .iter()
                    .any(|c| matches!(c, Container::Item { .. }));
                self.blank_pending |= !nested;
            }
            TagEnd::Item | TagEnd::FootnoteDefinition | TagEnd::DefinitionListDefinition => {
                self.flush();
                self.containers.pop();
            }
            TagEnd::DefinitionListTitle => {
                self.styles.pop();
                self.flush();
            }
            TagEnd::Table => self.emit_table(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.end_head();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    table.end_row();
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table.end_cell();
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, text)) = self.links.pop()
                    && !url.is_empty()
                    && !url.starts_with('#')
                    && url != text
                    && url.strip_prefix("mailto:") != Some(text.as_str())
                {
                    self.push(&format!(" ({url})"), theme::link_url());
                }
            }
            TagEnd::Image => self.push("]", theme::dim()),
            TagEnd::MetadataBlock(_) => {}
        }
    }

    fn emit_code(&mut self) {
        let Some((info, code)) = self.code.take() else {
            return;
        };
        self.open_block();
        let code = wrap::sanitize(&code);
        let body = code.strip_suffix('\n').unwrap_or(&code);
        let tag = info.as_deref().and_then(|i| i.split_whitespace().next());
        let lang = tag.and_then(highlight::lang);
        let first = self.prefix(true);
        let rest = self.prefix(false);
        let fence = |text: String| vec![(text, theme::border())];
        if let Some(info) = &info {
            let open = fence(format!("```{}", info.trim()));
            self.out
                .extend(wrap::wrap(&open, self.width, &first, &rest));
        }
        let first_code = if info.is_some() { &rest } else { &first };
        for (i, segs) in highlight::lines(body, lang, theme::code_block())
            .into_iter()
            .enumerate()
        {
            let lead = if i == 0 { first_code } else { &rest };
            self.out.extend(wrap::wrap(&segs, self.width, lead, &rest));
        }
        if info.is_some() {
            self.out.extend(wrap::wrap(
                &fence("```".to_owned()),
                self.width,
                &rest,
                &rest,
            ));
        }
    }

    fn emit_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        self.open_block();
        let first = self.prefix(true);
        let rest = self.prefix(false);
        self.out.extend(table.render(self.width, &first, &rest));
        self.blank_pending = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str, width: usize) -> Vec<String> {
        render(text, width, Style::new())
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn span_style(lines: &[Line<'_>], text: &str) -> Option<Style> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content == text)
            .map(|s| s.style)
    }

    #[test]
    fn blocks_are_separated_and_styled() {
        let text = "# Title\n\nSome **bold**, *it* and `code`.\n\n```rust\nfn main() {}\n```\n";
        let lines = render(text, 40, Style::new());
        let shown: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "Title",
                "",
                "Some bold, it and code.",
                "",
                "```rust",
                "fn main() {}",
                "```"
            ]
        );
        let bold = span_style(&lines, "bold").expect("bold span");
        assert!(bold.add_modifier.contains(Modifier::BOLD));
        let italic = span_style(&lines, "it").expect("italic span");
        assert!(italic.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(span_style(&lines, "code"), Some(theme::inline_code()));
        assert_eq!(span_style(&lines, "Title"), Some(theme::heading(1)));
        assert_eq!(span_style(&lines, "```rust"), Some(theme::border()));
        assert_eq!(
            span_style(&lines, "fn"),
            Some(theme::fg(theme::SYN_KEYWORD)),
            "highlighted"
        );
    }

    #[test]
    fn deeper_headings_show_their_marks_and_base_styles_the_text() {
        assert_eq!(plain("## Two\n\n### Three", 30), ["Two", "", "### Three"]);
        let base = theme::reasoning();
        let lines = render("plain **strong**", 30, base);
        assert_eq!(span_style(&lines, "plain "), Some(base));
        let strong = span_style(&lines, "strong").expect("strong span");
        assert!(strong.add_modifier.contains(Modifier::BOLD));
        assert!(strong.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn lists_nest_with_hanging_indents() {
        let text = "- one\n- two that wraps here\n  - inner\n1. first\n2. second\n";
        assert_eq!(
            plain(text, 16),
            [
                "- one",
                "- two that wraps",
                "  here",
                "  - inner",
                "",
                "1. first",
                "2. second",
            ]
        );
    }

    #[test]
    fn tables_quotes_links_and_rules() {
        let text = "| a | b |\n|---|--:|\n| x | 10 |\n\n> quoted\n\n\
                    [site](https://e.x) and <https://a.b>\n\n***\n";
        let shown = plain(text, 24);
        assert_eq!(
            shown,
            [
                "┌───┬────┐",
                "│ a │  b │",
                "├───┼────┤",
                "│ x │ 10 │",
                "└───┴────┘",
                "",
                "▏ quoted",
                "",
                "site (https://e.x) and",
                "https://a.b",
                "",
                "────────────────────────",
            ]
        );
        let wide = plain("***", 120);
        assert_eq!(wide.first().map(|r| r.chars().count()), Some(80));
    }

    #[test]
    fn unfinished_streams_render_what_exists() {
        assert_eq!(
            plain("```\nlet x = 1;\nlet", 30),
            ["```", "let x = 1;", "let", "```"]
        );
        assert_eq!(plain("**half bold", 30), ["**half bold"]);
    }
}
