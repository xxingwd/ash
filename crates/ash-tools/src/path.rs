use std::{
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ash_core::ToolError;
use cap_std::{ambient_authority, fs::Dir};

static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);
const MAX_TEMP_FILE_ATTEMPTS: usize = 100;

pub(crate) struct WorkspacePath {
    dir: Dir,
    relative: PathBuf,
    full_path: PathBuf,
}

pub(crate) struct SearchPath {
    workspace: PathBuf,
    full_path: PathBuf,
}

impl SearchPath {
    pub(crate) fn new(root: &Path, requested: &str) -> Result<Self, ToolError> {
        let workspace = std::fs::canonicalize(root).map_err(|error| {
            ToolError::Execution(format!(
                "cannot resolve working directory {}: {error}",
                root.display()
            ))
        })?;
        let path = WorkspacePath::new(&workspace, requested)?;
        let metadata = std::fs::symlink_metadata(path.full_path()).map_err(|error| {
            ToolError::Execution(format!(
                "cannot access {}: {error}",
                path.full_path().display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ToolError::Execution(format!(
                "search path cannot be a symbolic link: {}",
                path.full_path().display()
            )));
        }
        let full_path = std::fs::canonicalize(path.full_path()).map_err(|error| {
            ToolError::Execution(format!(
                "cannot resolve {}: {error}",
                path.full_path().display()
            ))
        })?;
        if !full_path.starts_with(&workspace) {
            return Err(ToolError::Execution(format!(
                "path is outside working directory: {}",
                full_path.display()
            )));
        }
        Ok(Self {
            workspace,
            full_path,
        })
    }

    pub(crate) fn full_path(&self) -> &Path {
        &self.full_path
    }

    pub(crate) fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .to_path_buf()
    }
}

impl WorkspacePath {
    pub(crate) fn new(root: &Path, requested: &str) -> Result<Self, ToolError> {
        let absolute_root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| {
                    ToolError::Execution(format!("cannot resolve working directory: {error}"))
                })?
                .join(root)
        };
        let relative = relative_path(&absolute_root, requested)?;
        let dir = Dir::open_ambient_dir(root, ambient_authority()).map_err(|error| {
            ToolError::Execution(format!(
                "cannot access working directory {}: {error}",
                root.display()
            ))
        })?;
        let full_path = absolute_root.join(&relative);
        Ok(Self {
            dir,
            relative,
            full_path,
        })
    }

    pub(crate) fn full_path(&self) -> &Path {
        &self.full_path
    }

    pub(crate) fn read(&self) -> std::io::Result<Vec<u8>> {
        self.dir.read(&self.relative)
    }

    pub(crate) fn write(&self, content: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = self.relative.parent() {
            self.dir.create_dir_all(parent)?;
        }
        self.dir.write(&self.relative, content)
    }

    pub(crate) fn open_with(
        &self,
        options: &cap_std::fs::OpenOptions,
    ) -> std::io::Result<cap_std::fs::File> {
        self.dir.open_with(&self.relative, options)
    }

    pub(crate) fn atomic_replace(
        &self,
        content: &[u8],
        permissions: cap_std::fs::Permissions,
    ) -> std::io::Result<()> {
        let parent = self.relative.parent().unwrap_or_else(|| Path::new(""));
        let file_name = self
            .relative
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);

        for _ in 0..MAX_TEMP_FILE_ATTEMPTS {
            let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let temp = parent.join(format!(".{file_name}.ash-edit-{}-{id}", std::process::id()));
            let mut file = match self.dir.open_with(&temp, &options) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let result = (|| {
                file.set_permissions(permissions.clone())?;
                file.write_all(content)?;
                file.sync_all()?;
                drop(file);
                self.dir.rename(&temp, &self.dir, &self.relative)
            })();
            if result.is_err() {
                let _ = self.dir.remove_file(&temp);
            }
            return result;
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "could not allocate a temporary edit file after {MAX_TEMP_FILE_ATTEMPTS} attempts"
            ),
        ))
    }
}

pub(crate) async fn run_blocking<T>(
    operation: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| ToolError::Execution(format!("filesystem task failed: {error}")))?
}

fn relative_path(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let requested = Path::new(requested);
    let relative = if requested.is_absolute() {
        requested.strip_prefix(root).map_err(|_| {
            ToolError::Execution(format!(
                "path is outside working directory: {}",
                requested.display()
            ))
        })?
    } else {
        requested
    };

    let mut clean = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !clean.pop() {
                    return Err(ToolError::Execution(format!(
                        "path escapes working directory: {}",
                        requested.display()
                    )));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolError::Execution(format!(
                    "invalid path: {}",
                    requested.display()
                )));
            }
        }
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_escape() {
        let root = tempfile::tempdir().unwrap();
        assert!(WorkspacePath::new(root.path(), "../secret").is_err());
    }

    #[test]
    fn accepts_nested_new_file() {
        let root = tempfile::tempdir().unwrap();
        let path = WorkspacePath::new(root.path(), "src/new/file.rs").unwrap();
        assert_eq!(path.full_path(), root.path().join("src/new/file.rs"));
    }

    #[test]
    fn search_path_resolves_paths_inside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();

        let path = SearchPath::new(root.path(), "src").unwrap();

        assert_eq!(path.full_path(), root.path().join("src"));
        assert_eq!(
            path.relative(&root.path().join("src/lib.rs")),
            Path::new("src/lib.rs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn cannot_follow_a_symlink_outside_the_workdir() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("outside.txt");
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(target, &link).unwrap();

        let path = WorkspacePath::new(root.path(), "outside.txt").unwrap();
        assert!(path.read().is_err());
        assert!(SearchPath::new(root.path(), "outside.txt").is_err());
    }
}
