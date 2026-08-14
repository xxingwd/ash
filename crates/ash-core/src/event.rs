use derive_more::Display;
use serde::{Deserialize, Serialize};

use crate::message::{Message, MessageId, SessionId, ToolCallId, TurnId};

/// A routed event emitted by a session. `sequence` is monotonic within one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub sequence: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub kind: SessionEventKind,
}

/// Provider-neutral usage accounting.
///
/// `estimated` marks locally estimated values (no API usage was reported);
/// `generation_ms` measures the wall-clock time from the first output token
/// of any kind (reasoning, text, or tool call) to the end of the stream.
/// Provider-reported `output_tokens` includes reasoning/thinking tokens, so
/// the clock must start at the first reasoning delta for the rate
/// (`output_tokens` / `generation_ms`) to be honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub generation_ms: u64,
    pub estimated: bool,
}

/// Terminal result of one turn. `StopReason` alone cannot express failure or
/// interruption, so the turn boundary carries its own result type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnResult {
    Completed(StopReason),
    Failed(String),
    /// A turn that was left open when the session ended (for example after a
    /// crash) is never exposed as normal history.
    Interrupted(String),
}

/// Completed-turn snapshot: the durable boundary for scrollback, replay, and
/// resume. `messages` is the canonical set of messages produced by the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnView {
    pub id: TurnId,
    pub result: TurnResult,
    pub messages: Vec<Message>,
    pub usage: Option<Usage>,
    /// Locally estimated token count of the model context after this turn
    /// (system prompt + tools + model context). Same estimator the runtime
    /// uses for compaction; the UI shows this as the current context size.
    #[serde(default)]
    pub context_tokens: Option<u64>,
}

/// Full projected state of a session, derived from its durable log. Never
/// mutated directly; rebuilt from `LogEntry` records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub messages: Vec<Message>,
    pub context: Vec<Message>,
    pub turns: Vec<TurnView>,
    /// Locally estimated token count of the current model context (system
    /// prompt + tools + `context`). Same estimator the runtime uses for
    /// compaction; the UI shows this as the current context size.
    #[serde(default)]
    pub context_tokens: Option<u64>,
}

/// Ephemeral streaming deltas for the current turn. Never persisted and never
/// replayed; the UI draws them as a live preview only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiveEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    ToolFinished {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
        output: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub title: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPoint {
    pub message_id: MessageId,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEventKind {
    /// A turn started executing; its input was already accepted.
    TurnStarted,
    /// Ephemeral streaming delta for the active turn's live preview.
    Live(LiveEvent),
    /// A turn settled: the canonical boundary for committing scrollback.
    TurnCompleted(TurnView),
    /// The model context was compacted while a turn was executing.
    ContextCompacted {
        before: u64,
        after: u64,
        dropped: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Display)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurns,
    Aborted,
    /// The model stream ended before the provider signalled a normal terminal
    /// state (no `finish_reason`, `message_stop`, `response.completed`, or
    /// `[DONE]`). Partial output may have been produced; the turn is not a
    /// clean stop and is eligible for a safe retry by the engine.
    Truncated,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_events_require_complete_payloads() {
        let id = ToolCallId::new();
        assert!(
            serde_json::from_value::<SessionEventKind>(serde_json::json!({
                "Live": {
                    "ToolStarted": {
                        "id": id,
                        "name": "read"
                    }
                }
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SessionEventKind>(serde_json::json!({
                "ContextCompacted": {
                    "before": 100,
                    "after": 50
                }
            }))
            .is_err()
        );
    }
}
