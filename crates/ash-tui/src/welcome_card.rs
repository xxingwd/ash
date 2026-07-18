use std::path::{Path, PathBuf};

use crate::{
    scrollback::sanitize_single_line,
    text_width::{truncate_end, truncate_start},
};

const SUBTITLE: &str = "Terminal coding agent";
const FULL_WORDMARK_MIN_WIDTH: u16 = 28;
const COMPACT_WORDMARK_MIN_WIDTH: u16 = 12;

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
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct WelcomeLine {
    pub(crate) text: String,
    pub(crate) style: WelcomeStyle,
}

pub(crate) fn welcome_card(available_width: u16, working_dir: &Path) -> Vec<WelcomeLine> {
    let width = usize::from(available_width.max(1));
    let wordmark: &[&str] = if available_width >= FULL_WORDMARK_MIN_WIDTH {
        &FULL_WORDMARK
    } else if available_width >= COMPACT_WORDMARK_MIN_WIDTH {
        &COMPACT_WORDMARK
    } else {
        &[]
    };
    let mut lines = if wordmark.is_empty() {
        vec![WelcomeLine {
            text: truncate_end("ASH", width),
            style: WelcomeStyle::Title,
        }]
    } else {
        wordmark
            .iter()
            .map(|line| WelcomeLine {
                text: truncate_end(line.trim_start(), width),
                style: WelcomeStyle::Logo,
            })
            .collect()
    };
    lines.extend([
        WelcomeLine {
            text: truncate_end(SUBTITLE, width),
            style: WelcomeStyle::Subtitle,
        },
        WelcomeLine {
            text: truncate_start(&workspace_label(working_dir), width),
            style: WelcomeStyle::Subtitle,
        },
    ]);
    lines
}

fn workspace_label(working_dir: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    workspace_label_with_home(working_dir, home.as_deref())
}

fn workspace_label_with_home(working_dir: &Path, home: Option<&Path>) -> String {
    let display = home
        .and_then(|home| working_dir.strip_prefix(home).ok())
        .map_or_else(
            || working_dir.display().to_string(),
            |relative| {
                if relative.as_os_str().is_empty() {
                    "~".to_string()
                } else {
                    format!("~/{}", relative.display())
                }
            },
        );
    sanitize_single_line(&display)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_left_aligned_welcome() {
        let lines = welcome_card(80, Path::new("/home/example/workspace/ash"));
        assert_eq!(lines[0].text, FULL_WORDMARK[0].trim_start());
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.style == WelcomeStyle::Logo)
                .count(),
            FULL_WORDMARK.len()
        );
        assert_eq!(lines[lines.len() - 2].text, SUBTITLE);
        assert_eq!(lines[lines.len() - 1].text, "/home/example/workspace/ash");
    }

    #[test]
    fn truncates_welcome_lines_without_centering_or_framing() {
        let lines = welcome_card(24, Path::new("/project"));
        assert_eq!(lines[0].text, COMPACT_WORDMARK[0]);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.style == WelcomeStyle::Logo)
                .count(),
            COMPACT_WORDMARK.len()
        );
        assert_eq!(lines[lines.len() - 2].text, SUBTITLE);
        assert_eq!(lines[lines.len() - 1].text, "/project");
    }

    #[test]
    fn abbreviates_the_home_directory_in_the_workspace_line() {
        assert_eq!(
            workspace_label_with_home(
                Path::new("/home/example/workspace/ash"),
                Some(Path::new("/home/example")),
            ),
            "~/workspace/ash"
        );
    }

    #[test]
    fn truncates_the_start_of_long_workspace_paths() {
        assert_eq!(truncate_start("~/very/long/workspace", 12), "…g/workspace");
    }
}
