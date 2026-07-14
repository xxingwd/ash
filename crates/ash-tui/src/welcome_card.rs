use unicode_width::UnicodeWidthStr;

const FULL_WORDMARK_MIN_WIDTH: u16 = 34;
const COMPACT_WORDMARK_MIN_WIDTH: u16 = 14;

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
    Logo,
    Title,
    Subtitle,
    Plain,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct WelcomeLine {
    pub(crate) text: String,
    pub(crate) style: WelcomeStyle,
}

pub(crate) fn welcome_card(terminal_width: u16) -> Vec<WelcomeLine> {
    let mut lines = vec![WelcomeLine {
        text: String::new(),
        style: WelcomeStyle::Plain,
    }];

    let wordmark: &[&str] = if terminal_width >= FULL_WORDMARK_MIN_WIDTH {
        &FULL_WORDMARK
    } else if terminal_width >= COMPACT_WORDMARK_MIN_WIDTH {
        &COMPACT_WORDMARK
    } else {
        lines.push(centered(terminal_width, "ASH", WelcomeStyle::Title));
        return lines;
    };

    for content in wordmark {
        lines.push(centered(terminal_width, content, WelcomeStyle::Logo));
    }

    if terminal_width >= 29 {
        lines.push(centered(
            terminal_width,
            "── TERMINAL CODING AGENT ──",
            WelcomeStyle::Subtitle,
        ));
    }
    lines
}

fn centered(terminal_width: u16, content: &str, style: WelcomeStyle) -> WelcomeLine {
    let content_width = UnicodeWidthStr::width(content);
    let left = usize::from(terminal_width).saturating_sub(content_width) / 2;
    WelcomeLine {
        text: format!("{}{content}", " ".repeat(left)),
        style,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_full_wordmark_and_keeps_it_inside_the_terminal() {
        let lines = welcome_card(80);
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.text.as_str()) <= 80));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.style == WelcomeStyle::Logo)
                .count(),
            FULL_WORDMARK.len()
        );
        assert!(lines.iter().any(|line| line.text.contains("███████")));
    }

    #[test]
    fn uses_a_compact_wordmark_in_narrow_terminals() {
        let lines = welcome_card(24);
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
        let lines = welcome_card(10);
        assert!(lines
            .iter()
            .any(|line| line.style == WelcomeStyle::Title && line.text.contains("ASH")));
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.text.as_str()) <= 10));
    }

    #[test]
    fn does_not_append_a_blank_line_after_the_wordmark() {
        let lines = welcome_card(80);
        assert_ne!(lines.last().map(|line| line.text.as_str()), Some(""));
    }
}
