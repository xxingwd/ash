use std::path::PathBuf;

/// Platform data root directory, i.e. `~/.ash`.
///
/// Sessions, message history and other platform-owned data live below this
/// directory. Returns `None` when the current user's home directory cannot be
/// determined.
pub fn ash_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".ash"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ash_home_is_a_dot_ash_dir_under_the_home_dir() {
        let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
        else {
            return;
        };
        assert_eq!(ash_home().as_deref(), Some(home.join(".ash").as_path()));
    }
}
