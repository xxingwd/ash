use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use ash_core::ash_data_dir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    sync::Mutex,
};
use tracing::warn;

const MAX_ENTRIES: usize = 1_000;
const COMPACT_AT: usize = 1_200;

#[derive(Clone, Debug)]
pub struct InputHistory {
    path: PathBuf,
    lines: Arc<Mutex<Option<usize>>>,
}

impl Default for InputHistory {
    fn default() -> Self {
        Self {
            path: ash_data_dir().join("history.jsonl"),
            lines: Arc::new(Mutex::new(None)),
        }
    }
}

impl InputHistory {
    /// Record one submitted prompt.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the history file cannot be written.
    pub async fn append(&self, text: &str) -> Result<(), ash_core::AshError> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let mut lines = self.lines.lock().await;
        let count = match *lines {
            Some(count) => count,
            None => read(&self.path).await?.1,
        };
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        write_line(&mut file, text).await?;
        file.flush().await?;
        *lines = Some(count.saturating_add(1));
        if lines.is_some_and(|count| count >= COMPACT_AT) {
            let entries = read_recent(&self.path).await?;
            rewrite(&self.path, &entries).await?;
            *lines = Some(entries.len());
        }
        Ok(())
    }

    /// Load prompts from newest retained history.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the history file exists but cannot be read.
    pub async fn load(&self) -> Result<Vec<String>, ash_core::AshError> {
        let (entries, lines) = read(&self.path).await?;
        *self.lines.lock().await = Some(lines);
        Ok(recent(entries))
    }
}

async fn read(path: &Path) -> Result<(Vec<String>, usize), ash_core::AshError> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), 0));
        }
        Err(error) => return Err(error.into()),
    };
    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut entries = Vec::new();
    let mut count = 0;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        match serde_json::from_str::<String>(&line) {
            Ok(text) => entries.push(text),
            Err(error) => warn!(%error, "skipping malformed input history line"),
        }
    }
    Ok((entries, count))
}

async fn read_recent(path: &Path) -> Result<Vec<String>, ash_core::AshError> {
    read(path).await.map(|(entries, _)| recent(entries))
}

fn recent(mut entries: Vec<String>) -> Vec<String> {
    let excess = entries.len().saturating_sub(MAX_ENTRIES);
    entries.drain(..excess);
    entries
}

async fn rewrite(path: &Path, entries: &[String]) -> Result<(), ash_core::AshError> {
    let temporary = path.with_extension("tmp");
    let mut file = tokio::fs::File::create(&temporary).await?;
    for entry in entries {
        write_line(&mut file, entry).await?;
    }
    file.flush().await?;
    file.sync_data().await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

async fn write_line(file: &mut tokio::fs::File, text: &str) -> Result<(), ash_core::AshError> {
    let mut line =
        serde_json::to_vec(text).map_err(|error| ash_core::AshError::Config(error.to_string()))?;
    line.push(b'\n');
    file.write_all(&line).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn store(directory: &TempDir) -> InputHistory {
        InputHistory {
            path: directory.path().join("history.jsonl"),
            lines: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn default_history_path_is_under_the_ash_data_dir() {
        assert_eq!(
            InputHistory::default().path,
            ash_data_dir().join("history.jsonl")
        );
    }

    #[tokio::test]
    async fn persists_plain_json_strings() {
        let directory = TempDir::new().unwrap();
        let store = store(&directory);
        store.append("first prompt").await.unwrap();
        store.append("second prompt").await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&store.path).await.unwrap(),
            "\"first prompt\"\n\"second prompt\"\n"
        );
        assert_eq!(
            store.load().await.unwrap(),
            vec!["first prompt", "second prompt"]
        );
    }

    #[tokio::test]
    async fn compacts_history_at_the_high_water_mark() {
        let directory = TempDir::new().unwrap();
        let store = store(&directory);
        for index in 0..COMPACT_AT {
            store.append(&format!("prompt {index}")).await.unwrap();
        }

        let entries = store.load().await.unwrap();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.first().map(String::as_str), Some("prompt 200"));
        assert_eq!(*store.lines.lock().await, Some(MAX_ENTRIES));
    }

    #[tokio::test]
    async fn append_counts_existing_history_without_loading_first() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("history.jsonl");
        tokio::fs::write(&path, "\"existing\"\n").await.unwrap();
        let store = store(&directory);

        store.append("new").await.unwrap();

        assert_eq!(*store.lines.lock().await, Some(2));
        assert_eq!(store.load().await.unwrap(), vec!["existing", "new"]);
    }
}
