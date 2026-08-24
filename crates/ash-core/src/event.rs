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

/// Session-level additive resource consumption, including local work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub tool_calls: u64,
    pub estimated: bool,
}

impl Usage {
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            tool_calls: self.tool_calls.saturating_add(other.tool_calls),
            estimated: self.estimated || other.estimated,
        }
    }

    #[must_use]
    pub const fn total_tokens(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Observable statistics for one turn. The live copy is ephemeral; the final
/// copy is persisted with the turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStats {
    #[serde(flatten)]
    pub usage: Usage,
    /// Sum of model generation windows, excluding tool execution and backoff.
    pub generation_ms: u64,
}

impl TurnStats {
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            usage: self.usage.saturating_add(other.usage),
            generation_ms: self.generation_ms.saturating_add(other.generation_ms),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveTurnStats {
    pub turn_id: TurnId,
    pub stats: TurnStats,
}

/// Latest read-only projection of one session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStats {
    /// Usage backed by durable session-log entries.
    pub settled_usage: Usage,
    /// Latest replaceable snapshot for the running turn.
    pub active_turn: Option<ActiveTurnStats>,
    /// Local estimate of the context prepared for the model.
    pub context_tokens: Option<u64>,
}

impl SessionStats {
    #[must_use]
    pub fn total_usage(self) -> Usage {
        self.active_turn.map_or(self.settled_usage, |active| {
            self.settled_usage.saturating_add(active.stats.usage)
        })
    }
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
    pub stats: TurnStats,
}

/// Full projected state of a session, derived from its durable log. Never
/// mutated directly; rebuilt from `LogEntry` records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub messages: Vec<Message>,
    pub context: Vec<Message>,
    pub turns: Vec<TurnView>,
    pub stats: SessionStats,
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

/// Observable result of a model-context compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUpdate {
    #[serde(rename = "before")]
    pub before_tokens: u64,
    #[serde(rename = "after")]
    pub after_tokens: u64,
    #[serde(rename = "dropped")]
    pub dropped_messages: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEventKind {
    /// A turn started executing; its input was already accepted.
    TurnStarted,
    /// Ephemeral streaming delta for the active turn's live preview.
    Live(LiveEvent),
    /// Ephemeral full snapshot of the active turn's statistics.
    TurnProgress(TurnStats),
    /// Local size of the context prepared for the next model request.
    ContextChanged { tokens: u64 },
    /// A turn settled: the canonical boundary for committing scrollback.
    TurnCompleted(TurnView),
    /// The model context was compacted while a turn was executing.
    ContextCompacted(ContextUpdate),
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

    #[test]
    fn usage_saturates_counters_and_propagates_estimates() {
        let usage = Usage {
            input_tokens: u64::MAX,
            output_tokens: 10,
            tool_calls: 2,
            estimated: false,
        }
        .saturating_add(Usage {
            input_tokens: 1,
            output_tokens: u64::MAX,
            tool_calls: u64::MAX,
            estimated: true,
        });

        assert_eq!(usage.input_tokens, u64::MAX);
        assert_eq!(usage.output_tokens, u64::MAX);
        assert_eq!(usage.tool_calls, u64::MAX);
        assert!(usage.estimated);
        assert_eq!(usage.total_tokens(), u64::MAX);
    }

    #[test]
    fn session_stats_add_active_usage_without_mutating_settled_usage() {
        let settled_usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            tool_calls: 1,
            estimated: false,
        };
        let stats = SessionStats {
            settled_usage,
            active_turn: Some(ActiveTurnStats {
                turn_id: TurnId::new(),
                stats: TurnStats {
                    usage: Usage {
                        input_tokens: 4,
                        output_tokens: 2,
                        tool_calls: 1,
                        estimated: true,
                    },
                    generation_ms: 100,
                },
            }),
            context_tokens: Some(20),
        };

        assert_eq!(stats.settled_usage, settled_usage);
        assert_eq!(
            stats.total_usage(),
            Usage {
                input_tokens: 14,
                output_tokens: 7,
                tool_calls: 2,
                estimated: true,
            }
        );
    }
}
