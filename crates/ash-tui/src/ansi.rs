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
#[cfg(test)]
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Display budget for a collapsed block: tool output, bash command
/// continuations, and live reasoning all preview at this many lines.
pub const COLLAPSED_MAX_LINES: usize = 5;

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
const fn is_command_boundary(ch: char) -> bool {
    matches!(ch, '&' | '|' | ';')
}

/// Highlight a bash command string with syntax colors, one `Line` per source
/// line. A lightweight hand-rolled lexer replaces the syntect engine: it
/// scans for quoted strings, `$var`, `# comments`, and known keywords, and
/// colors everything else with the default foreground. This covers the
/// simple commands agents actually run; exotic constructs just fall back to
/// plain text.
pub fn highlight_bash_command(command: &str) -> Vec<Line<'static>> {
    command.lines().map(highlight_bash_line).collect()
}

fn highlight_bash_line(line: &str) -> Line<'static> {
    let mut highlight = BashLineHighlight::new(line);
    while let Some((index, ch)) = highlight.chars.next() {
        match ch {
            '\'' => {
                let span = highlight.read_single_quote();
                highlight.push_styled(span);
            }
            '"' => {
                let span = highlight.read_double_quote();
                highlight.push_styled(span);
            }
            '$' => {
                let span = highlight.read_variable();
                highlight.push_styled(span);
            }
            '#' if highlight.starts_comment(index) => {
                let span = highlight.read_comment();
                highlight.push_styled(span);
                break;
            }
            c if c.is_ascii_alphanumeric() || c == '_' => {
                highlight.push_word(c);
            }
            '-' => {
                let span = highlight.read_flag();
                highlight.push_styled(span);
            }
            c if is_command_boundary(c) => {
                highlight.plain.push(ch);
                highlight.at_command_pos = true;
            }
            _ => {
                highlight.plain.push(ch);
            }
        }
    }
    highlight.finish()
}

struct BashLineHighlight<'a> {
    spans: Vec<Span<'static>>,
    plain: String,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    line: &'a str,
    at_command_pos: bool,
}

impl<'a> BashLineHighlight<'a> {
    fn new(line: &'a str) -> Self {
        Self {
            spans: Vec::new(),
            plain: String::new(),
            chars: line.char_indices().peekable(),
            line,
            at_command_pos: true,
        }
    }

    fn finish(mut self) -> Line<'static> {
        self.flush_plain();
        Line::from(self.spans)
    }

    fn flush_plain(&mut self) {
        if !self.plain.is_empty() {
            self.spans.push(Span::raw(std::mem::take(&mut self.plain)));
        }
    }

    fn push_styled(&mut self, span: Span<'static>) {
        self.flush_plain();
        self.spans.push(span);
    }

    fn starts_comment(&self, index: usize) -> bool {
        index == 0 || self.line.as_bytes()[index - 1] == b' '
    }

    fn read_single_quote(&mut self) -> Span<'static> {
        let mut content = String::from("'");
        for (_, c) in self.chars.by_ref() {
            content.push(c);
            if c == '\'' {
                break;
            }
        }
        styled_span(content, bash_palette::STRING)
    }

    fn read_double_quote(&mut self) -> Span<'static> {
        let mut content = String::from("\"");
        let mut escaped = false;
        for (_, c) in self.chars.by_ref() {
            content.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                break;
            }
        }
        styled_span(content, bash_palette::STRING)
    }

    fn read_variable(&mut self) -> Span<'static> {
        let mut content = String::from("$");
        if let Some((_, '{')) = self.chars.peek() {
            self.chars.next();
            content.push('{');
            for (_, c) in self.chars.by_ref() {
                content.push(c);
                if c == '}' {
                    break;
                }
            }
        } else {
            for (_, c) in self.chars.by_ref() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    content.push(c);
                } else {
                    break;
                }
            }
        }
        styled_span(content, bash_palette::VARIABLE)
    }

    fn read_comment(&mut self) -> Span<'static> {
        let content: String = self.chars.by_ref().map(|(_, c)| c).collect();
        styled_span(format!("#{content}"), bash_palette::COMMENT)
    }

    fn push_word(&mut self, first: char) {
        let mut word = String::from(first);
        while let Some((_, c)) = self.chars.peek() {
            if c.is_ascii_alphanumeric() || *c == '_' {
                word.push(*c);
                self.chars.next();
            } else {
                break;
            }
        }
        let is_keyword = BASH_KEYWORDS.contains(&word.as_str());
        let style = if is_keyword {
            Style::default().fg(bash_palette::KEYWORD)
        } else if self.at_command_pos {
            Style::default().fg(bash_palette::COMMAND)
        } else {
            Style::default()
        };
        // A control-flow keyword does not consume the command position:
        // `if cd /tmp` still colors `cd` as a command.
        if !is_keyword {
            self.at_command_pos = false;
        }
        self.push_styled(Span::styled(word, style));
    }

    fn read_flag(&mut self) -> Span<'static> {
        let mut word = String::from("-");
        while let Some((_, c)) = self.chars.peek() {
            if c.is_ascii_alphanumeric() || *c == '-' || *c == '_' {
                word.push(*c);
                self.chars.next();
            } else {
                break;
            }
        }
        styled_span(word, bash_palette::FLAG)
    }
}

fn styled_span(content: String, color: ratatui::style::Color) -> Span<'static> {
    Span::styled(content, Style::default().fg(color))
}

/// Parse one line of text that may contain ANSI escape sequences into a
/// ratatui `Line` with styled spans. Falls back to plain text on parse error.
pub fn parse_ansi_line(line: &str) -> Line<'static> {
    let expanded = expand_tabs(line);
    match expanded.as_ref().into_text() {
        Ok(text) => text.lines.into_iter().next().unwrap_or_default(),
        Err(_) => Line::from(expanded.to_string()),
    }
}

/// Wrap a highlighted `Line` with the shared token rule: whitespace and CJK
/// break, other tokens stay intact until they exceed the row width.
pub fn wrap_highlighted_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    crate::wrap::wrap_styled_line(line, width)
}

#[cfg(test)]
/// Render tool output for display: the first `head` lines, then an ellipsis
/// marker, then the last `tail` lines when the output is too long. Each shown
/// line is parsed for ANSI colors and dimmed to visually recede behind the
/// tool title. The first line uses `first_prefix` (e.g. `└ `) and all later
/// lines use `subsequent_prefix` (e.g. four spaces).
pub fn split_output(
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
pub fn split_with_ellipsis<T>(mut items: Vec<T>, head: usize, tail: usize, ellipsis: T) -> Vec<T> {
    let total = items.len();
    if total <= head + tail {
        return items;
    }
    let mut selected: Vec<T> = items.drain(..head).collect();
    selected.push(ellipsis);
    selected.extend(items.drain(total - head - tail..));
    selected
}

#[cfg(test)]
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

    fn plain(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    #[test]
    fn wraps_at_spaces_and_hard_breaks_overlong_tokens() {
        let line = parse_ansi_line("Edit /home/user/file.rs");
        let wrapped = wrap_highlighted_line(&line, 10);
        assert_eq!(
            wrapped.iter().map(plain).collect::<Vec<_>>(),
            vec!["Edit", "/home/user", "/file.rs"]
        );
    }
}
