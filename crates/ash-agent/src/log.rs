use ash_core::{
    Message, MessageContent, MessageId, ThreadView, TurnId, TurnResult, TurnView, Usage,
};
use serde::{Deserialize, Serialize};

use crate::{Input, Version};

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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThreadLog {
    entries: Vec<LogEntry>,
}

impl ThreadLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: impl IntoIterator<Item = Message>) -> Self {
        Self {
            entries: messages.into_iter().map(LogEntry::Message).collect(),
        }
    }

    pub(crate) fn from_entries(entries: Vec<LogEntry>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    pub fn revision(&self) -> Version {
        Version::from_entry_count(self.entries.len())
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
        self.entries.push(entry);
    }

    /// Full user-visible history, excluding messages removed by rollback.
    pub fn messages(&self) -> Vec<Message> {
        active_entries(&self.entries)
            .into_iter()
            .filter_map(|entry| match entry {
                LogEntry::Input(input) => Some(input.message.clone()),
                LogEntry::Message(message) => Some(message.clone()),
                LogEntry::TurnStart(_)
                | LogEntry::Checkpoint(_)
                | LogEntry::TurnEnd { .. }
                | LogEntry::Rollback => None,
            })
            .collect()
    }

    /// The model context for the next request, honoring checkpoints.
    pub fn model_context(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        let mut model_context = Vec::new();
        for entry in active_entries(&self.entries) {
            match entry {
                LogEntry::Input(input) => {
                    messages.push(input.message.clone());
                    model_context.push(input.message.clone());
                }
                LogEntry::Message(message) => {
                    messages.push(message.clone());
                    model_context.push(message.clone());
                }
                LogEntry::Checkpoint(checkpoint) => {
                    model_context = apply_checkpoint(&messages, checkpoint);
                }
                LogEntry::TurnStart(_) | LogEntry::TurnEnd { .. } | LogEntry::Rollback => {}
            }
        }
        model_context
    }

    /// Completed turn snapshots in log order. A turn left open when the
    /// session ended (no `TurnEnd`, e.g. after a crash) is projected as
    /// `Interrupted` so partial turns never masquerade as normal history.
    pub fn turns(&self) -> Vec<TurnView> {
        let mut turns = Vec::new();
        let mut open: Option<(TurnId, Vec<Message>)> = None;
        for entry in active_entries(&self.entries) {
            match entry {
                LogEntry::TurnStart(id) => {
                    if let Some((previous_id, messages)) = open.take() {
                        turns.push(TurnView {
                            id: previous_id,
                            result: TurnResult::Interrupted(
                                "turn left open when the session ended".to_string(),
                            ),
                            messages,
                            usage: None,
                        });
                    }
                    open = Some((*id, Vec::new()));
                }
                LogEntry::Input(input) => {
                    if let Some((_, messages)) = open.as_mut() {
                        messages.push(input.message.clone());
                    }
                }
                LogEntry::Message(message) => {
                    if let Some((_, messages)) = open.as_mut() {
                        messages.push(message.clone());
                    }
                }
                LogEntry::TurnEnd { id, result, usage } => {
                    if let Some((open_id, messages)) = open.take() {
                        debug_assert_eq!(open_id, *id);
                        turns.push(TurnView {
                            id: open_id,
                            result: result.clone(),
                            messages,
                            usage: *usage,
                        });
                    }
                }
                LogEntry::Checkpoint(_) | LogEntry::Rollback => {}
            }
        }
        if let Some((id, messages)) = open {
            turns.push(TurnView {
                id,
                result: TurnResult::Interrupted(
                    "turn left open when the session ended".to_string(),
                ),
                messages,
                usage: None,
            });
        }
        turns
    }

    /// Messages belonging to one turn: accepted inputs plus model and tool
    /// messages recorded under that turn id.
    pub fn turn_messages(&self, turn_id: TurnId) -> Vec<Message> {
        let mut collecting = false;
        let mut messages = Vec::new();
        for entry in active_entries(&self.entries) {
            match entry {
                LogEntry::TurnStart(id) if *id == turn_id => collecting = true,
                LogEntry::TurnStart(_) | LogEntry::TurnEnd { .. } => collecting = false,
                LogEntry::Input(input) if collecting => messages.push(input.message.clone()),
                LogEntry::Message(message) if collecting => messages.push(message.clone()),
                LogEntry::Input(_)
                | LogEntry::Message(_)
                | LogEntry::Checkpoint(_)
                | LogEntry::Rollback => {}
            }
        }
        messages
    }

    /// Full projected state: history, model context, and turn views.
    pub fn view(&self) -> ThreadView {
        ThreadView {
            messages: self.messages(),
            context: self.model_context(),
            turns: self.turns(),
        }
    }
}

fn active_entries(entries: &[LogEntry]) -> Vec<&LogEntry> {
    let mut active = Vec::new();
    for entry in entries {
        match entry {
            LogEntry::Rollback => rollback_last_turn(&mut active),
            LogEntry::Input(_)
            | LogEntry::Message(_)
            | LogEntry::Checkpoint(_)
            | LogEntry::TurnStart(_)
            | LogEntry::TurnEnd { .. } => active.push(entry),
        }
    }
    active
}

fn rollback_last_turn(entries: &mut Vec<&LogEntry>) {
    let Some(turn_start) = entries.iter().rposition(|entry| {
        matches!(
            entry,
            LogEntry::Input(AcceptedInput {
                message: Message {
                    content: MessageContent::User(_),
                    ..
                },
                ..
            }) | LogEntry::Message(Message {
                content: MessageContent::User(_),
                ..
            })
        )
    }) else {
        return;
    };
    entries.truncate(turn_start);
    // A rolled-back turn leaves its `TurnStart` marker behind; drop it so the
    // projection does not see a dangling open turn.
    while matches!(entries.last(), Some(LogEntry::TurnStart(_))) {
        entries.pop();
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
    use super::*;
    use ash_core::StopReason;

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
        let mut log = ThreadLog::from_messages([
            first.clone(),
            answer.clone(),
            second,
            Message::assistant_text("second answer"),
        ]);
        log.push(LogEntry::Rollback);

        assert_eq!(log.entries().len(), 5);
        assert_eq!(
            log.messages()
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [first.id, answer.id]
        );
        assert_eq!(log.model_context().len(), 2);
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
}
