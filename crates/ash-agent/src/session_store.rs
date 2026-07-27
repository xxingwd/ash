use std::path::{Path, PathBuf};

use ash_core::{Content, Message, MessageContent, MessageId, SessionId, SessionSummary};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::warn;

use crate::AgentConfig;

const SESSION_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SessionMetadata {
    pub format_version: u32,
    pub session_id: SessionId,
    pub created_at: String,
    pub protocol: String,
    pub model: String,
    pub working_dir: PathBuf,
    pub system_prompt: Option<String>,
    pub max_turns: u32,
    #[serde(default)]
    pub max_input_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<usize>,
    pub max_output_tokens: Option<u32>,
    pub tool_timeout_ms: u64,
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
            max_turns: config.max_turns,
            max_input_tokens: Some(config.max_input_tokens),
            max_context_tokens: None,
            max_output_tokens: config.max_output_tokens,
            tool_timeout_ms: u64::try_from(config.max_tool_duration.as_millis())
                .unwrap_or(u64::MAX),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TurnRolledBackRecord {
    num_turns: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContextCompactedRecord {
    summary: Message,
    tail_start_id: Option<MessageId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum SessionRecord {
    SessionMeta(SessionMetadata),
    UserMessage(Message),
    AssistantMessage(Message),
    ToolResult(Message),
    TurnFinished(serde_json::Value),
    TurnFailed(serde_json::Value),
    TurnRolledBack(TurnRolledBackRecord),
    ContextCompacted(ContextCompactedRecord),
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
            | Self::TurnRolledBack(_)
            | Self::ContextCompacted(_) => None,
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
    pub(crate) model_messages: Vec<Message>,
}

impl StoredSession {
    fn has_user_message(&self) -> bool {
        self.messages
            .iter()
            .any(|message| matches!(message.content, MessageContent::User(_)))
    }
}

pub(crate) struct SessionStore {
    path: PathBuf,
    metadata: SessionMetadata,
    file: Option<tokio::fs::File>,
}

impl SessionStore {
    pub(crate) fn new(config: &AgentConfig, session_id: SessionId) -> Self {
        Self::new_in(config, session_id, &Self::default_dir())
    }

    pub(crate) fn new_in(config: &AgentConfig, session_id: SessionId, directory: &Path) -> Self {
        let local_now = Local::now().fixed_offset();
        let created_at = local_now.with_timezone(&Utc);
        let path = directory.join(session_filename(session_id, local_now));
        Self {
            path,
            metadata: SessionMetadata::from_config(config, session_id, created_at),
            file: None,
        }
    }

    fn default_dir() -> PathBuf {
        directories::ProjectDirs::from("", "", "ash")
            .map(|dirs| dirs.data_dir().join("sessions"))
            .unwrap_or_else(|| PathBuf::from(".ash/sessions"))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) async fn append_message(
        &mut self,
        message: &Message,
    ) -> Result<(), ash_core::AshError> {
        self.append_record(SessionRecord::from_message(message))
            .await
    }

    pub(crate) async fn append_compaction(
        &mut self,
        compacted_messages: &[Message],
    ) -> Result<(), ash_core::AshError> {
        let summary = compacted_messages.first().cloned().ok_or_else(|| {
            ash_core::AshError::Config("compacted context has no summary message".to_string())
        })?;
        let tail_start_id = compacted_messages.get(1).map(|message| message.id);
        self.append_record(SessionRecord::ContextCompacted(ContextCompactedRecord {
            summary,
            tail_start_id,
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

    pub(crate) async fn truncate_last_turn(&mut self) -> Result<(), ash_core::AshError> {
        let file = self.file.as_mut().ok_or_else(|| {
            ash_core::AshError::Config("session store was not materialized".to_string())
        })?;
        file.flush().await?;
        let contents = tokio::fs::read_to_string(&self.path).await?;
        let length = last_active_turn_offset(&contents).ok_or_else(|| {
            ash_core::AshError::Config("session has no turn to truncate".to_string())
        })?;
        let length = u64::try_from(length)
            .map_err(|_| ash_core::AshError::Config("session file is too large".to_string()))?;
        file.set_len(length).await?;
        Ok(())
    }

    pub(crate) async fn load(&self) -> Result<StoredSession, ash_core::AshError> {
        read_session(&self.path).await
    }

    pub(crate) async fn summaries_except(
        excluded_path: &Path,
    ) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        Ok(Self::stored_sessions(Some(excluded_path))
            .await?
            .iter()
            .map(session_summary)
            .collect())
    }

    pub(crate) async fn find(
        session_id: SessionId,
    ) -> Result<Option<StoredSession>, ash_core::AshError> {
        Ok(Self::stored_sessions(None)
            .await?
            .into_iter()
            .find(|stored| stored.metadata.session_id == session_id))
    }

    async fn stored_sessions(
        excluded_path: Option<&Path>,
    ) -> Result<Vec<StoredSession>, ash_core::AshError> {
        Self::stored_sessions_in(Self::default_dir(), excluded_path).await
    }

    async fn stored_sessions_in(
        directory: PathBuf,
        excluded_path: Option<&Path>,
    ) -> Result<Vec<StoredSession>, ash_core::AshError> {
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut candidates = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if excluded_path.is_some_and(|excluded| path == excluded) || !is_session_file(&path) {
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
        let mut stored_sessions = Vec::new();
        for (_, path) in candidates {
            let stored = match read_session(&path).await {
                Ok(stored) if stored.has_user_message() => stored,
                Ok(_) => continue,
                Err(error) => {
                    warn!(path = %path.display(), %error, "skipping unreadable session");
                    continue;
                }
            };
            stored_sessions.push(stored);
        }
        Ok(stored_sessions)
    }

    pub(crate) async fn resume(stored: &StoredSession) -> Result<Self, ash_core::AshError> {
        let file = open_session_for_append(&stored.path).await?;
        Ok(Self {
            path: stored.path.clone(),
            metadata: stored.metadata.clone(),
            file: Some(file),
        })
    }
}

async fn open_session_for_append(path: &Path) -> Result<tokio::fs::File, ash_core::AshError> {
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .await?;
    ensure_newline_terminated(&mut file).await?;
    Ok(file)
}

fn session_summary(stored: &StoredSession) -> SessionSummary {
    SessionSummary {
        session_id: stored.metadata.session_id,
        title: session_title(&stored.messages),
        created_at: display_created_at(&stored.metadata.created_at),
    }
}

pub(crate) fn session_title(messages: &[Message]) -> String {
    let title = messages.iter().find_map(|message| {
        let MessageContent::User(contents) = &message.content else {
            return None;
        };
        let text = contents
            .iter()
            .filter_map(|content| match content {
                Content::Text(text) => Some(text.as_str()),
                Content::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
    });
    title
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "Untitled chat".to_string())
}

fn display_created_at(created_at: &str) -> String {
    DateTime::parse_from_rfc3339(created_at).map_or_else(
        |_| created_at.to_string(),
        |created_at| {
            created_at
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
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

fn last_active_turn_offset(contents: &str) -> Option<usize> {
    let mut turn_offsets = Vec::new();
    let mut offset = 0;
    for line in contents.split_inclusive('\n') {
        if let Ok(parsed) = serde_json::from_str::<SessionLine>(line.trim()) {
            match parsed.record {
                SessionRecord::UserMessage(_) => turn_offsets.push(offset),
                SessionRecord::TurnRolledBack(record) => {
                    for _ in 0..record.num_turns {
                        turn_offsets.pop();
                    }
                }
                SessionRecord::SessionMeta(_)
                | SessionRecord::AssistantMessage(_)
                | SessionRecord::ToolResult(_)
                | SessionRecord::TurnFinished(_)
                | SessionRecord::TurnFailed(_)
                | SessionRecord::ContextCompacted(_) => {}
            }
        }
        offset += line.len();
    }
    turn_offsets.last().copied()
}

async fn read_session(path: &Path) -> Result<StoredSession, ash_core::AshError> {
    let file = tokio::fs::File::open(path).await?;
    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut metadata = None;
    let mut messages = Vec::new();
    let mut model_messages = Vec::new();

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
                rollback_messages(&mut messages, record.num_turns);
                rollback_messages(&mut model_messages, record.num_turns);
            }
            SessionRecord::ContextCompacted(record) => {
                model_messages = apply_compaction(&messages, record);
            }
            record => {
                if let Some(message) = record.into_message() {
                    messages.push(message.clone());
                    model_messages.push(message);
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
        model_messages,
    })
}

fn apply_compaction(messages: &[Message], record: ContextCompactedRecord) -> Vec<Message> {
    let mut model_messages = vec![record.summary];
    let Some(tail_start_id) = record.tail_start_id else {
        return model_messages;
    };
    let Some(tail_start) = messages
        .iter()
        .position(|message| message.id == tail_start_id)
    else {
        return model_messages;
    };
    model_messages.extend_from_slice(&messages[tail_start..]);
    model_messages
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
            max_input_tokens: 1000,
            max_output_tokens: Some(200),
            max_tool_duration: Duration::from_secs(5),
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

    #[test]
    fn new_store_does_not_create_a_session_file() {
        let directory = TempDir::new().unwrap();
        let sessions_dir = directory.path().join("sessions");
        let store = SessionStore::new_in(
            &config(directory.path().to_path_buf()),
            SessionId::new(),
            &sessions_dir,
        );

        assert!(!store.path().exists());
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

        let contents = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(contents.contains("session_meta"));
        assert!(contents.contains("user_message"));
        assert!(!contents.contains("must-not-be-persisted"));
        assert!(!contents.contains("token=secret"));

        let loaded = read_session(store.path()).await.unwrap();
        assert_eq!(loaded.metadata.session_id, session_id);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.model_messages.len(), 1);
    }

    #[tokio::test]
    async fn truncates_last_turn_from_jsonl() {
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
        store.truncate_last_turn().await.unwrap();

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
        let contents = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(!contents.contains("second"));
        assert!(!contents.contains("turn_rolled_back"));
    }

    #[test]
    fn locates_the_last_turn_after_legacy_rollbacks() {
        let metadata = SessionLine::new(SessionRecord::SessionMeta(SessionMetadata::from_config(
            &config(PathBuf::from(".")),
            SessionId::new(),
            Utc::now(),
        )));
        let first = SessionLine::new(SessionRecord::from_message(&Message::user("first")));
        let second = SessionLine::new(SessionRecord::from_message(&Message::user("second")));
        let rollback = SessionLine::new(SessionRecord::TurnRolledBack(TurnRolledBackRecord {
            num_turns: 1,
        }));
        let lines = [&metadata, &first, &second, &rollback]
            .into_iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let contents = format!("{}\n", lines.join("\n"));

        assert_eq!(last_active_turn_offset(&contents), Some(lines[0].len() + 1));
    }

    #[tokio::test]
    async fn compaction_keeps_full_history_and_rebuilds_only_model_context() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let mut store = SessionStore::new_in(
            &config(directory.path().to_path_buf()),
            session_id,
            directory.path(),
        );
        store
            .append_message(&Message::user("old request"))
            .await
            .unwrap();
        store
            .append_message(&Message::assistant_text("old answer"))
            .await
            .unwrap();
        let recent_request = Message::user("recent request");
        let recent_answer = Message::assistant_text("recent answer");
        store.append_message(&recent_request).await.unwrap();
        store.append_message(&recent_answer).await.unwrap();
        let compacted = vec![
            Message::assistant_text("<context-summary>\nold facts\n</context-summary>"),
            recent_request,
            recent_answer,
        ];

        store.append_compaction(&compacted).await.unwrap();

        let loaded = read_session(store.path()).await.unwrap();
        assert_eq!(loaded.metadata.session_id, session_id);
        assert_eq!(loaded.messages.len(), 4);
        assert_eq!(loaded.model_messages.len(), compacted.len());
        assert_eq!(session_title(&loaded.messages), "old request");
        let summaries = SessionStore::stored_sessions_in(directory.path().to_path_buf(), None)
            .await
            .unwrap()
            .iter()
            .map(session_summary)
            .collect::<Vec<_>>();
        assert_eq!(summaries[0].title, "old request");
        let contents = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(contents.contains("old request"));
        assert!(contents.contains("old facts"));
        assert!(contents.contains("recent request"));

        let later_request = Message::user("later request");
        let later_answer = Message::assistant_text("later answer");
        store.append_message(&later_request).await.unwrap();
        store.append_message(&later_answer).await.unwrap();
        let second_compaction = vec![
            Message::assistant_text("<context-summary>\nnew facts\n</context-summary>"),
            later_request,
            later_answer,
        ];
        store.append_compaction(&second_compaction).await.unwrap();

        let loaded = read_session(store.path()).await.unwrap();
        assert_eq!(loaded.messages.len(), 6);
        assert_eq!(loaded.model_messages.len(), 3);
        assert!(matches!(
            &loaded.model_messages[0].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [ash_core::ContentBlock::Text(text)] if text.contains("new facts"))
        ));

        store.truncate_last_turn().await.unwrap();
        let rolled_back = read_session(store.path()).await.unwrap();
        assert_eq!(rolled_back.messages.len(), 4);
        assert_eq!(rolled_back.model_messages.len(), 3);
        assert!(matches!(
            &rolled_back.model_messages[0].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [ash_core::ContentBlock::Text(text)] if text.contains("old facts"))
        ));

        store.truncate_last_turn().await.unwrap();
        let rolled_back = read_session(store.path()).await.unwrap();
        assert_eq!(rolled_back.messages.len(), 2);
        assert_eq!(rolled_back.model_messages.len(), 2);
    }

    #[tokio::test]
    async fn lists_sessions_with_first_user_message_as_the_title() {
        let directory = TempDir::new().unwrap();
        let sessions_dir = directory.path().join("sessions");
        let session_id = SessionId::new();
        let mut store = SessionStore::new_in(
            &config(directory.path().to_path_buf()),
            session_id,
            &sessions_dir,
        );
        store
            .append_message(&Message::user("  First session title\nwith details  "))
            .await
            .unwrap();

        let stored = SessionStore::stored_sessions_in(sessions_dir, None)
            .await
            .unwrap();
        let summaries = stored.iter().map(session_summary).collect::<Vec<_>>();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, session_id);
        assert_eq!(summaries[0].title, "First session title with details");
        assert_eq!(summaries[0].created_at.len(), 16);
    }
}
