use crate::wrap::wrap_plain_text;
use ratatui::{
    style::{Modifier, Style},
    text::Span,
};

pub fn sanitize_terminal_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            match characters.peek().copied() {
                Some('[') => {
                    characters.next();
                    let _ = characters.find(|character| ('@'..='~').contains(character));
                }
                Some(']') => {
                    characters.next();
                    while let Some(character) = characters.next() {
                        if character == '\x07' {
                            break;
                        }
                        if character == '\x1b' && characters.next_if_eq(&'\\').is_some() {
                            break;
                        }
                    }
                }
                Some(_) => {
                    characters.next();
                }
                None => {}
            }
        } else if character == '\t' {
            sanitized.push_str("    ");
        } else if matches!(character, '\n' | '\r') || !character.is_control() {
            sanitized.push(character);
        }
    }
    sanitized
}

pub fn sanitize_single_line(text: &str) -> String {
    sanitize_terminal_text(text).replace(['\r', '\n'], " ")
}

/// Content-row prefix spans: a bullet marker on the first row and an
/// indentation gap on continuation rows. Shared by the markdown renderers so
/// the `• /  ` prefix stays identical across blocks.
pub fn content_row_prefix(first: bool) -> Vec<Span<'static>> {
    if first {
        vec![Span::styled(
            "• ",
            Style::default().add_modifier(Modifier::DIM),
        )]
    } else {
        vec![Span::raw("  ")]
    }
}

pub fn wrap_text(text: &str, width: u16) -> Vec<String> {
    wrap_plain_text(text, usize::from(width.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn wraps_wide_text_without_exceeding_width() {
        let lines = wrap_text("你好世界abc", 6);
        assert_eq!(lines, vec!["你好世", "界abc"]);
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 6));
    }

    #[test]
    fn strips_terminal_control_sequences() {
        assert_eq!(
            sanitize_terminal_text("ok\x1b[31m red\x1b[0m\nnext"),
            "ok red\nnext"
        );
    }
}
