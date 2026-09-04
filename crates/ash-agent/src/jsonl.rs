use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use ash_core::{
    ash_data_dir, Conversation, SessionId, SessionIdentity, SessionSummary, StorageError, Turn,
};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::warn;

const MAX_SESSION_TITLE_CHARS: usize = 160;
const MAX_INIT_RECORD_BYTES: usize = 16 * 1024;
const UNTITLED_CHAT: &str = "Untitled chat";

#[derive(Debug)]
pub(crate) struct StoredSession {
    pub(crate) identity: SessionIdentity,
    pub(crate) conversation: Conversation,
}

#[derive(Debug)]
pub(crate) struct OpenedSession {
    pub(crate) session: StoredSession,
    pub(crate) writer: SessionWriter,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Record {
    Init {
        identity: SessionIdentity,
        created_at: String,
        title: String,
    },
    Turn {
        turn: Arc<Turn>,
        summary: Option<String>,
    },
    Checkpoint {
        summary: String,
    },
}

#[derive(Debug)]
pub(crate) struct SessionWriter {
    path: PathBuf,
    identity: SessionIdentity,
    file: Option<tokio::fs::File>,
    failed: bool,
}

impl SessionWriter {
    fn new(directory: &Path, identity: SessionIdentity) -> Self {
        Self {
            path: directory.join(session_filename(identity.id())),
            identity,
            file: None,
            failed: false,
        }
    }

    fn existing(path: PathBuf, identity: SessionIdentity, file: tokio::fs::File) -> Self {
        Self {
            path,
            identity,
            file: Some(file),
            failed: false,
        }
    }

    pub(crate) async fn commit_turn(
        &mut self,
        turn: Arc<Turn>,
        summary: Option<String>,
    ) -> Result<(), ash_core::AshError> {
        self.append(vec![Record::Turn { turn, summary }]).await
    }

    pub(crate) async fn checkpoint(&mut self, summary: String) -> Result<(), ash_core::AshError> {
        self.append(vec![Record::Checkpoint { summary }]).await
    }

    pub(crate) async fn seed(&mut self, turns: &[Arc<Turn>]) -> Result<(), ash_core::AshError> {
        self.append(
            turns
                .iter()
                .cloned()
                .map(|turn| Record::Turn {
                    turn,
                    summary: None,
                })
                .collect(),
        )
        .await
    }

    async fn append(&mut self, records: Vec<Record>) -> Result<(), ash_core::AshError> {
        if records.is_empty() {
            return Ok(());
        }
        if self.failed {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session writer is unusable after a failed append",
            ))
            .into());
        }
        let result = self.append_inner(records).await;
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    async fn append_inner(&mut self, records: Vec<Record>) -> Result<(), ash_core::AshError> {
        let is_new = self.file.is_none();
        let directory = if is_new {
            Some(
                self.path
                    .parent()
                    .ok_or_else(|| {
                        StorageError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "session path has no parent directory",
                        ))
                    })?
                    .to_path_buf(),
            )
        } else {
            None
        };
        let mut data = Vec::new();
        if is_new {
            let first_turn = records.iter().find_map(|record| match record {
                Record::Turn { turn, .. } => Some(turn.as_ref()),
                Record::Init { .. } | Record::Checkpoint { .. } => None,
            });
            let Some(first_turn) = first_turn else {
                return Err(corrupt("a new session must start with a turn"));
            };
            encode(
                &mut data,
                &Record::Init {
                    identity: self.identity,
                    created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                    title: turn_title(first_turn),
                },
            )?;
        }
        for record in records {
            encode(&mut data, &record)?;
        }

        if let Some(directory) = &directory {
            ensure_directory(directory).await?;
            self.file = Some(create_locked(&self.path).await?);
        }
        let file = self.file.as_mut().ok_or_else(|| {
            StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session file is not open",
            ))
        })?;
        file.write_all(&data).await.map_err(StorageError::from)?;
        file.flush().await.map_err(StorageError::from)?;
        file.sync_data().await.map_err(StorageError::from)?;
        if let Some(directory) = &directory {
            sync_directory(directory).await?;
        }
        Ok(())
    }
}

pub(crate) struct JsonlSessionStore {
    directory: PathBuf,
}

impl Default for JsonlSessionStore {
    fn default() -> Self {
        Self::new(ash_data_dir().join("sessions"))
    }
}

impl JsonlSessionStore {
    pub(crate) fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub(crate) async fn open_new(
        &self,
        identity: SessionIdentity,
    ) -> Result<SessionWriter, ash_core::AshError> {
        ensure_directory(&self.directory).await?;
        Ok(SessionWriter::new(&self.directory, identity))
    }

    pub(crate) async fn open(
        &self,
        session_id: SessionId,
    ) -> Result<Option<OpenedSession>, ash_core::AshError> {
        let path = self.directory.join(session_filename(session_id));
        if !tokio::fs::try_exists(&path)
            .await
            .map_err(StorageError::from)?
        {
            return Ok(None);
        }
        let mut file = open_locked(&path).await?;
        let (stored, valid_len) = replay_file(&mut file, &path).await?;
        if stored.identity.id() != session_id {
            return Err(StorageError::IdentityMismatch {
                expected: session_id,
                actual: stored.identity.id(),
            }
            .into());
        }
        let length = file.metadata().await.map_err(StorageError::from)?.len();
        if valid_len < length {
            file.set_len(valid_len).await.map_err(StorageError::from)?;
            file.sync_data().await.map_err(StorageError::from)?;
        }
        file.seek(std::io::SeekFrom::End(0))
            .await
            .map_err(StorageError::from)?;
        Ok(Some(OpenedSession {
            writer: SessionWriter::existing(path, stored.identity, file),
            session: stored,
        }))
    }

    #[cfg(test)]
    pub(crate) async fn load(
        &self,
        session_id: SessionId,
    ) -> Result<Option<StoredSession>, ash_core::AshError> {
        let path = self.directory.join(session_filename(session_id));
        if !tokio::fs::try_exists(&path)
            .await
            .map_err(StorageError::from)?
        {
            return Ok(None);
        }
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(StorageError::from)?;
        let (stored, _) = replay_file(&mut file, &path).await?;
        Ok(Some(stored))
    }

    pub(crate) async fn list_roots(&self) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        self.summaries(|identity| identity.is_root()).await
    }

    pub(crate) async fn tree(
        &self,
        root_id: SessionId,
    ) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        self.summaries(|identity| identity.root_id() == root_id)
            .await
    }

    pub(crate) async fn delete_tree(
        &self,
        root_id: SessionId,
    ) -> Result<usize, ash_core::AshError> {
        let root_path = self.directory.join(session_filename(root_id));
        if !tokio::fs::try_exists(&root_path)
            .await
            .map_err(StorageError::from)?
        {
            return Ok(0);
        }
        let mut paths = Vec::new();
        for path in self.session_paths().await? {
            let init = match read_init(&path).await {
                Ok(init) => init,
                Err(error) => {
                    warn!(path = %path.display(), %error, "skipping unreadable session");
                    continue;
                }
            };
            if init.identity.root_id() == root_id {
                paths.push(path);
            }
        }
        let root = read_init(&root_path).await?;
        if !root.identity.is_root() || root.identity.id() != root_id {
            return Err(StorageError::IdentityMismatch {
                expected: root_id,
                actual: root.identity.id(),
            }
            .into());
        }

        let mut locks = Vec::with_capacity(paths.len());
        for path in &paths {
            locks.push(open_locked(path).await?);
        }
        for path in &paths {
            tokio::fs::remove_file(path)
                .await
                .map_err(StorageError::from)?;
        }
        drop(locks);
        if !paths.is_empty() {
            sync_directory(&self.directory).await?;
        }
        Ok(paths.len())
    }

    async fn summaries(
        &self,
        includes: impl Fn(SessionIdentity) -> bool,
    ) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        let mut summaries = Vec::new();
        for path in self.session_paths().await? {
            match read_init(&path).await {
                Ok(init) if includes(init.identity) => summaries.push(SessionSummary {
                    session_id: init.identity.id(),
                    title: init.title,
                    created_at: display_created_at(&init.created_at),
                }),
                Ok(_) => {}
                Err(error) => warn!(path = %path.display(), %error, "skipping unreadable session"),
            }
        }
        Ok(summaries)
    }

    async fn session_paths(&self) -> Result<Vec<PathBuf>, ash_core::AshError> {
        let mut directory = match tokio::fs::read_dir(&self.directory).await {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(StorageError::Io(error).into()),
        };
        let mut paths = Vec::new();
        while let Some(entry) = directory.next_entry().await.map_err(StorageError::from)? {
            let path = entry.path();
            if canonical_session_id(&path).is_some() {
                let modified = entry
                    .metadata()
                    .await
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                paths.push((modified, path));
            }
        }
        paths.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        Ok(paths.into_iter().map(|(_, path)| path).collect())
    }
}

struct Init {
    identity: SessionIdentity,
    created_at: String,
    title: String,
}

async fn replay_file(
    file: &mut tokio::fs::File,
    path: &Path,
) -> Result<(StoredSession, u64), ash_core::AshError> {
    file.seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(StorageError::from)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .await
        .map_err(StorageError::from)?;
    let valid_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let complete = &bytes[..valid_len];
    let mut records = complete
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty());
    let first = records
        .next()
        .ok_or_else(|| corrupt_at(path, "missing Init record", None))?;
    let init = init_from_record(decode(first, path)?, path)?;
    let mut conversation = Conversation::new();
    let mut ids = HashSet::new();
    for line in records {
        match decode(line, path)? {
            Record::Init { .. } => {
                return Err(corrupt_at(
                    path,
                    "Init record appears after the first line",
                    None,
                ));
            }
            Record::Turn { turn, summary } => {
                if !ids.insert(turn.id) {
                    return Err(corrupt_at(path, "duplicate turn id", None));
                }
                validate_summary(summary.as_deref(), path)?;
                conversation.push(turn, summary);
            }
            Record::Checkpoint { summary } => {
                validate_summary(Some(&summary), path)?;
                conversation.compact(summary);
            }
        }
    }
    Ok((
        StoredSession {
            identity: init.identity,
            conversation,
        },
        u64::try_from(valid_len).unwrap_or(u64::MAX),
    ))
}

async fn read_init(path: &Path) -> Result<Init, ash_core::AshError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(StorageError::from)?;
    let mut reader = tokio::io::BufReader::new(file).take(
        u64::try_from(MAX_INIT_RECORD_BYTES)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    );
    let mut line = Vec::new();
    let bytes = reader
        .read_until(b'\n', &mut line)
        .await
        .map_err(StorageError::from)?;
    if bytes > MAX_INIT_RECORD_BYTES {
        return Err(corrupt_at(path, "Init record is too large", None));
    }
    if bytes == 0 || !line.ends_with(b"\n") {
        return Err(corrupt_at(path, "missing complete Init record", None));
    }
    line.pop();
    init_from_record(decode(&line, path)?, path)
}

fn init_from_record(record: Record, path: &Path) -> Result<Init, ash_core::AshError> {
    match record {
        Record::Init {
            identity,
            created_at,
            title,
        } if !created_at.trim().is_empty() && !title.trim().is_empty() => Ok(Init {
            identity,
            created_at,
            title,
        }),
        Record::Init { .. } => Err(corrupt_at(path, "Init fields cannot be empty", None)),
        Record::Turn { .. } | Record::Checkpoint { .. } => {
            Err(corrupt_at(path, "first record is not Init", None))
        }
    }
}

fn validate_summary(summary: Option<&str>, path: &Path) -> Result<(), ash_core::AshError> {
    if summary.is_some_and(|summary| summary.trim().is_empty()) {
        return Err(corrupt_at(path, "checkpoint summary cannot be empty", None));
    }
    Ok(())
}

fn encode(data: &mut Vec<u8>, record: &Record) -> Result<(), ash_core::AshError> {
    serde_json::to_writer(&mut *data, record)
        .map_err(|error| corrupt_with_source("cannot encode session record", error))?;
    data.push(b'\n');
    Ok(())
}

fn decode(line: &[u8], path: &Path) -> Result<Record, ash_core::AshError> {
    serde_json::from_slice(line).map_err(|error| {
        corrupt_at(
            path,
            "invalid complete session record",
            Some(Box::new(error)),
        )
    })
}

fn corrupt(message: impl Into<String>) -> ash_core::AshError {
    StorageError::Corrupt {
        message: message.into(),
        source: None,
    }
    .into()
}

fn corrupt_with_source(
    message: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ash_core::AshError {
    StorageError::Corrupt {
        message: message.into(),
        source: Some(Box::new(source)),
    }
    .into()
}

fn corrupt_at(
    path: &Path,
    message: &str,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
) -> ash_core::AshError {
    StorageError::Corrupt {
        message: format!("{}: {message}", path.display()),
        source,
    }
    .into()
}

async fn ensure_directory(path: &Path) -> Result<(), ash_core::AshError> {
    if tokio::fs::try_exists(path)
        .await
        .map_err(StorageError::from)?
    {
        return Ok(());
    }
    tokio::fs::create_dir_all(path)
        .await
        .map_err(StorageError::from)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent).await?;
    }
    Ok(())
}

async fn sync_directory(path: &Path) -> Result<(), ash_core::AshError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
        .await
        .map_err(|error| {
            StorageError::Io(std::io::Error::other(format!(
                "directory sync task failed: {error}"
            )))
        })?
        .map_err(StorageError::from)?;
    Ok(())
}

async fn create_locked(path: &Path) -> Result<tokio::fs::File, ash_core::AshError> {
    locked(path, true).await
}

async fn open_locked(path: &Path) -> Result<tokio::fs::File, ash_core::AshError> {
    locked(path, false).await
}

async fn locked(path: &Path, create_new: bool) -> Result<tokio::fs::File, ash_core::AshError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).append(true);
    if create_new {
        options.create_new(true);
    }
    let file = options.open(path).map_err(StorageError::from)?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => StorageError::AlreadyOpen {
            path: path.display().to_string(),
        },
        std::fs::TryLockError::Error(error) => StorageError::Io(error),
    })?;
    Ok(tokio::fs::File::from_std(file))
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

fn turn_title(turn: &Turn) -> String {
    shorten_title(turn.input.title().unwrap_or(UNTITLED_CHAT))
}

fn shorten_title(title: &str) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = if title.is_empty() {
        UNTITLED_CHAT
    } else {
        &title
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use ash_core::{
        Content, Item, Step, StopReason, ToolCall, ToolCallId, ToolOutput, TurnResult, TurnStats,
    };
    use tempfile::TempDir;

    fn turn(id: u128, input: &str) -> Arc<Turn> {
        Arc::new(Turn {
            id: ash_core::TurnId::from_u128(id),
            input: input.into(),
            steps: Vec::new(),
            result: TurnResult::Stopped(StopReason::EndTurn),
            stats: TurnStats::default(),
        })
    }

    fn tool_turn(id: u128) -> Arc<Turn> {
        Arc::new(Turn {
            id: ash_core::TurnId::from_u128(id),
            input: "inspect".into(),
            steps: vec![Step {
                items: vec![Item::ToolCall(ToolCall {
                    id: ToolCallId::from_provider("call"),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                    result: Ok("done".into()),
                })],
            }],
            result: TurnResult::Stopped(StopReason::EndTurn),
            stats: TurnStats::default(),
        })
    }

    #[tokio::test]
    async fn root_commit_is_init_then_turn() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let id = SessionId::new();
        let mut writer = store.open_new(SessionIdentity::root(id)).await.unwrap();

        writer.commit_turn(turn(1, "hello"), None).await.unwrap();

        let data = tokio::fs::read_to_string(directory.path().join(session_filename(id)))
            .await
            .unwrap();
        let lines = data.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        let init = serde_json::from_str::<serde_json::Value>(lines[0]).unwrap();
        assert_eq!(init["type"], "init");
        assert_eq!(init["title"], "hello");
        assert!(init.get("turn").is_none());
        assert!(init.get("turns").is_none());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[1]).unwrap()["type"],
            "turn"
        );
    }

    #[tokio::test]
    async fn turn_summary_replays_at_the_turn_boundary() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let id = SessionId::new();
        let mut writer = store.open_new(SessionIdentity::root(id)).await.unwrap();
        writer.commit_turn(turn(1, "first"), None).await.unwrap();
        let second = turn(2, "second");
        writer
            .commit_turn(Arc::clone(&second), Some("first summary".to_string()))
            .await
            .unwrap();
        drop(writer);

        let stored = store.load(id).await.unwrap().unwrap();
        let context = stored.conversation.context();
        assert_eq!(context.summary(), Some("first summary"));
        assert_eq!(context.turns(), &[second]);
        assert_eq!(stored.conversation.turns().len(), 2);
    }

    #[tokio::test]
    async fn turn_round_trip_preserves_structured_content() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let id = SessionId::new();
        let expected = Arc::new(Turn {
            id: ash_core::TurnId::from_u128(1),
            input: ash_core::Input {
                content: vec![
                    Content::Text("inspect".to_string()),
                    Content::Image {
                        media_type: "image/png".to_string(),
                        data: vec![1, 2, 3],
                    },
                ],
            },
            steps: vec![Step {
                items: vec![
                    Item::Thought {
                        text: "reasoning".to_string(),
                        elapsed_seconds: 2,
                    },
                    Item::ToolCall(ToolCall {
                        id: ToolCallId::from_provider("call"),
                        name: "read".to_string(),
                        arguments: serde_json::json!({"path": "file.png"}),
                        result: Ok(ToolOutput::with_attachments(
                            "done",
                            vec![Content::Image {
                                media_type: "image/png".to_string(),
                                data: vec![4, 5, 6],
                            }],
                        )),
                    }),
                    Item::Text("answer".to_string()),
                ],
            }],
            result: TurnResult::Stopped(StopReason::EndTurn),
            stats: TurnStats {
                input_tokens: 10,
                output_tokens: 5,
                generation_ms: 20,
            },
        });
        let mut writer = store.open_new(SessionIdentity::root(id)).await.unwrap();
        writer
            .commit_turn(Arc::clone(&expected), None)
            .await
            .unwrap();
        drop(writer);

        let stored = store.load(id).await.unwrap().unwrap();

        assert_eq!(stored.conversation.turns(), &[expected]);
    }

    #[tokio::test]
    async fn new_records_omit_the_tool_count_and_legacy_records_still_load() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let id = SessionId::new();
        let path = directory.path().join(session_filename(id));
        let mut writer = store.open_new(SessionIdentity::root(id)).await.unwrap();
        writer.commit_turn(tool_turn(1), None).await.unwrap();
        drop(writer);

        let data = tokio::fs::read_to_string(&path).await.unwrap();
        let mut records = data
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(records[1]["turn"]["stats"].get("tool_calls").is_none());
        // Simulate a legacy record carrying the removed derived field.
        records[1]["turn"]["stats"]["tool_calls"] = serde_json::json!(99);
        let data = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        tokio::fs::write(&path, data).await.unwrap();

        let stored = store.load(id).await.unwrap().unwrap();

        let turn = &stored.conversation.turns()[0];
        assert_eq!(turn.completed_tool_calls(), 1, "steps are canonical");
    }

    #[tokio::test]
    async fn checkpoint_replays_after_all_existing_turns() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let id = SessionId::new();
        let mut writer = store.open_new(SessionIdentity::root(id)).await.unwrap();
        writer.commit_turn(turn(1, "first"), None).await.unwrap();
        writer.checkpoint("all turns".to_string()).await.unwrap();
        drop(writer);

        let stored = store.load(id).await.unwrap().unwrap();
        let context = stored.conversation.context();
        assert_eq!(context.summary(), Some("all turns"));
        assert!(context.turns().is_empty());
    }

    #[tokio::test]
    async fn seed_writes_independent_turn_records_without_summaries() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let id = SessionId::new();
        let mut writer = store.open_new(SessionIdentity::root(id)).await.unwrap();
        writer
            .seed(&[turn(1, "first"), turn(2, "second")])
            .await
            .unwrap();
        drop(writer);

        let data = tokio::fs::read_to_string(directory.path().join(session_filename(id)))
            .await
            .unwrap();
        let records = data
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["title"], "first");
        assert_eq!(records[1]["summary"], serde_json::Value::Null);
        assert_eq!(records[2]["summary"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn incomplete_tail_is_truncated_on_open() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let id = SessionId::new();
        let mut writer = store.open_new(SessionIdentity::root(id)).await.unwrap();
        writer.commit_turn(turn(1, "hello"), None).await.unwrap();
        drop(writer);
        let path = directory.path().join(session_filename(id));
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(b"{\"type\":").await.unwrap();
        drop(file);

        let opened = store.open(id).await.unwrap().unwrap();
        drop(opened);

        assert!(tokio::fs::read(&path).await.unwrap().ends_with(b"\n"));
    }

    #[tokio::test]
    async fn complete_invalid_record_is_corrupt() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let id = SessionId::new();
        let mut writer = store.open_new(SessionIdentity::root(id)).await.unwrap();
        writer.commit_turn(turn(1, "hello"), None).await.unwrap();
        drop(writer);
        let path = directory.path().join(session_filename(id));
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .await
            .unwrap();
        file.write_all(b"not-json\n").await.unwrap();

        assert!(matches!(
            store.open(id).await,
            Err(ash_core::AshError::Storage(StorageError::Corrupt { .. }))
        ));
    }

    #[tokio::test]
    async fn duplicate_turn_id_is_corrupt() {
        let directory = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(directory.path());
        let id = SessionId::new();
        let mut writer = store.open_new(SessionIdentity::root(id)).await.unwrap();
        writer.commit_turn(turn(1, "first"), None).await.unwrap();
        writer.commit_turn(turn(1, "again"), None).await.unwrap();
        drop(writer);

        assert!(matches!(
            store.open(id).await,
            Err(ash_core::AshError::Storage(StorageError::Corrupt { .. }))
        ));
    }

    #[tokio::test]
    async fn init_record_has_a_fixed_read_bound() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(session_filename(SessionId::new()));
        tokio::fs::write(&path, vec![b'x'; MAX_INIT_RECORD_BYTES + 1])
            .await
            .unwrap();

        assert!(matches!(
            read_init(&path).await,
            Err(ash_core::AshError::Storage(StorageError::Corrupt { .. }))
        ));
    }
}
