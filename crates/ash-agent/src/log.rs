use ash_core::{Message, MessageId, ThreadView, TurnId, TurnResult, TurnView, Usage};
use serde::{Deserialize, Deserializer, Serialize};

use crate::Input;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextCheckpoint {
    pub summary: Message,
    pub tail_start_id: Option<MessageId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AcceptedInput {
    pub turn_id: TurnId,
    pub input: Input,
    pub message: Message,
}

impl ContextCheckpoint {
    pub fn from_model_context(messages: &[Message]) -> Result<Self, ash_core::AshError> {
        let summary = messages.first().cloned().ok_or_else(|| {
            ash_core::AshError::Config("compacted context has no summary message".to_string())
        })?;
        Ok(Self {
            summary,
            tail_start_id: messages.get(1).map(|message| message.id),
        })
    }
}

/// One durable, append-only history entry. This is the only persisted truth;
/// full history, model context, turn views, and UI display are all projected
/// from it. Live deltas never appear here.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum LogEntry {
    TurnStart(TurnId),
    Input(AcceptedInput),
    Message(Message),
    Checkpoint(ContextCheckpoint),
    TurnEnd {
        id: TurnId,
        result: TurnResult,
        usage: Option<Usage>,
    },
    Rollback,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ThreadLog {
    entries: Vec<LogEntry>,
    #[serde(skip)]
    projection: Projector,
}

#[derive(Clone, Debug, Default)]
struct Projector {
    all: Vec<Message>,
    history: Vec<Message>,
    context: Vec<Message>,
    turns: Vec<TurnView>,
    open: Option<OpenTurn>,
    checkpoint: Option<AppliedCheckpoint>,
    turn_boundaries: Vec<ProjectionSnapshot>,
}

#[derive(Clone, Debug)]
struct OpenTurn {
    id: TurnId,
    messages: Vec<Message>,
    events: Vec<OpenEvent>,
    all_start: usize,
    history_start: usize,
    context_start: Vec<Message>,
    checkpoint_start: Option<AppliedCheckpoint>,
}

#[derive(Clone, Debug)]
enum OpenEvent {
    Message(Message),
    Checkpoint(ContextCheckpoint),
}

#[derive(Clone, Debug)]
struct ProjectionSnapshot {
    all_len: usize,
    history_len: usize,
    turns_len: usize,
    open: Option<OpenTurn>,
    checkpoint: Option<AppliedCheckpoint>,
    open_context: Option<Vec<Message>>,
}

#[derive(Clone, Debug)]
struct AppliedCheckpoint {
    value: ContextCheckpoint,
    all_len: usize,
}

impl<'de> Deserialize<'de> for ThreadLog {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredLog {
            entries: Vec<LogEntry>,
        }

        let stored = StoredLog::deserialize(deserializer)?;
        Ok(Self::from_entries(stored.entries))
    }
}

impl ThreadLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: impl IntoIterator<Item = Message>) -> Self {
        Self::from_entries(messages.into_iter().map(LogEntry::Message).collect())
    }

    pub(crate) fn from_entries(entries: Vec<LogEntry>) -> Self {
        let mut projection = Projector::default();
        for entry in &entries {
            projection.apply(entry);
        }
        Self {
            entries,
            projection,
        }
    }

    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    pub fn contains_idempotency_key(&self, key: &str) -> bool {
        self.entries.iter().any(|entry| {
            matches!(
                entry,
                LogEntry::Input(AcceptedInput { input, .. })
                    if input.idempotency_key.as_deref() == Some(key)
            )
        })
    }

    pub fn push(&mut self, entry: LogEntry) {
        self.projection.apply(&entry);
        self.entries.push(entry);
    }

    /// Full user-visible history, excluding messages removed by rollback and
    /// partial messages from a turn that never settled (e.g. after a crash):
    /// its accepted inputs stay visible, but half-streamed assistant and tool
    /// messages are not exposed as normal history.
    pub fn messages(&self) -> Vec<Message> {
        self.projection.history.clone()
    }

    /// The model context for the next request, honoring checkpoints. Messages
    /// of a turn that never settled are excluded (except its accepted inputs)
    /// so the model never resumes from a half-streamed fact.
    pub fn model_context(&self) -> Vec<Message> {
        self.projection.context.clone()
    }

    /// Completed turn snapshots in log order. A turn left open when the
    /// session ended (no `TurnEnd`, e.g. after a crash) is projected as
    /// `Interrupted` so partial turns never masquerade as normal history.
    pub fn turns(&self) -> Vec<TurnView> {
        self.projection.turns()
    }

    /// Messages belonging to one turn: accepted inputs plus model and tool
    /// messages recorded under that turn id.
    pub fn turn_messages(&self, turn_id: TurnId) -> Vec<Message> {
        self.turn_view(turn_id)
            .map(|turn| turn.messages)
            .unwrap_or_default()
    }

    pub(crate) fn turn_view(&self, turn_id: TurnId) -> Option<TurnView> {
        self.projection.turn_view(turn_id)
    }

    /// Full projected state: history, model context, and turn views.
    pub fn view(&self) -> ThreadView {
        ThreadView {
            messages: self.projection.history.clone(),
            context: self.projection.context.clone(),
            turns: self.projection.turns(),
            context_tokens: None,
        }
    }
}

impl Projector {
    fn apply(&mut self, entry: &LogEntry) {
        match entry {
            LogEntry::TurnStart(id) => {
                let boundary = self.snapshot();
                self.turn_boundaries.push(boundary);
                if let Some(previous) = self.open.take() {
                    self.turns
                        .push(interrupted_turn(previous.id, previous.messages));
                }
                self.open = Some(OpenTurn {
                    id: *id,
                    messages: Vec::new(),
                    events: Vec::new(),
                    all_start: self.all.len(),
                    history_start: self.history.len(),
                    context_start: self.context.clone(),
                    checkpoint_start: self.checkpoint.clone(),
                });
            }
            LogEntry::Input(input) => {
                self.push_message(input.message.clone(), true);
            }
            LogEntry::Message(message) => {
                self.push_message(message.clone(), message.is_user_turn());
            }
            LogEntry::Checkpoint(checkpoint) => {
                self.context = apply_checkpoint(&self.all, checkpoint);
                self.checkpoint = Some(AppliedCheckpoint {
                    value: checkpoint.clone(),
                    all_len: self.all.len(),
                });
                if let Some(open) = self.open.as_mut() {
                    open.events.push(OpenEvent::Checkpoint(checkpoint.clone()));
                }
            }
            LogEntry::TurnEnd { result, usage, .. } => {
                if let Some(open) = self.open.take() {
                    self.settle(&open);
                    self.turns.push(TurnView {
                        id: open.id,
                        result: result.clone(),
                        messages: open.messages,
                        usage: *usage,
                        context_tokens: None,
                    });
                }
            }
            LogEntry::Rollback => self.rollback(),
        }
    }

    fn push_message(&mut self, message: Message, visible_while_open: bool) {
        self.all.push(message.clone());
        if self.open.is_none() || visible_while_open {
            self.history.push(message.clone());
            self.context.push(message.clone());
        }
        if let Some(open) = self.open.as_mut() {
            open.messages.push(message.clone());
            open.events.push(OpenEvent::Message(message));
        }
    }

    fn settle(&mut self, open: &OpenTurn) {
        self.history.truncate(open.history_start);
        self.history.extend(open.messages.iter().cloned());

        let mut all = self.all[..open.all_start].to_vec();
        let mut context = open.context_start.clone();
        let mut latest_checkpoint = open.checkpoint_start.clone();
        for event in &open.events {
            match event {
                OpenEvent::Message(message) => {
                    all.push(message.clone());
                    context.push(message.clone());
                }
                OpenEvent::Checkpoint(checkpoint) => {
                    context = apply_checkpoint(&all, checkpoint);
                    latest_checkpoint = Some(AppliedCheckpoint {
                        value: checkpoint.clone(),
                        all_len: all.len(),
                    });
                }
            }
        }
        self.context = context;
        self.checkpoint = latest_checkpoint;
    }

    fn rollback(&mut self) {
        if let Some(boundary) = self.turn_boundaries.pop() {
            self.restore(boundary);
        }
    }

    fn snapshot(&self) -> ProjectionSnapshot {
        ProjectionSnapshot {
            all_len: self.all.len(),
            history_len: self.history.len(),
            turns_len: self.turns.len(),
            open: self.open.clone(),
            checkpoint: self.checkpoint.clone(),
            open_context: self.open.as_ref().map(|_| self.context.clone()),
        }
    }

    fn restore(&mut self, boundary: ProjectionSnapshot) {
        self.all.truncate(boundary.all_len);
        self.history.truncate(boundary.history_len);
        self.turns.truncate(boundary.turns_len);
        self.open = boundary.open;
        self.checkpoint = boundary.checkpoint;
        self.context = boundary
            .open_context
            .unwrap_or_else(|| rebuild_context(&self.all, self.checkpoint.as_ref()));
    }

    fn turns(&self) -> Vec<TurnView> {
        let mut turns = self.turns.clone();
        if let Some(open) = &self.open {
            turns.push(interrupted_turn(open.id, open.messages.clone()));
        }
        turns
    }

    fn turn_view(&self, turn_id: TurnId) -> Option<TurnView> {
        if let Some(open) = &self.open {
            if open.id == turn_id {
                return Some(interrupted_turn(open.id, open.messages.clone()));
            }
        }
        self.turns
            .iter()
            .rev()
            .find(|turn| turn.id == turn_id)
            .cloned()
    }
}

fn rebuild_context(all: &[Message], checkpoint: Option<&AppliedCheckpoint>) -> Vec<Message> {
    let Some(checkpoint) = checkpoint else {
        return all.to_vec();
    };
    let applied_at = checkpoint.all_len.min(all.len());
    let mut context = apply_checkpoint(&all[..applied_at], &checkpoint.value);
    context.extend_from_slice(&all[applied_at..]);
    context
}

fn interrupted_turn(id: TurnId, messages: Vec<Message>) -> TurnView {
    TurnView {
        id,
        result: TurnResult::Interrupted("turn left open when the session ended".to_string()),
        messages,
        usage: None,
        context_tokens: None,
    }
}

fn apply_checkpoint(messages: &[Message], checkpoint: &ContextCheckpoint) -> Vec<Message> {
    let mut model_context = vec![checkpoint.summary.clone()];
    let Some(tail_start_id) = checkpoint.tail_start_id else {
        return model_context;
    };
    let Some(tail_start) = messages
        .iter()
        .position(|message| message.id == tail_start_id)
    else {
        return model_context;
    };
    model_context.extend_from_slice(&messages[tail_start..]);
    model_context
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use ash_core::{MessageContent, StopReason};

    fn reference_projection(entries: &[LogEntry]) -> ThreadView {
        let mut active = Vec::new();
        for entry in entries {
            if matches!(entry, LogEntry::Rollback) {
                reference_rollback(&mut active);
            } else {
                active.push(entry);
            }
        }

        let mut open_ids = HashSet::new();
        let mut current = None;
        for entry in &active {
            match entry {
                LogEntry::TurnStart(id) => {
                    if let Some(previous) = current.replace(*id) {
                        open_ids.insert(previous);
                    }
                }
                LogEntry::TurnEnd { id, .. } if current == Some(*id) => current = None,
                _ => {}
            }
        }
        if let Some(id) = current {
            open_ids.insert(id);
        }

        let mut all = Vec::new();
        let mut messages = Vec::new();
        let mut context = Vec::new();
        let mut turns = Vec::new();
        let mut current = None;
        let mut open: Option<(TurnId, Vec<Message>)> = None;
        for entry in active {
            match entry {
                LogEntry::TurnStart(id) => {
                    if let Some((id, messages)) = open.take() {
                        turns.push(interrupted_turn(id, messages));
                    }
                    current = Some(*id);
                    open = Some((*id, Vec::new()));
                }
                LogEntry::Input(input) => {
                    all.push(input.message.clone());
                    messages.push(input.message.clone());
                    context.push(input.message.clone());
                    if let Some((_, turn_messages)) = open.as_mut() {
                        turn_messages.push(input.message.clone());
                    }
                }
                LogEntry::Message(message) => {
                    all.push(message.clone());
                    let partial =
                        current.is_some_and(|id| open_ids.contains(&id) && !message.is_user_turn());
                    if !partial {
                        messages.push(message.clone());
                        context.push(message.clone());
                    }
                    if let Some((_, turn_messages)) = open.as_mut() {
                        turn_messages.push(message.clone());
                    }
                }
                LogEntry::Checkpoint(checkpoint) => {
                    context = apply_checkpoint(&all, checkpoint);
                }
                LogEntry::TurnEnd { id, result, usage } => {
                    if current == Some(*id) {
                        current = None;
                    }
                    if let Some((open_id, messages)) = open.take() {
                        turns.push(TurnView {
                            id: open_id,
                            result: result.clone(),
                            messages,
                            usage: *usage,
                            context_tokens: None,
                        });
                    }
                }
                LogEntry::Rollback => unreachable!(),
            }
        }
        if let Some((id, messages)) = open {
            turns.push(interrupted_turn(id, messages));
        }
        ThreadView {
            messages,
            context,
            turns,
            context_tokens: None,
        }
    }

    fn reference_rollback(entries: &mut Vec<&LogEntry>) {
        if let Some(start) = entries
            .iter()
            .rposition(|entry| matches!(entry, LogEntry::TurnStart(_)))
        {
            entries.truncate(start);
        }
    }

    fn accepted(turn_id: TurnId, text: &str) -> LogEntry {
        LogEntry::Input(AcceptedInput {
            turn_id,
            input: Input::user(text),
            message: Message::user(text),
        })
    }

    #[test]
    fn projects_full_history_and_compacted_model_context_from_one_log() {
        let first = Message::user("first");
        let answer = Message::assistant_text("answer");
        let recent = Message::user("recent");
        let summary = Message::system("summary");
        let mut log = ThreadLog::from_messages([first.clone(), answer.clone(), recent.clone()]);
        log.push(LogEntry::Checkpoint(ContextCheckpoint {
            summary: summary.clone(),
            tail_start_id: Some(recent.id),
        }));

        assert_eq!(
            log.messages()
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [first.id, answer.id, recent.id]
        );
        assert_eq!(
            log.model_context()
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [summary.id, recent.id]
        );
    }

    #[test]
    fn rollback_is_an_entry_that_changes_both_projections() {
        let first = Message::user("first");
        let answer = Message::assistant_text("answer");
        let second = Message::user("second");
        let mut log = ThreadLog::new();
        log.push(LogEntry::TurnStart(TurnId::new()));
        log.push(LogEntry::Message(first.clone()));
        log.push(LogEntry::Message(answer.clone()));
        log.push(LogEntry::TurnStart(TurnId::new()));
        log.push(LogEntry::Message(second));
        log.push(LogEntry::Message(Message::assistant_text("second answer")));
        log.push(LogEntry::Rollback);

        assert_eq!(log.entries().len(), 7);
        // history keeps only user messages; the assistant reply lives in the
        // settled turn, which rollback truncates away with the second turn.
        assert_eq!(
            log.messages()
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [first.id]
        );
        assert_eq!(log.model_context().len(), 1);
    }

    #[test]
    fn input_entries_preserve_trigger_metadata_and_idempotency() {
        let mut input = Input::from_text(crate::InputSource::Heartbeat, "check health");
        input.idempotency_key = Some("heartbeat:42".to_string());
        input
            .metadata
            .insert("source".to_string(), serde_json::json!("scheduler"));
        let message = Message::user_content(input.content.clone());
        let mut log = ThreadLog::new();
        log.push(LogEntry::Input(AcceptedInput {
            turn_id: TurnId::new(),
            input,
            message: message.clone(),
        }));

        assert!(log.contains_idempotency_key("heartbeat:42"));
        assert_eq!(log.messages()[0].id, message.id);
        assert_eq!(log.model_context()[0].id, message.id);
    }

    #[test]
    fn projects_one_turn_view_per_settled_turn() {
        let turn_id = TurnId::new();
        let mut log = ThreadLog::new();
        log.push(LogEntry::TurnStart(turn_id));
        log.push(LogEntry::Message(Message::user("question")));
        log.push(LogEntry::Message(Message::assistant_text("answer")));
        log.push(LogEntry::TurnEnd {
            id: turn_id,
            result: TurnResult::Completed(StopReason::EndTurn),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                generation_ms: 300,
                estimated: false,
            }),
        });

        let turns = log.turns();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].id, turn_id);
        assert!(matches!(
            turns[0].result,
            TurnResult::Completed(StopReason::EndTurn)
        ));
        assert_eq!(turns[0].messages.len(), 2);
        assert_eq!(turns[0].usage.unwrap().input_tokens, 10);
        assert_eq!(log.turn_messages(turn_id).len(), 2);
    }

    #[test]
    fn open_turn_is_projected_as_interrupted() {
        let turn_id = TurnId::new();
        let mut log = ThreadLog::new();
        log.push(LogEntry::TurnStart(turn_id));
        log.push(LogEntry::Message(Message::user("question")));
        log.push(LogEntry::Message(Message::assistant_text("partial answer")));

        let turns = log.turns();
        assert_eq!(turns.len(), 1);
        assert!(matches!(turns[0].result, TurnResult::Interrupted(_)));
        assert_eq!(turns[0].messages.len(), 2);
        assert_eq!(log.view().turns.len(), 1);
    }

    #[test]
    fn settled_turn_keeps_all_of_its_messages() {
        let turn_id = TurnId::new();
        let mut log = ThreadLog::new();
        log.push(LogEntry::TurnStart(turn_id));
        log.push(LogEntry::Message(Message::user("question")));
        log.push(LogEntry::Message(Message::assistant_text("answer")));
        log.push(LogEntry::TurnEnd {
            id: turn_id,
            result: TurnResult::Completed(StopReason::EndTurn),
            usage: None,
        });

        // A settled turn's assistant messages are normal history.
        assert_eq!(log.messages().len(), 2);
        assert_eq!(log.model_context().len(), 2);
    }

    #[test]
    fn open_turn_partial_messages_stay_out_of_history_and_context() {
        let turn_id = TurnId::new();
        let mut log = ThreadLog::new();
        log.push(LogEntry::TurnStart(turn_id));
        log.push(LogEntry::Message(Message::user("question")));
        log.push(LogEntry::Message(Message::assistant_text("partial answer")));
        log.push(LogEntry::Message(Message::assistant_text("more partial")));

        // The accepted input stays visible; half-streamed assistant messages
        // are not exposed as normal history or model context.
        let messages = log.messages();
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0].content, MessageContent::User(_)));
        assert_eq!(log.model_context().len(), 1);
        // The turn snapshot keeps the full audit trail.
        assert_eq!(log.turns()[0].messages.len(), 3);
    }

    #[test]
    fn rollback_drops_the_open_turn_before_it_settles() {
        let turn_id = TurnId::new();
        let mut log = ThreadLog::new();
        log.push(LogEntry::TurnStart(turn_id));
        log.push(LogEntry::Message(Message::user("question")));
        log.push(LogEntry::Message(Message::assistant_text("partial")));
        log.push(LogEntry::Rollback);

        assert!(log.turns().is_empty());
        assert!(log.messages().is_empty());
    }

    #[test]
    fn rollback_drops_every_input_in_the_latest_turn() {
        let turn_id = TurnId::new();
        let first_input = Message::user("note");
        let second_input = Message::user("task");
        let mut log = ThreadLog::new();
        log.push(LogEntry::TurnStart(turn_id));
        log.push(LogEntry::Input(AcceptedInput {
            turn_id,
            input: Input::user("note"),
            message: first_input,
        }));
        log.push(LogEntry::Input(AcceptedInput {
            turn_id,
            input: Input::user("task"),
            message: second_input,
        }));
        log.push(LogEntry::Message(Message::assistant_text("done")));
        log.push(LogEntry::TurnEnd {
            id: turn_id,
            result: TurnResult::Completed(StopReason::EndTurn),
            usage: None,
        });
        log.push(LogEntry::Rollback);

        assert!(log.turns().is_empty());
        assert!(log.messages().is_empty());
        assert!(log.model_context().is_empty());
    }

    #[test]
    fn incremental_projection_matches_the_previous_projection_rules() {
        let first = TurnId::new();
        let second = TurnId::new();
        let first_input = accepted(first, "first");
        let tail_id = match &first_input {
            LogEntry::Input(input) => input.message.id,
            _ => unreachable!(),
        };
        let entries = vec![
            LogEntry::TurnStart(first),
            first_input,
            LogEntry::Message(Message::assistant_text("partial")),
            LogEntry::Checkpoint(ContextCheckpoint {
                summary: Message::system("summary"),
                tail_start_id: Some(tail_id),
            }),
            LogEntry::Message(Message::assistant_text("settled")),
            LogEntry::TurnEnd {
                id: first,
                result: TurnResult::Completed(StopReason::EndTurn),
                usage: None,
            },
            LogEntry::TurnStart(second),
            accepted(second, "second"),
            LogEntry::Message(Message::assistant_text("unfinished")),
            LogEntry::Rollback,
            LogEntry::TurnStart(second),
            accepted(second, "replacement"),
            LogEntry::Message(Message::assistant_text("replacement answer")),
            LogEntry::TurnEnd {
                id: second,
                result: TurnResult::Completed(StopReason::MaxTokens),
                usage: None,
            },
        ];

        let mut incremental = ThreadLog::new();
        for (index, entry) in entries.iter().cloned().enumerate() {
            incremental.push(entry);
            assert_eq!(
                incremental.view(),
                reference_projection(&entries[..=index]),
                "projection diverged after entry {index}"
            );
        }
    }

    #[test]
    fn rollback_after_mid_turn_checkpoint_restores_previous_context() {
        let original = Message::user("original");
        let turn_id = TurnId::new();
        let mut log = ThreadLog::from_messages([original.clone()]);
        log.push(LogEntry::TurnStart(turn_id));
        log.push(accepted(turn_id, "new task"));
        log.push(LogEntry::Checkpoint(ContextCheckpoint {
            summary: Message::system("temporary summary"),
            tail_start_id: None,
        }));
        log.push(LogEntry::Rollback);

        assert_eq!(log.model_context(), vec![original]);
        assert!(log.turns().is_empty());
    }

    #[test]
    fn consecutive_rollbacks_restore_each_turn_boundary() {
        let first = TurnId::new();
        let second = TurnId::new();
        let mut log = ThreadLog::new();
        for (turn_id, prompt, answer) in [
            (first, "first", "first answer"),
            (second, "second", "second answer"),
        ] {
            log.push(LogEntry::TurnStart(turn_id));
            log.push(accepted(turn_id, prompt));
            log.push(LogEntry::Message(Message::assistant_text(answer)));
            log.push(LogEntry::TurnEnd {
                id: turn_id,
                result: TurnResult::Completed(StopReason::EndTurn),
                usage: None,
            });
        }

        log.push(LogEntry::Rollback);
        assert_eq!(log.turns().len(), 1);
        assert_eq!(log.turns()[0].id, first);

        log.push(LogEntry::Rollback);
        assert!(log.turns().is_empty());
        assert!(log.messages().is_empty());
    }

    #[test]
    fn deserialization_rebuilds_the_incremental_projection() {
        let turn_id = TurnId::new();
        let mut log = ThreadLog::new();
        log.push(LogEntry::TurnStart(turn_id));
        log.push(accepted(turn_id, "question"));
        log.push(LogEntry::Message(Message::assistant_text("answer")));
        log.push(LogEntry::TurnEnd {
            id: turn_id,
            result: TurnResult::Completed(StopReason::EndTurn),
            usage: None,
        });

        let restored: ThreadLog =
            serde_json::from_value(serde_json::to_value(&log).unwrap()).unwrap();
        assert_eq!(restored.view(), log.view());
    }
}
