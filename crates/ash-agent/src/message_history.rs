use std::path::PathBuf;

use ash_core::SessionId;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tracing::warn;

const MAX_LOADED_ENTRIES: usize = 1000;

#[derive(Debug, Serialize, Deserialize)]
struct MessageHistoryEntry {
    timestamp: String,
    session_id: SessionId,
    text: String,
    #[serde(default)]
    undone: bool,
}

#[derive(Clone, Debug)]
pub struct MessageHistoryStore {
    path: PathBuf,
}

impl Default for MessageHistoryStore {
    fn default() -> Self {
        Self {
            path: directories::ProjectDirs::from("", "", "ash")
                .map(|dirs| dirs.data_dir().join("history.jsonl"))
                .unwrap_or_else(|| PathBuf::from(".ash/history.jsonl")),
        }
    }
}

impl MessageHistoryStore {
    pub async fn append(
        &self,
        session_id: SessionId,
        text: &str,
    ) -> Result<(), ash_core::AshError> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let entry = MessageHistoryEntry {
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            session_id,
            text: text.to_string(),
            undone: false,
        };
        self.append_entry(&entry).await
    }

    pub async fn undo(&self, session_id: SessionId, text: &str) -> Result<(), ash_core::AshError> {
        let entry = MessageHistoryEntry {
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            session_id,
            text: text.to_string(),
            undone: true,
        };
        self.append_entry(&entry).await
    }

    async fn append_entry(&self, entry: &MessageHistoryEntry) -> Result<(), ash_core::AshError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut line = serde_json::to_string(&entry)
            .map_err(|error| ash_core::AshError::Config(error.to_string()))?;
        line.push('\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    pub async fn load(&self) -> Result<Vec<String>, ash_core::AshError> {
        let file = match tokio::fs::File::open(&self.path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut lines = tokio::io::BufReader::new(file).lines();
        let mut entries: Vec<(SessionId, String)> = Vec::new();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<MessageHistoryEntry>(&line) {
                Ok(entry) if entry.undone => {
                    if let Some(index) = entries.iter().rposition(|(session_id, text)| {
                        *session_id == entry.session_id && *text == entry.text
                    }) {
                        entries.remove(index);
                    }
                }
                Ok(entry) => entries.push((entry.session_id, entry.text)),
                Err(error) => warn!(%error, "skipping malformed input history line"),
            }
        }
        if entries.len() > MAX_LOADED_ENTRIES {
            entries.drain(..entries.len() - MAX_LOADED_ENTRIES);
        }
        Ok(entries.into_iter().map(|(_, text)| text).collect())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn persists_and_loads_prompt_history_as_jsonl() {
        let directory = TempDir::new().unwrap();
        let store = MessageHistoryStore {
            path: directory.path().join("history.jsonl"),
        };
        store
            .append(SessionId::new(), "first prompt")
            .await
            .unwrap();
        store
            .append(SessionId::new(), "second prompt")
            .await
            .unwrap();
        assert_eq!(
            store.load().await.unwrap(),
            vec!["first prompt", "second prompt"]
        );
    }

    #[tokio::test]
    async fn removes_an_undone_prompt_from_replayed_history() {
        let directory = TempDir::new().unwrap();
        let store = MessageHistoryStore {
            path: directory.path().join("history.jsonl"),
        };
        let session_id = SessionId::new();
        store.append(session_id, "first prompt").await.unwrap();
        store.append(session_id, "second prompt").await.unwrap();
        store.undo(session_id, "second prompt").await.unwrap();

        assert_eq!(store.load().await.unwrap(), vec!["first prompt"]);
    }
}
