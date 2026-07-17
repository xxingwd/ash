use std::path::Path;

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

pub(crate) fn fit_status_left(model: &str, path: &str, width: u16) -> (String, Option<String>) {
    let model_width = UnicodeWidthStr::width(model) as u16;
    if model_width >= width || path.is_empty() {
        return (truncate_end(model, usize::from(width)), None);
    }

    let path_width = width.saturating_sub(model_width.saturating_add(3));
    if path_width == 0 {
        (truncate_end(model, usize::from(width)), None)
    } else {
        (
            model.to_string(),
            Some(truncate_end(path, usize::from(path_width))),
        )
    }
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
    fn prioritizes_model_then_path() {
        assert_eq!(
            fit_status_left("gpt-5", "~/workspace/ash", 24),
            ("gpt-5".to_string(), Some("~/workspace/ash".to_string()))
        );
        assert_eq!(
            fit_status_left("gpt-5", "~/workspace/ash", 12),
            ("gpt-5".to_string(), Some("~/w…".to_string()))
        );
    }
}
