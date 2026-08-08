use std::path::{Path, PathBuf};

use ash_core::{
    Content, Message, MessageContent, MessageId, ThreadId, ThreadSummary, TurnId, TurnResult, Usage,
};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::warn;

use crate::{
    AcceptedInput, ContextCheckpoint, LogEntry, OpenedThread, StoredThread, ThreadAppender,
    ThreadKind, ThreadLog, ThreadMetadata, ThreadStore,
};

const MAX_THREAD_TITLE_CHARS: usize = 160;
const UNTITLED_CHAT: &str = "Untitled chat";

/// Immutable data needed to discover and display a thread without replaying it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ThreadHeader {
    thread_id: ThreadId,
    title: Option<String>,
    #[serde(default)]
    kind: ThreadKind,
}

#[derive(Clone, Debug)]
struct StoredHeader {
    thread_id: ThreadId,
    created_at: String,
    title: Option<String>,
    kind: ThreadKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContextCompactedRecord {
    summary: Message,
    tail_start_id: Option<MessageId>,
}

/// One durable entry on disk. Messages are split by role so a thread file is
/// readable at a glance; `from_entry`/`into_entry` map them to `LogEntry`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum FileRecord {
    ThreadHeader(ThreadHeader),
    TurnStart {
        turn_id: TurnId,
    },
    InputAccepted(AcceptedInput),
    UserMessage(Message),
    AssistantMessage(Message),
    ToolResult(Message),
    TurnEnd {
        turn_id: TurnId,
        result: TurnResult,
        usage: Option<Usage>,
    },
    Rollback,
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

    fn from_entry(entry: LogEntry) -> Self {
        match entry {
            LogEntry::TurnStart(turn_id) => Self::TurnStart { turn_id },
            LogEntry::Input(input) => Self::InputAccepted(input),
            LogEntry::Message(message) => Self::from_message(&message),
            LogEntry::Checkpoint(checkpoint) => Self::ContextCompacted(ContextCompactedRecord {
                summary: checkpoint.summary,
                tail_start_id: checkpoint.tail_start_id,
            }),
            LogEntry::TurnEnd { id, result, usage } => Self::TurnEnd {
                turn_id: id,
                result,
                usage,
            },
            LogEntry::Rollback => Self::Rollback,
        }
    }

    fn into_entry(self) -> Option<LogEntry> {
        match self {
            Self::TurnStart { turn_id } => Some(LogEntry::TurnStart(turn_id)),
            Self::InputAccepted(input) => Some(LogEntry::Input(input)),
            Self::UserMessage(message)
            | Self::AssistantMessage(message)
            | Self::ToolResult(message) => Some(LogEntry::Message(message)),
            Self::ContextCompacted(record) => Some(LogEntry::Checkpoint(ContextCheckpoint {
                summary: record.summary,
                tail_start_id: record.tail_start_id,
            })),
            Self::TurnEnd {
                turn_id,
                result,
                usage,
            } => Some(LogEntry::TurnEnd {
                id: turn_id,
                result,
                usage,
            }),
            Self::Rollback => Some(LogEntry::Rollback),
            Self::ThreadHeader(_) => None,
        }
    }

    fn user_message(&self) -> Option<&Message> {
        match self {
            Self::InputAccepted(input) => Some(&input.message),
            Self::UserMessage(message) => Some(message),
            _ => None,
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
struct StoredFile {
    header: StoredHeader,
    log: ThreadLog,
}

impl StoredFile {
    fn into_stored(self) -> StoredThread {
        StoredThread {
            metadata: ThreadMetadata {
                thread_id: self.header.thread_id,
                kind: self.header.kind,
            },
            log: self.log,
        }
    }

    #[cfg(test)]
    fn messages(&self) -> Vec<Message> {
        self.log.messages()
    }

    #[cfg(test)]
    fn model_context(&self) -> Vec<Message> {
        self.log.model_context()
    }
}

#[derive(Default)]
struct Replay {
    header: Option<StoredHeader>,
    records: Vec<FileRecord>,
}

impl Replay {
    fn apply(&mut self, line: FileLine) {
        if let Some(header) = stored_header(&line.record, &line.timestamp) {
            self.header.get_or_insert(header);
        } else {
            self.records.push(line.record);
        }
    }

    fn finish(self, path: &Path) -> Result<StoredFile, ash_core::AshError> {
        let mut header = self.header.ok_or_else(|| missing_header(path))?;
        let log = ThreadLog::from_entries(
            self.records
                .into_iter()
                .filter_map(FileRecord::into_entry)
                .collect(),
        );
        header.title = header
            .title
            .or_else(|| title_from_messages(&log.messages()));
        Ok(StoredFile { header, log })
    }
}

/// An exclusively locked append handle. The lock follows the file descriptor
/// and is released automatically when the writer is dropped.
#[derive(Debug)]
pub struct ThreadWriter {
    path: PathBuf,
    metadata: Option<ThreadMetadata>,
    file: Option<tokio::fs::File>,
}

impl ThreadWriter {
    fn new(directory: &Path, metadata: ThreadMetadata) -> Self {
        Self {
            path: directory.join(thread_filename(metadata.thread_id)),
            metadata: Some(metadata),
            file: None,
        }
    }

    fn existing(path: PathBuf, file: tokio::fs::File) -> Self {
        Self {
            path,
            metadata: None,
            file: Some(file),
        }
    }

    pub(crate) async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError> {
        if entries.is_empty() {
            return Ok(());
        }
        if entries
            .iter()
            .any(|entry| matches!(entry, LogEntry::Rollback))
            && self.file.is_none()
        {
            return Err(ash_core::AshError::Config(
                "thread store has no persisted turn to roll back".to_string(),
            ));
        }

        let is_new = self.file.is_none();
        let mut data = String::new();
        if is_new {
            let metadata = self.metadata.ok_or_else(|| {
                ash_core::AshError::Config("new thread is missing metadata".to_string())
            })?;
            push_line(
                &mut data,
                FileRecord::ThreadHeader(ThreadHeader {
                    thread_id: metadata.thread_id,
                    title: title_from_entries(entries),
                    kind: metadata.kind,
                }),
            )?;
        }
        for entry in entries.iter().cloned() {
            push_line(&mut data, FileRecord::from_entry(entry))?;
        }

        if is_new {
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            self.file = Some(create_locked_thread(&self.path).await?);
        }
        let file = self.file.as_mut().ok_or_else(|| {
            ash_core::AshError::Config("thread store was not materialized".to_string())
        })?;
        file.write_all(data.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ThreadAppender for ThreadWriter {
    async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError> {
        ThreadWriter::append(self, entries).await
    }
}

pub struct JsonlThreadStore {
    directory: PathBuf,
}

impl Default for JsonlThreadStore {
    fn default() -> Self {
        Self {
            directory: default_thread_dir(),
        }
    }
}

impl JsonlThreadStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    async fn find_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<PathBuf>, ash_core::AshError> {
        let path = self.directory.join(thread_filename(thread_id));
        if tokio::fs::try_exists(&path).await? {
            return Ok(Some(path));
        }

        let mut entries = match tokio::fs::read_dir(&self.directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if canonical_thread_id(&path) == Some(thread_id) {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    async fn thread_paths(&self) -> Result<Vec<PathBuf>, ash_core::AshError> {
        let mut entries = match tokio::fs::read_dir(&self.directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut candidates = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !is_thread_file(&path) {
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
        Ok(candidates.into_iter().map(|(_, path)| path).collect())
    }
}

#[async_trait::async_trait]
impl ThreadStore for JsonlThreadStore {
    async fn create(
        &self,
        metadata: ThreadMetadata,
        entries: &[LogEntry],
    ) -> Result<(), ash_core::AshError> {
        ThreadWriter::new(&self.directory, metadata)
            .append(entries)
            .await
    }

    async fn load(&self, thread_id: ThreadId) -> Result<Option<StoredThread>, ash_core::AshError> {
        let Some(path) = self.find_thread(thread_id).await? else {
            return Ok(None);
        };
        let stored = read_thread(&path).await?;
        ensure_thread_id(stored.header.thread_id, thread_id, &path)?;
        Ok(Some(stored.into_stored()))
    }

    async fn open(&self, thread_id: ThreadId) -> Result<Option<OpenedThread>, ash_core::AshError> {
        let Some(path) = self.find_thread(thread_id).await? else {
            return Ok(None);
        };
        let file = open_locked_thread(&path).await?;
        let mut reader = tokio::io::BufReader::new(file);
        let stored = replay_thread(&mut reader, &path).await?;
        ensure_thread_id(stored.header.thread_id, thread_id, &path)?;
        let mut file = reader.into_inner();
        ensure_newline_terminated(&mut file).await?;
        Ok(Some(OpenedThread {
            thread: stored.into_stored(),
            writer: Box::new(ThreadWriter::existing(path, file)),
        }))
    }

    async fn list(
        &self,
        excluded_thread: Option<ThreadId>,
    ) -> Result<Vec<ThreadSummary>, ash_core::AshError> {
        let mut summaries = Vec::new();
        for path in self.thread_paths().await? {
            match read_summary(&path).await {
                Ok(Some(summary)) if Some(summary.thread_id) != excluded_thread => {
                    summaries.push(summary);
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(path = %path.display(), %error, "skipping unreadable thread");
                }
            }
        }
        Ok(summaries)
    }

    async fn open_writer(
        &self,
        metadata: ThreadMetadata,
    ) -> Result<Box<dyn ThreadAppender>, ash_core::AshError> {
        Ok(Box::new(ThreadWriter::new(&self.directory, metadata)))
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

fn default_thread_dir() -> PathBuf {
    thread_dir_from_data_dir(
        directories::ProjectDirs::from("", "", "ash").map(|dirs| dirs.data_dir().to_path_buf()),
    )
}

/// The default threads directory is the platform data dir plus `threads`, or
/// the relative `.ash/threads` fallback when no platform data dir is
/// available. Pure so both branches are covered by regression tests.
fn thread_dir_from_data_dir(data_dir: Option<PathBuf>) -> PathBuf {
    data_dir
        .map(|dir| dir.join("threads"))
        .unwrap_or_else(|| PathBuf::from(".ash").join("threads"))
}

fn thread_filename(thread_id: ThreadId) -> String {
    format!("{thread_id}.jsonl")
}

fn is_thread_file(path: &Path) -> bool {
    canonical_thread_id(path).is_some()
}

fn canonical_thread_id(path: &Path) -> Option<ThreadId> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        return None;
    }
    path.file_stem()?.to_str()?.parse().ok()
}

async fn create_locked_thread(path: &Path) -> Result<tokio::fs::File, ash_core::AshError> {
    locked_thread(path, true).await
}

async fn open_locked_thread(path: &Path) -> Result<tokio::fs::File, ash_core::AshError> {
    locked_thread(path, false).await
}

async fn locked_thread(
    path: &Path,
    create_new: bool,
) -> Result<tokio::fs::File, ash_core::AshError> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true).append(true);
    if create_new {
        options.create_new(true);
    }
    let file = options.open(path).await?;
    lock_thread(file, path).await
}

async fn lock_thread(
    file: tokio::fs::File,
    path: &Path,
) -> Result<tokio::fs::File, ash_core::AshError> {
    let file = file.into_std().await;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => ash_core::AshError::Config(format!(
            "thread is already open in another runtime: {}",
            path.display()
        )),
        std::fs::TryLockError::Error(error) => error.into(),
    })?;
    Ok(tokio::fs::File::from_std(file))
}

async fn read_thread(path: &Path) -> Result<StoredFile, ash_core::AshError> {
    let file = tokio::fs::File::open(path).await?;
    replay_thread(&mut tokio::io::BufReader::new(file), path).await
}

async fn replay_thread<R>(reader: &mut R, path: &Path) -> Result<StoredFile, ash_core::AshError>
where
    R: AsyncBufRead + Unpin,
{
    let mut replay = Replay::default();
    let mut line = String::new();
    while reader.read_line(&mut line).await? != 0 {
        let value = line.trim();
        if !value.is_empty() {
            match serde_json::from_str::<FileLine>(value) {
                Ok(parsed) => replay.apply(parsed),
                Err(error) => {
                    warn!(path = %path.display(), %error, "skipping malformed thread line");
                }
            }
        }
        line.clear();
    }
    replay.finish(path)
}

async fn read_summary(path: &Path) -> Result<Option<ThreadSummary>, ash_core::AshError> {
    let file = tokio::fs::File::open(path).await?;
    let mut reader = tokio::io::BufReader::new(file);
    let Some(first) = read_first_record(&mut reader, path).await? else {
        return Err(missing_header(path));
    };
    let mut header =
        stored_header(&first.record, &first.timestamp).ok_or_else(|| missing_header(path))?;
    if let Some(thread_id) = canonical_thread_id(path) {
        ensure_thread_id(header.thread_id, thread_id, path)?;
    }
    if header.kind == ThreadKind::Subagent {
        return Ok(None);
    }
    if header.title.is_none() {
        header.title = read_first_user_title(&mut reader, path).await?;
    }
    Ok(header.title.map(|title| ThreadSummary {
        thread_id: header.thread_id,
        title,
        created_at: display_created_at(&header.created_at),
    }))
}

fn stored_header(record: &FileRecord, timestamp: &str) -> Option<StoredHeader> {
    match record {
        FileRecord::ThreadHeader(header) => Some(StoredHeader {
            thread_id: header.thread_id,
            created_at: timestamp.to_string(),
            title: header.title.clone(),
            kind: header.kind,
        }),
        _ => None,
    }
}

async fn read_first_record<R>(
    reader: &mut R,
    path: &Path,
) -> Result<Option<FileLine>, ash_core::AshError>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    while reader.read_line(&mut line).await? != 0 {
        let value = line.trim();
        if value.is_empty() {
            line.clear();
            continue;
        }
        return serde_json::from_str(value).map(Some).map_err(|error| {
            ash_core::AshError::Config(format!(
                "invalid thread header in {}: {error}",
                path.display()
            ))
        });
    }
    Ok(None)
}

async fn read_first_user_title<R>(
    reader: &mut R,
    path: &Path,
) -> Result<Option<String>, ash_core::AshError>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    while reader.read_line(&mut line).await? != 0 {
        let value = line.trim();
        if !value.is_empty() {
            match serde_json::from_str::<FileLine>(value) {
                Ok(parsed) => {
                    if let Some(title) = parsed.record.user_message().and_then(message_title) {
                        return Ok(Some(title));
                    }
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "skipping malformed thread line");
                }
            }
        }
        line.clear();
    }
    Ok(None)
}

fn missing_header(path: &Path) -> ash_core::AshError {
    ash_core::AshError::Config(format!(
        "thread file is missing its header: {}",
        path.display()
    ))
}

fn ensure_thread_id(
    actual: ThreadId,
    expected: ThreadId,
    path: &Path,
) -> Result<(), ash_core::AshError> {
    if actual == expected {
        return Ok(());
    }
    Err(ash_core::AshError::Config(format!(
        "thread id in header does not match filename {}: expected {expected}, found {actual}",
        path.display()
    )))
}

fn title_from_entries(entries: &[LogEntry]) -> Option<String> {
    entries.iter().find_map(|entry| match entry {
        LogEntry::Input(input) => message_title(&input.message),
        LogEntry::Message(message) => message_title(message),
        _ => None,
    })
}

fn title_from_messages(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(message_title)
}

fn message_title(message: &Message) -> Option<String> {
    let MessageContent::User(contents) = &message.content else {
        return None;
    };
    let title = contents
        .iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.as_str()),
            Content::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(shorten_title(if title.is_empty() {
        UNTITLED_CHAT.to_string()
    } else {
        title
    }))
}

fn shorten_title(title: String) -> String {
    let mut chars = title.chars();
    let prefix = chars
        .by_ref()
        .take(MAX_THREAD_TITLE_CHARS)
        .collect::<String>();
    if chars.next().is_none() {
        return prefix;
    }
    prefix
        .chars()
        .take(MAX_THREAD_TITLE_CHARS - 3)
        .chain("...".chars())
        .collect()
}

#[cfg(test)]
fn thread_title(messages: &[Message]) -> String {
    title_from_messages(messages).unwrap_or_else(|| UNTITLED_CHAT.to_string())
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
    use tempfile::TempDir;

    use super::*;

    fn metadata(thread_id: ThreadId, kind: ThreadKind) -> ThreadMetadata {
        ThreadMetadata { thread_id, kind }
    }

    async fn create_thread(
        store: &JsonlThreadStore,
        thread_id: ThreadId,
        kind: ThreadKind,
        entries: &[LogEntry],
    ) {
        store
            .create(metadata(thread_id, kind), entries)
            .await
            .unwrap();
    }

    #[test]
    fn uses_the_thread_id_as_the_filename() {
        let thread_id = ThreadId::new();

        assert_eq!(thread_filename(thread_id), format!("{thread_id}.jsonl"));
    }

    #[tokio::test]
    async fn opening_a_new_writer_does_not_create_a_file() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let store = JsonlThreadStore::new(directory.path());
        let writer = store
            .open_writer(metadata(thread_id, ThreadKind::Root))
            .await
            .unwrap();

        assert!(!directory.path().join(thread_filename(thread_id)).exists());
        drop(writer);
    }

    #[tokio::test]
    async fn creating_a_thread_without_entries_does_not_create_a_file() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let store = JsonlThreadStore::new(directory.path());

        create_thread(&store, thread_id, ThreadKind::Root, &[]).await;

        assert!(!directory.path().join(thread_filename(thread_id)).exists());
        assert!(store.load(thread_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn writes_a_compact_header_before_the_log() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let store = JsonlThreadStore::new(directory.path());
        create_thread(
            &store,
            thread_id,
            ThreadKind::Root,
            &[LogEntry::Message(Message::user(
                "  First thread title\nwith details  ",
            ))],
        )
        .await;

        let path = directory.path().join(thread_filename(thread_id));
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let first = serde_json::from_str::<FileLine>(contents.lines().next().unwrap()).unwrap();
        assert!(matches!(
            first.record,
            FileRecord::ThreadHeader(ThreadHeader { title: Some(ref title), .. })
                if title == "First thread title with details"
        ));
        assert!(!contents.contains("system_prompt"));

        let loaded = read_thread(&path).await.unwrap();
        assert_eq!(loaded.header.thread_id, thread_id);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.model_context().len(), 1);
    }

    #[tokio::test]
    async fn holds_an_exclusive_lock_for_the_open_writer() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let store = JsonlThreadStore::new(directory.path());
        create_thread(
            &store,
            thread_id,
            ThreadKind::Root,
            &[LogEntry::Message(Message::user("question"))],
        )
        .await;

        let opened = store.open(thread_id).await.unwrap().unwrap();
        let error = store.open(thread_id).await.unwrap_err();
        assert!(error.to_string().contains("already open"));

        drop(opened);
        assert!(store.open(thread_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn appends_rollback_and_replays_without_the_last_turn() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let store = JsonlThreadStore::new(directory.path());
        let first = Message::user("first");
        let first_answer = Message::assistant_text("first answer");
        let second = Message::user("second");
        create_thread(
            &store,
            thread_id,
            ThreadKind::Root,
            &[
                LogEntry::TurnStart(TurnId::new()),
                LogEntry::Message(first),
                LogEntry::Message(first_answer),
                LogEntry::TurnStart(TurnId::new()),
                LogEntry::Message(second),
                LogEntry::Message(Message::assistant_text("second answer")),
            ],
        )
        .await;
        let mut opened = store.open(thread_id).await.unwrap().unwrap();
        opened.writer.append(&[LogEntry::Rollback]).await.unwrap();
        drop(opened);

        let loaded = store.load(thread_id).await.unwrap().unwrap();
        let messages = loaded.log.messages();
        // history keeps only user messages; the second turn is rolled back.
        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0].content, MessageContent::User(_)));
    }

    #[tokio::test]
    async fn compaction_keeps_full_history_and_rebuilds_only_model_context() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let store = JsonlThreadStore::new(directory.path());
        let recent_request = Message::user("recent request");
        let recent_answer = Message::assistant_text("recent answer");
        create_thread(
            &store,
            thread_id,
            ThreadKind::Root,
            &[
                LogEntry::Message(Message::user("old request")),
                LogEntry::Message(Message::assistant_text("old answer")),
                LogEntry::Message(recent_request.clone()),
                LogEntry::Message(recent_answer.clone()),
            ],
        )
        .await;
        let mut opened = store.open(thread_id).await.unwrap().unwrap();
        opened
            .writer
            .append(&[LogEntry::Checkpoint(
                ContextCheckpoint::from_model_context(&[
                    Message::assistant_text("<context-summary>\nold facts\n</context-summary>"),
                    recent_request,
                    recent_answer,
                ])
                .unwrap(),
            )])
            .await
            .unwrap();
        drop(opened);

        let loaded = store.load(thread_id).await.unwrap().unwrap();
        assert_eq!(loaded.log.messages().len(), 4);
        assert_eq!(loaded.log.model_context().len(), 3);
        assert_eq!(thread_title(&loaded.log.messages()), "old request");
    }

    #[tokio::test]
    async fn lists_new_threads_from_the_first_line() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let store = JsonlThreadStore::new(directory.path());
        create_thread(
            &store,
            thread_id,
            ThreadKind::Root,
            &[LogEntry::Message(Message::user("thread title"))],
        )
        .await;
        let path = directory.path().join(thread_filename(thread_id));
        let mut contents = tokio::fs::read_to_string(&path).await.unwrap();
        contents.push_str("malformed tail that list must not read\n");
        tokio::fs::write(&path, contents).await.unwrap();

        let listed = store.list(None).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].thread_id, thread_id);
        assert_eq!(listed[0].title, "thread title");
        assert_eq!(listed[0].created_at.len(), 16);
    }

    #[tokio::test]
    async fn loading_skips_a_malformed_record_and_keeps_the_valid_tail() {
        let directory = TempDir::new().unwrap();
        let thread_id = ThreadId::new();
        let path = directory.path().join(thread_filename(thread_id));
        let mut contents = String::new();
        push_line(
            &mut contents,
            FileRecord::ThreadHeader(ThreadHeader {
                thread_id,
                title: Some("recoverable".to_string()),
                kind: ThreadKind::Root,
            }),
        )
        .unwrap();
        push_line(
            &mut contents,
            FileRecord::UserMessage(Message::user("before damage")),
        )
        .unwrap();
        contents.push_str("malformed record\n");
        push_line(
            &mut contents,
            FileRecord::UserMessage(Message::user("after damage")),
        )
        .unwrap();
        tokio::fs::write(path, contents).await.unwrap();

        let store = JsonlThreadStore::new(directory.path());
        let loaded = store.load(thread_id).await.unwrap().unwrap();
        let prompts = loaded
            .log
            .messages()
            .iter()
            .filter_map(Message::user_turn_text)
            .collect::<Vec<_>>();
        assert_eq!(prompts, ["before damage", "after damage"]);
    }

    #[tokio::test]
    async fn rejects_a_header_that_does_not_match_the_filename() {
        let directory = TempDir::new().unwrap();
        let filename_id = ThreadId::new();
        let header_id = ThreadId::new();
        let path = directory.path().join(thread_filename(filename_id));
        let mut contents = String::new();
        push_line(
            &mut contents,
            FileRecord::ThreadHeader(ThreadHeader {
                thread_id: header_id,
                title: Some("mismatched".to_string()),
                kind: ThreadKind::Root,
            }),
        )
        .unwrap();
        tokio::fs::write(path, contents).await.unwrap();
        let store = JsonlThreadStore::new(directory.path());

        let error = store.load(filename_id).await.unwrap_err();
        assert!(error.to_string().contains("does not match filename"));
        assert!(store.list(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn hides_subagent_threads_from_the_session_list() {
        let directory = TempDir::new().unwrap();
        let root_id = ThreadId::new();
        let subagent_id = ThreadId::new();
        let store = JsonlThreadStore::new(directory.path());
        create_thread(
            &store,
            root_id,
            ThreadKind::Root,
            &[LogEntry::Message(Message::user("root"))],
        )
        .await;
        create_thread(
            &store,
            subagent_id,
            ThreadKind::Subagent,
            &[LogEntry::Message(Message::user("subagent"))],
        )
        .await;

        let listed = store.list(None).await.unwrap();
        assert!(listed.iter().any(|summary| summary.thread_id == root_id));
        assert!(!listed
            .iter()
            .any(|summary| summary.thread_id == subagent_id));
    }

    #[tokio::test]
    async fn writer_reuses_the_locked_file_handle() {
        let directory = TempDir::new().unwrap();
        let store = JsonlThreadStore::new(directory.path());
        let thread_id = ThreadId::new();
        let mut writer = store
            .open_writer(metadata(thread_id, ThreadKind::Root))
            .await
            .unwrap();
        let turn_id = TurnId::new();

        writer
            .append(&[
                LogEntry::TurnStart(turn_id),
                LogEntry::Message(Message::user("question")),
            ])
            .await
            .unwrap();
        assert!(directory.path().join(thread_filename(thread_id)).exists());
        writer
            .append(&[
                LogEntry::Message(Message::assistant_text("answer")),
                LogEntry::TurnEnd {
                    id: turn_id,
                    result: ash_core::TurnResult::Completed(ash_core::StopReason::EndTurn),
                    usage: None,
                },
            ])
            .await
            .unwrap();

        let loaded = store.load(thread_id).await.unwrap().unwrap();
        assert_eq!(loaded.log.messages().len(), 2);
    }

    #[test]
    fn default_thread_dir_appends_threads_to_the_platform_data_dir() {
        let data_dir = PathBuf::from("var/lib/ash");
        assert_eq!(
            thread_dir_from_data_dir(Some(data_dir.clone())),
            data_dir.join("threads")
        );
    }

    #[test]
    fn default_thread_dir_falls_back_to_a_relative_dot_ash_dir() {
        assert_eq!(
            thread_dir_from_data_dir(None),
            PathBuf::from(".ash").join("threads")
        );
    }

    #[test]
    fn limits_titles_stored_in_the_header() {
        let title = "a".repeat(MAX_THREAD_TITLE_CHARS + 20);
        let shortened = thread_title(&[Message::user(&title)]);

        assert_eq!(shortened.chars().count(), MAX_THREAD_TITLE_CHARS);
        assert!(shortened.ends_with("..."));
    }
}
