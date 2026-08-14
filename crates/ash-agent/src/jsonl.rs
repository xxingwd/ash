use std::path::{Path, PathBuf};

use ash_core::{
    parse_agent_path, AgentPath, Content, Message, MessageContent, MessageId, SessionId,
    SessionIdentity, SessionSummary, TurnId, TurnResult, Usage,
};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::warn;

use crate::store::{OpenedSession, SessionStore, StoredSession};
use crate::{AcceptedInput, ContextCheckpoint, LogEntry, SessionAppender, SessionLog};

const MAX_SESSION_TITLE_CHARS: usize = 160;
const UNTITLED_CHAT: &str = "Untitled chat";

/// Immutable data needed to discover and display a session without replaying it.
/// Lineage fields are all required: a file without them cannot be placed in a
/// session tree and is rejected instead of silently becoming a root session.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionHeader {
    session_id: SessionId,
    root_id: SessionId,
    parent_id: Option<SessionId>,
    path: AgentPath,
    title: Option<String>,
}

impl SessionHeader {
    fn new(identity: SessionIdentity, title: Option<String>) -> Self {
        Self {
            session_id: identity.id,
            root_id: identity.root_id,
            parent_id: identity.parent_id,
            path: identity.path,
            title,
        }
    }

    fn identity(&self) -> SessionIdentity {
        SessionIdentity {
            id: self.session_id,
            root_id: self.root_id,
            parent_id: self.parent_id,
            path: self.path.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct StoredHeader {
    identity: SessionIdentity,
    created_at: String,
    title: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContextCompactedRecord {
    summary: Message,
    tail_start_id: Option<MessageId>,
}

/// One durable entry on disk. Messages are split by role so a session file is
/// readable at a glance; `from_entry`/`into_entry` map them to `LogEntry`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum FileRecord {
    SessionHeader(SessionHeader),
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
    fn from_entry(entry: LogEntry) -> Self {
        match entry {
            LogEntry::TurnStart(turn_id) => Self::TurnStart { turn_id },
            LogEntry::Input(input) => Self::InputAccepted(input),
            LogEntry::Message(message) => {
                // Match on a borrow to pick the variant, then move the message
                // into it without cloning.
                let variant = match &message.content {
                    MessageContent::User(_) | MessageContent::System(_) => Self::UserMessage,
                    MessageContent::Assistant(_) => Self::AssistantMessage,
                    MessageContent::ToolResult { .. } => Self::ToolResult,
                };
                variant(message)
            }
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
            Self::SessionHeader(_) => None,
        }
    }

    const fn user_message(&self) -> Option<&Message> {
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
    log: SessionLog,
}

impl StoredFile {
    fn into_stored(self) -> StoredSession {
        StoredSession {
            identity: self.header.identity,
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
        ensure_canonical_path(&header, path)?;
        let log = SessionLog::from_entries(
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

/// Validate one replayed header: the agent path must parse and already be in
/// canonical form before the header is trusted.
fn ensure_canonical_path(header: &StoredHeader, path: &Path) -> Result<(), ash_core::AshError> {
    let invalid = || {
        ash_core::AshError::Config(format!(
            "invalid session header in {}: {}",
            path.display(),
            header.identity.path
        ))
    };
    let parsed = parse_agent_path(header.identity.path.as_str()).map_err(|_| invalid())?;
    if parsed != header.identity.path {
        return Err(invalid());
    }
    Ok(())
}

/// An exclusively locked append handle. The lock follows the file descriptor
/// and is released automatically when the writer is dropped.
#[derive(Debug)]
pub struct SessionWriter {
    path: PathBuf,
    state: WriterState,
}

/// A writer is either brand new (header not yet written) or attached to an
/// already-open locked file. The two states are mutually exclusive, so they
/// are modelled as one enum instead of two optional fields.
#[derive(Debug)]
enum WriterState {
    New { identity: SessionIdentity },
    Open { file: tokio::fs::File },
}

impl SessionWriter {
    fn new(directory: &Path, identity: SessionIdentity) -> Self {
        Self {
            path: directory.join(session_filename(identity.id)),
            state: WriterState::New { identity },
        }
    }

    const fn existing(path: PathBuf, file: tokio::fs::File) -> Self {
        Self {
            path,
            state: WriterState::Open { file },
        }
    }

    pub(crate) async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError> {
        if entries.is_empty() {
            return Ok(());
        }
        let is_new = matches!(self.state, WriterState::New { .. });
        if entries
            .iter()
            .any(|entry| matches!(entry, LogEntry::Rollback))
            && is_new
        {
            return Err(ash_core::AshError::Config(
                "session store has no persisted turn to roll back".to_string(),
            ));
        }

        let mut data = String::new();
        if is_new {
            let WriterState::New { identity } = &self.state else {
                // `is_new` was captured before any mutation, so this is
                // unreachable; fail closed rather than panic.
                return Err(ash_core::AshError::Config(
                    "session writer state changed during append".to_string(),
                ));
            };
            push_line(
                &mut data,
                FileRecord::SessionHeader(SessionHeader::new(
                    identity.clone(),
                    title_from_entries(entries),
                )),
            )?;
        }
        for entry in entries.iter().cloned() {
            push_line(&mut data, FileRecord::from_entry(entry))?;
        }

        if is_new {
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let file = create_locked_session(&self.path).await?;
            self.state = WriterState::Open { file };
        }
        let WriterState::Open { file } = &mut self.state else {
            // Guaranteed by the `is_new` transition above.
            return Err(ash_core::AshError::Config(
                "session store was not materialized".to_string(),
            ));
        };
        file.write_all(data.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionAppender for SessionWriter {
    async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError> {
        Self::append(self, entries).await
    }
}

pub struct JsonlSessionStore {
    directory: PathBuf,
}

impl Default for JsonlSessionStore {
    fn default() -> Self {
        let data_dir =
            directories::ProjectDirs::from("", "", "ash").map(|dirs| dirs.data_dir().to_path_buf());
        Self {
            directory: session_dir_from_data_dir(data_dir),
        }
    }
}

impl JsonlSessionStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    async fn find_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<PathBuf>, ash_core::AshError> {
        let path = self.directory.join(session_filename(session_id));
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
            if canonical_session_id(&path) == Some(session_id) {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    async fn session_paths(&self) -> Result<Vec<PathBuf>, ash_core::AshError> {
        let mut entries = match tokio::fs::read_dir(&self.directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut candidates = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if canonical_session_id(&path).is_none() {
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

    async fn read_summaries(
        &self,
        scope: SummaryScope,
    ) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        let mut summaries = Vec::new();
        for path in self.session_paths().await? {
            match read_summary(&path, scope).await {
                Ok(Some(summary)) => summaries.push(summary),
                Ok(_) => {}
                Err(error) => {
                    warn!(path = %path.display(), %error, "skipping unreadable session");
                }
            }
        }
        Ok(summaries)
    }
}
#[async_trait::async_trait]
impl SessionStore for JsonlSessionStore {
    async fn open_new(
        &self,
        identity: SessionIdentity,
    ) -> Result<Box<dyn SessionAppender>, ash_core::AshError> {
        Ok(Box::new(SessionWriter::new(&self.directory, identity)))
    }

    async fn open(
        &self,
        session_id: SessionId,
    ) -> Result<Option<OpenedSession>, ash_core::AshError> {
        let Some(path) = self.find_session(session_id).await? else {
            return Ok(None);
        };
        let file = open_locked_session(&path).await?;
        let mut reader = tokio::io::BufReader::new(file);
        let stored = replay_session(&mut reader, &path).await?;
        ensure_session_id(stored.header.identity.id, session_id, &path)?;
        let mut file = reader.into_inner();
        ensure_newline_terminated(&mut file).await?;
        Ok(Some(OpenedSession {
            session: stored.into_stored(),
            writer: Box::new(SessionWriter::existing(path, file)),
        }))
    }

    async fn load(
        &self,
        session_id: SessionId,
    ) -> Result<Option<StoredSession>, ash_core::AshError> {
        let Some(path) = self.find_session(session_id).await? else {
            return Ok(None);
        };
        let stored = read_session(&path).await?;
        ensure_session_id(stored.header.identity.id, session_id, &path)?;
        Ok(Some(stored.into_stored()))
    }

    async fn list_roots(&self) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        self.read_summaries(SummaryScope::Roots).await
    }

    async fn tree(&self, root_id: SessionId) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        self.read_summaries(SummaryScope::Tree(root_id)).await
    }
}

/// Which sessions a summary query should surface.
#[derive(Clone, Copy)]
enum SummaryScope {
    /// Root sessions only: the default session picker.
    Roots,
    /// Every session belonging to one collaboration tree.
    Tree(SessionId),
}

impl SummaryScope {
    fn includes(&self, identity: &SessionIdentity) -> bool {
        match self {
            Self::Roots => identity.parent_id.is_none(),
            Self::Tree(root_id) => identity.root_id == *root_id,
        }
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

fn session_dir_from_data_dir(data_dir: Option<PathBuf>) -> PathBuf {
    data_dir.map_or_else(
        || PathBuf::from(".ash").join("sessions"),
        |dir| dir.join("sessions"),
    )
}

fn session_filename(session_id: SessionId) -> String {
    format!("{session_id}.jsonl")
}

fn canonical_session_id(path: &Path) -> Option<SessionId> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        return None;
    }
    path.file_stem()?.to_str()?.parse().ok()
}

async fn create_locked_session(path: &Path) -> Result<tokio::fs::File, ash_core::AshError> {
    locked_session(path, true).await
}

async fn open_locked_session(path: &Path) -> Result<tokio::fs::File, ash_core::AshError> {
    locked_session(path, false).await
}

async fn locked_session(
    path: &Path,
    create_new: bool,
) -> Result<tokio::fs::File, ash_core::AshError> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true).append(true);
    if create_new {
        options.create_new(true);
    }
    let file = options.open(path).await?;
    lock_session(file, path).await
}

async fn lock_session(
    file: tokio::fs::File,
    path: &Path,
) -> Result<tokio::fs::File, ash_core::AshError> {
    let file = file.into_std().await;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => ash_core::AshError::Config(format!(
            "session is already open in another runtime: {}",
            path.display()
        )),
        std::fs::TryLockError::Error(error) => error.into(),
    })?;
    Ok(tokio::fs::File::from_std(file))
}

async fn read_session(path: &Path) -> Result<StoredFile, ash_core::AshError> {
    let file = tokio::fs::File::open(path).await?;
    replay_session(&mut tokio::io::BufReader::new(file), path).await
}

async fn replay_session<R>(reader: &mut R, path: &Path) -> Result<StoredFile, ash_core::AshError>
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
                    warn!(path = %path.display(), %error, "skipping malformed session line");
                }
            }
        }
        line.clear();
    }
    replay.finish(path)
}

async fn read_summary(
    path: &Path,
    scope: SummaryScope,
) -> Result<Option<SessionSummary>, ash_core::AshError> {
    let file = tokio::fs::File::open(path).await?;
    let mut reader = tokio::io::BufReader::new(file);
    let Some(first) = read_first_record(&mut reader, path).await? else {
        return Err(missing_header(path));
    };
    let mut header =
        stored_header(&first.record, &first.timestamp).ok_or_else(|| missing_header(path))?;
    ensure_canonical_path(&header, path)?;
    if let Some(session_id) = canonical_session_id(path) {
        ensure_session_id(header.identity.id, session_id, path)?;
    }
    if !scope.includes(&header.identity) {
        return Ok(None);
    }
    if header.title.is_none() {
        header.title = read_first_user_title(&mut reader, path).await?;
    }
    Ok(header.title.map(|title| SessionSummary {
        session_id: header.identity.id,
        title,
        created_at: display_created_at(&header.created_at),
    }))
}

fn stored_header(record: &FileRecord, timestamp: &str) -> Option<StoredHeader> {
    match record {
        FileRecord::SessionHeader(header) => Some(StoredHeader {
            identity: header.identity(),
            created_at: timestamp.to_string(),
            title: header.title.clone(),
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
                "invalid session header in {}: {error}",
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
                    warn!(path = %path.display(), %error, "skipping malformed session line");
                }
            }
        }
        line.clear();
    }
    Ok(None)
}

fn missing_header(path: &Path) -> ash_core::AshError {
    ash_core::AshError::Config(format!(
        "session file is missing its header: {}",
        path.display()
    ))
}

fn ensure_session_id(
    actual: SessionId,
    expected: SessionId,
    path: &Path,
) -> Result<(), ash_core::AshError> {
    if actual == expected {
        return Ok(());
    }
    Err(ash_core::AshError::Config(format!(
        "session id in header does not match filename {}: expected {expected}, found {actual}",
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
        UNTITLED_CHAT
    } else {
        title.as_str()
    }))
}

fn shorten_title(title: &str) -> String {
    let mut chars = title.chars();
    let prefix = chars
        .by_ref()
        .take(MAX_SESSION_TITLE_CHARS)
        .collect::<String>();
    if chars.next().is_none() {
        return prefix;
    }
    prefix
        .chars()
        .take(MAX_SESSION_TITLE_CHARS - 3)
        .chain("...".chars())
        .collect()
}

#[cfg(test)]
fn session_title(messages: &[Message]) -> String {
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

    async fn create_session(
        store: &JsonlSessionStore,
        session_id: SessionId,
        entries: &[LogEntry],
    ) {
        let mut writer = store
            .open_new(SessionIdentity::root(session_id))
            .await
            .unwrap();
        writer.append(entries).await.unwrap();
    }

    async fn create_child_session(
        store: &JsonlSessionStore,
        parent: SessionIdentity,
        task_name: &str,
        entries: &[LogEntry],
    ) -> SessionId {
        let session_id = SessionId::new();
        let identity = parent.child(session_id, task_name).unwrap();
        let mut writer = store.open_new(identity).await.unwrap();
        writer.append(entries).await.unwrap();
        session_id
    }

    async fn stored_log(store: &JsonlSessionStore, session_id: SessionId) -> SessionLog {
        store.load(session_id).await.unwrap().unwrap().log
    }

    #[test]
    fn uses_the_session_id_as_the_filename() {
        let session_id = SessionId::new();

        assert_eq!(session_filename(session_id), format!("{session_id}.jsonl"));
    }

    #[tokio::test]
    async fn opening_a_new_writer_does_not_create_a_file() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());
        let writer = store
            .open_new(SessionIdentity::root(session_id))
            .await
            .unwrap();

        assert!(!directory.path().join(session_filename(session_id)).exists());
        drop(writer);
    }

    #[tokio::test]
    async fn creating_a_session_without_entries_does_not_create_a_file() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());

        create_session(&store, session_id, &[]).await;

        assert!(!directory.path().join(session_filename(session_id)).exists());
        assert!(store.load(session_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn writes_a_compact_header_before_the_log() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());
        create_session(
            &store,
            session_id,
            &[LogEntry::Message(Message::user(
                "  First session title\nwith details  ",
            ))],
        )
        .await;

        let path = directory.path().join(session_filename(session_id));
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let first = serde_json::from_str::<FileLine>(contents.lines().next().unwrap()).unwrap();
        assert!(matches!(
            first.record,
            FileRecord::SessionHeader(SessionHeader { title: Some(ref title), .. })
                if title == "First session title with details"
        ));
        assert!(!contents.contains("system_prompt"));
        assert!(contents.contains(r#""type":"session_header""#));
        assert!(contents.contains(r#""session_id""#));

        let loaded = read_session(&path).await.unwrap();
        assert_eq!(loaded.header.identity.id, session_id);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.model_context().len(), 1);
    }

    #[tokio::test]
    async fn holds_an_exclusive_lock_for_the_open_writer() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());
        create_session(
            &store,
            session_id,
            &[LogEntry::Message(Message::user("question"))],
        )
        .await;

        let opened = store.open(session_id).await.unwrap().unwrap();
        let error = store.open(session_id).await.unwrap_err();
        assert!(error.to_string().contains("already open"));

        drop(opened);
        assert!(store.open(session_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn appends_rollback_and_replays_without_the_last_turn() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());
        let first = Message::user("first");
        let first_answer = Message::assistant_text("first answer");
        let second = Message::user("second");
        create_session(
            &store,
            session_id,
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
        let mut opened = store.open(session_id).await.unwrap().unwrap();
        opened.writer.append(&[LogEntry::Rollback]).await.unwrap();
        drop(opened);

        let loaded = stored_log(&store, session_id).await;
        let messages = loaded.messages();
        // history keeps only user messages; the second turn is rolled back.
        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0].content, MessageContent::User(_)));
    }

    #[tokio::test]
    async fn compaction_keeps_full_history_and_rebuilds_only_model_context() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());
        let recent_request = Message::user("recent request");
        let recent_answer = Message::assistant_text("recent answer");
        create_session(
            &store,
            session_id,
            &[
                LogEntry::Message(Message::user("old request")),
                LogEntry::Message(Message::assistant_text("old answer")),
                LogEntry::Message(recent_request.clone()),
                LogEntry::Message(recent_answer.clone()),
            ],
        )
        .await;
        let mut opened = store.open(session_id).await.unwrap().unwrap();
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

        let loaded = stored_log(&store, session_id).await;
        assert_eq!(loaded.messages().len(), 4);
        assert_eq!(loaded.model_context().len(), 3);
        assert_eq!(session_title(&loaded.messages()), "old request");
    }

    #[tokio::test]
    async fn lists_new_sessions_from_the_first_line() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());
        create_session(
            &store,
            session_id,
            &[LogEntry::Message(Message::user("session title"))],
        )
        .await;
        let path = directory.path().join(session_filename(session_id));
        let mut contents = tokio::fs::read_to_string(&path).await.unwrap();
        contents.push_str("malformed tail that list must not read\n");
        tokio::fs::write(&path, contents).await.unwrap();

        let listed = store.list_roots().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, session_id);
        assert_eq!(listed[0].title, "session title");
        assert_eq!(listed[0].created_at.len(), 16);
    }

    #[tokio::test]
    async fn loading_skips_a_malformed_record_and_keeps_the_valid_tail() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let path = directory.path().join(session_filename(session_id));
        let mut contents = String::new();
        push_line(
            &mut contents,
            FileRecord::SessionHeader(SessionHeader::new(
                SessionIdentity::root(session_id),
                Some("recoverable".to_string()),
            )),
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

        let store = JsonlSessionStore::new(directory.path());
        let loaded = stored_log(&store, session_id).await;
        let prompts = loaded
            .messages()
            .iter()
            .filter_map(Message::user_turn_text)
            .collect::<Vec<_>>();
        assert_eq!(prompts, ["before damage", "after damage"]);
    }

    #[tokio::test]
    async fn rejects_a_header_that_does_not_match_the_filename() {
        let directory = TempDir::new().unwrap();
        let filename_id = SessionId::new();
        let header_id = SessionId::new();
        let path = directory.path().join(session_filename(filename_id));
        let mut contents = String::new();
        push_line(
            &mut contents,
            FileRecord::SessionHeader(SessionHeader::new(
                SessionIdentity::root(header_id),
                Some("mismatched".to_string()),
            )),
        )
        .unwrap();
        tokio::fs::write(path, contents).await.unwrap();
        let store = JsonlSessionStore::new(directory.path());

        let error = store.open(filename_id).await.unwrap_err();
        assert!(error.to_string().contains("does not match filename"));
        assert!(store.list_roots().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn hides_child_sessions_from_the_session_list() {
        let directory = TempDir::new().unwrap();
        let root_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());
        create_session(&store, root_id, &[LogEntry::Message(Message::user("root"))]).await;
        let child_id = create_child_session(
            &store,
            SessionIdentity::root(root_id),
            "research",
            &[LogEntry::Message(Message::user("subagent"))],
        )
        .await;

        let listed = store.list_roots().await.unwrap();
        assert!(listed.iter().any(|summary| summary.session_id == root_id));
        assert!(!listed.iter().any(|summary| summary.session_id == child_id));
    }

    #[tokio::test]
    async fn tree_lists_every_session_in_a_root_tree() {
        let directory = TempDir::new().unwrap();
        let root_id = SessionId::new();
        let other_root_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());
        create_session(&store, root_id, &[LogEntry::Message(Message::user("root"))]).await;
        let child_id = create_child_session(
            &store,
            SessionIdentity::root(root_id),
            "research",
            &[LogEntry::Message(Message::user("subagent"))],
        )
        .await;
        let grandchild_id = create_child_session(
            &store,
            SessionIdentity::root(root_id)
                .child(child_id, "research")
                .unwrap(),
            "scan",
            &[LogEntry::Message(Message::user("deep"))],
        )
        .await;
        create_session(
            &store,
            other_root_id,
            &[LogEntry::Message(Message::user("other"))],
        )
        .await;

        let tree = store.tree(root_id).await.unwrap();
        let tree_ids = tree
            .iter()
            .map(|summary| summary.session_id)
            .collect::<Vec<_>>();
        assert!(tree_ids.contains(&root_id));
        assert!(tree_ids.contains(&child_id));
        assert!(tree_ids.contains(&grandchild_id));
        assert!(!tree_ids.contains(&other_root_id));
    }

    #[tokio::test]
    async fn child_sessions_persist_and_restore_their_lineage() {
        let directory = TempDir::new().unwrap();
        let root_id = SessionId::new();
        let store = JsonlSessionStore::new(directory.path());
        let child_id = create_child_session(
            &store,
            SessionIdentity::root(root_id),
            "research",
            &[LogEntry::Message(Message::user("child work"))],
        )
        .await;

        let loaded = store.load(child_id).await.unwrap().unwrap();
        assert_eq!(loaded.identity.id, child_id);
        assert_eq!(loaded.identity.root_id, root_id);
        assert_eq!(loaded.identity.parent_id, Some(root_id));
        assert_eq!(loaded.identity.path.as_str(), "/root/research");
    }

    #[tokio::test]
    async fn rejects_a_header_without_lineage_fields() {
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new();
        let path = directory.path().join(session_filename(session_id));
        tokio::fs::write(
            &path,
            format!("{{\"type\":\"session_header\",\"session_id\":\"{session_id}\"}}\n"),
        )
        .await
        .unwrap();
        let store = JsonlSessionStore::new(directory.path());

        let error = store.open(session_id).await.unwrap_err();
        assert!(error.to_string().contains("missing its header"));
    }

    #[tokio::test]
    async fn writer_reuses_the_locked_file_handle() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let session_id = SessionId::new();
        let mut writer = store
            .open_new(SessionIdentity::root(session_id))
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
        assert!(directory.path().join(session_filename(session_id)).exists());
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

        let loaded = stored_log(&store, session_id).await;
        assert_eq!(loaded.messages().len(), 2);
    }

    #[test]
    fn default_session_dir_appends_sessions_to_the_platform_data_dir() {
        let data_dir = PathBuf::from("var/lib/ash");
        assert_eq!(
            session_dir_from_data_dir(Some(data_dir.clone())),
            data_dir.join("sessions")
        );
    }

    #[test]
    fn default_session_dir_falls_back_to_a_relative_dot_ash_dir() {
        assert_eq!(
            session_dir_from_data_dir(None),
            PathBuf::from(".ash").join("sessions")
        );
    }

    #[test]
    fn limits_titles_stored_in_the_header() {
        let title = "a".repeat(MAX_SESSION_TITLE_CHARS + 20);
        let shortened = session_title(&[Message::user(&title)]);

        assert_eq!(shortened.chars().count(), MAX_SESSION_TITLE_CHARS);
        assert!(shortened.ends_with("..."));
    }
}
