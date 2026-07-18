use std::path::{Path, PathBuf};

use crate::{
    scrollback::sanitize_single_line,
    text_width::{truncate_end, truncate_start},
};

const SUBTITLE: &str = "Terminal coding agent";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WelcomeStyle {
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
    vec![
        WelcomeLine {
            text: truncate_end("ASH", width),
            style: WelcomeStyle::Title,
        },
        WelcomeLine {
            text: truncate_end(SUBTITLE, width),
            style: WelcomeStyle::Subtitle,
        },
        WelcomeLine {
            text: truncate_start(&workspace_label(working_dir), width),
            style: WelcomeStyle::Subtitle,
        },
    ]
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
        assert_eq!(lines[0].text, "ASH");
        assert_eq!(lines[1].text, SUBTITLE);
        assert_eq!(lines[2].text, "/home/example/workspace/ash");
    }

    #[test]
    fn truncates_welcome_lines_without_centering_or_framing() {
        let lines = welcome_card(24, Path::new("/project"));
        assert_eq!(lines[0].text, "ASH");
        assert_eq!(lines[1].text, SUBTITLE);
        assert_eq!(lines[2].text, "/project");
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
