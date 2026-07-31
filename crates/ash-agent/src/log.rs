use ash_core::{Message, MessageContent, MessageId, StopReason, TurnId};
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Record {
    TurnStarted { turn_id: TurnId },
    InputAccepted(AcceptedInput),
    Message(Message),
    ContextCheckpoint(ContextCheckpoint),
    TurnCompleted { turn_id: TurnId, reason: StopReason },
    TurnFailed { turn_id: TurnId, error: String },
    TurnRolledBack,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThreadLog {
    entries: Vec<Record>,
}

impl ThreadLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: impl IntoIterator<Item = Message>) -> Self {
        Self {
            entries: messages.into_iter().map(Record::Message).collect(),
        }
    }

    pub(crate) fn from_entries(entries: Vec<Record>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[Record] {
        &self.entries
    }

    pub fn revision(&self) -> Version {
        Version::from_entry_count(self.entries.len())
    }

    pub fn contains_idempotency_key(&self, key: &str) -> bool {
        self.entries.iter().any(|entry| {
            matches!(
                entry,
                Record::InputAccepted(AcceptedInput { input, .. })
                    if input.idempotency_key.as_deref() == Some(key)
            )
        })
    }

    pub fn push(&mut self, entry: Record) {
        self.entries.push(entry);
    }

    pub fn messages(&self) -> Vec<Message> {
        active_entries(&self.entries)
            .into_iter()
            .filter_map(|entry| match entry {
                Record::InputAccepted(input) => Some(input.message.clone()),
                Record::Message(message) => Some(message.clone()),
                Record::TurnStarted { .. }
                | Record::ContextCheckpoint(_)
                | Record::TurnCompleted { .. }
                | Record::TurnFailed { .. }
                | Record::TurnRolledBack => None,
            })
            .collect()
    }

    pub fn model_context(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        let mut model_context = Vec::new();
        for entry in active_entries(&self.entries) {
            match entry {
                Record::InputAccepted(input) => {
                    messages.push(input.message.clone());
                    model_context.push(input.message.clone());
                }
                Record::Message(message) => {
                    messages.push(message.clone());
                    model_context.push(message.clone());
                }
                Record::ContextCheckpoint(checkpoint) => {
                    model_context = apply_checkpoint(&messages, checkpoint);
                }
                Record::TurnStarted { .. }
                | Record::TurnCompleted { .. }
                | Record::TurnFailed { .. }
                | Record::TurnRolledBack => {}
            }
        }
        model_context
    }
}

fn active_entries(entries: &[Record]) -> Vec<&Record> {
    let mut active = Vec::new();
    for entry in entries {
        match entry {
            Record::TurnRolledBack => rollback_last_turn(&mut active),
            Record::InputAccepted(_)
            | Record::Message(_)
            | Record::ContextCheckpoint(_)
            | Record::TurnStarted { .. }
            | Record::TurnCompleted { .. }
            | Record::TurnFailed { .. } => active.push(entry),
        }
    }
    active
}

fn rollback_last_turn(entries: &mut Vec<&Record>) {
    let Some(turn_start) = entries.iter().rposition(|entry| {
        matches!(
            entry,
            Record::InputAccepted(AcceptedInput {
                message: Message {
                    content: MessageContent::User(_),
                    ..
                },
                ..
            }) | Record::Message(Message {
                content: MessageContent::User(_),
                ..
            })
        )
    }) else {
        return;
    };
    entries.truncate(turn_start);
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

    #[test]
    fn projects_full_history_and_compacted_model_context_from_one_log() {
        let first = Message::user("first");
        let answer = Message::assistant_text("answer");
        let recent = Message::user("recent");
        let summary = Message::system("summary");
        let mut log = ThreadLog::from_messages([first.clone(), answer.clone(), recent.clone()]);
        log.push(Record::ContextCheckpoint(ContextCheckpoint {
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
        log.push(Record::TurnRolledBack);

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
        log.push(Record::InputAccepted(AcceptedInput {
            turn_id: TurnId::new(),
            input,
            message: message.clone(),
        }));

        assert!(log.contains_idempotency_key("heartbeat:42"));
        assert_eq!(log.messages()[0].id, message.id);
        assert_eq!(log.model_context()[0].id, message.id);
    }
}
