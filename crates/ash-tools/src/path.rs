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
use cap_std::{ambient_authority, fs::Dir};
use ignore::WalkBuilder;

static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);
const MAX_TEMP_FILE_ATTEMPTS: usize = 100;
const IO_BUFFER_BYTES: usize = 8 * 1024;

pub struct WorkspacePath {
    dir: Arc<Dir>,
    relative: PathBuf,
    full_path: PathBuf,
}

pub struct SearchPath {
    dir: Arc<Dir>,
    workspace: PathBuf,
    full_path: PathBuf,
}

/// Capability for filesystem operations rooted at one canonical workspace.
/// The directory handle is opened once and shared by all built-in tools.
pub(crate) struct Workspace {
    root: PathBuf,
    dir: Arc<Dir>,
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
        let dir = Dir::open_ambient_dir(&root, ambient_authority()).map_err(|error| {
            ToolError::Execution(format!(
                "cannot access working directory {}: {error}",
                root.display()
            ))
        })?;
        Ok(Arc::new(Self {
            root,
            dir: Arc::new(dir),
        }))
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn path(&self, requested: &str) -> Result<WorkspacePath, ToolError> {
        let relative = relative_path(&self.root, requested)?;
        let full_path = self.root.join(&relative);
        Ok(WorkspacePath {
            dir: Arc::clone(&self.dir),
            relative,
            full_path,
        })
    }

    pub(crate) fn search_path(&self, requested: &str) -> Result<SearchPath, ToolError> {
        let path = self.path(requested)?;
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
        if !full_path.starts_with(&self.root) {
            return Err(ToolError::Execution(format!(
                "path is outside working directory: {}",
                full_path.display()
            )));
        }
        Ok(SearchPath {
            dir: Arc::clone(&self.dir),
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
        if !resolved.starts_with(&self.root) {
            return Err(ToolError::Execution(format!(
                "working directory is outside the session working directory: {}",
                resolved.display()
            )));
        }
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

    /// Opens a file inside the workspace through the capability-backed
    /// directory handle, so symlink escapes and absolute/`..` redirects are
    /// rejected by the sandbox instead of being followed.
    pub(crate) fn open_file(&self, path: &Path) -> std::io::Result<cap_std::fs::File> {
        let relative = path.strip_prefix(&self.workspace).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path escapes the workspace",
            )
        })?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        self.dir.open_with(relative, &options)
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
        options: &cap_std::fs::OpenOptions,
    ) -> std::io::Result<cap_std::fs::File> {
        self.dir.open_with(&self.relative, options)
    }

    pub(crate) fn atomic_write(
        &self,
        content: &[u8],
        permissions: Option<cap_std::fs::Permissions>,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), ToolError> {
        ensure_running(cancellation, deadline)?;
        let parent = self.relative.parent().unwrap_or_else(|| Path::new(""));
        self.dir.create_dir_all(parent).map_err(write_error)?;
        ensure_running(cancellation, deadline)?;

        let permissions = self.prepare_permissions(permissions)?;
        let file_name = self
            .relative
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);

        for _ in 0..MAX_TEMP_FILE_ATTEMPTS {
            let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let temp = parent.join(format!(
                ".{file_name}.ash-write-{}-{id}",
                std::process::id()
            ));
            let file = match self.dir.open_with(&temp, &options) {
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
                let _ = self.dir.remove_file(&temp);
            }
            return result;
        }

        Err(ToolError::Execution(format!(
            "cannot write file: could not allocate a temporary file after {MAX_TEMP_FILE_ATTEMPTS} attempts"
        )))
    }

    /// Decides the permissions for the replacement file: an explicit request,
    /// the existing file's permissions, or none for a new file. Existing
    /// symlinks are rejected because the atomic rename would otherwise replace
    /// the link itself.
    fn prepare_permissions(
        &self,
        permissions: Option<cap_std::fs::Permissions>,
    ) -> Result<Option<cap_std::fs::Permissions>, ToolError> {
        if let Some(permissions) = permissions {
            return Ok(Some(permissions));
        }
        match self.dir.symlink_metadata(&self.relative) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(ToolError::Execution(
                "cannot write file: symbolic links are not writable".into(),
            )),
            Ok(metadata) => Ok(Some(metadata.permissions())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(write_error(error)),
        }
    }

    /// Writes the content into the freshly-created temporary file, syncs it,
    /// and renames it over the target path. On error the caller removes the
    /// temporary file.
    fn commit_temp_file(
        &self,
        mut file: cap_std::fs::File,
        temp: &Path,
        content: &[u8],
        permissions: Option<&cap_std::fs::Permissions>,
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
        self.dir
            .rename(temp, &self.dir, &self.relative)
            .map_err(write_error)
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
            // `relative` is always the result of `strip_prefix` (for absolute
            // requests) or a plain relative request, so it never begins with a
            // root or prefix component; this arm is defensive and unreachable.
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
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        assert!(path.open_with(&options).is_err());
        assert!(SearchPath::new(root.path(), "outside.txt").is_err());
    }
}
