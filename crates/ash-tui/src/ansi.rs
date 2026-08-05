//! ANSI terminal output rendering for tool results.
//!
//! bash and other tools may emit ANSI escape sequences (colored output from
//! `ls --color`, build tools, git, etc.). We parse those sequences into
//! ratatui `Span`s so the colors survive into the TUI, instead of stripping
//! them as plain text.
//!
//! Truncation (head + ellipsis + tail) happens here, at the display layer,
//! so the agent still receives the full tool output for reasoning.

use std::sync::OnceLock;

use ansi_to_tui::IntoText;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Default display budget for a bash/tool output block.
pub(crate) const TOOL_OUTPUT_MAX_LINES: usize = 5;
/// Display budget when a tool block is expanded (`Ctrl+o`).
pub(crate) const TOOL_OUTPUT_EXPANDED_MAX_LINES: usize = 50;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        // Same default theme as codex: Catppuccin Mocha (the dark variant).
        // An adaptive light/dark probe is possible but adds startup
        // complexity; keep the fixed dark theme for now.
        let themes = two_face::theme::extra();
        themes
            .get(two_face::theme::EmbeddedThemeName::CatppuccinMocha)
            .clone()
    })
}

fn bash_syntax() -> Option<&'static SyntaxReference> {
    syntax_set()
        .find_syntax_by_name("Shell Script (bash)")
        .or_else(|| syntax_set().find_syntax_by_extension("sh"))
        .or_else(|| syntax_set().find_syntax_by_name("Shell-Unix-Generic"))
}

/// Highlight a bash command string with syntax colors, one `Line` per source
/// line. Falls back to plain text when the bash grammar is unavailable.
pub(crate) fn highlight_bash_command(command: &str) -> Vec<Line<'static>> {
    let Some(syntax) = bash_syntax() else {
        return command
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect();
    };
    let mut highlighter = HighlightLines::new(syntax, theme());
    let mut lines = Vec::new();
    for source in LinesWithEndings::from(command) {
        match highlighter.highlight_line(source, syntax_set()) {
            Ok(ranges) => {
                let spans: Vec<Span<'static>> = ranges
                    .into_iter()
                    .map(|(style, text)| Span::styled(text.to_string(), syntect_style(style)))
                    .collect();
                lines.push(Line::from(spans));
            }
            Err(_) => lines.push(Line::from(source.to_string())),
        }
    }
    lines
}

/// Convert a syntect style to a ratatui style, following codex:
/// - alpha 0x01 ("terminal default foreground") → omit the foreground so the
///   terminal's own default color shows through (plain text reads as plain
///   text, not as a hardcoded gray).
/// - alpha 0xFF → plain RGB.
/// - italic/underline are skipped because many terminals render them poorly;
///   bold is kept.
fn syntect_style(style: syntect::highlighting::Style) -> Style {
    const ANSI_ALPHA_DEFAULT: u8 = 0x01;

    let mut ratatui_style = Style::default();
    let fg = style.foreground;
    if fg.a != ANSI_ALPHA_DEFAULT {
        // Non-ANSI alpha values in some bundled themes; treat as RGB.
        ratatui_style = ratatui_style.fg(Color::Rgb(fg.r, fg.g, fg.b));
    }
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::BOLD)
    {
        ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
    }
    ratatui_style
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
    let total = lines.len();
    if total <= head + tail {
        return lines
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
            .collect();
    }

    let mut rendered_lines = Vec::with_capacity(head + tail + 1);
    for (index, line) in lines.iter().take(head).enumerate() {
        let mut rendered = parse_ansi_line(line);
        let prefix = if index == 0 {
            first_prefix
        } else {
            subsequent_prefix
        };
        prefix_line(&mut rendered, prefix, dim);
        rendered_lines.push(rendered);
    }

    let omitted = total - head - tail;
    let mut ellipsis = Line::from(format!(
        "{subsequent_prefix}… +{omitted} lines (truncated for display)"
    ));
    for span in &mut ellipsis.spans {
        span.style = span.style.add_modifier(Modifier::DIM);
    }
    rendered_lines.push(ellipsis);

    for line in lines.iter().skip(total - tail) {
        let mut rendered = parse_ansi_line(line);
        prefix_line(&mut rendered, subsequent_prefix, dim);
        rendered_lines.push(rendered);
    }
    rendered_lines
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
}

#[cfg(test)]
mod current_theme_tests {
    use super::*;

    #[test]
    fn print_current_theme_colors() {
        let lines = highlight_bash_command("cd /tmp && ls -la");
        for span in &lines[0].spans {
            println!(
                "span: fg={:?} 内容={:?}",
                span.style.fg,
                span.content.to_string()
            );
        }
    }
}

#[cfg(test)]
mod alpha_check_tests {
    use super::*;

    #[test]
    fn print_alpha_values() {
        let ss = two_face::syntax::extra_newlines();
        let syntax = ss
            .find_syntax_by_name("Shell Script (bash)")
            .unwrap_or_else(|| ss.find_syntax_by_name("Shell-Unix-Generic").unwrap());
        let theme = theme();
        let mut h = syntect::easy::HighlightLines::new(syntax, theme);
        let ranges = h.highlight_line("cd /tmp && ls -la\n", &ss).unwrap();
        for (style, text) in ranges {
            println!(
                "alpha={} fg=({},{},{}) 内容={:?}",
                style.foreground.a,
                style.foreground.r,
                style.foreground.g,
                style.foreground.b,
                text
            );
        }
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
