use std::{path::PathBuf, str::FromStr, sync::Arc};

use ash_core::{SessionId, SessionIdentity};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt, sync::Mutex};

use crate::{AgentStatus, WorkflowStatus};

const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("workflow storage I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("workflow storage JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workflow storage is corrupt: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Event {
    Started {
        label: String,
        root: SessionIdentity,
        script: Option<String>,
    },
    AgentSpawned {
        id: SessionId,
        parent_id: SessionId,
        label: String,
        prompt: String,
    },
    AgentFinished {
        id: SessionId,
        status: AgentStatus,
        error: Option<String>,
    },
    Finished {
        status: WorkflowStatus,
        result: Option<serde_json::Value>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record {
    Header {
        version: u32,
        id: SessionId,
        root_id: SessionId,
    },
    Event {
        seq: u64,
        event: Event,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Replay {
    pub(crate) id: SessionId,
    pub(crate) root_id: SessionId,
    pub(crate) events: Vec<(u64, Event)>,
}

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
    id: SessionId,
    root_id: SessionId,
    file: Arc<Mutex<Option<fs::File>>>,
    next_seq: Arc<Mutex<u64>>,
}

impl Store {
    pub(crate) fn new(directory: PathBuf, root_id: SessionId, id: SessionId) -> Self {
        Self {
            path: directory
                .join("workflows")
                .join(root_id.to_string())
                .join(format!("{id}.jsonl")),
            id,
            root_id,
            file: Arc::new(Mutex::new(None)),
            next_seq: Arc::new(Mutex::new(0)),
        }
    }

    pub(crate) async fn append(&self, event: Event) -> Result<(), StoreError> {
        let mut file = self.file.lock().await;
        if file.is_none() {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent).await?;
            }
            let mut opened = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await?;
            let metadata = opened.metadata().await?;
            if metadata.len() == 0 {
                write_record(
                    &mut opened,
                    &Record::Header {
                        version: FORMAT_VERSION,
                        id: self.id,
                        root_id: self.root_id,
                    },
                )
                .await?;
            } else {
                let (replay, valid_len) = read_replay(&self.path).await?;
                if replay.id != self.id || replay.root_id != self.root_id {
                    return Err(StoreError::Corrupt(
                        "workflow header identity does not match the requested log".into(),
                    ));
                }
                opened.set_len(valid_len).await?;
                let mut next_seq = self.next_seq.lock().await;
                *next_seq = u64::try_from(replay.events.len()).unwrap_or(u64::MAX);
            }
            *file = Some(opened);
        }

        let mut seq = self.next_seq.lock().await;
        let record = Record::Event { seq: *seq, event };
        let opened = file
            .as_mut()
            .ok_or_else(|| StoreError::Corrupt("workflow file was not initialized".into()))?;
        write_record(opened, &record).await?;
        *seq = seq.saturating_add(1);
        Ok(())
    }

    pub(crate) async fn replay(&self) -> Result<Replay, StoreError> {
        read_replay(&self.path).await.map(|(replay, _)| replay)
    }

    pub(crate) async fn ids(
        directory: PathBuf,
        root_id: SessionId,
    ) -> Result<Vec<SessionId>, StoreError> {
        let path = directory.join("workflows").join(root_id.to_string());
        let mut entries = match fs::read_dir(path).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut ids = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| SessionId::from_str(stem).ok())
            {
                let modified = entry
                    .metadata()
                    .await
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                ids.push((modified, id));
            }
        }
        ids.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        Ok(ids.into_iter().map(|(_, id)| id).collect())
    }
}

async fn write_record(file: &mut fs::File, record: &Record) -> Result<(), StoreError> {
    let mut data = serde_json::to_vec(record)?;
    data.push(b'\n');
    file.write_all(&data).await?;
    file.flush().await?;
    file.sync_data().await?;
    Ok(())
}

async fn read_replay(path: &PathBuf) -> Result<(Replay, u64), StoreError> {
    let data = match fs::read(path).await {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(StoreError::Corrupt("workflow log does not exist".into()))
        }
        Err(error) => return Err(error.into()),
    };
    let mut lines = data.split_inclusive(|byte| *byte == b'\n');
    let header = lines
        .next()
        .ok_or_else(|| StoreError::Corrupt("missing workflow header".into()))?;
    if !header.ends_with(b"\n") {
        return Err(StoreError::Corrupt("incomplete workflow header".into()));
    }
    let Record::Header {
        version,
        id,
        root_id,
    } = serde_json::from_slice(header)
        .map_err(|error| StoreError::Corrupt(format!("invalid header: {error}")))?
    else {
        return Err(StoreError::Corrupt("first record is not a header".into()));
    };
    if version != FORMAT_VERSION {
        return Err(StoreError::Corrupt(format!(
            "unsupported workflow format version: {version}"
        )));
    }

    let mut events = Vec::new();
    let mut expected = 0;
    let mut valid_len = u64::try_from(header.len()).unwrap_or(u64::MAX);
    for line in lines {
        if !line.ends_with(b"\n") {
            break;
        }
        let Record::Event { seq, event } = serde_json::from_slice(line)
            .map_err(|error| StoreError::Corrupt(format!("invalid event: {error}")))?
        else {
            return Err(StoreError::Corrupt(
                "header appears after first record".into(),
            ));
        };
        if seq != expected {
            return Err(StoreError::Corrupt(format!(
                "event sequence gap: expected {expected}, got {seq}"
            )));
        }
        expected = expected.saturating_add(1);
        valid_len = valid_len.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
        events.push((seq, event));
    }
    Ok((
        Replay {
            id,
            root_id,
            events,
        },
        valid_len,
    ))
}
