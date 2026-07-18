use std::path::{Path, PathBuf};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::scrollback::{sanitize_terminal_text, wrap_text};
use crate::status_line::prompt_header_line;

const USER_HORIZONTAL_INSET: u16 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HistoryBlock {
    User {
        text: String,
        model: String,
        working_dir: PathBuf,
    },
    Info(String),
    Error(String),
    Worked(String),
    SessionStarted,
    SessionResumed,
}

impl HistoryBlock {
    pub(crate) fn user_with_prompt(text: &str, model: &str, working_dir: &Path) -> Self {
        Self::User {
            text: normalize_multiline(text),
            model: model.to_string(),
            working_dir: working_dir.to_path_buf(),
        }
    }

    pub(crate) fn info(message: &str) -> Self {
        Self::Info(normalize_multiline(message))
    }

    pub(crate) fn error(error: &str) -> Self {
        Self::Error(normalize_multiline(error))
    }

    pub(crate) fn worked(elapsed: String) -> Self {
        Self::Worked(elapsed)
    }

    pub(crate) const fn session_started() -> Self {
        Self::SessionStarted
    }

    pub(crate) const fn session_resumed() -> Self {
        Self::SessionResumed
    }

    pub(crate) fn render(&self, width: u16) -> Buffer {
        match self {
            Self::User {
                text,
                model,
                working_dir,
            } => render_user(text, model, working_dir, width.max(1)),
            Self::Info(message) => render_info(message, width.max(1)),
            Self::Error(error) => render_error(error, width.max(1)),
            Self::Worked(elapsed) => render_worked(elapsed, width.max(1)),
            Self::SessionStarted => render_session_marker("New session", width.max(1)),
            Self::SessionResumed => render_session_marker("Resume session", width.max(1)),
        }
    }
}

fn normalize_multiline(text: &str) -> String {
    sanitize_terminal_text(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn render_user(text: &str, model: &str, working_dir: &Path, width: u16) -> Buffer {
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
    let show_header = !model.is_empty() || !working_dir.as_os_str().is_empty();
    let header_rows = u16::from(show_header);
    let area = Rect::new(0, 0, width, header_rows.saturating_add(content_height));
    let mut buffer = Buffer::empty(area);

    if show_header {
        buffer.set_line(0, 0, &prompt_header_line(model, working_dir, width), width);
    }
    if show_prefix {
        buffer.set_string(
            0,
            header_rows,
            "›",
            Style::default().add_modifier(Modifier::BOLD),
        );
    }
    paragraph.render(
        Rect::new(content_x, header_rows, content_width, content_height),
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

fn render_worked(elapsed: &str, width: u16) -> Buffer {
    let label = format!("Worked for {elapsed}");
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
    let style = Style::default().add_modifier(Modifier::DIM);
    buffer.set_string(0, 0, "•", style);
    buffer.set_string(USER_HORIZONTAL_INSET, 0, label, style);
    buffer
}

fn render_session_marker(label: &str, width: u16) -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
    let style = Style::default().add_modifier(Modifier::DIM);
    buffer.set_string(0, 0, "•", style);
    buffer.set_string(USER_HORIZONTAL_INSET, 0, label, style);
    buffer
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
    fn user_block_matches_the_plain_composer_style() {
        let buffer = HistoryBlock::user_with_prompt("abcdefghij", "", Path::new("")).render(9);

        assert_eq!(buffer.area.height, 2);
        assert_eq!(row_text(&buffer, 0), "› abcdefg");
        assert_eq!(row_text(&buffer, 1), "  hij");
        assert_eq!(buffer.cell((8, 0)).expect("cell").bg, Color::Reset);
    }

    #[test]
    fn long_user_block_can_be_taller_than_the_terminal() {
        let buffer =
            HistoryBlock::user_with_prompt(&"word ".repeat(100), "", Path::new("")).render(10);

        assert!(buffer.area.height > 24);
    }

    #[test]
    fn tiny_width_falls_back_to_content_without_a_prefix() {
        let buffer = HistoryBlock::user_with_prompt("ash", "", Path::new("")).render(1);

        assert_eq!(row_text(&buffer, 0), "a");
        assert_eq!(row_text(&buffer, 1), "s");
        assert_eq!(row_text(&buffer, 2), "h");
    }

    #[test]
    fn user_block_normalizes_carriage_returns() {
        let block = HistoryBlock::user_with_prompt("one\r\ntwo\rthree", "", Path::new(""));

        assert_eq!(
            block,
            HistoryBlock::User {
                text: "one\ntwo\nthree".to_string(),
                model: String::new(),
                working_dir: PathBuf::new(),
            }
        );
    }

    #[test]
    fn user_prompt_block_renders_header_above_input() {
        let buffer =
            HistoryBlock::user_with_prompt("hello", "gpt-5", Path::new("/tmp/ash")).render(40);

        assert_eq!(row_text(&buffer, 0), "/tmp/ash · gpt-5");
        assert_eq!(row_text(&buffer, 1), "› hello");
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
    fn worked_block_has_a_dim_bullet() {
        let buffer = HistoryBlock::worked("2m 05s".to_string()).render(32);

        assert_eq!(row_text(&buffer, 0), "• Worked for 2m 05s");
        assert!(buffer
            .cell((0, 0))
            .expect("label")
            .modifier
            .contains(Modifier::DIM));
        assert_eq!(
            row_text(&HistoryBlock::worked("0s".to_string()).render(10), 0),
            "• Worked f"
        );
    }

    #[test]
    fn session_started_is_a_dim_label() {
        let buffer = HistoryBlock::session_started().render(32);

        assert_eq!(row_text(&buffer, 0), "• New session");
        assert!(buffer
            .cell((0, 0))
            .expect("session marker")
            .modifier
            .contains(Modifier::DIM));
    }

    #[test]
    fn resumed_session_uses_the_same_dim_prefix() {
        let buffer = HistoryBlock::session_resumed().render(32);

        assert_eq!(row_text(&buffer, 0), "• Resume session");
    }
}
