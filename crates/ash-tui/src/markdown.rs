use std::io::{self, Write};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color as RatatuiColor, Modifier, Style},
    text::{Line as RatatuiLine, Span as RatatuiSpan},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum AnsiColor {
    #[default]
    Default,
    Cyan,
    Green,
    Blue,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TextStyle {
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) underlined: bool,
    pub(crate) crossed_out: bool,
    pub(crate) color: AnsiColor,
}

impl TextStyle {
    pub(crate) const fn bold() -> Self {
        Self {
            bold: true,
            ..Self::plain()
        }
    }

    pub(crate) const fn dim() -> Self {
        Self {
            dim: true,
            ..Self::plain()
        }
    }

    pub(crate) const fn dim_italic() -> Self {
        Self {
            dim: true,
            italic: true,
            ..Self::plain()
        }
    }

    pub(crate) const fn plain() -> Self {
        Self {
            bold: false,
            dim: false,
            italic: false,
            underlined: false,
            crossed_out: false,
            color: AnsiColor::Default,
        }
    }

    fn patch(self, other: Self) -> Self {
        Self {
            bold: self.bold || other.bold,
            dim: self.dim || other.dim,
            italic: self.italic || other.italic,
            underlined: self.underlined || other.underlined,
            crossed_out: self.crossed_out || other.crossed_out,
            color: if other.color == AnsiColor::Default {
                self.color
            } else {
                other.color
            },
        }
    }

    fn write_prefix(self, writer: &mut impl Write) -> io::Result<()> {
        write!(writer, "{}", theme::RESET)?;
        if self.bold {
            write!(writer, "\x1b[1m")?;
        }
        if self.dim {
            write!(writer, "\x1b[2m")?;
        }
        if self.italic {
            write!(writer, "\x1b[3m")?;
        }
        if self.underlined {
            write!(writer, "\x1b[4m")?;
        }
        if self.crossed_out {
            write!(writer, "\x1b[9m")?;
        }
        let color = match self.color {
            AnsiColor::Default => "",
            AnsiColor::Cyan => "\x1b[36m",
            AnsiColor::Green => "\x1b[32m",
            AnsiColor::Blue => "\x1b[34m",
        };
        write!(writer, "{color}")
    }

    fn ratatui_style(self) -> Style {
        let mut style = Style::default();
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.dim {
            style = style.add_modifier(Modifier::DIM);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.underlined {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if self.crossed_out {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        match self.color {
            AnsiColor::Default => style,
            AnsiColor::Cyan => style.fg(RatatuiColor::Cyan),
            AnsiColor::Green => style.fg(RatatuiColor::Green),
            AnsiColor::Blue => style.fg(RatatuiColor::Blue),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StyledSpan {
    pub(crate) text: String,
    pub(crate) style: TextStyle,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RenderedLine {
    spans: Vec<StyledSpan>,
}

impl RenderedLine {
    pub(crate) fn is_blank(&self) -> bool {
        self.spans.iter().all(|span| span.text.trim().is_empty())
    }

    pub(crate) fn patch_style(&mut self, style: TextStyle) {
        for span in &mut self.spans {
            span.style = span.style.patch(style);
        }
    }

    pub(crate) fn write_ansi(&self, writer: &mut impl Write) -> io::Result<()> {
        for span in &self.spans {
            span.style.write_prefix(writer)?;
            write!(writer, "{}", span.text)?;
        }
        write!(writer, "{}", theme::RESET)
    }

    pub(crate) fn ratatui_line(&self) -> RatatuiLine<'static> {
        RatatuiLine::from(
            self.spans
                .iter()
                .map(|span| RatatuiSpan::styled(span.text.clone(), span.style.ratatui_style()))
                .collect::<Vec<_>>(),
        )
    }

    #[cfg(test)]
    pub(crate) fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

#[derive(Clone, Debug, Default)]
struct LogicalLine {
    spans: Vec<StyledSpan>,
    continuation_indent: usize,
}

#[derive(Clone, Debug)]
struct ListState {
    next: Option<u64>,
}

#[derive(Default)]
struct TableState {
    header: Option<Vec<String>>,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    in_head: bool,
}

struct MarkdownWriter {
    width: usize,
    lines: Vec<LogicalLine>,
    current: LogicalLine,
    inline_styles: Vec<TextStyle>,
    needs_block_gap: bool,
    blockquote_depth: usize,
    code_block: bool,
    lists: Vec<ListState>,
    list_needs_blank_before_next_item: Vec<bool>,
    list_item_start_line_counts: Vec<usize>,
    item_marker: Option<String>,
    item_marker_used: bool,
    table: Option<TableState>,
}

impl MarkdownWriter {
    fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            lines: Vec::new(),
            current: LogicalLine::default(),
            inline_styles: Vec::new(),
            needs_block_gap: false,
            blockquote_depth: 0,
            code_block: false,
            lists: Vec::new(),
            list_needs_blank_before_next_item: Vec::new(),
            list_item_start_line_counts: Vec::new(),
            item_marker: None,
            item_marker_used: false,
            table: None,
        }
    }

    fn current_style(&self) -> TextStyle {
        let mut style = TextStyle::plain();
        if self.blockquote_depth > 0 {
            style.color = AnsiColor::Green;
        }
        if self.code_block {
            style.color = AnsiColor::Cyan;
        }
        for inline in &self.inline_styles {
            style = style.patch(*inline);
        }
        style
    }

    fn ensure_prefix(&mut self) {
        if !self.current.spans.is_empty() {
            return;
        }

        let quote_prefix = "│ ".repeat(self.blockquote_depth);
        if !quote_prefix.is_empty() {
            self.current.spans.push(StyledSpan {
                text: quote_prefix,
                style: TextStyle {
                    color: AnsiColor::Green,
                    dim: true,
                    ..TextStyle::plain()
                },
            });
        }

        let list_indent = "  ".repeat(self.lists.len().saturating_sub(1));
        if !list_indent.is_empty() {
            self.current.spans.push(StyledSpan {
                text: list_indent.clone(),
                style: TextStyle::plain(),
            });
        }

        if let Some(marker) = self.item_marker.clone() {
            let prefix = if self.item_marker_used {
                " ".repeat(UnicodeWidthStr::width(marker.as_str()))
            } else {
                self.item_marker_used = true;
                marker
            };
            self.current.continuation_indent = UnicodeWidthStr::width(prefix.as_str())
                + UnicodeWidthStr::width(list_indent.as_str())
                + UnicodeWidthStr::width("│ ") * self.blockquote_depth;
            self.current.spans.push(StyledSpan {
                text: prefix,
                style: TextStyle {
                    color: AnsiColor::Blue,
                    ..TextStyle::plain()
                },
            });
        }

        if self.code_block {
            self.current.spans.push(StyledSpan {
                text: "  ".to_string(),
                style: TextStyle::dim(),
            });
            self.current.continuation_indent = self.current.continuation_indent.max(2);
        }
    }

    fn push_text(&mut self, text: &str) {
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.finish_line(/*force*/ true);
            }
            if part.is_empty() {
                continue;
            }
            self.ensure_prefix();
            self.current.spans.push(StyledSpan {
                text: part.to_string(),
                style: self.current_style(),
            });
        }
    }

    fn finish_line(&mut self, force: bool) {
        if force || !self.current.spans.is_empty() {
            self.lines.push(std::mem::take(&mut self.current));
        }
    }

    fn block_gap(&mut self) {
        self.finish_line(/*force*/ false);
        if self.needs_block_gap {
            self.push_blank_line();
        }
        self.needs_block_gap = false;
    }

    fn push_blank_line(&mut self) {
        self.finish_line(/*force*/ false);
        if self.lines.is_empty() || self.lines.last().is_some_and(logical_line_is_blank) {
            return;
        }
        if self.blockquote_depth > 0 {
            self.ensure_prefix();
            self.finish_line(/*force*/ true);
        } else {
            self.lines.push(LogicalLine::default());
        }
    }

    fn push_style(&mut self, style: TextStyle) {
        self.inline_styles.push(style);
    }

    fn pop_style(&mut self) {
        self.inline_styles.pop();
    }

    fn start_item(&mut self) {
        if self
            .list_needs_blank_before_next_item
            .last_mut()
            .map(std::mem::take)
            .unwrap_or(false)
        {
            self.push_blank_line();
        }
        self.finish_line(/*force*/ false);
        self.list_item_start_line_counts.push(self.lines.len());
        let marker = self.lists.last_mut().map_or_else(
            || "- ".to_string(),
            |list| match list.next.as_mut() {
                Some(next) => {
                    let marker = format!("{next}. ");
                    *next += 1;
                    marker
                }
                None => "- ".to_string(),
            },
        );
        self.item_marker = Some(marker);
        self.item_marker_used = false;
        self.needs_block_gap = false;
    }

    fn finish_item(&mut self) {
        self.finish_line(/*force*/ false);
        let start_line_count = self.list_item_start_line_counts.pop().unwrap_or_default();
        if self.lines.len().saturating_sub(start_line_count) > 1 {
            if let Some(needs_blank) = self.list_needs_blank_before_next_item.last_mut() {
                *needs_blank = true;
            }
        }
        self.item_marker = None;
        self.item_marker_used = false;
    }

    fn handle_table_event(&mut self, event: Event<'_>) -> bool {
        let Some(table) = self.table.as_mut() else {
            return false;
        };
        match event {
            Event::Start(Tag::TableHead) => {
                table.in_head = true;
                table.row.clear();
            }
            Event::End(TagEnd::TableHead) => {
                if !table.cell.is_empty() {
                    table.row.push(std::mem::take(&mut table.cell));
                }
                table.header = Some(std::mem::take(&mut table.row));
                table.in_head = false;
            }
            Event::Start(Tag::TableRow) => table.row.clear(),
            Event::End(TagEnd::TableRow) => {
                if !table.cell.is_empty() {
                    table.row.push(std::mem::take(&mut table.cell));
                }
                let row = std::mem::take(&mut table.row);
                if table.in_head {
                    table.header = Some(row);
                } else {
                    table.rows.push(row);
                }
            }
            Event::Start(Tag::TableCell) => table.cell.clear(),
            Event::End(TagEnd::TableCell) => {
                table.row.push(table.cell.trim().to_string());
                table.cell.clear();
            }
            Event::Text(text) | Event::Code(text) | Event::Html(text) => {
                if !table.cell.is_empty() {
                    table.cell.push(' ');
                }
                table.cell.push_str(text.trim());
            }
            Event::SoftBreak | Event::HardBreak => table.cell.push(' '),
            Event::End(TagEnd::Table) => return true,
            _ => {}
        }
        false
    }

    fn finish_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let mut all_rows = Vec::new();
        if let Some(header) = table.header.clone() {
            all_rows.push(header);
        }
        all_rows.extend(table.rows.clone());
        let columns = all_rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }

        let gap_width = 2 * columns.saturating_sub(1);
        let available = self.width.saturating_sub(gap_width).max(columns);
        let equal_cap = (available / columns).max(3);
        let mut widths = vec![1usize; columns];
        for row in &all_rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index]
                    .max(UnicodeWidthStr::width(cell.as_str()))
                    .min(equal_cap);
            }
        }

        let push_row = |writer: &mut Self, row: &[String], header: bool| {
            let mut line = LogicalLine::default();
            for (column, column_width) in widths.iter().copied().enumerate().take(columns) {
                if column > 0 {
                    line.spans.push(StyledSpan {
                        text: "  ".to_string(),
                        style: TextStyle::dim(),
                    });
                }
                let value = row.get(column).map(String::as_str).unwrap_or("");
                let value = fit_plain(value, column_width);
                let padding = column_width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
                line.spans.push(StyledSpan {
                    text: format!("{value}{}", " ".repeat(padding)),
                    style: if header {
                        TextStyle::bold()
                    } else {
                        TextStyle::plain()
                    },
                });
            }
            writer.lines.push(line);
        };

        if let Some(header) = table.header {
            push_row(self, &header, true);
            let separator = widths
                .iter()
                .map(|width| "─".repeat(*width))
                .collect::<Vec<_>>()
                .join("  ");
            self.lines.push(LogicalLine {
                spans: vec![StyledSpan {
                    text: separator,
                    style: TextStyle::dim(),
                }],
                continuation_indent: 0,
            });
        }
        for row in table.rows {
            push_row(self, &row, false);
        }
        self.needs_block_gap = true;
    }

    fn run(mut self, source: &str) -> Vec<RenderedLine> {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_TASKLISTS);

        for event in Parser::new_ext(source, options) {
            if self.table.is_some() {
                if self.handle_table_event(event) {
                    self.finish_table();
                }
                continue;
            }

            match event {
                Event::Start(Tag::Paragraph) => self.block_gap(),
                Event::End(TagEnd::Paragraph) => {
                    self.finish_line(/*force*/ false);
                    self.needs_block_gap = true;
                }
                Event::Start(Tag::Heading { level, .. }) => {
                    self.block_gap();
                    self.push_style(heading_style(level));
                }
                Event::End(TagEnd::Heading(_)) => {
                    self.finish_line(/*force*/ false);
                    self.pop_style();
                    self.needs_block_gap = true;
                }
                Event::Start(Tag::BlockQuote) => {
                    self.block_gap();
                    self.blockquote_depth += 1;
                }
                Event::End(TagEnd::BlockQuote) => {
                    self.finish_line(/*force*/ false);
                    self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                    self.needs_block_gap = true;
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    self.block_gap();
                    self.code_block = true;
                    if let CodeBlockKind::Fenced(language) = kind {
                        if !language.trim().is_empty() {
                            self.ensure_prefix();
                            self.current.spans.push(StyledSpan {
                                text: language.to_string(),
                                style: TextStyle {
                                    dim: true,
                                    color: AnsiColor::Cyan,
                                    ..TextStyle::plain()
                                },
                            });
                            self.finish_line(/*force*/ false);
                        }
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    self.finish_line(/*force*/ false);
                    self.code_block = false;
                    self.needs_block_gap = true;
                }
                Event::Start(Tag::List(start)) => {
                    if self.lists.is_empty() {
                        self.block_gap();
                    }
                    self.lists.push(ListState { next: start });
                    self.list_needs_blank_before_next_item.push(false);
                }
                Event::End(TagEnd::List(_)) => {
                    self.lists.pop();
                    self.list_needs_blank_before_next_item.pop();
                    self.needs_block_gap = true;
                }
                Event::Start(Tag::Item) => self.start_item(),
                Event::End(TagEnd::Item) => self.finish_item(),
                Event::Start(Tag::Emphasis) => self.push_style(TextStyle {
                    italic: true,
                    ..TextStyle::plain()
                }),
                Event::End(TagEnd::Emphasis) => self.pop_style(),
                Event::Start(Tag::Strong) => self.push_style(TextStyle::bold()),
                Event::End(TagEnd::Strong) => self.pop_style(),
                Event::Start(Tag::Strikethrough) => self.push_style(TextStyle {
                    crossed_out: true,
                    ..TextStyle::plain()
                }),
                Event::End(TagEnd::Strikethrough) => self.pop_style(),
                Event::Start(Tag::Link { .. }) => self.push_style(TextStyle {
                    underlined: true,
                    color: AnsiColor::Cyan,
                    ..TextStyle::plain()
                }),
                Event::End(TagEnd::Link) => self.pop_style(),
                Event::Start(Tag::Table(_)) => {
                    self.block_gap();
                    self.table = Some(TableState::default());
                }
                Event::Text(text) | Event::Html(text) => self.push_text(&text),
                Event::Code(code) => {
                    let style = TextStyle {
                        color: AnsiColor::Cyan,
                        ..TextStyle::plain()
                    };
                    self.push_style(style);
                    self.push_text(&code);
                    self.pop_style();
                }
                Event::SoftBreak => self.finish_line(/*force*/ true),
                Event::HardBreak => self.finish_line(/*force*/ true),
                Event::Rule => {
                    self.block_gap();
                    self.lines.push(LogicalLine {
                        spans: vec![StyledSpan {
                            text: "─".repeat(self.width.min(24)),
                            style: TextStyle::dim(),
                        }],
                        continuation_indent: 0,
                    });
                    self.needs_block_gap = true;
                }
                Event::TaskListMarker(checked) => {
                    self.push_text(if checked { "[x] " } else { "[ ] " });
                }
                Event::FootnoteReference(reference) => {
                    self.push_text(&format!("[{reference}]"));
                }
                Event::InlineHtml(html) => self.push_text(&html),
                _ => {}
            }
        }
        self.finish_line(/*force*/ false);

        let mut rendered = self
            .lines
            .into_iter()
            .flat_map(|line| wrap_line(line, self.width))
            .collect::<Vec<_>>();
        while rendered.first().is_some_and(RenderedLine::is_blank) {
            rendered.remove(0);
        }
        while rendered.last().is_some_and(RenderedLine::is_blank) {
            rendered.pop();
        }
        let mut collapsed = Vec::with_capacity(rendered.len());
        for line in rendered {
            if line.is_blank() && collapsed.last().is_some_and(RenderedLine::is_blank) {
                continue;
            }
            collapsed.push(line);
        }
        collapsed
    }
}

fn heading_style(level: HeadingLevel) -> TextStyle {
    match level {
        HeadingLevel::H1 => TextStyle {
            bold: true,
            underlined: true,
            ..TextStyle::plain()
        },
        HeadingLevel::H2 => TextStyle::bold(),
        HeadingLevel::H3 => TextStyle {
            bold: true,
            italic: true,
            ..TextStyle::plain()
        },
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => TextStyle {
            italic: true,
            ..TextStyle::plain()
        },
    }
}

fn logical_line_is_blank(line: &LogicalLine) -> bool {
    line.spans.iter().all(|span| span.text.trim().is_empty())
}

fn fit_plain(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    let available = width.saturating_sub(1);
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > available {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
}

fn wrap_line(line: LogicalLine, width: usize) -> Vec<RenderedLine> {
    if line.spans.is_empty() {
        return vec![RenderedLine::default()];
    }

    let mut output = Vec::new();
    let mut current = RenderedLine::default();
    let mut current_width = 0usize;
    for span in line.spans {
        for character in span.text.chars() {
            let character_width = character.width().unwrap_or(0);
            if current_width > 0 && current_width + character_width > width {
                output.push(std::mem::take(&mut current));
                current_width = 0;
                if line.continuation_indent > 0 {
                    let indent = " ".repeat(line.continuation_indent.min(width.saturating_sub(1)));
                    current.spans.push(StyledSpan {
                        text: indent.clone(),
                        style: TextStyle::plain(),
                    });
                    current_width = UnicodeWidthStr::width(indent.as_str());
                }
            }
            if current
                .spans
                .last()
                .is_some_and(|last| last.style == span.style)
            {
                if let Some(last) = current.spans.last_mut() {
                    last.text.push(character);
                }
            } else {
                current.spans.push(StyledSpan {
                    text: character.to_string(),
                    style: span.style,
                });
            }
            current_width += character_width;
        }
    }
    output.push(current);
    output
}

pub(crate) fn render_markdown(source: &str, width: u16) -> Vec<RenderedLine> {
    MarkdownWriter::new(usize::from(width.max(1))).run(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[RenderedLine]) -> Vec<String> {
        lines.iter().map(RenderedLine::plain_text).collect()
    }

    #[test]
    fn renders_common_markdown_blocks() {
        let lines = render_markdown(
            "## Title\n\nA **bold** and `code` line.\n\n- one\n- two\n\n> quote",
            80,
        );
        assert_eq!(
            text(&lines),
            vec![
                "Title",
                "",
                "A bold and code line.",
                "",
                "- one",
                "- two",
                "",
                "│ quote",
            ]
        );
        assert!(lines[0].spans[0].style.bold);
        assert!(lines[2].spans[1].style.bold);
        assert_eq!(lines[2].spans[3].style.color, AnsiColor::Cyan);
    }

    #[test]
    fn renders_simple_tables() {
        let lines = render_markdown("| Name | State |\n|---|---|\n| ash | ready |", 40);
        assert_eq!(
            text(&lines),
            vec!["Name  State", "────  ─────", "ash   ready"]
        );
    }

    #[test]
    fn wraps_markdown_while_preserving_styles() {
        let lines = render_markdown("**abcdefghij**", 5);
        assert_eq!(text(&lines), vec!["abcde", "fghij"]);
        assert!(lines.iter().all(|line| line.spans[0].style.bold));
    }

    #[test]
    fn follows_codex_block_spacing_and_soft_breaks() {
        assert_eq!(
            text(&render_markdown("Hello\nWorld", 80)),
            vec!["Hello", "World"]
        );
        assert_eq!(
            text(&render_markdown("## One\n## Two\n\nParagraph", 80)),
            vec!["One", "", "Two", "", "Paragraph"]
        );
        assert_eq!(
            text(&render_markdown("> first\n>\n> second", 80)),
            vec!["│ first", "│ ", "│ second"]
        );
    }

    #[test]
    fn keeps_tight_lists_compact_and_separates_loose_items() {
        assert_eq!(
            text(&render_markdown("- one\n- two", 80)),
            vec!["- one", "- two"]
        );
        assert_eq!(
            text(&render_markdown(
                "1. First paragraph\n\n   Second paragraph\n\n2. Next item",
                80,
            )),
            vec![
                "1. First paragraph",
                "",
                "   Second paragraph",
                "",
                "2. Next item",
            ]
        );
    }

    #[test]
    fn separates_code_tables_and_rules_like_codex() {
        assert_eq!(
            text(&render_markdown(
                "Before\n\n```\ncode\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n---\n\nAfter",
                80,
            )),
            vec![
                "Before",
                "",
                "  code",
                "",
                "A  B",
                "─  ─",
                "1  2",
                "",
                "────────────────────────",
                "",
                "After",
            ]
        );
    }
}
