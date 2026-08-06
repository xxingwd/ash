use std::path::Path;

use unicode_width::UnicodeWidthStr;

use crate::{status_line::compact_path, text_width::truncate_start};

const FULL_WORDMARK_MIN_WIDTH: u16 = 34;
const COMPACT_WORDMARK_MIN_WIDTH: u16 = 14;
const MIN_CARD_WIDTH: u16 = 10;
const FRAME_BORDER_COLUMNS: u16 = 2;
const SUBTITLE: &str = "── TERMINAL CODING AGENT ──";

const FULL_WORDMARK: [&str; 6] = [
    " █████╗ ███████╗██╗  ██╗ ",
    "██╔══██╗██╔════╝██║  ██║",
    "███████║███████╗███████║",
    "██╔══██║╚════██║██╔══██║",
    "██║  ██║███████║██║  ██║",
    "╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝",
];

const COMPACT_WORDMARK: [&str; 2] = ["▄▀█  █▀  █ █", "█▀█  ▄█  █▀█"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WelcomeStyle {
    Frame,
    Logo,
    Title,
    Subtitle,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct WelcomeLine {
    pub(crate) text: String,
    pub(crate) style: WelcomeStyle,
}

pub(crate) fn welcome_card(available_width: u16, working_dir: &Path) -> Vec<WelcomeLine> {
    let outer_width = available_width;
    let inner_width = outer_width.saturating_sub(FRAME_BORDER_COLUMNS);
    if outer_width < MIN_CARD_WIDTH {
        return vec![centered(available_width, "ASH", WelcomeStyle::Title)];
    }

    let mut lines = vec![framed_top(outer_width)];
    let wordmark: &[&str] = if inner_width >= FULL_WORDMARK_MIN_WIDTH {
        &FULL_WORDMARK
    } else if inner_width >= COMPACT_WORDMARK_MIN_WIDTH {
        &COMPACT_WORDMARK
    } else {
        &[]
    };

    if wordmark.is_empty() {
        lines.push(framed_row(inner_width, "ASH", WelcomeStyle::Title));
    } else {
        lines.push(framed_row(inner_width, "", WelcomeStyle::Frame));
        lines.extend(
            wordmark
                .iter()
                .map(|line| framed_row(inner_width, line, WelcomeStyle::Logo)),
        );
        lines.push(framed_row(inner_width, "", WelcomeStyle::Frame));
    }

    if usize::from(inner_width) >= UnicodeWidthStr::width(SUBTITLE) {
        lines.push(framed_row(inner_width, SUBTITLE, WelcomeStyle::Subtitle));
    }
    lines.push(framed_row(
        inner_width,
        &truncate_start(&workspace_label(working_dir), usize::from(inner_width)),
        WelcomeStyle::Subtitle,
    ));
    lines.push(framed_row(inner_width, "", WelcomeStyle::Frame));
    lines.push(framed_bottom(outer_width));
    lines
}

fn framed_top(outer_width: u16) -> WelcomeLine {
    let prefix = "╭─ ASH ";
    let fill_width = usize::from(outer_width)
        .saturating_sub(UnicodeWidthStr::width(prefix))
        .saturating_sub(1);
    WelcomeLine {
        text: format!("{prefix}{}╮", "─".repeat(fill_width)),
        style: WelcomeStyle::Frame,
    }
}

fn framed_bottom(outer_width: u16) -> WelcomeLine {
    WelcomeLine {
        text: format!(
            "╰{}╯",
            "─".repeat(usize::from(
                outer_width.saturating_sub(FRAME_BORDER_COLUMNS),
            ))
        ),
        style: WelcomeStyle::Frame,
    }
}

fn framed_row(inner_width: u16, content: &str, style: WelcomeStyle) -> WelcomeLine {
    let content_width = UnicodeWidthStr::width(content);
    let padding = usize::from(inner_width).saturating_sub(content_width);
    let left_padding = padding / 2;
    let right_padding = padding.saturating_sub(left_padding);
    WelcomeLine {
        text: format!(
            "│{}{}{}│",
            " ".repeat(left_padding),
            content,
            " ".repeat(right_padding)
        ),
        style,
    }
}

fn centered(terminal_width: u16, content: &str, style: WelcomeStyle) -> WelcomeLine {
    let content_width = UnicodeWidthStr::width(content);
    let left = usize::from(terminal_width).saturating_sub(content_width) / 2;
    WelcomeLine {
        text: format!("{}{content}", " ".repeat(left)),
        style,
    }
}

fn workspace_label(working_dir: &Path) -> String {
    compact_path(working_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_claude_style_card_with_the_full_wordmark() {
        let lines = welcome_card(80, Path::new("/home/example/workspace/ash"));
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.text.as_str()) <= 80));
        assert!(lines
            .first()
            .is_some_and(|line| line.text.starts_with("╭─ ASH ")));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.style == WelcomeStyle::Logo)
                .count(),
            FULL_WORDMARK.len()
        );
        assert!(lines.iter().any(|line| line.text.contains(SUBTITLE)));
        assert!(lines.iter().any(|line| line.text.contains("/home/example")));
    }

    #[test]
    fn uses_a_compact_wordmark_when_the_card_is_narrow() {
        let lines = welcome_card(24, Path::new("/project"));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.style == WelcomeStyle::Logo)
                .count(),
            COMPACT_WORDMARK.len()
        );
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.text.as_str()) <= 24));
    }

    #[test]
    fn falls_back_to_plain_text_in_tiny_terminals() {
        let lines = welcome_card(8, Path::new("/project"));
        assert_eq!(lines, vec![centered(8, "ASH", WelcomeStyle::Title)]);
    }

    #[test]
    fn truncates_the_start_of_long_workspace_paths() {
        assert_eq!(truncate_start("~/very/long/workspace", 12), "…g/workspace");
    }
}
