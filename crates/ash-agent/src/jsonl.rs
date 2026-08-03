use std::path::{Path, PathBuf};

use ash_core::{
    Content, Message, MessageContent, MessageId, ModelId, StopReason, ThreadId, ThreadSummary,
    TurnId,
};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::warn;

#[cfg(test)]
use crate::RunConfig;
use crate::{
    AcceptedInput, ContextCheckpoint, Record, StoredThread, ThreadLog, ThreadMetadata, ThreadStore,
    Version,
};

const THREAD_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct FileMetadata {
    pub format_version: u32,
    pub thread_id: ThreadId,
    pub created_at: String,
    pub protocol: String,
    pub model: String,
    pub working_dir: PathBuf,
    pub system_prompt: Option<String>,
    pub max_turns: u32,
    pub max_context_tokens: usize,
    pub tool_timeout_ms: u64,
    #[serde(default)]
    pub kind: crate::ThreadKind,
}

impl FileMetadata {
    #[cfg(test)]
    fn from_config(
        config: &RunConfig,
        thread_id: ThreadId,
        created_at: DateTime<Utc>,
        model_backend: &str,
    ) -> Self {
        Self {
            format_version: THREAD_FORMAT_VERSION,
            thread_id,
            created_at: created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            protocol: model_backend.to_string(),
            model: config.model.as_str().to_string(),
            working_dir: config.working_dir.clone(),
            system_prompt: config.system_prompt.clone(),
            max_turns: config.max_turns,
            max_context_tokens: config.max_context_tokens,
            tool_timeout_ms: u64::try_from(config.max_tool_duration.as_millis())
                .unwrap_or(u64::MAX),
            kind: config_kind(config),
        }
    }

    fn from_metadata(metadata: ThreadMetadata, created_at: DateTime<Utc>) -> Self {
        Self {
            format_version: THREAD_FORMAT_VERSION,
            thread_id: metadata.thread_id,
            created_at: created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            protocol: metadata.model_backend,
            model: metadata.model.as_str().to_string(),
            working_dir: metadata.working_dir,
            system_prompt: metadata.system_prompt,
            max_turns: metadata.max_turns,
            max_context_tokens: metadata.max_context_tokens,
            tool_timeout_ms: u64::try_from(metadata.max_tool_duration.as_millis())
                .unwrap_or(u64::MAX),
            kind: metadata.kind,
        }
    }
}

#[cfg(test)]
fn config_kind(config: &RunConfig) -> crate::ThreadKind {
    if config
        .metadata
        .get("kind")
        .and_then(serde_json::Value::as_str)
        == Some("subagent")
    {
        crate::ThreadKind::Subagent
    } else {
        crate::ThreadKind::Root
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContextCompactedRecord {
    summary: Message,
    tail_start_id: Option<MessageId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum FileRecord {
    #[serde(rename = "thread_meta", alias = "session_meta")]
    ThreadMeta(FileMetadata),
    TurnStarted {
        turn_id: TurnId,
    },
    InputAccepted(AcceptedInput),
    UserMessage(Message),
    AssistantMessage(Message),
    ToolResult(Message),
    TurnRolledBack,
    TurnCompleted {
        turn_id: TurnId,
        reason: StopReason,
    },
    TurnFailed {
        turn_id: TurnId,
        error: String,
    },
    ContextCompacted(ContextCompactedRecord),
}

impl FileRecord {
    fn from_message(message: &Message) -> Self {
        match &message.content {
            MessageContent::User(_) => Self::UserMessage(message.clone()),
            MessageContent::Assistant(_) => Self::AssistantMessage(message.clone()),
            MessageContent::ToolResult { .. } => Self::ToolResult(message.clone()),
        }
    }

    fn from_entry(entry: Record) -> Self {
        match entry {
            Record::TurnStarted { turn_id } => Self::TurnStarted { turn_id },
            Record::InputAccepted(input) => Self::InputAccepted(input),
            Record::Message(message) => Self::from_message(&message),
            Record::ContextCheckpoint(checkpoint) => {
                Self::ContextCompacted(ContextCompactedRecord {
                    summary: checkpoint.summary,
                    tail_start_id: checkpoint.tail_start_id,
                })
            }
            Record::TurnRolledBack => Self::TurnRolledBack,
            Record::TurnCompleted { turn_id, reason } => Self::TurnCompleted { turn_id, reason },
            Record::TurnFailed { turn_id, error } => Self::TurnFailed { turn_id, error },
        }
    }

    fn into_entry(self) -> Option<Record> {
        match self {
            Self::TurnStarted { turn_id } => Some(Record::TurnStarted { turn_id }),
            Self::InputAccepted(input) => Some(Record::InputAccepted(input)),
            Self::UserMessage(message)
            | Self::AssistantMessage(message)
            | Self::ToolResult(message) => Some(Record::Message(message)),
            Self::ContextCompacted(record) => Some(Record::ContextCheckpoint(ContextCheckpoint {
                summary: record.summary,
                tail_start_id: record.tail_start_id,
            })),
            Self::TurnRolledBack => Some(Record::TurnRolledBack),
            Self::TurnCompleted { turn_id, reason } => {
                Some(Record::TurnCompleted { turn_id, reason })
            }
            Self::TurnFailed { turn_id, error } => Some(Record::TurnFailed { turn_id, error }),
            Self::ThreadMeta(_) => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileLine {
    timestamp: String,
    #[serde(flatten)]
    record: FileRecord,
}

impl FileLine {
    fn new(record: FileRecord) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            record,
        }
    }
}

#[derive(Debug)]
pub(crate) struct StoredFile {
    pub(crate) path: PathBuf,
    pub(crate) metadata: FileMetadata,
    pub(crate) log: ThreadLog,
}

#[derive(Default)]
struct Replay {
    metadata: Option<FileMetadata>,
    records: Vec<FileRecord>,
}

impl StoredFile {
    fn has_user_message(&self) -> bool {
        self.log
            .messages()
            .iter()
            .any(|message| matches!(message.content, MessageContent::User(_)))
    }

    #[cfg(test)]
    pub(crate) fn messages(&self) -> Vec<Message> {
        self.log.messages()
    }

    #[cfg(test)]
    pub(crate) fn model_context(&self) -> Vec<Message> {
        self.log.model_context()
    }
}

impl Replay {
    fn apply(&mut self, record: FileRecord) {
        match record {
            FileRecord::ThreadMeta(metadata) => {
                self.metadata.get_or_insert(metadata);
            }
            record => self.records.push(record),
        }
    }

    fn finish(self, path: &Path) -> Result<StoredFile, ash_core::AshError> {
        let metadata = self.metadata.ok_or_else(|| {
            ash_core::AshError::Config(format!(
                "thread file is missing metadata: {}",
                path.display()
            ))
        })?;
        if metadata.format_version != THREAD_FORMAT_VERSION {
            return Err(ash_core::AshError::Config(format!(
                "unsupported thread format version {} in {}",
                metadata.format_version,
                path.display()
            )));
        }
        let log = ThreadLog::from_entries(
            self.records
                .into_iter()
                .filter_map(FileRecord::into_entry)
                .collect(),
        );
        Ok(StoredFile {
            path: path.to_path_buf(),
            metadata,
            log,
        })
    }
}

pub(crate) struct ThreadFile {
    path: PathBuf,
    metadata: FileMetadata,
    file: Option<tokio::fs::File>,
    revision: Version,
}

pub struct JsonlThreadStore {
    directory: PathBuf,
    gate: tokio::sync::Mutex<()>,
}

impl Default for JsonlThreadStore {
    fn default() -> Self {
        Self {
            directory: ThreadFile::default_dir(),
            gate: tokio::sync::Mutex::new(()),
        }
    }
}

impl JsonlThreadStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            gate: tokio::sync::Mutex::new(()),
        }
    }
}

impl ThreadFile {
    #[cfg(test)]
    pub(crate) fn new(config: &RunConfig, thread_id: ThreadId) -> Self {
        Self::new_with_backend(config, thread_id, "custom")
    }

    #[cfg(test)]
    pub(crate) fn new_with_backend(
        config: &RunConfig,
        thread_id: ThreadId,
        model_backend: &str,
    ) -> Self {
        Self::new_in_with_backend(config, thread_id, &Self::default_dir(), model_backend)
    }

    #[cfg(test)]
    pub(crate) fn new_in(config: &RunConfig, thread_id: ThreadId, directory: &Path) -> Self {
        Self::new_in_with_backend(config, thread_id, directory, "custom")
    }

    #[cfg(test)]
    fn new_in_with_backend(
        config: &RunConfig,
        thread_id: ThreadId,
        directory: &Path,
        model_backend: &str,
    ) -> Self {
        let local_now = Local::now().fixed_offset();
        let created_at = local_now.with_timezone(&Utc);
        let path = directory.join(thread_filename(thread_id, local_now));
        Self {
            path,
            metadata: FileMetadata::from_config(config, thread_id, created_at, model_backend),
            file: None,
            revision: Version::initial(),
        }
    }

    fn from_metadata(metadata: ThreadMetadata, directory: &Path) -> Self {
        let local_now = Local::now().fixed_offset();
        let created_at = local_now.with_timezone(&Utc);
        let path = directory.join(thread_filename(metadata.thread_id, local_now));
        Self {
            path,
            metadata: FileMetadata::from_metadata(metadata, created_at),
            file: None,
            revision: Version::initial(),
        }
    }

    fn default_dir() -> PathBuf {
        directories::ProjectDirs::from("", "", "ash")
            .map(|dirs| dirs.data_dir().join("threads"))
            .unwrap_or_else(|| PathBuf::from(".ash/threads"))
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) async fn append_message(
        &mut self,
        message: &Message,
    ) -> Result<(), ash_core::AshError> {
        self.append(self.revision, &[Record::Message(message.clone())])
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    pub(crate) async fn append_compaction(
        &mut self,
        compacted_messages: &[Message],
    ) -> Result<(), ash_core::AshError> {
        self.append(
            self.revision,
            &[Record::ContextCheckpoint(
                ContextCheckpoint::from_model_context(compacted_messages)?,
            )],
        )
        .await
        .map(|_| ())
    }

    #[cfg(test)]
    fn revision(&self) -> Version {
        self.revision
    }

    async fn append(
        &mut self,
        expected_revision: Version,
        records: &[Record],
    ) -> Result<Version, ash_core::AshError> {
        if expected_revision != self.revision {
            return Err(ash_core::AshError::Config(format!(
                "thread version conflict: expected {}, actual {}",
                expected_revision.value(),
                self.revision.value()
            )));
        }
        if records
            .iter()
            .any(|record| matches!(record, Record::TurnRolledBack))
            && self.file.is_none()
        {
            return Err(ash_core::AshError::Config(
                "thread store has no persisted turn to roll back".to_string(),
            ));
        }

        let is_new = self.file.is_none();
        let mut data = String::new();
        if is_new {
            push_line(&mut data, FileRecord::ThreadMeta(self.metadata.clone()))?;
        }
        for record in records.iter().cloned() {
            push_line(&mut data, FileRecord::from_entry(record))?;
        }

        if is_new {
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            self.file = Some(
                tokio::fs::OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .append(true)
                    .open(&self.path)
                    .await?,
            );
        }
        let file = self.file.as_mut().ok_or_else(|| {
            ash_core::AshError::Config("thread store was not materialized".to_string())
        })?;
        file.write_all(data.as_bytes()).await?;
        file.flush().await?;
        self.revision = self.revision.advance(records.len());
        Ok(self.revision)
    }

    #[cfg(test)]
    pub(crate) async fn append_rollback(&mut self) -> Result<(), ash_core::AshError> {
        if self.file.is_none() {
            return Err(ash_core::AshError::Config(
                "thread store has no persisted turn to roll back".to_string(),
            ));
        }
        self.append(self.revision, &[Record::TurnRolledBack])
            .await
            .map(|_| ())
    }

    async fn stored_threads_in(
        directory: PathBuf,
        excluded_path: Option<&Path>,
    ) -> Result<Vec<StoredFile>, ash_core::AshError> {
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut candidates = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if excluded_path.is_some_and(|excluded| path == excluded) || !is_thread_file(&path) {
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
        let mut stored_threads = Vec::new();
        for (_, path) in candidates {
            let stored = match read_thread(&path).await {
                Ok(stored) => stored,
                Err(error) => {
                    warn!(path = %path.display(), %error, "skipping unreadable thread");
                    continue;
                }
            };
            stored_threads.push(stored);
        }
        Ok(stored_threads)
    }

    pub(crate) async fn resume(stored: &StoredFile) -> Result<Self, ash_core::AshError> {
        let file = open_thread_for_append(&stored.path).await?;
        Ok(Self {
            path: stored.path.clone(),
            metadata: stored.metadata.clone(),
            file: Some(file),
            revision: stored.log.revision(),
        })
    }
}

fn push_line(data: &mut String, record: FileRecord) -> Result<(), ash_core::AshError> {
    data.push_str(
        &serde_json::to_string(&FileLine::new(record))
            .map_err(|error| ash_core::AshError::Config(error.to_string()))?,
    );
    data.push('\n');
    Ok(())
}

#[async_trait::async_trait]
impl ThreadStore for JsonlThreadStore {
    async fn create(
        &self,
        metadata: ThreadMetadata,
        records: &[Record],
    ) -> Result<Version, ash_core::AshError> {
        let _guard = self.gate.lock().await;
        let mut thread = ThreadFile::from_metadata(metadata, &self.directory);
        thread.append(Version::initial(), records).await
    }

    async fn load(&self, thread_id: ThreadId) -> Result<Option<StoredThread>, ash_core::AshError> {
        let _guard = self.gate.lock().await;
        let stored = ThreadFile::stored_threads_in(self.directory.clone(), None)
            .await?
            .into_iter()
            .find(|stored| stored.metadata.thread_id == thread_id);
        Ok(stored.map(|stored| StoredThread {
            version: stored.log.revision(),
            log: stored.log,
            metadata: thread_metadata(&stored.metadata),
        }))
    }

    async fn append(
        &self,
        thread_id: ThreadId,
        expected_version: Version,
        records: &[Record],
    ) -> Result<Version, ash_core::AshError> {
        let _guard = self.gate.lock().await;
        let stored = ThreadFile::stored_threads_in(self.directory.clone(), None)
            .await?
            .into_iter()
            .find(|stored| stored.metadata.thread_id == thread_id)
            .ok_or_else(|| ash_core::AshError::Config(format!("thread not found: {thread_id}")))?;
        let mut thread = ThreadFile::resume(&stored).await?;
        thread.append(expected_version, records).await
    }

    async fn list(
        &self,
        excluded_thread: Option<ThreadId>,
    ) -> Result<Vec<ThreadSummary>, ash_core::AshError> {
        let _guard = self.gate.lock().await;
        Ok(ThreadFile::stored_threads_in(self.directory.clone(), None)
            .await?
            .into_iter()
            .filter(|stored| {
                stored.metadata.kind != crate::ThreadKind::Subagent
                    && stored.has_user_message()
                    && Some(stored.metadata.thread_id) != excluded_thread
            })
            .map(|stored| thread_summary(&stored))
            .collect())
    }
}

fn thread_metadata(metadata: &FileMetadata) -> ThreadMetadata {
    ThreadMetadata {
        thread_id: metadata.thread_id,
        model_backend: metadata.protocol.clone(),
        model: ModelId::new(metadata.model.clone()),
        working_dir: metadata.working_dir.clone(),
        system_prompt: metadata.system_prompt.clone(),
        max_turns: metadata.max_turns,
        max_context_tokens: metadata.max_context_tokens,
        max_tool_duration: std::time::Duration::from_millis(metadata.tool_timeout_ms),
        kind: metadata.kind,
    }
}

async fn open_thread_for_append(path: &Path) -> Result<tokio::fs::File, ash_core::AshError> {
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .await?;
    ensure_newline_terminated(&mut file).await?;
    Ok(file)
}

fn thread_summary(stored: &StoredFile) -> ThreadSummary {
    ThreadSummary {
        thread_id: stored.metadata.thread_id,
        title: thread_title(&stored.log.messages()),
        created_at: display_created_at(&stored.metadata.created_at),
    }
}

pub(crate) fn thread_title(messages: &[Message]) -> String {
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

fn thread_filename(thread_id: ThreadId, timestamp: DateTime<chrono::FixedOffset>) -> String {
    format!(
        "thread-{}-{thread_id}.jsonl",
        timestamp.format("%Y-%m-%dT%H-%M-%S%.3f")
    )
}

fn is_thread_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            (name.starts_with("thread-") || name.starts_with("session-"))
                && name.ends_with(".jsonl")
        })
}

async fn read_thread(path: &Path) -> Result<StoredFile, ash_core::AshError> {
    let file = tokio::fs::File::open(path).await?;
    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut replay = Replay::default();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let parsed = match serde_json::from_str::<FileLine>(&line) {
            Ok(parsed) => parsed,
            Err(error) => {
                warn!(path = %path.display(), %error, "skipping malformed thread line");
                continue;
            }
        };
        replay.apply(parsed.record);
    }
    replay.finish(path)
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

    use ash_core::ModelId;
    use chrono::{FixedOffset, TimeZone};
    use tempfile::TempDir;

    use super::*;

    fn config(working_dir: PathBuf) -> RunConfig {
        RunConfig {
            system_prompt: Some("system prompt".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 10,
            working_dir,
            max_context_tokens: 1000,
            context_policy: std::sync::Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(5),
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
        }
    }

    #[test]
    fn puts_the_local_date_directly_in_the_thread_filename() {
        let timestamp = FixedOffset::east_opt(8 * 60 * 60)
            .unwrap()
            .with_ymd_and_hms(2026, 7, 14, 16, 30, 25)
            .unwrap();
        let thread_id = ThreadId::new();
        let filename = thread_filename(thread_id, timestamp);
        assert_eq!(
            filename,
            format!("thread-2026-07-14T16-30-25.000-{thread_id}.jsonl")
        );
    }

    #[test]
    fn new_store_does_not_create_a_thread_file() {
        let directory = TempDir::new().unwrap();
        let threads_dir = directory.path().join("threads");
        let store = ThreadFile::new_in(
            &config(directory.path().to_path_buf()),
            ThreadId::new(),
            &threads_dir,
        );

        assert!(!store.path().exists());
    }

    #[tokio::test]
    async fn stores_replayable_messages_without_credentials() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let mut store = ThreadFile::new(&config(directory.path().to_path_buf()), thread_id);
        store.path = directory.path().join(
            store
                .path
                .file_name()
                .expect("thread filename should exist"),
        );
        store.append_message(&Message::user("hello")).await.unwrap();

        let contents = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(contents.contains("thread_meta"));
        assert!(contents.contains("user_message"));
        assert!(!contents.contains("must-not-be-persisted"));
        assert!(!contents.contains("token=secret"));

        let loaded = read_thread(store.path()).await.unwrap();
        assert_eq!(loaded.metadata.thread_id, thread_id);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.model_context().len(), 1);
    }

    #[tokio::test]
    async fn rejects_an_append_from_a_stale_revision() {
        let directory = TempDir::new().unwrap();
        let mut store = ThreadFile::new_in(
            &config(directory.path().to_path_buf()),
            ThreadId::new(),
            directory.path(),
        );
        let stale_revision = store.revision();
        store.append_message(&Message::user("first")).await.unwrap();

        let error = store
            .append(stale_revision, &[Record::Message(Message::user("stale"))])
            .await
            .unwrap_err();

        assert!(error.to_string().contains("thread version conflict"));
        assert_eq!(store.revision().value(), 1);
        assert_eq!(read_thread(store.path()).await.unwrap().messages().len(), 1);
    }

    #[tokio::test]
    async fn appends_rollback_and_replays_without_the_last_turn() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let mut store = ThreadFile::new(&config(directory.path().to_path_buf()), thread_id);
        store.path = directory.path().join(
            store
                .path
                .file_name()
                .expect("thread filename should exist"),
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
        let length_before_rollback = tokio::fs::metadata(store.path()).await.unwrap().len();
        store.append_rollback().await.unwrap();

        let loaded = read_thread(store.path()).await.unwrap();
        let messages = loaded.messages();
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0].content, MessageContent::User(_)));
        assert!(matches!(&messages[1].content, MessageContent::Assistant(_)));
        let contents = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(contents.contains("second"));
        assert!(contents.contains("turn_rolled_back"));
        assert!(tokio::fs::metadata(store.path()).await.unwrap().len() > length_before_rollback);
    }

    #[tokio::test]
    async fn compaction_keeps_full_history_and_rebuilds_only_model_context() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let mut store = ThreadFile::new_in(
            &config(directory.path().to_path_buf()),
            thread_id,
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

        let loaded = read_thread(store.path()).await.unwrap();
        assert_eq!(loaded.metadata.thread_id, thread_id);
        assert_eq!(loaded.messages().len(), 4);
        assert_eq!(loaded.model_context().len(), compacted.len());
        assert_eq!(thread_title(&loaded.messages()), "old request");
        let summaries = ThreadFile::stored_threads_in(directory.path().to_path_buf(), None)
            .await
            .unwrap()
            .iter()
            .map(thread_summary)
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

        let loaded = read_thread(store.path()).await.unwrap();
        assert_eq!(loaded.messages().len(), 6);
        assert_eq!(loaded.model_context().len(), 3);
        assert!(matches!(
            &loaded.model_context()[0].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [ash_core::ContentBlock::Text(text)] if text.contains("new facts"))
        ));

        store.append_rollback().await.unwrap();
        let rolled_back = read_thread(store.path()).await.unwrap();
        assert_eq!(rolled_back.messages().len(), 4);
        assert_eq!(rolled_back.model_context().len(), 3);
        assert!(matches!(
            &rolled_back.model_context()[0].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [ash_core::ContentBlock::Text(text)] if text.contains("old facts"))
        ));

        store.append_rollback().await.unwrap();
        let rolled_back = read_thread(store.path()).await.unwrap();
        assert_eq!(rolled_back.messages().len(), 2);
        assert_eq!(rolled_back.model_context().len(), 2);
    }

    #[tokio::test]
    async fn lists_threads_with_first_user_message_as_the_title() {
        let directory = TempDir::new().unwrap();
        let threads_dir = directory.path().join("threads");
        let thread_id = ThreadId::new();
        let mut store = ThreadFile::new_in(
            &config(directory.path().to_path_buf()),
            thread_id,
            &threads_dir,
        );
        store
            .append_message(&Message::user("  First thread title\nwith details  "))
            .await
            .unwrap();

        let stored = ThreadFile::stored_threads_in(threads_dir, None)
            .await
            .unwrap();
        let summaries = stored.iter().map(thread_summary).collect::<Vec<_>>();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].thread_id, thread_id);
        assert_eq!(summaries[0].title, "First thread title with details");
        assert_eq!(summaries[0].created_at.len(), 16);
    }

    #[tokio::test]
    async fn hides_subagent_threads_from_the_session_list() {
        let directory = TempDir::new().unwrap();
        let threads_dir = directory.path().join("threads");
        let root_id = ThreadId::new();
        let subagent_id = ThreadId::new();
        let cfg = config(directory.path().to_path_buf());
        let metadata = |id: ThreadId, kind: crate::ThreadKind| ThreadMetadata {
            thread_id: id,
            model_backend: "custom".to_string(),
            model: cfg.model.clone(),
            working_dir: cfg.working_dir.clone(),
            system_prompt: cfg.system_prompt.clone(),
            max_turns: cfg.max_turns,
            max_context_tokens: cfg.max_context_tokens,
            max_tool_duration: cfg.max_tool_duration,
            kind,
        };
        let records = |id: ThreadId| {
            vec![Record::Message(Message {
                id: ash_core::MessageId::new(),
                role: ash_core::Role::User,
                content: ash_core::MessageContent::User(vec![ash_core::Content::Text(format!(
                    "task {id}"
                ))]),
            })]
        };

        let store = JsonlThreadStore::new(threads_dir);
        store
            .create(
                metadata(root_id, crate::ThreadKind::Root),
                &records(root_id),
            )
            .await
            .unwrap();
        store
            .create(
                metadata(subagent_id, crate::ThreadKind::Subagent),
                &records(subagent_id),
            )
            .await
            .unwrap();

        let listed = store.list(None).await.unwrap();
        assert!(listed.iter().any(|summary| summary.thread_id == root_id));
        assert!(!listed
            .iter()
            .any(|summary| summary.thread_id == subagent_id));
    }
}
