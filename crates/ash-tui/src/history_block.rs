use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Paragraph, Widget, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    scrollback::{sanitize_terminal_text, wrap_text},
    status_line::{format_token_count, format_token_rate},
};

const USER_HORIZONTAL_INSET: u16 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HistoryBlock {
    User(String),
    Info(String),
    Interrupted,
    Error(String),
    Worked(Worked),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Worked {
    elapsed: String,
    input_tokens: u64,
    output_tokens: u64,
    generation_ms: u64,
}

impl HistoryBlock {
    pub(crate) fn user(text: &str) -> Self {
        Self::User(normalize_multiline(text))
    }

    pub(crate) fn info(message: &str) -> Self {
        Self::Info(normalize_multiline(message))
    }

    pub(crate) fn error(error: &str) -> Self {
        Self::Error(normalize_multiline(error))
    }

    pub(crate) const fn interrupted() -> Self {
        Self::Interrupted
    }

    pub(crate) fn worked(
        elapsed: String,
        input_tokens: u64,
        output_tokens: u64,
        generation_ms: u64,
    ) -> Self {
        Self::Worked(Worked {
            elapsed,
            input_tokens,
            output_tokens,
            generation_ms,
        })
    }

    pub(crate) fn render(&self, width: u16) -> Buffer {
        match self {
            Self::User(text) => render_user(text, width.max(1)),
            Self::Info(message) => render_info(message, width.max(1)),
            Self::Interrupted => render_interrupted(width.max(1)),
            Self::Error(error) => render_error(error, width.max(1)),
            Self::Worked(worked) => render_worked(worked, width.max(1)),
        }
    }
}

fn normalize_multiline(text: &str) -> String {
    sanitize_terminal_text(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn render_user(text: &str, width: u16) -> Buffer {
    let show_prefix = width > USER_HORIZONTAL_INSET;
    let content_x = if show_prefix {
        USER_HORIZONTAL_INSET
    } else {
        0
    };
    let content_width = width.saturating_sub(content_x).max(1);
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let content_height = u16::try_from(paragraph.line_count(content_width))
        .unwrap_or(u16::MAX)
        .max(1);
    let area = Rect::new(0, 0, width, content_height);
    let mut buffer = Buffer::empty(area);

    if show_prefix {
        buffer.set_string(
            0,
            0,
            "›",
            Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
        );
    }
    paragraph.render(
        Rect::new(content_x, 0, content_width, content_height),
        &mut buffer,
    );
    buffer
}

fn render_info(message: &str, width: u16) -> Buffer {
    let content_width = width.saturating_sub(4).max(1);
    let mut rows = message
        .lines()
        .flat_map(|line| wrap_text(line, content_width))
        .collect::<Vec<_>>();
    if rows.is_empty() {
        rows.push(String::new());
    }
    let height = u16::try_from(rows.len()).unwrap_or(u16::MAX);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let content_x = if width > USER_HORIZONTAL_INSET {
        USER_HORIZONTAL_INSET
    } else {
        0
    };
    for (index, row) in rows.iter().take(usize::from(height)).enumerate() {
        let y = u16::try_from(index).unwrap_or(u16::MAX);
        if index == 0 && content_x > 0 {
            buffer.set_string(0, y, "•", Style::default().add_modifier(Modifier::DIM));
        }
        buffer.set_string(content_x, y, row, Style::default());
    }
    buffer
}

fn render_interrupted(width: u16) -> Buffer {
    const MESSAGE: &str = "Conversation interrupted.";
    let content_x = if width > 1 { 2 } else { 0 };
    let rows = wrap_text(MESSAGE, width.saturating_sub(content_x).max(1));
    let height = u16::try_from(rows.len()).unwrap_or(u16::MAX).max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let style = Style::default().fg(Color::Red);
    for (index, row) in rows.iter().take(usize::from(height)).enumerate() {
        let y = u16::try_from(index).unwrap_or(u16::MAX);
        if index == 0 && content_x > 0 {
            buffer.set_string(0, y, "■", style);
        }
        buffer.set_string(content_x, y, row, style);
    }
    buffer
}

fn render_error(error: &str, width: u16) -> Buffer {
    if width <= 9 {
        let paragraph = Paragraph::new(format!("Error: {error}"))
            .style(Style::default().add_modifier(Modifier::BOLD))
            .wrap(Wrap { trim: false });
        let height = u16::try_from(paragraph.line_count(width))
            .unwrap_or(u16::MAX)
            .max(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        paragraph.render(buffer.area, &mut buffer);
        return buffer;
    }

    let rows = wrap_text(error, width.saturating_sub(9).max(1));
    let height = u16::try_from(rows.len()).unwrap_or(u16::MAX).max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    for (index, row) in rows.iter().take(usize::from(height)).enumerate() {
        let y = u16::try_from(index).unwrap_or(u16::MAX);
        if index == 0 {
            buffer.set_string(
                0,
                y,
                "•",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            );
            buffer.set_string(
                USER_HORIZONTAL_INSET,
                y,
                "Error:",
                Style::default().add_modifier(Modifier::BOLD),
            );
            buffer.set_string(9, y, row, Style::default());
        } else {
            buffer.set_string(USER_HORIZONTAL_INSET, y, row, Style::default());
        }
    }
    buffer
}

fn render_worked(worked: &Worked, width: u16) -> Buffer {
    let separator = worked_separator(worked, width);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
    buffer.set_string(
        0,
        0,
        separator,
        Style::default().add_modifier(Modifier::DIM),
    );
    buffer
}

fn worked_separator(worked: &Worked, width: u16) -> String {
    let mut label = format!("─ Worked for {}", worked.elapsed);
    if worked.input_tokens > 0 || worked.output_tokens > 0 {
        label.push_str(&format!(
            " · {} in / {} out",
            format_token_count(worked.input_tokens),
            format_token_count(worked.output_tokens),
        ));
        if let Some(rate) = format_token_rate(worked.output_tokens, worked.generation_ms) {
            label.push_str(&format!(" · {rate}"));
        }
    }
    label.push_str(" ─");
    let width = usize::from(width);
    let label_width = UnicodeWidthStr::width(label.as_str());
    if label_width >= width {
        return label.chars().take(width).collect();
    }
    format!("{label}{}", "─".repeat(width - label_width))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(buffer: &Buffer, row: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|column| buffer.cell((column, row)))
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn user_block_uses_content_height_and_hanging_indent() {
        let buffer = HistoryBlock::user("abcdefghij").render(9);

        assert_eq!(buffer.area.height, 2);
        assert_eq!(row_text(&buffer, 0), "› abcdefg");
        assert_eq!(row_text(&buffer, 1), "  hij");
        assert_eq!(buffer.cell((8, 0)).expect("cell").bg, Color::Reset);
    }

    #[test]
    fn long_user_block_can_be_taller_than_the_terminal() {
        let buffer = HistoryBlock::user(&"word ".repeat(100)).render(10);

        assert!(buffer.area.height > 24);
    }

    #[test]
    fn tiny_width_falls_back_to_content_without_a_prefix() {
        let buffer = HistoryBlock::user("ash").render(1);

        assert_eq!(row_text(&buffer, 0), "a");
        assert_eq!(row_text(&buffer, 1), "s");
        assert_eq!(row_text(&buffer, 2), "h");
    }

    #[test]
    fn user_block_normalizes_carriage_returns() {
        let block = HistoryBlock::user("one\r\ntwo\rthree");

        assert_eq!(block, HistoryBlock::User("one\ntwo\nthree".to_string()));
    }

    #[test]
    fn info_block_preserves_bullet_wrap_and_continuation_indent() {
        let buffer = HistoryBlock::info("abcdefghij").render(12);

        assert_eq!(buffer.area.height, 2);
        assert_eq!(row_text(&buffer, 0), "• abcdefgh");
        assert_eq!(row_text(&buffer, 1), "  ij");
        assert!(buffer
            .cell((0, 0))
            .expect("bullet")
            .modifier
            .contains(Modifier::DIM));
    }

    #[test]
    fn error_block_preserves_label_and_error_style() {
        let buffer = HistoryBlock::error("abcdefghijklmnop").render(20);

        assert_eq!(buffer.area.height, 2);
        assert_eq!(row_text(&buffer, 0), "• Error: abcdefghijk");
        assert_eq!(row_text(&buffer, 1), "  lmnop");
        let bullet = buffer.cell((0, 0)).expect("bullet");
        assert_eq!(bullet.fg, Color::Red);
        assert!(bullet.modifier.contains(Modifier::BOLD));
        assert!(buffer
            .cell((2, 0))
            .expect("label")
            .modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn interrupted_block_uses_the_codex_square_marker() {
        let buffer = HistoryBlock::interrupted().render(30);

        assert_eq!(row_text(&buffer, 0), "■ Conversation interrupted.");
        assert_eq!(buffer.cell((0, 0)).expect("marker").fg, Color::Red);
        assert_eq!(buffer.cell((2, 0)).expect("message").fg, Color::Red);
    }

    #[test]
    fn worked_block_fills_or_truncates_to_the_terminal_width() {
        let buffer = HistoryBlock::worked("2m 05s".to_string(), 1_200, 345, 1_500).render(64);
        let separator = row_text(&buffer, 0);

        assert!(separator.starts_with("─ Worked for 2m 05s · 1.2k in / 345 out · 230 tok/s ─"));
        assert_eq!(UnicodeWidthStr::width(separator.as_str()), 64);
        assert!(buffer
            .cell((0, 0))
            .expect("separator")
            .modifier
            .contains(Modifier::DIM));
        assert_eq!(
            row_text(
                &HistoryBlock::worked("0s".to_string(), 0, 0, 0).render(10),
                0
            ),
            "─ Worked f"
        );
    }
}
