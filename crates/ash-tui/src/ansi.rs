//! ANSI terminal output rendering for tool results.
//!
//! bash and other tools may emit ANSI escape sequences (colored output from
//! `ls --color`, build tools, git, etc.). We parse those sequences into
//! ratatui `Span`s so the colors survive into the TUI, instead of stripping
//! them as plain text.
//!
//! Truncation (head + ellipsis + tail) happens here, at the display layer,
//! so the agent still receives the full tool output for reasoning.

use ansi_to_tui::IntoText;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Display budget for a collapsed block: tool output, bash command
/// continuations, and live reasoning all preview at this many lines.
pub(crate) const COLLAPSED_MAX_LINES: usize = 5;
/// Display budget when tool blocks are expanded (`Ctrl+o`).
pub(crate) const EXPANDED_MAX_LINES: usize = 50;

/// Bash highlighting palette. Uses the same base ratatui colors as the
/// markdown renderer (Green for quotes, Cyan for code, Blue for markers) so
/// the TUI stays visually consistent.
mod bash_palette {
    use ratatui::style::Color;

    /// Control-flow keywords (`if`, `then`, `fi`…).
    pub(super) const KEYWORD: Color = Color::Magenta;
    /// Quoted strings and here-strings.
    pub(super) const STRING: Color = Color::Green;
    /// `$variable` expansions.
    pub(super) const VARIABLE: Color = Color::Cyan;
    /// `# comments`.
    pub(super) const COMMENT: Color = Color::DarkGray;
    /// `-flag` options.
    pub(super) const FLAG: Color = Color::Blue;
    /// The command word at the start of a command.
    pub(super) const COMMAND: Color = Color::Cyan;
}

/// Control-flow words that keep the keyword color wherever they appear.
/// Builtins like `cd`/`echo` are deliberately absent: they are commands and
/// get the command color when they start a command.
const BASH_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "time", "coproc", "break", "continue",
];

/// The first word after a command boundary (start of line, `&&`, `||`, `;`,
/// or `|`) is the command and gets the command color. A keyword in that
/// position keeps the keyword color and does not consume the boundary, so
/// `if cd /tmp` still colors `cd` as a command.
fn is_command_boundary(ch: char) -> bool {
    matches!(ch, '&' | '|' | ';')
}

/// Highlight a bash command string with syntax colors, one `Line` per source
/// line. A lightweight hand-rolled lexer replaces the syntect engine: it
/// scans for quoted strings, `$var`, `# comments`, and known keywords, and
/// colors everything else with the default foreground. This covers the
/// simple commands agents actually run; exotic constructs just fall back to
/// plain text.
pub(crate) fn highlight_bash_command(command: &str) -> Vec<Line<'static>> {
    command.lines().map(highlight_bash_line).collect()
}

fn highlight_bash_line(line: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut chars = line.char_indices().peekable();
    // True when the next word starts a command (start of line or right after
    // `&&`, `||`, `;`, `|`).
    let mut at_command_pos = true;

    macro_rules! flush_plain {
        () => {
            if !plain.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut plain)));
            }
        };
    }

    while let Some((index, ch)) = chars.next() {
        match ch {
            // Single-quoted string: literal until the closing quote.
            '\'' => {
                flush_plain!();
                let mut content = String::from("'");
                let mut closed = false;
                for (_, c) in chars.by_ref() {
                    content.push(c);
                    if c == '\'' {
                        closed = true;
                        break;
                    }
                }
                spans.push(Span::styled(
                    content,
                    Style::default().fg(bash_palette::STRING),
                ));
                let _ = closed;
            }
            // Double-quoted string: honor backslash escapes.
            '"' => {
                flush_plain!();
                let mut content = String::from("\"");
                let mut closed = false;
                let mut escaped = false;
                for (_, c) in chars.by_ref() {
                    content.push(c);
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        closed = true;
                        break;
                    }
                }
                spans.push(Span::styled(
                    content,
                    Style::default().fg(bash_palette::STRING),
                ));
                let _ = closed;
            }
            // Variable: `$name`, `${name}`, `$1`.
            '$' => {
                flush_plain!();
                let mut content = String::from("$");
                if let Some((_, '{')) = chars.peek() {
                    chars.next();
                    content.push('{');
                    for (_, c) in chars.by_ref() {
                        content.push(c);
                        if c == '}' {
                            break;
                        }
                    }
                } else {
                    for (_, c) in chars.by_ref() {
                        if c.is_ascii_alphanumeric() || c == '_' {
                            content.push(c);
                        } else {
                            break;
                        }
                    }
                }
                spans.push(Span::styled(
                    content,
                    Style::default().fg(bash_palette::VARIABLE),
                ));
            }
            // Comment: to end of line.
            '#' if index == 0 || line.as_bytes()[index - 1] == b' ' => {
                flush_plain!();
                let content: String = chars.by_ref().map(|(_, c)| c).collect::<String>();
                spans.push(Span::styled(
                    format!("#{content}"),
                    Style::default().fg(bash_palette::COMMENT),
                ));
                break;
            }
            // Alphanumeric run: command, keyword, or plain word.
            c if c.is_ascii_alphanumeric() || c == '_' => {
                flush_plain!();
                let mut word = String::from(c);
                while let Some((_, c)) = chars.peek() {
                    if c.is_ascii_alphanumeric() || *c == '_' {
                        word.push(*c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let is_keyword = BASH_KEYWORDS.contains(&word.as_str());
                let style = if is_keyword {
                    Style::default().fg(bash_palette::KEYWORD)
                } else if at_command_pos {
                    Style::default().fg(bash_palette::COMMAND)
                } else {
                    Style::default()
                };
                // A control-flow keyword does not consume the command
                // position: `if cd /tmp` still colors `cd` as a command.
                if !is_keyword {
                    at_command_pos = false;
                }
                spans.push(Span::styled(word, style));
            }
            '-' => {
                flush_plain!();
                let mut word = String::from("-");
                while let Some((_, c)) = chars.peek() {
                    if c.is_ascii_alphanumeric() || *c == '-' || *c == '_' {
                        word.push(*c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                spans.push(Span::styled(word, Style::default().fg(bash_palette::FLAG)));
            }
            // Command separator: the next word is a command.
            c if is_command_boundary(c) => {
                plain.push(ch);
                at_command_pos = true;
            }
            _ => {
                plain.push(ch);
            }
        }
    }
    flush_plain!();
    Line::from(spans)
}

/// Parse one line of text that may contain ANSI escape sequences into a
/// ratatui `Line` with styled spans. Falls back to plain text on parse error.
pub(crate) fn parse_ansi_line(line: &str) -> Line<'static> {
    let expanded = expand_tabs(line);
    match expanded.as_ref().into_text() {
        Ok(text) => text.lines.into_iter().next().unwrap_or_default(),
        Err(_) => Line::from(expanded.to_string()),
    }
}

/// Wrap a highlighted `Line` to a display width, splitting at the last space
/// that fits on each row so long tokens stay intact. Styles are preserved per
/// character, so syntax colors survive the wrap.
pub(crate) fn wrap_highlighted_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    // Flatten the line into (style, char) pairs.
    let mut chars: Vec<(Style, String)> = Vec::new();
    for span in &line.spans {
        for character in span.content.chars() {
            chars.push((span.style, character.to_string()));
        }
    }
    if chars.is_empty() {
        return vec![Line::default()];
    }

    let char_width = |text: &str| {
        unicode_width::UnicodeWidthChar::width(text.chars().next().unwrap_or(' '))
            .unwrap_or(0)
            .max(1)
    };

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<(Style, String)> = Vec::new();
    let mut current_width = 0usize;
    // Byte index (into `current`) of the last breakable space, and the row
    // width at that point.
    let mut last_break: Option<(usize, usize)> = None;

    let flush = |rows: &mut Vec<Line<'static>>, segment: &[(Style, String)]| {
        let mut row = Line::default();
        for (s, c) in segment {
            row.spans.push(Span::styled(c.clone(), *s));
        }
        rows.push(row);
    };

    for (style, text) in chars {
        let cw = char_width(&text);
        let is_space = text == " ";
        if current_width > 0 && current_width + cw > width {
            // Row is full. Break at the last space if one was seen, else here.
            let (split, _) = last_break.unwrap_or((current.len(), current_width));
            flush(&mut rows, &current[..split]);
            // The remainder after the split becomes the start of the next row.
            current = current.split_off(split);
            current_width = 0;
            for (_, c) in &current {
                current_width += char_width(c);
            }
            last_break = current
                .iter()
                .rposition(|(_, c)| c == " ")
                .map(|position| (position, 0));
        }
        current.push((style, text));
        current_width += cw;
        if is_space {
            last_break = Some((current.len(), current_width));
        }
    }
    if !current.is_empty() {
        flush(&mut rows, &current);
    }
    rows
}

/// Render tool output for display: the first `head` lines, then an ellipsis
/// marker, then the last `tail` lines when the output is too long. Each shown
/// line is parsed for ANSI colors and dimmed to visually recede behind the
/// tool title. The first line uses `first_prefix` (e.g. `└ `) and all later
/// lines use `subsequent_prefix` (e.g. four spaces).
pub(crate) fn split_output(
    output: &str,
    head: usize,
    tail: usize,
    first_prefix: &str,
    subsequent_prefix: &str,
    dim: bool,
) -> Vec<Line<'static>> {
    let lines: Vec<&str> = output.lines().collect();
    let rendered = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let mut rendered = parse_ansi_line(line);
            let prefix = if index == 0 {
                first_prefix
            } else {
                subsequent_prefix
            };
            prefix_line(&mut rendered, prefix, dim);
            rendered
        })
        .collect::<Vec<_>>();
    let total = rendered.len();
    if total <= head + tail {
        return rendered;
    }
    let omitted = total - head - tail;
    let mut ellipsis = Line::from(format!(
        "{subsequent_prefix}… +{omitted} lines (truncated for display)"
    ));
    for span in &mut ellipsis.spans {
        span.style = span.style.add_modifier(Modifier::DIM);
    }
    split_with_ellipsis(rendered, head, tail, ellipsis)
}

/// Keep the first `head` and last `tail` items of `items`, inserting
/// `ellipsis` between them when anything was omitted. Shared by tool output
/// and multi-line bash command truncation.
pub(crate) fn split_with_ellipsis<T>(
    mut items: Vec<T>,
    head: usize,
    tail: usize,
    ellipsis: T,
) -> Vec<T> {
    let total = items.len();
    if total <= head + tail {
        return items;
    }
    let mut selected: Vec<T> = items.drain(..head).collect();
    selected.push(ellipsis);
    selected.extend(items.drain(total - head - tail..));
    selected
}

fn prefix_line(line: &mut Line<'static>, prefix: &str, dim: bool) {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(prefix.to_string()));
    spans.extend(std::mem::take(&mut line.spans));
    if dim {
        for span in &mut spans {
            span.style = span.style.add_modifier(Modifier::DIM);
        }
    }
    line.spans = spans;
}

/// Expand tabs to spaces so gutter alignment stays stable.
fn expand_tabs(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\t') {
        std::borrow::Cow::Owned(text.replace('\t', "    "))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ansi_color_into_spans() {
        let line = parse_ansi_line("\x1b[31mred\x1b[0m text");
        let combined: String = line
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(combined, "red text");
        // First span carries the red foreground.
        assert!(line.spans[0].style.fg.is_some());
    }

    #[test]
    fn split_output_keeps_head_and_tail_with_ellipsis() {
        let output = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = split_output(&output, 3, 2, "  └ ", "    ", true);
        let text: Vec<String> = lines.iter().map(plain).collect();
        assert_eq!(text[0], "  └ line 1");
        assert_eq!(text[1], "    line 2");
        assert_eq!(text[2], "    line 3");
        assert!(text[3].contains("+5 lines"));
        assert_eq!(text[4], "    line 9");
        assert_eq!(text[5], "    line 10");
    }

    #[test]
    fn split_output_returns_all_lines_when_short() {
        let lines = split_output("a\nb", 3, 3, "", "", true);
        let text: Vec<String> = lines.iter().map(plain).collect();
        assert_eq!(text, vec!["a", "b"]);
    }

    fn plain(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }
}

#[cfg(test)]
mod highlight_tests {
    use super::*;

    #[test]
    fn highlights_bash_syntax_with_colors() {
        let lines = highlight_bash_command("cargo build --release");
        assert_eq!(lines.len(), 1);
        let styled = lines[0]
            .spans
            .iter()
            .filter(|span| span.style.fg.is_some())
            .count();
        assert!(styled > 0, "bash command should be syntax-highlighted");
    }

    #[test]
    fn multi_line_command_keeps_line_count() {
        let lines = highlight_bash_command("echo one\necho two");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn strings_variables_and_comments_get_their_own_colors() {
        let line = highlight_bash_line("echo \"hi $NAME\" $HOME # note");
        let fg_of = |needle: &str| {
            line.spans
                .iter()
                .find(|span| span.content.contains(needle))
                .and_then(|span| span.style.fg)
        };
        assert_eq!(
            fg_of("hi $NAME"),
            Some(bash_palette::STRING),
            "string color"
        );
        assert_eq!(
            fg_of("$HOME"),
            Some(bash_palette::VARIABLE),
            "variable color"
        );
        assert_eq!(fg_of("note"), Some(bash_palette::COMMENT), "comment color");
    }

    #[test]
    fn keywords_and_flags_get_distinct_colors() {
        let line = highlight_bash_line("cd /tmp && ls -la");
        let fg_of = |needle: &str| {
            line.spans
                .iter()
                .find(|span| span.content == needle)
                .and_then(|span| span.style.fg)
        };
        assert_eq!(
            fg_of("cd"),
            Some(bash_palette::COMMAND),
            "builtin as command"
        );
        assert_eq!(fg_of("ls"), Some(bash_palette::COMMAND), "command after &&");
        assert_eq!(fg_of("-la"), Some(bash_palette::FLAG), "flag color");
    }

    #[test]
    fn control_flow_keywords_keep_keyword_color() {
        let line = highlight_bash_line("if cd /tmp; then ls; fi");
        let fg_of = |needle: &str| {
            line.spans
                .iter()
                .find(|span| span.content == needle)
                .and_then(|span| span.style.fg)
        };
        assert_eq!(fg_of("if"), Some(bash_palette::KEYWORD), "if keyword");
        assert_eq!(fg_of("then"), Some(bash_palette::KEYWORD), "then keyword");
        assert_eq!(fg_of("fi"), Some(bash_palette::KEYWORD), "fi keyword");
        assert_eq!(fg_of("cd"), Some(bash_palette::COMMAND), "command after if");
        assert_eq!(
            fg_of("ls"),
            Some(bash_palette::COMMAND),
            "command after then"
        );
    }

    #[test]
    fn unclosed_quote_falls_back_to_string_color_without_panicking() {
        let line = highlight_bash_line("echo \"oops");
        let content: String = line
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(content, "echo \"oops");
    }
}

#[cfg(test)]
mod wrap_hl_tests {
    use super::*;

    #[test]
    fn wraps_long_highlighted_line() {
        let line = parse_ansi_line("a b c d e f g h");
        let wrapped = wrap_highlighted_line(&line, 10);
        assert!(
            wrapped.len() > 1,
            "should wrap, got {:?}",
            wrapped
                .iter()
                .map(|l| l
                    .spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>())
                .collect::<Vec<_>>()
        );
        for l in &wrapped {
            let w: usize = l
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.to_string().as_str()))
                .sum();
            assert!(w <= 10, "line too wide: {w}");
        }
    }
}
