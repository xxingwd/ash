use pulldown_cmark::{
    Alignment, CodeBlockKind, Event as MarkdownEvent, HeadingLevel, LinkType, Options, Parser, Tag,
    TagEnd,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line as RatatuiLine, Span as RatatuiSpan},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StyledSpan {
    pub(crate) text: String,
    pub(crate) style: Style,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderedLine {
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
pub struct StreamingMarkdownCache {
    width: Option<u16>,
    stable_source_len: usize,
    stable_reference_links: usize,
    stable_lines: Vec<RenderedLine>,
}

impl StreamingMarkdownCache {
    pub(crate) fn update(&mut self, source: &str, width: u16) -> Vec<RenderedLine> {
        let width = width.max(1);
        if self.width != Some(width) || self.stable_source_len > source.len() {
            self.width = Some(width);
            self.stable_source_len = 0;
            self.stable_reference_links = 0;
            self.stable_lines.clear();
        }

        if self.stable_source_len > 0 {
            let reference_links = reference_link_count_before(source, self.stable_source_len);
            if reference_links != self.stable_reference_links {
                self.stable_source_len = 0;
                self.stable_reference_links = 0;
                self.stable_lines.clear();
            }
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
            self.stable_reference_links += reference_link_count_before(&remaining[..split], split);
            self.stable_source_len += split;
        }

        render_markdown(&source[self.stable_source_len..], width)
    }

    pub(crate) fn stable_lines(&self) -> &[RenderedLine] {
        &self.stable_lines
    }

    pub(crate) fn latest_lines(&self, tail: &[RenderedLine], maximum: usize) -> Vec<RenderedLine> {
        let total = combined_line_count(&self.stable_lines, tail);
        let start = total.saturating_sub(maximum);
        (start..total)
            .map(|index| {
                combined_line(&self.stable_lines, tail, index)
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }
}

/// Total combined row count: stable lines, an optional blank gap row between
/// the stable and streaming tail, then the tail lines.
pub fn combined_line_count(stable: &[RenderedLine], tail: &[RenderedLine]) -> usize {
    stable
        .len()
        .saturating_add(usize::from(!stable.is_empty() && !tail.is_empty()))
        .saturating_add(tail.len())
}

/// Maps a combined row index (stable lines, then the optional blank gap row,
/// then the streaming tail lines) to the underlying line. Returns `None` for
/// the gap row and for out-of-range indices.
pub fn combined_line<'a>(
    stable: &'a [RenderedLine],
    tail: &'a [RenderedLine],
    index: usize,
) -> Option<&'a RenderedLine> {
    if index < stable.len() {
        return stable.get(index);
    }
    let has_gap = !stable.is_empty() && !tail.is_empty();
    if has_gap && index == stable.len() {
        return None;
    }
    let tail_index = index
        .saturating_sub(stable.len())
        .saturating_sub(usize::from(has_gap));
    tail.get(tail_index)
}

#[derive(Clone, Debug, Default)]
struct LogicalLine {
    /// Prefix rendered only on the first visual row.
    initial: Vec<StyledSpan>,
    spans: Vec<StyledSpan>,
    /// Prefix re-applied to every wrapped continuation row (quote bars,
    /// list indent, marker gap, code indent).
    continuation: Vec<StyledSpan>,
    prefixed: bool,
    verbatim: bool,
}

#[derive(Clone, Debug)]
struct ListState {
    next: Option<u64>,
}

#[derive(Clone, Debug)]
struct ItemState {
    marker: String,
    marker_used: bool,
    task_marker: Option<&'static str>,
}

#[derive(Clone, Debug, Default)]
struct TableCell {
    spans: Vec<StyledSpan>,
}

impl TableCell {
    fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.text.is_empty())
    }

    fn width(&self) -> usize {
        styled_width(&self.spans)
    }
}

#[derive(Default)]
struct TableState {
    alignments: Vec<Alignment>,
    header: Option<Vec<TableCell>>,
    rows: Vec<Vec<TableCell>>,
    row: Vec<TableCell>,
    cell: TableCell,
    in_head: bool,
}

#[derive(Clone, Debug)]
struct LinkState {
    destination: String,
    label: String,
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
    items: Vec<ItemState>,
    links: Vec<LinkState>,
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
            items: Vec::new(),
            links: Vec::new(),
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
        if self.current.prefixed {
            return;
        }
        self.current.prefixed = true;

        let quote_prefix = "│ ".repeat(self.blockquote_depth);
        if !quote_prefix.is_empty() {
            let style = Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::DIM);
            self.current.initial.push(StyledSpan {
                text: quote_prefix.clone(),
                style,
            });
            self.current.continuation.push(StyledSpan {
                text: quote_prefix,
                style,
            });
        }

        let list_indent = "  ".repeat(self.lists.len().saturating_sub(1));
        if !list_indent.is_empty() {
            self.current.initial.push(StyledSpan {
                text: list_indent.clone(),
                style: Style::default(),
            });
            self.current.continuation.push(StyledSpan {
                text: list_indent,
                style: Style::default(),
            });
        }

        // The item marker plus any task checkbox (`- [x] `); continuation
        // rows repeat the marker width as spaces so wrapped text stays
        // aligned with the first line's content.
        if let Some(item) = self.items.last_mut() {
            let marker_text = item.task_marker.map_or_else(
                || item.marker.clone(),
                |task| format!("{}{task}", item.marker),
            );
            let marker_width = UnicodeWidthStr::width(marker_text.as_str());
            let prefix = if item.marker_used {
                " ".repeat(marker_width)
            } else {
                item.marker_used = true;
                marker_text
            };
            let style = Style::default().fg(Color::Blue);
            self.current.continuation.push(StyledSpan {
                text: " ".repeat(marker_width),
                style,
            });
            self.current.initial.push(StyledSpan {
                text: prefix,
                style,
            });
        }

        if self.code_block {
            let style = Style::default().add_modifier(Modifier::DIM);
            self.current.initial.push(StyledSpan {
                text: "  ".to_string(),
                style,
            });
            self.current.continuation.push(StyledSpan {
                text: "  ".to_string(),
                style,
            });
            self.current.verbatim = true;
        }
    }

    fn push_text(&mut self, text: &str) {
        if let Some(link) = self.links.last_mut() {
            link.label.push_str(text);
        }
        self.push_text_untracked(text);
    }

    fn push_text_untracked(&mut self, text: &str) {
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.force_line_break();
            }
            if part.is_empty() {
                continue;
            }
            self.ensure_prefix();
            let style = self.current_style();
            push_styled_text(&mut self.current.spans, part, style);
        }
    }

    fn finish_line(&mut self) {
        if !logical_line_is_empty(&self.current) {
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

    fn start_link(&mut self, destination: &str) {
        self.links.push(LinkState {
            destination: destination.to_string(),
            label: String::new(),
        });
        self.push_style(link_style());
    }

    fn finish_link(&mut self) -> Option<String> {
        self.pop_style();
        let link = self.links.pop()?;
        let label = link.label.trim();
        let destination = link.destination.trim();
        (!destination.is_empty() && label != destination).then(|| format!(" ({destination})"))
    }

    fn push_link_suffix(&mut self, suffix: &str) {
        self.push_style(link_destination_style());
        self.push_text_untracked(suffix);
        self.pop_style();
    }

    fn start_item(&mut self) {
        self.finish_line();
        if self.needs_block_gap {
            self.push_blank_line();
        }
        let marker = self.lists.last_mut().map_or_else(
            || "- ".to_string(),
            |list| {
                list.next.as_mut().map_or_else(
                    || "- ".to_string(),
                    |next| {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    },
                )
            },
        );
        self.items.push(ItemState {
            marker,
            marker_used: false,
            task_marker: None,
        });
        self.needs_block_gap = false;
    }

    fn finish_item(&mut self) {
        self.finish_line();
        self.items.pop();
    }

    fn push_table_text(&mut self, text: &str) {
        if let Some(link) = self.links.last_mut() {
            link.label.push_str(text);
        }
        self.push_table_text_untracked(text);
    }

    fn push_table_text_untracked(&mut self, text: &str) {
        let style = self.current_style();
        if let Some(table) = self.table.as_mut() {
            push_styled_text(&mut table.cell.spans, text, style);
        }
    }

    fn finish_table_cell(&mut self) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        trim_styled_spans(&mut table.cell.spans);
        table.row.push(std::mem::take(&mut table.cell));
    }

    fn finish_table_row(&mut self) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        let row = std::mem::take(&mut table.row);
        if table.in_head {
            table.header = Some(row);
        } else {
            table.rows.push(row);
        }
    }

    fn handle_table_event(&mut self, event: MarkdownEvent<'_>) -> bool {
        if self.table.is_none() {
            return false;
        }
        match event {
            MarkdownEvent::Start(Tag::TableHead) => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                    table.row.clear();
                }
            }
            MarkdownEvent::End(TagEnd::TableHead) => {
                if self
                    .table
                    .as_ref()
                    .is_some_and(|table| !table.cell.is_empty())
                {
                    self.finish_table_cell();
                }
                if self
                    .table
                    .as_ref()
                    .is_some_and(|table| !table.row.is_empty())
                {
                    self.finish_table_row();
                }
                if let Some(table) = self.table.as_mut() {
                    table.in_head = false;
                }
            }
            MarkdownEvent::Start(Tag::TableRow) => {
                if let Some(table) = self.table.as_mut() {
                    table.row.clear();
                }
            }
            MarkdownEvent::End(TagEnd::TableRow) => {
                if self
                    .table
                    .as_ref()
                    .is_some_and(|table| !table.cell.is_empty())
                {
                    self.finish_table_cell();
                }
                self.finish_table_row();
            }
            MarkdownEvent::Start(Tag::TableCell) => {
                if let Some(table) = self.table.as_mut() {
                    table.cell = TableCell::default();
                }
            }
            MarkdownEvent::End(TagEnd::TableCell) => self.finish_table_cell(),
            MarkdownEvent::Start(Tag::Emphasis) => {
                self.push_style(Style::default().add_modifier(Modifier::ITALIC));
            }
            MarkdownEvent::Start(Tag::Strong) => {
                self.push_style(Style::default().add_modifier(Modifier::BOLD));
            }
            MarkdownEvent::Start(Tag::Strikethrough) => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            MarkdownEvent::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough) => {
                self.pop_style();
            }
            MarkdownEvent::Start(Tag::Link { dest_url, .. }) => self.start_link(&dest_url),
            MarkdownEvent::End(TagEnd::Link) => {
                if let Some(suffix) = self.finish_link() {
                    let style = link_destination_style();
                    if let Some(table) = self.table.as_mut() {
                        push_styled_text(&mut table.cell.spans, &suffix, style);
                    }
                }
            }
            MarkdownEvent::Text(text)
            | MarkdownEvent::Html(text)
            | MarkdownEvent::InlineHtml(text) => self.push_table_text(&text),
            MarkdownEvent::Code(text) => {
                self.push_style(Style::default().fg(Color::Cyan));
                self.push_table_text(&text);
                self.pop_style();
            }
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => self.push_table_text(" "),
            MarkdownEvent::End(TagEnd::Table) => return true,
            _ => {}
        }
        false
    }

    fn finish_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let columns = table
            .header
            .iter()
            .chain(table.rows.iter())
            .map(Vec::len)
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return;
        }

        let gap_width = 2 * columns.saturating_sub(1);
        let content_width = self
            .width
            .saturating_sub(self.structural_prefix_width())
            .max(1);
        let grid_minimum = gap_width.saturating_add(columns.saturating_mul(3));
        if table.header.is_some() && content_width < grid_minimum {
            self.render_table_records(&table, content_width);
            self.needs_block_gap = true;
            return;
        }

        let column_budget = content_width.saturating_sub(gap_width).max(columns);
        let mut widths = vec![1usize; columns];
        for row in table.header.iter().chain(table.rows.iter()) {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.width()).min(column_budget);
            }
        }
        shrink_column_widths(&mut widths, column_budget, 1);

        if let Some(header) = table.header.as_deref() {
            let spans = table_row_spans(
                header,
                &widths,
                &table.alignments,
                Style::default().add_modifier(Modifier::BOLD),
            );
            self.push_spans_line(spans, true);
            let separator = widths
                .iter()
                .map(|width| "─".repeat(*width))
                .collect::<Vec<_>>()
                .join("  ");
            self.push_spans_line(
                vec![StyledSpan {
                    text: separator,
                    style: Style::default().add_modifier(Modifier::DIM),
                }],
                true,
            );
        }
        for row in &table.rows {
            let spans = table_row_spans(row, &widths, &table.alignments, Style::default());
            self.push_spans_line(spans, true);
        }
        self.needs_block_gap = true;
    }

    fn structural_prefix_width(&self) -> usize {
        let quote_width = UnicodeWidthStr::width("│ ") * self.blockquote_depth;
        let list_width = UnicodeWidthStr::width("  ") * self.lists.len().saturating_sub(1);
        let item_width = self.items.last().map_or(0, |item| {
            UnicodeWidthStr::width(item.marker.as_str())
                + item.task_marker.map_or(0, UnicodeWidthStr::width)
        });
        let code_width = usize::from(self.code_block) * UnicodeWidthStr::width("  ");
        quote_width + list_width + item_width + code_width
    }

    fn push_spans_line(&mut self, spans: Vec<StyledSpan>, verbatim: bool) {
        self.ensure_prefix();
        for span in spans {
            push_styled_text(&mut self.current.spans, &span.text, span.style);
        }
        self.current.verbatim |= verbatim;
        self.finish_line();
    }

    fn render_table_records(&mut self, table: &TableState, content_width: usize) {
        let Some(header) = table.header.as_deref() else {
            return;
        };
        if table.rows.is_empty() {
            for label in header {
                let mut spans = label.spans.clone();
                for span in &mut spans {
                    span.style = span
                        .style
                        .patch(Style::default().add_modifier(Modifier::BOLD));
                }
                self.push_spans_line(truncate_styled_spans(&spans, content_width), true);
            }
            return;
        }
        for (row_index, row) in table.rows.iter().enumerate() {
            if row_index > 0 {
                self.push_blank_line();
            }
            for (column, value) in row.iter().enumerate() {
                let Some(label) = header.get(column) else {
                    continue;
                };
                let mut label_spans = label.spans.clone();
                for span in &mut label_spans {
                    span.style = span
                        .style
                        .patch(Style::default().add_modifier(Modifier::BOLD));
                }
                if label.width().saturating_add(2) >= content_width {
                    self.push_spans_line(truncate_styled_spans(&label_spans, content_width), true);
                    self.push_spans_line(value.spans.clone(), false);
                    continue;
                }
                push_styled_text(
                    &mut label_spans,
                    ": ",
                    Style::default().add_modifier(Modifier::DIM),
                );
                for span in &value.spans {
                    push_styled_text(&mut label_spans, &span.text, span.style);
                }
                self.push_spans_line(label_spans, false);
            }
        }
    }

    /// Render one source string into styled lines. This is the parser event
    /// loop: one match arm per markdown construct, all sharing the same
    /// incremental renderer state.
    #[allow(clippy::too_many_lines)]
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
                }
                MarkdownEvent::End(TagEnd::List(_)) => {
                    self.lists.pop();
                    self.needs_block_gap = true;
                }
                MarkdownEvent::Start(Tag::Item) => self.start_item(),
                MarkdownEvent::End(TagEnd::Item) => self.finish_item(),
                MarkdownEvent::Start(Tag::Emphasis) => {
                    self.push_style(Style::default().add_modifier(Modifier::ITALIC));
                }
                MarkdownEvent::Start(Tag::Strong) => {
                    self.push_style(Style::default().add_modifier(Modifier::BOLD));
                }
                MarkdownEvent::Start(Tag::Strikethrough) => {
                    self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
                }
                MarkdownEvent::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough) => {
                    self.pop_style();
                }
                MarkdownEvent::Start(Tag::Link { dest_url, .. }) => self.start_link(&dest_url),
                MarkdownEvent::End(TagEnd::Link) => {
                    if let Some(suffix) = self.finish_link() {
                        self.push_link_suffix(&suffix);
                    }
                }
                MarkdownEvent::Start(Tag::Table(alignments)) => {
                    self.block_gap();
                    self.table = Some(TableState {
                        alignments,
                        ..TableState::default()
                    });
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
                    let rule_width = self
                        .width
                        .saturating_sub(self.structural_prefix_width())
                        .clamp(1, 24);
                    self.push_spans_line(
                        vec![StyledSpan {
                            text: "─".repeat(rule_width),
                            style: Style::default().add_modifier(Modifier::DIM),
                        }],
                        true,
                    );
                    self.needs_block_gap = true;
                }
                MarkdownEvent::TaskListMarker(checked) => {
                    if let Some(item) = self.items.last_mut() {
                        item.task_marker = Some(if checked { "[x] " } else { "[ ] " });
                    }
                    self.ensure_prefix();
                }
                MarkdownEvent::FootnoteReference(reference) => {
                    self.push_text(&format!("[{reference}]"));
                }
                MarkdownEvent::InlineHtml(html) => self.push_text(&html),
                _ => {}
            }
        }
        self.finish_line();

        let rendered = self
            .lines
            .into_iter()
            .flat_map(|line| wrap_line(line, self.width))
            .collect::<Vec<_>>();
        trim_and_collapse_blank_lines(rendered)
    }
}

fn push_styled_text(spans: &mut Vec<StyledSpan>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
        last.text.push_str(text);
    } else {
        spans.push(StyledSpan {
            text: text.to_string(),
            style,
        });
    }
}

fn styled_width(spans: &[StyledSpan]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum()
}

fn trim_styled_spans(spans: &mut Vec<StyledSpan>) {
    while let Some(first) = spans.first_mut() {
        first.text = first.text.trim_start().to_string();
        if first.text.is_empty() {
            spans.remove(0);
        } else {
            break;
        }
    }
    while let Some(last) = spans.last_mut() {
        last.text = last.text.trim_end().to_string();
        if last.text.is_empty() {
            spans.pop();
        } else {
            break;
        }
    }

    let mut merged: Vec<StyledSpan> = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        push_styled_text(&mut merged, &span.text, span.style);
    }
    *spans = merged;
}

fn link_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED)
}

fn link_destination_style() -> Style {
    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
}

fn shrink_column_widths(widths: &mut [usize], budget: usize, minimum: usize) {
    for width in widths.iter_mut() {
        *width = (*width).min(budget).max(minimum);
    }
    let minimum_total = minimum.saturating_mul(widths.len());
    if budget < minimum_total {
        let equal_width = budget / widths.len().max(1);
        let remainder = budget % widths.len().max(1);
        for (index, width) in widths.iter_mut().enumerate() {
            *width = equal_width + usize::from(index < remainder);
        }
        return;
    }

    let total = widths.iter().sum::<usize>();
    if total <= budget {
        return;
    }

    let extra_budget = budget - minimum_total;
    let total_requested = total - minimum_total;
    let mut remainders = Vec::with_capacity(widths.len());
    let mut allocated = minimum_total;
    for (index, width) in widths.iter_mut().enumerate() {
        let requested = *width - minimum;
        let scaled = requested.saturating_mul(extra_budget);
        let granted = scaled / total_requested;
        *width = minimum + granted;
        allocated += granted;
        remainders.push((scaled % total_requested, index, requested - granted));
    }

    remainders.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, index, capacity) in remainders {
        if allocated == budget {
            break;
        }
        if capacity > 0 {
            widths[index] += 1;
            allocated += 1;
        }
    }
}

fn table_row_spans(
    row: &[TableCell],
    widths: &[usize],
    alignments: &[Alignment],
    row_style: Style,
) -> Vec<StyledSpan> {
    let mut output = Vec::new();
    for (column, column_width) in widths.iter().copied().enumerate() {
        if column > 0 {
            push_styled_text(
                &mut output,
                "  ",
                Style::default().add_modifier(Modifier::DIM),
            );
        }

        let mut cell = row
            .get(column)
            .map(|cell| cell.spans.clone())
            .unwrap_or_default();
        for span in &mut cell {
            span.style = span.style.patch(row_style);
        }
        let cell = truncate_styled_spans(&cell, column_width);
        let padding = column_width.saturating_sub(styled_width(&cell));
        let (left_padding, right_padding) = match alignments.get(column) {
            Some(Alignment::Right) => (padding, 0),
            Some(Alignment::Center) => (padding / 2, padding.saturating_sub(padding / 2)),
            _ => (0, padding),
        };
        push_styled_text(&mut output, &" ".repeat(left_padding), Style::default());
        for span in cell {
            push_styled_text(&mut output, &span.text, span.style);
        }
        if column + 1 < widths.len() {
            push_styled_text(&mut output, &" ".repeat(right_padding), Style::default());
        }
    }
    output
}

fn truncate_styled_spans(spans: &[StyledSpan], maximum_width: usize) -> Vec<StyledSpan> {
    if styled_width(spans) <= maximum_width {
        return spans.to_vec();
    }
    if maximum_width == 0 {
        return Vec::new();
    }

    let content_width = maximum_width.saturating_sub(1);
    let mut output = Vec::new();
    let mut width = 0usize;
    let mut ellipsis_style = Style::default();
    'spans: for span in spans {
        ellipsis_style = span.style;
        for grapheme in UnicodeSegmentation::graphemes(span.text.as_str(), true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if width.saturating_add(grapheme_width) > content_width {
                break 'spans;
            }
            push_styled_text(&mut output, grapheme, span.style);
            width += grapheme_width;
        }
    }
    push_styled_text(&mut output, "…", ellipsis_style);
    output
}

fn trim_and_collapse_blank_lines(lines: Vec<RenderedLine>) -> Vec<RenderedLine> {
    let mut collapsed = Vec::with_capacity(lines.len());
    let mut seen_content = false;
    for line in lines {
        if !seen_content {
            if line.is_blank() {
                continue;
            }
            seen_content = true;
        }
        if line.is_blank() && collapsed.last().is_some_and(RenderedLine::is_blank) {
            continue;
        }
        collapsed.push(line);
    }
    while collapsed.last().is_some_and(RenderedLine::is_blank) {
        collapsed.pop();
    }
    collapsed
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

const fn logical_line_is_empty(line: &LogicalLine) -> bool {
    line.initial.is_empty() && line.spans.is_empty()
}

fn logical_line_is_blank(line: &LogicalLine) -> bool {
    line.initial
        .iter()
        .chain(line.spans.iter())
        .all(|span| span.text.trim().is_empty())
}

fn wrap_line(line: LogicalLine, width: usize) -> Vec<RenderedLine> {
    if logical_line_is_empty(&line) {
        return vec![RenderedLine::default()];
    }

    let prefix_limit = if line.spans.is_empty() {
        width
    } else {
        width.saturating_sub(1)
    };
    let initial = clip_styled_spans(&line.initial, prefix_limit);
    let continuation = clip_styled_spans(&line.continuation, prefix_limit);
    if line.verbatim {
        wrap_verbatim(line.spans, &initial, &continuation, width)
    } else {
        wrap_words(line.spans, &initial, &continuation, width)
    }
}

fn clip_styled_spans(spans: &[StyledSpan], maximum_width: usize) -> Vec<StyledSpan> {
    let mut output = Vec::new();
    let mut width = 0usize;
    'spans: for span in spans {
        for grapheme in UnicodeSegmentation::graphemes(span.text.as_str(), true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if width.saturating_add(grapheme_width) > maximum_width {
                break 'spans;
            }
            push_styled_text(&mut output, grapheme, span.style);
            width += grapheme_width;
        }
    }
    output
}

#[derive(Clone, Debug)]
struct StyledGrapheme {
    range: std::ops::Range<usize>,
    style: Style,
    width: usize,
    whitespace: bool,
}

#[derive(Clone, Debug, Default)]
struct StyledText {
    text: String,
    graphemes: Vec<StyledGrapheme>,
}

fn styled_graphemes(spans: Vec<StyledSpan>) -> StyledText {
    let mut output = StyledText::default();
    for span in spans {
        for grapheme in UnicodeSegmentation::graphemes(span.text.as_str(), true) {
            let start = output.text.len();
            output.text.push_str(grapheme);
            output.graphemes.push(StyledGrapheme {
                range: start..output.text.len(),
                style: span.style,
                width: UnicodeWidthStr::width(grapheme),
                whitespace: grapheme.chars().all(char::is_whitespace),
            });
        }
    }
    output
}

fn prefixed_line(prefix: &[StyledSpan]) -> (RenderedLine, usize) {
    let mut line = RenderedLine::default();
    for span in prefix {
        push_styled_text(&mut line.spans, &span.text, span.style);
    }
    (line, styled_width(prefix))
}

fn append_grapheme(line: &mut RenderedLine, source: &str, grapheme: &StyledGrapheme) {
    push_styled_text(
        &mut line.spans,
        &source[grapheme.range.clone()],
        grapheme.style,
    );
}

fn wrap_verbatim(
    spans: Vec<StyledSpan>,
    initial: &[StyledSpan],
    continuation: &[StyledSpan],
    width: usize,
) -> Vec<RenderedLine> {
    let styled = styled_graphemes(spans);
    let (mut current, mut current_width) = prefixed_line(initial);
    let mut content_width = 0usize;
    let mut output = Vec::new();

    for grapheme in &styled.graphemes {
        if content_width > 0 && current_width.saturating_add(grapheme.width) > width {
            output.push(std::mem::take(&mut current));
            let prefixed = prefixed_line(continuation);
            current = prefixed.0;
            current_width = prefixed.1;
            content_width = 0;
        }
        append_grapheme(&mut current, &styled.text, grapheme);
        current_width += grapheme.width;
        content_width += grapheme.width;
    }
    output.push(current);
    output
}

// `pending_whitespace` is refilled on later iterations, so draining keeps
// the allocation while `into_iter` would move the buffer out of the loop.
#[allow(clippy::iter_with_drain)]
fn wrap_words(
    spans: Vec<StyledSpan>,
    initial: &[StyledSpan],
    continuation: &[StyledSpan],
    width: usize,
) -> Vec<RenderedLine> {
    let styled = styled_graphemes(spans);
    let graphemes = &styled.graphemes;
    let (mut current, mut current_width) = prefixed_line(initial);
    let mut content_width = 0usize;
    let mut output = Vec::new();
    let mut pending_whitespace = Vec::new();
    let mut index = 0usize;

    while index < graphemes.len() {
        if graphemes[index].whitespace {
            pending_whitespace.push(graphemes[index].clone());
            index += 1;
            continue;
        }

        let word_start = index;
        while index < graphemes.len() && !graphemes[index].whitespace {
            index += 1;
        }
        let word = &graphemes[word_start..index];
        let word_width = word.iter().map(|grapheme| grapheme.width).sum::<usize>();
        let whitespace_width = pending_whitespace
            .iter()
            .map(|grapheme| grapheme.width)
            .sum::<usize>();

        if content_width > 0
            && current_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                > width
        {
            output.push(std::mem::take(&mut current));
            let prefixed = prefixed_line(continuation);
            current = prefixed.0;
            current_width = prefixed.1;
            content_width = 0;
        }

        if content_width > 0 {
            for grapheme in pending_whitespace.drain(..) {
                append_grapheme(&mut current, &styled.text, &grapheme);
                current_width += grapheme.width;
                content_width += grapheme.width;
            }
        } else {
            pending_whitespace.clear();
        }

        for grapheme in word {
            if content_width > 0 && current_width.saturating_add(grapheme.width) > width {
                output.push(std::mem::take(&mut current));
                let prefixed = prefixed_line(continuation);
                current = prefixed.0;
                current_width = prefixed.1;
                content_width = 0;
            }
            append_grapheme(&mut current, &styled.text, grapheme);
            current_width += grapheme.width;
            content_width += grapheme.width;
        }
    }

    output.push(current);
    output
}

pub fn render_markdown(source: &str, width: u16) -> Vec<RenderedLine> {
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

fn reference_link_count_before(source: &str, end: usize) -> usize {
    Parser::new_ext(source, markdown_options())
        .into_offset_iter()
        .filter(|(event, range)| {
            range.start < end
                && matches!(
                    event,
                    MarkdownEvent::Start(Tag::Link {
                        link_type: LinkType::Reference
                            | LinkType::ReferenceUnknown
                            | LinkType::Collapsed
                            | LinkType::CollapsedUnknown
                            | LinkType::Shortcut
                            | LinkType::ShortcutUnknown,
                        ..
                    })
                )
        })
        .count()
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

const fn is_block_tag(tag: &Tag<'_>) -> bool {
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

const fn is_block_end(tag: TagEnd) -> bool {
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
    fn wrapped_quote_rows_keep_the_quote_bar() {
        let lines = render_markdown(
            "> a very long quoted line that will definitely wrap at a narrow width",
            20,
        );
        assert_eq!(
            text(&lines),
            vec![
                "│ a very long quoted",
                "│ line that will",
                "│ definitely wrap at",
                "│ a narrow width",
            ]
        );
    }

    #[test]
    fn wrapped_list_rows_keep_a_consistent_continuation_indent() {
        let plain = render_markdown("- plain item with a long description that wraps", 16);
        assert_eq!(
            text(&plain),
            vec![
                "- plain item",
                "  with a long",
                "  description",
                "  that wraps"
            ]
        );
        let numbered = render_markdown("10. item with marker width four and a long line", 12);
        assert_eq!(
            text(&numbered),
            vec![
                "10. item",
                "    with",
                "    marker",
                "    width",
                "    four and",
                "    a long",
                "    line",
            ]
        );
    }

    #[test]
    fn wrapped_task_items_align_under_the_item_text() {
        let lines = render_markdown(
            "- [x] a checked task item with a long description that wraps",
            16,
        );
        let rendered = text(&lines);
        assert_eq!(rendered[0], "- [x] a checked");
        // The checkbox is part of the marker, so continuation rows indent by
        // the full `- [x] ` width instead of just the bullet width.
        assert!(rendered[1..]
            .iter()
            .all(|line| line.starts_with("      ") && UnicodeWidthStr::width(line.as_str()) <= 16));
    }

    #[test]
    fn narrow_tables_stay_within_the_content_width() {
        let lines = render_markdown("| Name | State |\n|---|---|\n| ash | ready |", 4);
        assert_eq!(text(&lines), vec!["Name", "ash", "Sta…", "read", "y"]);
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.plain_text().as_str()) <= 4));
    }

    #[test]
    fn keeps_parent_item_context_after_a_nested_list() {
        let lines = render_markdown("- outer\n  - inner\n\n  tail\n- next", 80);
        assert_eq!(
            text(&lines),
            vec!["- outer", "", "  - inner", "", "  tail", "", "- next",]
        );
    }

    #[test]
    fn tight_multiline_items_do_not_gain_a_blank_row() {
        let lines = render_markdown("- first line\n  continuation\n- second", 80);
        assert_eq!(
            text(&lines),
            vec!["- first line", "  continuation", "- second"]
        );
    }

    #[test]
    fn wrapping_keeps_unicode_graphemes_intact() {
        assert_eq!(text(&render_markdown("- 👩‍💻👩‍💻", 4)), vec!["- 👩‍💻", "  👩‍💻"]);
        assert_eq!(
            text(&render_markdown("- 中文中文", 6)),
            vec!["- 中文", "  中文"]
        );
    }

    #[test]
    fn links_include_non_redundant_destinations() {
        let lines = render_markdown(
            "Read [the docs](https://example.com/docs) or <https://example.com>.",
            80,
        );
        assert_eq!(
            text(&lines),
            vec!["Read the docs (https://example.com/docs) or https://example.com."]
        );
        assert!(lines[0].spans.iter().any(|span| {
            span.text.contains("(https://example.com/docs)")
                && span.style.add_modifier.contains(Modifier::DIM)
        }));
    }

    #[test]
    fn table_cells_keep_inline_style_and_text_adjacency() {
        let lines = render_markdown("| Value |\n|---|\n| foo**bar**baz |", 40);
        assert_eq!(text(&lines), vec!["Value", "─────────", "foobarbaz"]);
        assert!(lines[2].spans.iter().any(|span| {
            span.text == "bar" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn tables_honor_column_alignment() {
        let lines = render_markdown(
            "| Left | Center | Right |\n|:---|:---:|---:|\n| a | b | c |",
            40,
        );
        assert_eq!(text(&lines)[2], "a       b         c");
    }

    #[test]
    fn streaming_recomputes_links_when_a_reference_definition_arrives() {
        assert_streaming_matches_full(
            "Read [the docs][docs].\n\n[docs]: https://example.com/docs\n\nAfter",
            80,
        );
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
