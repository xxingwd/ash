use std::path::PathBuf;

use ash_core::SessionId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tracing::warn;

const MAX_LOADED_ENTRIES: usize = 1000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MessageHistoryRecord {
    Submitted { session_id: SessionId, text: String },
    Undone { session_id: SessionId, text: String },
}

#[derive(Deserialize)]
struct LegacyMessageHistoryRecord {
    session_id: SessionId,
    text: String,
    #[serde(default)]
    undone: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredMessageHistoryRecord {
    Current(MessageHistoryRecord),
    Legacy(LegacyMessageHistoryRecord),
}

impl StoredMessageHistoryRecord {
    fn into_current(self) -> MessageHistoryRecord {
        match self {
            Self::Current(record) => record,
            Self::Legacy(record) if record.undone => MessageHistoryRecord::Undone {
                session_id: record.session_id,
                text: record.text,
            },
            Self::Legacy(record) => MessageHistoryRecord::Submitted {
                session_id: record.session_id,
                text: record.text,
            },
        }
    }
}

#[derive(Default)]
struct MessageHistoryReplay {
    entries: Vec<(SessionId, String)>,
}

impl MessageHistoryReplay {
    fn apply(&mut self, record: MessageHistoryRecord) {
        match record {
            MessageHistoryRecord::Submitted { session_id, text } => {
                self.entries.push((session_id, text));
            }
            MessageHistoryRecord::Undone { session_id, text } => {
                if let Some(index) =
                    self.entries
                        .iter()
                        .rposition(|(stored_session, stored_text)| {
                            *stored_session == session_id && stored_text == &text
                        })
                {
                    self.entries.remove(index);
                }
            }
        }
    }

    fn into_recent(mut self) -> Vec<String> {
        let excess = self.entries.len().saturating_sub(MAX_LOADED_ENTRIES);
        self.entries.drain(..excess);
        self.entries.into_iter().map(|(_, text)| text).collect()
    }
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
        let entry = MessageHistoryRecord::Submitted {
            session_id,
            text: text.to_string(),
        };
        self.append_entry(&entry).await
    }

    pub async fn undo(&self, session_id: SessionId, text: &str) -> Result<(), ash_core::AshError> {
        let entry = MessageHistoryRecord::Undone {
            session_id,
            text: text.to_string(),
        };
        self.append_entry(&entry).await
    }

    async fn append_entry(&self, entry: &MessageHistoryRecord) -> Result<(), ash_core::AshError> {
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
        let mut replay = MessageHistoryReplay::default();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<StoredMessageHistoryRecord>(&line)
                .map(StoredMessageHistoryRecord::into_current)
            {
                Ok(record) => replay.apply(record),
                Err(error) => warn!(%error, "skipping malformed input history line"),
            }
        }
        Ok(replay.into_recent())
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
        let contents = tokio::fs::read_to_string(&store.path).await.unwrap();
        assert!(contents.contains(r#""type":"submitted""#));
        assert!(!contents.contains("timestamp"));
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

    #[tokio::test]
    async fn loads_legacy_boolean_history_records() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("history.jsonl");
        let session_id = SessionId::new();
        tokio::fs::write(
            &path,
            format!(
                "{{\"timestamp\":\"old\",\"session_id\":\"{session_id}\",\"text\":\"first\",\"undone\":false}}\n\
                 {{\"timestamp\":\"old\",\"session_id\":\"{session_id}\",\"text\":\"second\",\"undone\":false}}\n\
                 {{\"timestamp\":\"old\",\"session_id\":\"{session_id}\",\"text\":\"second\",\"undone\":true}}\n"
            ),
        )
        .await
        .unwrap();
        let store = MessageHistoryStore { path };

        assert_eq!(store.load().await.unwrap(), vec!["first"]);
    }
}
