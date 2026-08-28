use std::path::Path;

use unicode_width::UnicodeWidthStr;

use crate::text_width::truncate_end;

pub fn compact_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    compact_path_with_home(path, home.as_deref())
}

fn compact_path_with_home(path: &Path, home: Option<&Path>) -> String {
    home.and_then(|home| path.strip_prefix(home).ok())
        .map_or_else(
            || path.display().to_string(),
            |relative| {
                if relative.as_os_str().is_empty() {
                    "~".into()
                } else {
                    format!("~/{}", relative.display())
                }
            },
        )
}

pub fn format_token_count(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format_compact(tokens, 1_000, "k"),
        _ => format_compact(tokens, 1_000_000, "M"),
    }
}

pub fn format_token_usage(input_tokens: u64, output_tokens: u64) -> String {
    format!(
        "{} in / {} out",
        format_token_count(input_tokens),
        format_token_count(output_tokens),
    )
}

pub fn format_token_rate(tokens: u64, duration_ms: u64) -> Option<String> {
    (tokens > 0 && duration_ms > 0).then(|| {
        let tenths = tokens.saturating_mul(10_000) / duration_ms;
        if tenths.is_multiple_of(10) {
            format!("{} tok/s", tenths / 10)
        } else {
            format!("{}.{:01} tok/s", tenths / 10, tenths % 10)
        }
    })
}

pub fn format_elapsed(elapsed_seconds: u64) -> String {
    if elapsed_seconds < 60 {
        return format!("{elapsed_seconds}s");
    }
    if elapsed_seconds < 3600 {
        return format!("{}m {:02}s", elapsed_seconds / 60, elapsed_seconds % 60);
    }
    format!(
        "{}h {:02}m {:02}s",
        elapsed_seconds / 3600,
        (elapsed_seconds % 3600) / 60,
        elapsed_seconds % 60
    )
}

fn format_compact(value: u64, scale: u64, suffix: &str) -> String {
    let tenths = value.saturating_mul(10) / scale;
    format!("{}.{:01}{suffix}", tenths / 10, tenths % 10)
}

pub fn fit_status_left(model: &str, path: &str, width: u16) -> (String, Option<String>) {
    let model_width = u16::try_from(UnicodeWidthStr::width(model)).unwrap_or(u16::MAX);
    let path_width = width.saturating_sub(model_width.saturating_add(3));
    if model_width >= width || path.is_empty() || path_width == 0 {
        return (truncate_end(model, usize::from(width)), None);
    }

    (
        model.to_string(),
        Some(truncate_end(path, usize::from(path_width))),
    )
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

    #[test]
    fn abbreviates_home_prefix_with_tilde() {
        assert_eq!(
            compact_path_with_home(
                Path::new("/home/example/workspace/ash"),
                Some(Path::new("/home/example"))
            ),
            "~/workspace/ash"
        );
        assert_eq!(
            compact_path_with_home(Path::new("/home/example"), Some(Path::new("/home/example"))),
            "~"
        );
        assert_eq!(
            compact_path_with_home(Path::new("/elsewhere"), Some(Path::new("/home/example"))),
            "/elsewhere"
        );
    }

    #[test]
    fn formats_token_counts_and_rates() {
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(12_345), "12.3k");
        assert_eq!(format_token_count(1_234_567), "1.2M");
        assert_eq!(format_token_usage(12_345, 678), "12.3k in / 678 out");
        assert_eq!(format_token_rate(250, 2_000).as_deref(), Some("125 tok/s"));
        assert_eq!(format_token_rate(1, 300).as_deref(), Some("3.3 tok/s"));
        assert_eq!(format_token_rate(1, 0), None);
    }

    #[test]
    fn formats_elapsed_status_like_codex() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(61), "1m 01s");
        assert_eq!(format_elapsed(3661), "1h 01m 01s");
    }
}
