use std::path::{Path, PathBuf};

/// Ash data directory, i.e. `<user home>/.ash`.
///
/// Sessions, message history and other platform-owned data live below this
/// directory. Falls back to a relative `.ash` directory when the current
/// user's home directory cannot be determined.
pub fn ash_data_dir() -> PathBuf {
    let base_dirs = directories::BaseDirs::new();
    ash_data_dir_from_home(base_dirs.as_ref().map(|dirs| dirs.home_dir()))
}

fn ash_data_dir_from_home(home_dir: Option<&Path>) -> PathBuf {
    home_dir.map_or_else(|| PathBuf::from(".ash"), |home| home.join(".ash"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ash_data_dir_is_under_the_user_home() {
        assert_eq!(
            ash_data_dir_from_home(Some(Path::new("user-home"))),
            PathBuf::from("user-home").join(".ash")
        );
    }

    #[test]
    fn ash_data_dir_falls_back_to_a_relative_dot_ash_dir() {
        assert_eq!(ash_data_dir_from_home(None), PathBuf::from(".ash"));
    }
}
