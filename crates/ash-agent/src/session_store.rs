use std::path::{Path, PathBuf};

use ash_core::{Message, MessageContent, SessionId, StopReason, ToolDefinition};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::warn;

use crate::AgentConfig;

const SESSION_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub format_version: u32,
    pub session_id: SessionId,
    pub created_at: String,
    pub protocol: String,
    pub model: String,
    pub working_dir: PathBuf,
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolDefinition>,
    pub max_turns: u32,
    pub max_context_tokens: Option<usize>,
    pub max_output_tokens: Option<u32>,
    pub tool_timeout_ms: u64,
    pub uses_custom_base_url: bool,
}

impl SessionMetadata {
    fn from_config(config: &AgentConfig, session_id: SessionId, created_at: DateTime<Utc>) -> Self {
        Self {
            format_version: SESSION_FORMAT_VERSION,
            session_id,
            created_at: created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            protocol: config.provider.protocol.as_cli_name().to_string(),
            model: config.model.as_str().to_string(),
            working_dir: config.working_dir.clone(),
            system_prompt: config.system_prompt.clone(),
            tools: config.tools.iter().map(|tool| tool.definition()).collect(),
            max_turns: config.max_turns,
            max_context_tokens: config.max_context_tokens,
            max_output_tokens: config.max_output_tokens,
            tool_timeout_ms: u64::try_from(config.tool_timeout.as_millis()).unwrap_or(u64::MAX),
            uses_custom_base_url: config.provider.base_url.is_some(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TurnFinishedRecord {
    reason: StopReason,
    duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TurnFailedRecord {
    error_kind: String,
    duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TurnRolledBackRecord {
    num_turns: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum SessionRecord {
    SessionMeta(SessionMetadata),
    UserMessage(Message),
    AssistantMessage(Message),
    ToolResult(Message),
    TurnFinished(TurnFinishedRecord),
    TurnFailed(TurnFailedRecord),
    TurnRolledBack(TurnRolledBackRecord),
}

impl SessionRecord {
    fn from_message(message: &Message) -> Self {
        match &message.content {
            MessageContent::User(_) => Self::UserMessage(message.clone()),
            MessageContent::Assistant(_) => Self::AssistantMessage(message.clone()),
            MessageContent::ToolResult { .. } => Self::ToolResult(message.clone()),
        }
    }

    fn into_message(self) -> Option<Message> {
        match self {
            Self::UserMessage(message)
            | Self::AssistantMessage(message)
            | Self::ToolResult(message) => Some(message),
            Self::SessionMeta(_)
            | Self::TurnFinished(_)
            | Self::TurnFailed(_)
            | Self::TurnRolledBack(_) => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionLine {
    timestamp: String,
    #[serde(flatten)]
    record: SessionRecord,
}

impl SessionLine {
    fn new(record: SessionRecord) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            record,
        }
    }
}

#[derive(Debug)]
pub(crate) struct StoredSession {
    pub(crate) path: PathBuf,
    pub(crate) metadata: SessionMetadata,
    pub(crate) messages: Vec<Message>,
}

impl StoredSession {
    fn has_user_message(&self) -> bool {
        self.messages
            .iter()
            .any(|message| matches!(message.content, MessageContent::User(_)))
    }
}

pub struct SessionStore {
    path: PathBuf,
    metadata: SessionMetadata,
    file: Option<tokio::fs::File>,
}

impl SessionStore {
    pub fn new(config: &AgentConfig, session_id: SessionId) -> Self {
        Self::new_in(config, session_id, Self::default_dir())
    }

    pub(crate) fn new_in(config: &AgentConfig, session_id: SessionId, directory: PathBuf) -> Self {
        let local_now = Local::now().fixed_offset();
        let created_at = local_now.with_timezone(&Utc);
        let path = directory.join(session_filename(session_id, local_now));
        Self {
            path,
            metadata: SessionMetadata::from_config(config, session_id, created_at),
            file: None,
        }
    }

    pub fn default_dir() -> PathBuf {
        directories::ProjectDirs::from("", "", "ash")
            .map(|dirs| dirs.data_dir().join("sessions"))
            .unwrap_or_else(|| PathBuf::from(".ash/sessions"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn append_message(&mut self, message: &Message) -> Result<(), ash_core::AshError> {
        self.append_record(SessionRecord::from_message(message))
            .await
    }

    pub async fn append_turn_finished(
        &mut self,
        reason: StopReason,
        duration_ms: u64,
    ) -> Result<(), ash_core::AshError> {
        self.append_record(SessionRecord::TurnFinished(TurnFinishedRecord {
            reason,
            duration_ms,
        }))
        .await
    }

    pub async fn append_turn_failed(
        &mut self,
        error_kind: &str,
        duration_ms: u64,
    ) -> Result<(), ash_core::AshError> {
        self.append_record(SessionRecord::TurnFailed(TurnFailedRecord {
            error_kind: error_kind.to_string(),
            duration_ms,
        }))
        .await
    }

    pub async fn append_turn_rolled_back(
        &mut self,
        num_turns: u32,
    ) -> Result<(), ash_core::AshError> {
        self.append_record(SessionRecord::TurnRolledBack(TurnRolledBackRecord {
            num_turns,
        }))
        .await
    }

    async fn append_record(&mut self, record: SessionRecord) -> Result<(), ash_core::AshError> {
        self.materialize().await?;
        let line = serde_json::to_string(&SessionLine::new(record))
            .map_err(|error| ash_core::AshError::Config(error.to_string()))?;
        let file = self.file.as_mut().ok_or_else(|| {
            ash_core::AshError::Config("session store was not materialized".to_string())
        })?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        Ok(())
    }

    async fn materialize(&mut self) -> Result<(), ash_core::AshError> {
        if self.file.is_some() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&self.path)
            .await?;
        let line = serde_json::to_string(&SessionLine::new(SessionRecord::SessionMeta(
            self.metadata.clone(),
        )))
        .map_err(|error| ash_core::AshError::Config(error.to_string()))?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        self.file = Some(file);
        Ok(())
    }

    pub(crate) async fn latest_except(
        excluded_path: &Path,
    ) -> Result<Option<StoredSession>, ash_core::AshError> {
        let directory = Self::default_dir();
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut candidates = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path == excluded_path || !is_session_file(&path) {
                continue;
            }
            let modified = entry
                .metadata()
                .await
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((modified, path));
        }
        candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        for (_, path) in candidates {
            let stored = match read_session(&path).await {
                Ok(stored) if stored.has_user_message() => stored,
                Ok(_) => continue,
                Err(error) => {
                    warn!(path = %path.display(), %error, "skipping unreadable session");
                    continue;
                }
            };
            return Ok(Some(stored));
        }
        Ok(None)
    }

    pub(crate) async fn resume(stored: &StoredSession) -> Result<Self, ash_core::AshError> {
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&stored.path)
            .await?;
        ensure_newline_terminated(&mut file).await?;
        Ok(Self {
            path: stored.path.clone(),
            metadata: stored.metadata.clone(),
            file: Some(file),
        })
    }
}

fn session_filename(session_id: SessionId, timestamp: DateTime<chrono::FixedOffset>) -> String {
    format!(
        "session-{}-{session_id}.jsonl",
        timestamp.format("%Y-%m-%dT%H-%M-%S%.3f")
    )
}

fn is_session_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("session-") && name.ends_with(".jsonl"))
}

async fn read_session(path: &Path) -> Result<StoredSession, ash_core::AshError> {
    let file = tokio::fs::File::open(path).await?;
    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut metadata = None;
    let mut messages = Vec::new();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let parsed = match serde_json::from_str::<SessionLine>(&line) {
            Ok(parsed) => parsed,
            Err(error) => {
                warn!(path = %path.display(), %error, "skipping malformed session line");
                continue;
            }
        };
        match parsed.record {
            SessionRecord::SessionMeta(value) if metadata.is_none() => metadata = Some(value),
            SessionRecord::TurnRolledBack(record) => {
                rollback_messages(&mut messages, record.num_turns)
            }
            record => {
                if let Some(message) = record.into_message() {
                    messages.push(message);
                }
            }
        }
    }

    let metadata = metadata.ok_or_else(|| {
        ash_core::AshError::Config(format!(
            "session file is missing session metadata: {}",
            path.display()
        ))
    })?;
    if metadata.format_version != SESSION_FORMAT_VERSION {
        return Err(ash_core::AshError::Config(format!(
            "unsupported session format version {} in {}",
            metadata.format_version,
            path.display()
        )));
    }
    Ok(StoredSession {
        path: path.to_path_buf(),
        metadata,
        messages,
    })
}

fn rollback_messages(messages: &mut Vec<Message>, num_turns: u32) {
    for _ in 0..num_turns {
        let Some(turn_start) = messages
            .iter()
            .rposition(|message| matches!(&message.content, MessageContent::User(_)))
        else {
            messages.clear();
            return;
        };
        messages.truncate(turn_start);
    }
}

async fn ensure_newline_terminated(file: &mut tokio::fs::File) -> std::io::Result<()> {
    let length = file.metadata().await?.len();
    if length == 0 {
        return Ok(());
    }
    file.seek(std::io::SeekFrom::End(-1)).await?;
    let mut last_byte = [0];
    file.read_exact(&mut last_byte).await?;
    if last_byte[0] != b'\n' {
        file.write_all(b"\n").await?;
        file.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ash_core::{ModelId, Protocol, ProviderConfig};
    use chrono::{FixedOffset, TimeZone};
    use secrecy::SecretString;
    use tempfile::TempDir;

    use super::*;

    fn config(working_dir: PathBuf) -> AgentConfig {
        AgentConfig {
            provider: ProviderConfig {
                protocol: Protocol::OpenaiResponses,
                api_key: SecretString::from("must-not-be-persisted"),
                base_url: Some("https://example.invalid/v1?token=secret".to_string()),
            },
            system_prompt: Some("system prompt".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 10,
            working_dir,
            max_context_tokens: Some(1000),
            max_output_tokens: Some(200),
            tool_timeout: Duration::from_secs(5),
            agent_path: "/root".to_string(),
            root_session_id: None,
        }
    }

    #[test]
    fn puts_the_local_date_directly_in_the_session_filename() {
        let timestamp = FixedOffset::east_opt(8 * 60 * 60)
            .unwrap()
            .with_ymd_and_hms(2026, 7, 14, 16, 30, 25)
            .unwrap();
        let session_id = SessionId::new();
        let filename = session_filename(session_id, timestamp);
        assert_eq!(
            filename,
            format!("session-2026-07-14T16-30-25.000-{session_id}.jsonl")
        );
    }

    #[tokio::test]
    async fn stores_replayable_messages_without_credentials() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let mut store = SessionStore::new(&config(directory.path().to_path_buf()), session_id);
        store.path = directory.path().join(
            store
                .path
                .file_name()
                .expect("session filename should exist"),
        );
        store.append_message(&Message::user("hello")).await.unwrap();
        store
            .append_turn_finished(StopReason::EndTurn, 25)
            .await
            .unwrap();

        let contents = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(contents.contains("session_meta"));
        assert!(contents.contains("user_message"));
        assert!(!contents.contains("must-not-be-persisted"));
        assert!(!contents.contains("token=secret"));

        let loaded = read_session(store.path()).await.unwrap();
        assert_eq!(loaded.metadata.session_id, session_id);
        assert_eq!(loaded.messages.len(), 1);
    }

    #[tokio::test]
    async fn replays_turn_rollbacks_from_jsonl() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let mut store = SessionStore::new(&config(directory.path().to_path_buf()), session_id);
        store.path = directory.path().join(
            store
                .path
                .file_name()
                .expect("session filename should exist"),
        );
        store.append_message(&Message::user("first")).await.unwrap();
        store
            .append_message(&Message::assistant_text("first answer"))
            .await
            .unwrap();
        store
            .append_message(&Message::user("second"))
            .await
            .unwrap();
        store
            .append_message(&Message::assistant_text("second answer"))
            .await
            .unwrap();
        store.append_turn_rolled_back(1).await.unwrap();

        let loaded = read_session(store.path()).await.unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert!(matches!(
            &loaded.messages[0].content,
            MessageContent::User(_)
        ));
        assert!(matches!(
            &loaded.messages[1].content,
            MessageContent::Assistant(_)
        ));
    }
}
