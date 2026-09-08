use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use ash_core::{CancellationToken, ToolError};
use ignore::WalkBuilder;

static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);
const MAX_TEMP_FILE_ATTEMPTS: usize = 100;
const IO_BUFFER_BYTES: usize = 8 * 1024;

pub struct WorkspacePath {
    full_path: PathBuf,
}

pub struct SearchPath {
    workspace: PathBuf,
    full_path: PathBuf,
}

/// Filesystem operations rooted at one workspace.
pub(crate) struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub(crate) fn new(root: impl AsRef<Path>) -> Result<Arc<Self>, ToolError> {
        let requested = root.as_ref();
        let root = std::fs::canonicalize(requested).map_err(|error| {
            ToolError::Execution(format!(
                "cannot resolve working directory {}: {error}",
                requested.display()
            ))
        })?;
        Ok(Arc::new(Self { root }))
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn path(&self, requested: &str) -> Result<WorkspacePath, ToolError> {
        let full_path = resolve_path(&self.root, requested);
        Ok(WorkspacePath { full_path })
    }

    pub(crate) fn search_path(&self, requested: &str) -> Result<SearchPath, ToolError> {
        let path = self.path(requested)?;
        let full_path = std::fs::canonicalize(path.full_path()).map_err(|error| {
            ToolError::Execution(format!(
                "cannot resolve {}: {error}",
                path.full_path().display()
            ))
        })?;
        Ok(SearchPath {
            workspace: self.root.clone(),
            full_path,
        })
    }

    pub(crate) fn resolve_dir(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let candidate = self.path(requested)?.full_path().to_path_buf();
        let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
            ToolError::Execution(format!(
                "cannot access working directory {}: {error}",
                candidate.display()
            ))
        })?;
        if !resolved.is_dir() {
            return Err(ToolError::Execution(format!(
                "working directory is not a directory: {}",
                resolved.display()
            )));
        }
        Ok(resolved)
    }
}

pub fn file_walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .require_git(false)
        .sort_by_file_path(std::cmp::Ord::cmp);
    builder
}

pub fn ensure_running(
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(ToolError::DeadlineExceeded);
    }
    Ok(())
}

pub fn read_all(
    reader: &mut impl Read,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>, ToolError> {
    read_limited(reader, usize::MAX, cancellation, deadline)
}

pub fn read_limited(
    reader: &mut impl Read,
    max_bytes: usize,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>, ToolError> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    loop {
        ensure_running(cancellation, deadline)?;
        let count = reader
            .read(&mut buffer)
            .map_err(|error| ToolError::Execution(format!("cannot read file: {error}")))?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > max_bytes {
            return Err(ToolError::Execution(format!(
                "file exceeds the {} read limit",
                crate::truncate::format_size(max_bytes)
            )));
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

fn write_all(
    writer: &mut impl Write,
    content: &[u8],
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), ToolError> {
    for chunk in content.chunks(IO_BUFFER_BYTES) {
        ensure_running(cancellation, deadline)?;
        writer
            .write_all(chunk)
            .map_err(|error| ToolError::Execution(format!("cannot write file: {error}")))?;
    }
    Ok(())
}

impl SearchPath {
    #[cfg(test)]
    pub(crate) fn new(root: &Path, requested: &str) -> Result<Self, ToolError> {
        Workspace::new(root)?.search_path(requested)
    }

    pub(crate) fn full_path(&self) -> &Path {
        &self.full_path
    }

    pub(crate) fn relative(&self, path: &Path) -> PathBuf {
        // `path` always comes from walking `self.full_path`, so stripping the
        // canonical workspace prefix cannot fail; keep the full path as a
        // defensive fallback.
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .to_path_buf()
    }

    pub(crate) fn open_file(&self, path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::File::open(path)
    }
}

impl WorkspacePath {
    #[cfg(test)]
    pub(crate) fn new(root: &Path, requested: &str) -> Result<Self, ToolError> {
        Workspace::new(root)?.path(requested)
    }

    pub(crate) fn full_path(&self) -> &Path {
        &self.full_path
    }

    pub(crate) fn open_with(
        &self,
        options: &std::fs::OpenOptions,
    ) -> std::io::Result<std::fs::File> {
        options.open(&self.full_path)
    }

    pub(crate) fn atomic_write(
        &self,
        content: &[u8],
        permissions: Option<std::fs::Permissions>,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), ToolError> {
        ensure_running(cancellation, deadline)?;
        let parent = self.full_path.parent().unwrap_or_else(|| Path::new(""));
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(write_error)?;
        }
        ensure_running(cancellation, deadline)?;

        let permissions = self.prepare_permissions(permissions)?;
        let file_name = self
            .full_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);

        for _ in 0..MAX_TEMP_FILE_ATTEMPTS {
            let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let temp = parent.join(format!(
                ".{file_name}.ash-write-{}-{id}",
                std::process::id()
            ));
            let file = match options.open(&temp) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(write_error(error)),
            };
            let result = self.commit_temp_file(
                file,
                &temp,
                content,
                permissions.as_ref(),
                cancellation,
                deadline,
            );
            if result.is_err() {
                let _ = std::fs::remove_file(&temp);
            }
            return result;
        }

        Err(ToolError::Execution(format!(
            "cannot write file: could not allocate a temporary file after {MAX_TEMP_FILE_ATTEMPTS} attempts"
        )))
    }

    fn prepare_permissions(
        &self,
        permissions: Option<std::fs::Permissions>,
    ) -> Result<Option<std::fs::Permissions>, ToolError> {
        if let Some(permissions) = permissions {
            return Ok(Some(permissions));
        }
        match std::fs::metadata(&self.full_path) {
            Ok(metadata) => Ok(Some(metadata.permissions())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(write_error(error)),
        }
    }

    fn commit_temp_file(
        &self,
        mut file: std::fs::File,
        temp: &Path,
        content: &[u8],
        permissions: Option<&std::fs::Permissions>,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), ToolError> {
        if let Some(permissions) = permissions {
            file.set_permissions(permissions.clone())
                .map_err(write_error)?;
        }
        write_all(&mut file, content, cancellation, deadline)?;
        ensure_running(cancellation, deadline)?;
        file.sync_all().map_err(write_error)?;
        drop(file);
        ensure_running(cancellation, deadline)?;
        std::fs::rename(temp, &self.full_path).map_err(write_error)
    }
}

fn write_error(error: std::io::Error) -> ToolError {
    ToolError::Execution(format!("cannot write file: {error}"))
}

pub async fn run_blocking<T>(
    operation: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| ToolError::Execution(format!("filesystem task failed: {error}")))?
}

pub async fn run_tool_blocking<T>(
    cancellation: CancellationToken,
    deadline: Instant,
    operation: impl FnOnce(CancellationToken, Instant) -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError>
where
    T: Send + 'static,
{
    ensure_running(&cancellation, deadline)?;
    tokio::task::spawn_blocking(move || operation(cancellation, deadline))
        .await
        .map_err(|error| ToolError::Execution(format!("filesystem task failed: {error}")))?
}

fn resolve_path(root: &Path, requested: &str) -> PathBuf {
    let requested = Path::new(requested);
    let combined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    normalize_path(&combined)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            Component::Normal(part) => clean.push(part),
            Component::RootDir => clean.push(Component::RootDir),
            Component::Prefix(prefix) => clean.push(Component::Prefix(prefix)),
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::time::{Duration, Instant};

    struct CancellingReader {
        cancellation: CancellationToken,
        read: bool,
    }

    impl Read for CancellingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(!self.read, "reader was called after cancellation");
            self.read = true;
            buffer[0] = b'x';
            self.cancellation.cancel();
            Ok(1)
        }
    }

    struct CancellingWriter {
        cancellation: CancellationToken,
        writes: usize,
    }

    impl Write for CancellingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            assert_eq!(self.writes, 0, "writer was called after cancellation");
            self.writes += 1;
            self.cancellation.cancel();
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn ensure_running_rejects_cancellation() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(matches!(
            ensure_running(&cancellation, Instant::now() + Duration::from_mins(1)),
            Err(ToolError::Cancelled)
        ));
    }

    #[test]
    fn ensure_running_rejects_expired_deadline() {
        assert!(matches!(
            ensure_running(&CancellationToken::new(), Instant::now()),
            Err(ToolError::DeadlineExceeded)
        ));
    }

    #[test]
    fn read_limited_rejects_input_over_the_byte_budget() {
        let cancellation = CancellationToken::new();
        let mut reader = std::io::Cursor::new(vec![b'x'; 16]);

        let error = read_limited(
            &mut reader,
            8,
            &cancellation,
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ToolError::Execution(message) if message == "file exceeds the 8B read limit"
        ));
    }

    #[test]
    fn read_all_stops_between_chunks_when_cancelled() {
        let cancellation = CancellationToken::new();
        let mut reader = CancellingReader {
            cancellation: cancellation.clone(),
            read: false,
        };

        let error = read_all(
            &mut reader,
            &cancellation,
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap_err();

        assert!(matches!(error, ToolError::Cancelled));
    }

    #[test]
    fn write_all_stops_between_chunks_when_cancelled() {
        let cancellation = CancellationToken::new();
        let mut writer = CancellingWriter {
            cancellation: cancellation.clone(),
            writes: 0,
        };

        let error = write_all(
            &mut writer,
            &vec![b'x'; IO_BUFFER_BYTES + 1],
            &cancellation,
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap_err();

        assert!(matches!(error, ToolError::Cancelled));
    }

    #[test]
    fn allows_parent_escape() {
        let root = tempfile::tempdir().unwrap();
        let path = WorkspacePath::new(root.path(), "../secret").unwrap();
        let expected = root.path().parent().unwrap().join("secret");
        assert_eq!(path.full_path(), expected);
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
    fn can_follow_a_symlink_outside_the_workdir() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("outside.txt");
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let path = WorkspacePath::new(root.path(), "outside.txt").unwrap();
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        assert!(path.open_with(&options).is_ok());
        let search = SearchPath::new(root.path(), "outside.txt").unwrap();
        assert_eq!(search.full_path(), std::fs::canonicalize(&target).unwrap());
    }
}
