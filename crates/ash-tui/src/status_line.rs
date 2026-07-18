use std::path::Path;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::text_width::truncate_end;

pub(crate) fn compact_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if let Some(relative) = home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        if relative.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", relative.display())
        }
    } else {
        path.display().to_string()
    }
}

pub(crate) fn prompt_header_line(model: &str, path: &Path, width: u16) -> Line<'static> {
    let path = compact_path(path);
    let model = model.trim();
    let width = usize::from(width.max(1));

    if path.is_empty() {
        return Line::from(Span::styled(
            truncate_end(model, width),
            Style::default().fg(Color::Cyan),
        ));
    }
    if model.is_empty() {
        return Line::from(Span::styled(
            truncate_end(&path, width),
            Style::default().fg(Color::Green),
        ));
    }

    let model_width = UnicodeWidthStr::width(model);
    if model_width.saturating_add(3) >= width {
        return Line::from(Span::styled(
            truncate_end(&format!("{path} · {model}"), width),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    let path_width = width - model_width - 3;
    Line::from(vec![
        Span::styled(
            truncate_end(&path, path_width),
            Style::default().fg(Color::Green),
        ),
        Span::styled(" · ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(model.to_string(), Style::default().fg(Color::Cyan)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_status_to_terminal_width() {
        let value = truncate_end("openai-responses · gpt-5 · ~/workspace/ash", 20);
        assert!(UnicodeWidthStr::width(value.as_str()) <= 20);
        assert!(value.ends_with('…'));
    }

    #[test]
    fn prompt_header_places_model_after_the_path() {
        let line = prompt_header_line("gpt-5", Path::new("/tmp/ash"), 24);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "/tmp/ash · gpt-5");
    }
}
