//! Shared wrapping for tool chrome, markdown prose, and plain text.
//!
//! Break at whitespace and CJK characters. Latin words, paths, and URLs stay
//! intact until they are wider than a full row, then they hard-break.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Debug)]
pub struct WrapGrapheme<S> {
    pub cluster: String,
    pub width: usize,
    pub whitespace: bool,
    pub style: S,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TokenKind {
    Space,
    Cjk,
    Word,
}

pub fn wrap_plain_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for source_line in text.split('\n') {
        let source_line = source_line.strip_suffix('\r').unwrap_or(source_line);
        let graphemes = plain_graphemes(source_line);
        for row in wrap_graphemes(&graphemes, width) {
            lines.push(row.into_iter().map(|grapheme| grapheme.cluster).collect());
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub fn wrap_styled_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    wrap_styled_line_with_prefix(line, Line::default(), Line::default(), width)
}

/// Wrap `line` into rows of `width`, placing `prefix` on the first row and
/// `hanging` on every continuation row. Prefixes occupy width; the remaining
/// columns are used for content.
pub fn wrap_styled_line_with_prefix(
    line: &Line<'static>,
    prefix: Line<'static>,
    hanging: Line<'static>,
    width: usize,
) -> Vec<Line<'static>> {
    let graphemes = styled_line_graphemes(line);
    let prefix_width = line_width(&prefix);
    let hanging_width = line_width(&hanging);
    let first_width = width.saturating_sub(prefix_width).max(1);
    let rest_width = width.saturating_sub(hanging_width).max(1);
    let rows = if graphemes.is_empty() {
        vec![Vec::new()]
    } else {
        wrap_graphemes_with_widths(&graphemes, first_width, rest_width)
    };
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut spans = if index == 0 {
                prefix.spans.clone()
            } else {
                hanging.spans.clone()
            };
            spans.extend(line_from_graphemes(row).spans);
            Line::from(spans)
        })
        .collect()
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

pub fn wrap_graphemes<S: Clone>(
    graphemes: &[WrapGrapheme<S>],
    width: usize,
) -> Vec<Vec<WrapGrapheme<S>>> {
    wrap_graphemes_with_widths(graphemes, width, width)
}

pub fn wrap_graphemes_with_widths<S: Clone>(
    graphemes: &[WrapGrapheme<S>],
    first_width: usize,
    rest_width: usize,
) -> Vec<Vec<WrapGrapheme<S>>> {
    let first_width = first_width.max(1);
    let rest_width = rest_width.max(1);
    if graphemes.is_empty() {
        return vec![Vec::new()];
    }

    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;
    let mut row_width = first_width;

    for token in tokenize(graphemes) {
        let token_width = token.iter().map(|grapheme| grapheme.width).sum::<usize>();
        let is_space = token.first().is_some_and(|grapheme| grapheme.whitespace);

        if !is_space && token_width > row_width {
            flush_row(&mut rows, &mut current, &mut current_width);
            if !rows.is_empty() {
                row_width = rest_width;
            }
            for grapheme in token {
                if current_width > 0 && current_width.saturating_add(grapheme.width) > row_width {
                    rows.push(std::mem::take(&mut current));
                    current_width = 0;
                    row_width = rest_width;
                }
                current_width = current_width.saturating_add(grapheme.width);
                current.push(grapheme);
            }
            continue;
        }

        if current_width > 0 && current_width.saturating_add(token_width) > row_width {
            flush_row(&mut rows, &mut current, &mut current_width);
            row_width = rest_width;
            if is_space {
                continue;
            }
        }

        current_width = current_width.saturating_add(token_width);
        current.extend(token);
    }

    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}

fn flush_row<S: Clone>(
    rows: &mut Vec<Vec<WrapGrapheme<S>>>,
    current: &mut Vec<WrapGrapheme<S>>,
    current_width: &mut usize,
) {
    while current.last().is_some_and(|grapheme| grapheme.whitespace) {
        current.pop();
    }
    if !current.is_empty() {
        rows.push(std::mem::take(current));
    }
    *current_width = 0;
}

fn tokenize<S: Clone>(graphemes: &[WrapGrapheme<S>]) -> Vec<Vec<WrapGrapheme<S>>> {
    let mut tokens = Vec::new();
    let mut current = Vec::new();
    let mut kind = None;

    for grapheme in graphemes {
        let next = token_kind(grapheme);
        if next == TokenKind::Cjk {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            tokens.push(vec![grapheme.clone()]);
            kind = None;
            continue;
        }
        if kind.is_some_and(|kind| kind != next) && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        kind = Some(next);
        current.push(grapheme.clone());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn token_kind<S>(grapheme: &WrapGrapheme<S>) -> TokenKind {
    if grapheme.whitespace {
        TokenKind::Space
    } else if is_cjk_grapheme(&grapheme.cluster) {
        TokenKind::Cjk
    } else {
        TokenKind::Word
    }
}

fn plain_graphemes(text: &str) -> Vec<WrapGrapheme<()>> {
    UnicodeSegmentation::graphemes(text, true)
        .map(|cluster| WrapGrapheme {
            width: display_width(cluster),
            whitespace: cluster.chars().all(char::is_whitespace),
            cluster: cluster.to_string(),
            style: (),
        })
        .collect()
}

fn styled_line_graphemes(line: &Line<'static>) -> Vec<WrapGrapheme<Style>> {
    let mut graphemes = Vec::new();
    for span in &line.spans {
        for cluster in UnicodeSegmentation::graphemes(span.content.as_ref(), true) {
            graphemes.push(WrapGrapheme {
                width: display_width(cluster),
                whitespace: cluster.chars().all(char::is_whitespace),
                cluster: cluster.to_string(),
                style: span.style,
            });
        }
    }
    graphemes
}

fn line_from_graphemes(row: Vec<WrapGrapheme<Style>>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for grapheme in row {
        if let Some(last) = spans.last_mut().filter(|span| span.style == grapheme.style) {
            last.content.to_mut().push_str(&grapheme.cluster);
        } else {
            spans.push(Span::styled(grapheme.cluster, grapheme.style));
        }
    }
    Line::from(spans)
}

fn display_width(cluster: &str) -> usize {
    UnicodeWidthStr::width(cluster).max(
        cluster
            .chars()
            .next()
            .and_then(UnicodeWidthChar::width)
            .unwrap_or(0),
    )
}

fn is_cjk_grapheme(cluster: &str) -> bool {
    cluster.chars().any(is_cjk_char)
}

fn is_cjk_char(character: char) -> bool {
    matches!(
        character,
        '\u{1100}'..='\u{11FF}'
            | '\u{2E80}'..='\u{2FDF}'
            | '\u{3040}'..='\u{30FF}'
            | '\u{3100}'..='\u{312F}'
            | '\u{3130}'..='\u{318F}'
            | '\u{31A0}'..='\u{31BF}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7AF}'
            | '\u{D7B0}'..='\u{D7FF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FF66}'..='\u{FF9D}'
            | '\u{20000}'..='\u{2A6DF}'
            | '\u{2A700}'..='\u{2B73F}'
            | '\u{2B740}'..='\u{2B81F}'
            | '\u{2B820}'..='\u{2CEAF}'
            | '\u{2CEB0}'..='\u{2EBEF}'
            | '\u{30000}'..='\u{3134F}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(text: &str, width: usize) -> Vec<String> {
        wrap_plain_text(text, width)
    }

    #[test]
    fn wraps_english_at_spaces() {
        assert_eq!(wrap("hello world", 8), vec!["hello", "world"]);
    }

    #[test]
    fn keeps_paths_intact_when_they_fit_on_the_next_row() {
        assert_eq!(
            wrap("see crates/ash-tui.rs", 20),
            vec!["see", "crates/ash-tui.rs"]
        );
    }

    #[test]
    fn hard_breaks_tokens_wider_than_the_row() {
        assert_eq!(
            wrap("Edit /home/user/file.rs", 10),
            vec!["Edit", "/home/user", "/file.rs"]
        );
    }

    #[test]
    fn wraps_cjk_per_character_and_keeps_latin_tokens() {
        assert_eq!(wrap("你好世界abc", 6), vec!["你好世", "界abc"]);
        assert_eq!(
            wrap("查看crates/ash-tui", 10),
            vec!["查看", "crates/ash", "-tui"]
        );
    }

    #[test]
    fn preserves_source_newlines() {
        assert_eq!(wrap("one\ntwo", 80), vec!["one", "two"]);
    }

    #[test]
    fn hanging_prefix_aligns_continuation_rows() {
        let rows = wrap_styled_line_with_prefix(
            &Line::from("one two three four"),
            Line::from("Read "),
            Line::from("     "),
            12,
        );
        let texts: Vec<String> = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect();
        assert_eq!(texts, vec!["Read one two", "     three", "     four"]);
    }
}
