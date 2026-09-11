use crate::state::{ChatEntry, Tree};
use ash_core::{SessionId, ToolError};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub(crate) fn directory(base: &Path, root: SessionId) -> PathBuf {
    base.join("collab").join(root.to_string())
}
pub(crate) fn group_directory(base: &Path, root: SessionId, group: &str) -> PathBuf {
    directory(base, root).join("groups").join(group)
}

pub(crate) async fn save(base: &Path, root: SessionId, tree: &Tree) -> Result<(), ToolError> {
    let directory = directory(base, root);
    tokio::fs::create_dir_all(&directory).await.map_err(error)?;
    let data = serde_json::to_vec(tree).map_err(error)?;
    let temporary = directory.join("state.tmp");
    let mut file = tokio::fs::File::create(&temporary).await.map_err(error)?;
    file.write_all(&data).await.map_err(error)?;
    file.sync_all().await.map_err(error)?;
    tokio::fs::rename(&temporary, directory.join("state.json"))
        .await
        .map_err(error)?;
    tokio::fs::File::open(&directory)
        .await
        .map_err(error)?
        .sync_all()
        .await
        .map_err(error)
}

pub(crate) async fn load(base: &Path, root: SessionId) -> Result<Option<Tree>, ToolError> {
    match tokio::fs::read(directory(base, root).join("state.json")).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(error),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(failure) => Err(error(failure)),
    }
}

pub(crate) async fn create_group(
    base: &Path,
    root: SessionId,
    group: &str,
) -> Result<(), ToolError> {
    let directory = group_directory(base, root, group);
    let parent = directory
        .parent()
        .ok_or_else(|| error("group directory has no parent"))?;
    tokio::fs::create_dir_all(parent).await.map_err(error)?;
    tokio::fs::create_dir(&directory).await.map_err(error)?;
    let creation = async {
        for name in ["prompt.md", "chat.jsonl"] {
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(name))
                .await
            {
                Ok(file) => file.sync_all().await.map_err(error)?,
                Err(failure) => return Err(error(failure)),
            }
        }
        tokio::fs::File::open(&directory)
            .await
            .map_err(error)?
            .sync_all()
            .await
            .map_err(error)?;
        if let Some(parent) = directory.parent() {
            tokio::fs::File::open(parent)
                .await
                .map_err(error)?
                .sync_all()
                .await
                .map_err(error)?;
        }
        Ok(())
    }
    .await;
    if creation.is_err() {
        let _ = tokio::fs::remove_dir_all(&directory).await;
    }
    creation
}

pub(crate) async fn write_prompt(path: &Path, prompt: &str) -> Result<(), ToolError> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .await
        .map_err(error)?;
    file.write_all(prompt.as_bytes()).await.map_err(error)?;
    file.sync_all().await.map_err(error)
}

pub(crate) async fn repair_chat(
    base: &Path,
    root: SessionId,
    group: &str,
) -> Result<(), ToolError> {
    let path = group_directory(base, root, group).join("chat.jsonl");
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await
        .map_err(error)?;
    let length = file.metadata().await.map_err(error)?.len();
    let mut offset = length;
    let mut boundary = 0;
    while offset > 0 {
        let start = offset.saturating_sub(8192);
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(error)?;
        let mut bytes = vec![0; usize::try_from(offset - start).map_err(error)?];
        file.read_exact(&mut bytes).await.map_err(error)?;
        if let Some(index) = bytes.iter().rposition(|byte| *byte == b'\n') {
            boundary = start + u64::try_from(index).map_err(error)? + 1;
            break;
        }
        offset = start;
    }
    if boundary < length {
        file.set_len(boundary).await.map_err(error)?;
        file.sync_data().await.map_err(error)?;
    }
    Ok(())
}

pub(crate) async fn append(
    base: &Path,
    root: SessionId,
    group: &str,
    entry: &ChatEntry,
) -> Result<(), ToolError> {
    let mut data = serde_json::to_vec(entry).map_err(error)?;
    data.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(group_directory(base, root, group).join("chat.jsonl"))
        .await
        .map_err(error)?;
    file.write_all(&data).await.map_err(error)?;
    file.sync_data().await.map_err(error)
}

pub(crate) async fn history(
    base: &Path,
    root: SessionId,
    group: &str,
) -> Result<Vec<ChatEntry>, ToolError> {
    let text = tokio::fs::read_to_string(group_directory(base, root, group).join("chat.jsonl"))
        .await
        .map_err(error)?;
    text.split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .map(|line| serde_json::from_str(line).map_err(error))
        .collect()
}

pub(crate) fn error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(error.to_string())
}
