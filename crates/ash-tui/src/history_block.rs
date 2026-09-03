use std::fmt::Write;

use ash_core::TurnStats;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    scrollback::sanitize_terminal_text,
    status_line::{format_token_rate, format_token_usage},
    wrap::render_hanging_lines,
};

const USER_HORIZONTAL_INSET: u16 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryBlock {
    User(String),
    Info(String),
    Interrupted,
    Error(String),
    Worked(Worked),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worked {
    elapsed: String,
    stats: TurnStats,
    tool_calls: usize,
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

    pub(crate) const fn worked(elapsed: String, stats: TurnStats, tool_calls: usize) -> Self {
        Self::Worked(Worked {
            elapsed,
            stats,
            tool_calls,
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
    let prefix = if width > USER_HORIZONTAL_INSET {
        Line::from(vec![Span::styled(
            "› ",
            Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
        )])
    } else {
        Line::default()
    };
    render_hanging_lines(
        text.lines()
            .map(|line| (prefix.clone(), Line::from(line.to_string()))),
        width,
    )
}

fn render_info(message: &str, width: u16) -> Buffer {
    let prefix = if width > USER_HORIZONTAL_INSET {
        Line::from(vec![Span::styled(
            "• ",
            Style::default().add_modifier(Modifier::DIM),
        )])
    } else {
        Line::default()
    };
    let lines = if message.is_empty() {
        vec![(prefix, Line::default())]
    } else {
        message
            .lines()
            .map(|line| (prefix.clone(), Line::from(line.to_string())))
            .collect()
    };
    render_hanging_lines(lines, width)
}

fn render_interrupted(width: u16) -> Buffer {
    let style = Style::default().fg(Color::Red);
    let prefix = if width > 1 {
        Line::from(vec![Span::styled("■", style)])
    } else {
        Line::default()
    };
    render_hanging_lines(
        [(prefix, Line::styled(" Conversation interrupted.", style))],
        width,
    )
}

fn render_error(error: &str, width: u16) -> Buffer {
    let prefix = Line::from(vec![
        Span::styled(
            "• ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Error: ", Style::default().add_modifier(Modifier::BOLD)),
    ]);
    let contents = if error.is_empty() {
        vec![String::new()]
    } else {
        error.lines().map(ToString::to_string).collect()
    };
    render_hanging_lines(
        contents
            .into_iter()
            .map(|line| (prefix.clone(), Line::from(line))),
        width,
    )
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
    let stats = worked.stats;
    if stats.input_tokens > 0 || stats.output_tokens > 0 || worked.tool_calls > 0 {
        let _ = write!(
            label,
            " · {}",
            format_token_usage(stats.input_tokens, stats.output_tokens)
        );
        if let Some(rate) = format_token_rate(stats.output_tokens, stats.generation_ms) {
            let _ = write!(label, " · {rate}");
        }
        if worked.tool_calls > 0 {
            let _ = write!(label, " · {} tools", worked.tool_calls);
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
            .map(ratatui::buffer::Cell::symbol)
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
        let buffer = HistoryBlock::info("abcdefghij").render(8);

        assert_eq!(buffer.area.height, 2);
        assert_eq!(row_text(&buffer, 0), "• abcdef");
        assert_eq!(row_text(&buffer, 1), "  ghij");
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
        assert_eq!(row_text(&buffer, 1), "         lmnop");
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
    }

    #[test]
    fn worked_block_fills_or_truncates_to_the_terminal_width() {
        let stats = TurnStats {
            input_tokens: 1_200,
            output_tokens: 345,
            generation_ms: 1_500,
        };
        let buffer = HistoryBlock::worked("2m 05s".to_string(), stats, 2).render(64);
        let separator = row_text(&buffer, 0);

        assert!(
            separator.starts_with("─ Worked for 2m 05s · 1.2k in / 345 out · 230 tok/s · 2 tools")
        );
        assert_eq!(UnicodeWidthStr::width(separator.as_str()), 64);
        assert!(buffer
            .cell((0, 0))
            .expect("separator")
            .modifier
            .contains(Modifier::DIM));
        assert_eq!(
            row_text(
                &HistoryBlock::worked("0s".to_string(), TurnStats::default(), 0).render(10),
                0
            ),
            "─ Worked f"
        );
    }
}
