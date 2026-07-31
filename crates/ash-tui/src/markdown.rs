use pulldown_cmark::{
    CodeBlockKind, Event as MarkdownEvent, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line as RatatuiLine, Span as RatatuiSpan},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::text_width::truncate_end;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StyledSpan {
    pub(crate) text: String,
    pub(crate) style: Style,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RenderedLine {
    spans: Vec<StyledSpan>,
}

impl RenderedLine {
    pub(crate) fn is_blank(&self) -> bool {
        self.spans.iter().all(|span| span.text.trim().is_empty())
    }

    pub(crate) fn patch_style(&mut self, style: Style) {
        for span in &mut self.spans {
            span.style = span.style.patch(style);
        }
    }

    pub(crate) fn ratatui_line(&self) -> RatatuiLine<'_> {
        RatatuiLine::from(
            self.spans
                .iter()
                .map(|span| RatatuiSpan::styled(span.text.as_str(), span.style))
                .collect::<Vec<_>>(),
        )
    }

    #[cfg(test)]
    pub(crate) fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StreamingMarkdownCache {
    width: Option<u16>,
    stable_source_len: usize,
    stable_lines: Vec<RenderedLine>,
}

impl StreamingMarkdownCache {
    pub(crate) fn update(&mut self, source: &str, width: u16) -> Vec<RenderedLine> {
        let width = width.max(1);
        if self.width != Some(width) || self.stable_source_len > source.len() {
            self.width = Some(width);
            self.stable_source_len = 0;
            self.stable_lines.clear();
        }

        let remaining = &source[self.stable_source_len..];
        if let Some(split) = stable_markdown_split(remaining) {
            let fragment = render_markdown(&remaining[..split], width);
            if !fragment.is_empty() {
                if !self.stable_lines.is_empty() {
                    self.stable_lines.push(RenderedLine::default());
                }
                self.stable_lines.extend(fragment);
            }
            self.stable_source_len += split;
        }

        render_markdown(&source[self.stable_source_len..], width)
    }

    pub(crate) fn stable_lines(&self) -> &[RenderedLine] {
        &self.stable_lines
    }

    pub(crate) fn latest_lines(&self, tail: &[RenderedLine], maximum: usize) -> Vec<RenderedLine> {
        let has_gap = !self.stable_lines.is_empty() && !tail.is_empty();
        let total = self
            .stable_lines
            .len()
            .saturating_add(usize::from(has_gap))
            .saturating_add(tail.len());
        let start = total.saturating_sub(maximum);
        (start..total)
            .filter_map(|index| {
                if index < self.stable_lines.len() {
                    return self.stable_lines.get(index).cloned();
                }
                if has_gap && index == self.stable_lines.len() {
                    return Some(RenderedLine::default());
                }
                let tail_index = index
                    .saturating_sub(self.stable_lines.len())
                    .saturating_sub(usize::from(has_gap));
                tail.get(tail_index).cloned()
            })
            .collect()
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
    inline_styles: Vec<Style>,
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

    fn current_style(&self) -> Style {
        let mut style = Style::default();
        if self.blockquote_depth > 0 {
            style = style.fg(Color::Green);
        }
        if self.code_block {
            style = style.fg(Color::Cyan);
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
                style: Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::DIM),
            });
        }

        let list_indent = "  ".repeat(self.lists.len().saturating_sub(1));
        if !list_indent.is_empty() {
            self.current.spans.push(StyledSpan {
                text: list_indent.clone(),
                style: Style::default(),
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
                style: Style::default().fg(Color::Blue),
            });
        }

        if self.code_block {
            self.current.spans.push(StyledSpan {
                text: "  ".to_string(),
                style: Style::default().add_modifier(Modifier::DIM),
            });
            self.current.continuation_indent = self.current.continuation_indent.max(2);
        }
    }

    fn push_text(&mut self, text: &str) {
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.force_line_break();
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

    fn finish_line(&mut self) {
        if !self.current.spans.is_empty() {
            self.lines.push(std::mem::take(&mut self.current));
        }
    }

    fn force_line_break(&mut self) {
        self.lines.push(std::mem::take(&mut self.current));
    }

    fn block_gap(&mut self) {
        self.finish_line();
        if self.needs_block_gap {
            self.push_blank_line();
        }
        self.needs_block_gap = false;
    }

    fn push_blank_line(&mut self) {
        self.finish_line();
        if self.lines.is_empty() || self.lines.last().is_some_and(logical_line_is_blank) {
            return;
        }
        if self.blockquote_depth > 0 {
            self.ensure_prefix();
            self.force_line_break();
        } else {
            self.lines.push(LogicalLine::default());
        }
    }

    fn push_style(&mut self, style: Style) {
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
        self.finish_line();
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
        self.finish_line();
        let start_line_count = self.list_item_start_line_counts.pop().unwrap_or_default();
        if self.lines.len().saturating_sub(start_line_count) > 1 {
            if let Some(needs_blank) = self.list_needs_blank_before_next_item.last_mut() {
                *needs_blank = true;
            }
        }
        self.item_marker = None;
        self.item_marker_used = false;
    }

    fn handle_table_event(&mut self, event: MarkdownEvent<'_>) -> bool {
        let Some(table) = self.table.as_mut() else {
            return false;
        };
        match event {
            MarkdownEvent::Start(Tag::TableHead) => {
                table.in_head = true;
                table.row.clear();
            }
            MarkdownEvent::End(TagEnd::TableHead) => {
                if !table.cell.is_empty() {
                    table.row.push(std::mem::take(&mut table.cell));
                }
                table.header = Some(std::mem::take(&mut table.row));
                table.in_head = false;
            }
            MarkdownEvent::Start(Tag::TableRow) => table.row.clear(),
            MarkdownEvent::End(TagEnd::TableRow) => {
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
            MarkdownEvent::Start(Tag::TableCell) => table.cell.clear(),
            MarkdownEvent::End(TagEnd::TableCell) => {
                table.row.push(table.cell.trim().to_string());
                table.cell.clear();
            }
            MarkdownEvent::Text(text) | MarkdownEvent::Code(text) | MarkdownEvent::Html(text) => {
                if !table.cell.is_empty() {
                    table.cell.push(' ');
                }
                table.cell.push_str(text.trim());
            }
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => table.cell.push(' '),
            MarkdownEvent::End(TagEnd::Table) => return true,
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

        let push_row = |writer: &mut Self, row: &[String], style: Style| {
            let mut line = LogicalLine::default();
            for (column, column_width) in widths.iter().copied().enumerate().take(columns) {
                if column > 0 {
                    line.spans.push(StyledSpan {
                        text: "  ".to_string(),
                        style: Style::default().add_modifier(Modifier::DIM),
                    });
                }
                let value = row.get(column).map(String::as_str).unwrap_or("");
                let value = truncate_end(value, column_width);
                let padding = column_width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
                line.spans.push(StyledSpan {
                    text: format!("{value}{}", " ".repeat(padding)),
                    style,
                });
            }
            writer.lines.push(line);
        };

        if let Some(header) = table.header {
            push_row(self, &header, Style::default().add_modifier(Modifier::BOLD));
            let separator = widths
                .iter()
                .map(|width| "─".repeat(*width))
                .collect::<Vec<_>>()
                .join("  ");
            self.lines.push(LogicalLine {
                spans: vec![StyledSpan {
                    text: separator,
                    style: Style::default().add_modifier(Modifier::DIM),
                }],
                continuation_indent: 0,
            });
        }
        for row in table.rows {
            push_row(self, &row, Style::default());
        }
        self.needs_block_gap = true;
    }

    fn run(mut self, source: &str) -> Vec<RenderedLine> {
        for event in Parser::new_ext(source, markdown_options()) {
            if self.table.is_some() {
                if self.handle_table_event(event) {
                    self.finish_table();
                }
                continue;
            }

            match event {
                MarkdownEvent::Start(Tag::Paragraph) => self.block_gap(),
                MarkdownEvent::End(TagEnd::Paragraph) => {
                    self.finish_line();
                    self.needs_block_gap = true;
                }
                MarkdownEvent::Start(Tag::Heading { level, .. }) => {
                    self.block_gap();
                    self.push_style(heading_style(level));
                }
                MarkdownEvent::End(TagEnd::Heading(_)) => {
                    self.finish_line();
                    self.pop_style();
                    self.needs_block_gap = true;
                }
                MarkdownEvent::Start(Tag::BlockQuote(_)) => {
                    self.block_gap();
                    self.blockquote_depth += 1;
                }
                MarkdownEvent::End(TagEnd::BlockQuote(_)) => {
                    self.finish_line();
                    self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                    self.needs_block_gap = true;
                }
                MarkdownEvent::Start(Tag::CodeBlock(kind)) => {
                    self.block_gap();
                    self.code_block = true;
                    if let CodeBlockKind::Fenced(language) = kind {
                        if !language.trim().is_empty() {
                            self.ensure_prefix();
                            self.current.spans.push(StyledSpan {
                                text: language.to_string(),
                                style: Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                            });
                            self.finish_line();
                        }
                    }
                }
                MarkdownEvent::End(TagEnd::CodeBlock) => {
                    self.finish_line();
                    self.code_block = false;
                    self.needs_block_gap = true;
                }
                MarkdownEvent::Start(Tag::List(start)) => {
                    if self.lists.is_empty() {
                        self.block_gap();
                    }
                    self.lists.push(ListState { next: start });
                    self.list_needs_blank_before_next_item.push(false);
                }
                MarkdownEvent::End(TagEnd::List(_)) => {
                    self.lists.pop();
                    self.list_needs_blank_before_next_item.pop();
                    self.needs_block_gap = true;
                }
                MarkdownEvent::Start(Tag::Item) => self.start_item(),
                MarkdownEvent::End(TagEnd::Item) => self.finish_item(),
                MarkdownEvent::Start(Tag::Emphasis) => {
                    self.push_style(Style::default().add_modifier(Modifier::ITALIC))
                }
                MarkdownEvent::End(TagEnd::Emphasis) => self.pop_style(),
                MarkdownEvent::Start(Tag::Strong) => {
                    self.push_style(Style::default().add_modifier(Modifier::BOLD))
                }
                MarkdownEvent::End(TagEnd::Strong) => self.pop_style(),
                MarkdownEvent::Start(Tag::Strikethrough) => {
                    self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT))
                }
                MarkdownEvent::End(TagEnd::Strikethrough) => self.pop_style(),
                MarkdownEvent::Start(Tag::Link { .. }) => self.push_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                MarkdownEvent::End(TagEnd::Link) => self.pop_style(),
                MarkdownEvent::Start(Tag::Table(_)) => {
                    self.block_gap();
                    self.table = Some(TableState::default());
                }
                MarkdownEvent::Text(text) | MarkdownEvent::Html(text) => self.push_text(&text),
                MarkdownEvent::Code(code) => {
                    let style = Style::default().fg(Color::Cyan);
                    self.push_style(style);
                    self.push_text(&code);
                    self.pop_style();
                }
                MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => self.force_line_break(),
                MarkdownEvent::Rule => {
                    self.block_gap();
                    self.lines.push(LogicalLine {
                        spans: vec![StyledSpan {
                            text: "─".repeat(self.width.min(24)),
                            style: Style::default().add_modifier(Modifier::DIM),
                        }],
                        continuation_indent: 0,
                    });
                    self.needs_block_gap = true;
                }
                MarkdownEvent::TaskListMarker(checked) => {
                    self.push_text(if checked { "[x] " } else { "[ ] " });
                }
                MarkdownEvent::FootnoteReference(reference) => {
                    self.push_text(&format!("[{reference}]"));
                }
                MarkdownEvent::InlineHtml(html) => self.push_text(&html),
                _ => {}
            }
        }
        self.finish_line();

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

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HeadingLevel::H2 => Style::default().add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
            Style::default().add_modifier(Modifier::ITALIC)
        }
    }
}

fn logical_line_is_blank(line: &LogicalLine) -> bool {
    line.spans.iter().all(|span| span.text.trim().is_empty())
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
                        style: Style::default(),
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

fn markdown_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options
}

fn stable_markdown_split(source: &str) -> Option<usize> {
    let mut block_depth = 0usize;
    let mut top_level_starts = Vec::new();
    let mut contains_html_block = false;

    for (event, range) in Parser::new_ext(source, markdown_options()).into_offset_iter() {
        match event {
            MarkdownEvent::Start(tag) if is_block_tag(&tag) => {
                contains_html_block |= matches!(tag, Tag::HtmlBlock);
                if block_depth == 0 {
                    top_level_starts.push(range.start);
                }
                block_depth += 1;
            }
            MarkdownEvent::End(tag) if is_block_end(tag) => {
                block_depth = block_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    if contains_html_block {
        return None;
    }

    // A blank line prevents an incomplete table row or list continuation from
    // retroactively changing a block that has already been cached.
    top_level_starts
        .get(1..)?
        .iter()
        .rev()
        .copied()
        .find(|start| follows_blank_line(source, *start))
}

fn follows_blank_line(source: &str, start: usize) -> bool {
    let Some(before) = source.get(..start) else {
        return false;
    };
    let before = before.strip_suffix('\n').unwrap_or(before);
    before
        .rsplit_once('\n')
        .map_or(before, |(_, line)| line)
        .trim_matches([' ', '\t', '\r'])
        .is_empty()
}

fn is_block_tag(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::BlockQuote(_)
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::List(_)
            | Tag::Item
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::MetadataBlock(_)
    )
}

fn is_block_end(tag: TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::MetadataBlock(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[RenderedLine]) -> Vec<String> {
        lines.iter().map(RenderedLine::plain_text).collect()
    }

    fn assert_streaming_matches_full(source: &str, width: u16) {
        let mut cache = StreamingMarkdownCache::default();
        for (end, _) in source
            .char_indices()
            .skip(1)
            .chain(std::iter::once((source.len(), '\0')))
        {
            let prefix = &source[..end];
            let tail = cache.update(prefix, width);
            let mut actual = cache.stable_lines().to_vec();
            if !actual.is_empty() && !tail.is_empty() {
                actual.push(RenderedLine::default());
            }
            actual.extend(tail);
            assert_eq!(actual, render_markdown(prefix, width), "prefix: {prefix:?}");
        }
    }

    #[test]
    fn streaming_cache_matches_full_render_for_block_markdown() {
        for source in [
            "## One\n## Two\n\nParagraph with **bold** and `code`.",
            "> first\n>\n> second\n\nAfter the quote.",
            "Before\n\n```rust\nfn main() {\n\n}\n```\n\nAfter",
            "1. First paragraph\n\n   Second paragraph\n\n2. Next item\n\nAfter",
            "- outer\n  - inner\n  - next\n- final\n\nAfter",
            "| Name | State |\n|---|---|\n| ash | ready |\n\nAfter",
            "Before\n\n---\n\nAfter\n\nAnother paragraph",
            "<section>\nhtml block\n</section>\n\nAfter",
        ] {
            assert_streaming_matches_full(source, 40);
        }
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
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(lines[2].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(lines[2].spans[3].style.fg, Some(Color::Cyan));
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
        assert!(lines
            .iter()
            .all(|line| line.spans[0].style.add_modifier.contains(Modifier::BOLD)));
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
