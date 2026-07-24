use std::path::{Path, PathBuf};

pub(crate) fn directories(working_dir: &Path) -> Vec<PathBuf> {
    let root = working_dir
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(working_dir);

    let mut directories = working_dir
        .ancestors()
        .take_while(|directory| *directory != root)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    directories.push(root.to_path_buf());
    directories.reverse();
    directories
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_directories_from_project_root_to_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let working_dir = project.join("src/module");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(&working_dir).unwrap();

        assert_eq!(
            directories(&working_dir),
            vec![project.clone(), project.join("src"), working_dir]
        );
    }

    #[test]
    fn uses_working_directory_when_project_root_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();

        assert_eq!(directories(&working_dir), vec![working_dir]);
    }
}
