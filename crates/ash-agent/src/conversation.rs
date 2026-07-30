use ash_core::{Message, MessageContent, MessageId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextCheckpoint {
    pub summary: Message,
    pub tail_start_id: Option<MessageId>,
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
pub enum ConversationEntry {
    Message(Message),
    ContextCheckpoint(ContextCheckpoint),
    TurnRolledBack,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConversationLog {
    entries: Vec<ConversationEntry>,
}

impl ConversationLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: impl IntoIterator<Item = Message>) -> Self {
        Self {
            entries: messages
                .into_iter()
                .map(ConversationEntry::Message)
                .collect(),
        }
    }

    pub(crate) fn from_entries(entries: Vec<ConversationEntry>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    pub fn push(&mut self, entry: ConversationEntry) {
        self.entries.push(entry);
    }

    pub fn messages(&self) -> Vec<Message> {
        active_entries(&self.entries)
            .into_iter()
            .filter_map(|entry| match entry {
                ConversationEntry::Message(message) => Some(message.clone()),
                ConversationEntry::ContextCheckpoint(_) | ConversationEntry::TurnRolledBack => None,
            })
            .collect()
    }

    pub fn model_context(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        let mut model_context = Vec::new();
        for entry in active_entries(&self.entries) {
            match entry {
                ConversationEntry::Message(message) => {
                    messages.push(message.clone());
                    model_context.push(message.clone());
                }
                ConversationEntry::ContextCheckpoint(checkpoint) => {
                    model_context = apply_checkpoint(&messages, checkpoint);
                }
                ConversationEntry::TurnRolledBack => {}
            }
        }
        model_context
    }
}

fn active_entries(entries: &[ConversationEntry]) -> Vec<&ConversationEntry> {
    let mut active = Vec::new();
    for entry in entries {
        match entry {
            ConversationEntry::TurnRolledBack => rollback_last_turn(&mut active),
            ConversationEntry::Message(_) | ConversationEntry::ContextCheckpoint(_) => {
                active.push(entry)
            }
        }
    }
    active
}

fn rollback_last_turn(entries: &mut Vec<&ConversationEntry>) {
    let Some(turn_start) = entries.iter().rposition(|entry| {
        matches!(
            entry,
            ConversationEntry::Message(Message {
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
        let mut log =
            ConversationLog::from_messages([first.clone(), answer.clone(), recent.clone()]);
        log.push(ConversationEntry::ContextCheckpoint(ContextCheckpoint {
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
        let mut log = ConversationLog::from_messages([
            first.clone(),
            answer.clone(),
            second,
            Message::assistant_text("second answer"),
        ]);
        log.push(ConversationEntry::TurnRolledBack);

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
}
