use ash_agent::{AgentSnapshot, Session};
use ash_core::{CancellationToken, SessionId, SessionIdentity, Turn, TurnResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Tree {
    #[serde(skip)]
    pub closing: bool,
    #[serde(default)]
    pub blueprints: Vec<serde_json::Value>,
    pub nodes: BTreeMap<SessionId, Node>,
    pub groups: BTreeMap<String, Group>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Node {
    pub identity: SessionIdentity,
    pub definition: AgentSnapshot,
    pub group_id: Option<String>,
    pub started: bool,
    pub line: Line,
    #[serde(skip)]
    pub session: Option<Session>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Group {
    pub id: String,
    pub name: String,
    pub owner: SessionId,
    pub members: Vec<SessionId>,
    pub line: Line,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Line {
    pub execution: Option<Execution>,
    pub result: Option<Completion>,
    pub unread: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Execution {
    pub started: bool,
    pub failure: Option<String>,
    pub id: SessionId,
    pub delivery: Delivery,
    pub successor: Option<Delivery>,
    pub replacement: Option<Delivery>,
    #[serde(skip)]
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Delivery {
    pub id: String,
    pub sender: SessionId,
    pub receiver: SessionId,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Stopped,
    Failed,
    Cancelled,
    Truncated,
    Interrupted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageSource {
    Agent,
    Runtime,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Completion {
    pub message_id: String,
    pub sender_id: SessionId,
    pub source: MessageSource,
    pub message: String,
    pub status: ExecutionStatus,
}

impl Completion {
    pub(crate) fn diagnostic(
        sender_id: SessionId,
        status: ExecutionStatus,
        message: impl Into<String>,
    ) -> Self {
        Self {
            message_id: SessionId::new().to_string(),
            sender_id,
            source: MessageSource::Runtime,
            message: message.into(),
            status,
        }
    }

    pub(crate) fn from_turn(sender: SessionId, turn: Result<&Turn, String>) -> Self {
        let turn = match turn {
            Ok(turn) => turn,
            Err(error) => return Self::diagnostic(sender, ExecutionStatus::Failed, error),
        };
        let last = turn
            .steps
            .iter()
            .rev()
            .flat_map(|step| step.items.iter().rev())
            .find_map(|item| match item {
                ash_core::Item::Text(text) if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            });
        let (status, diagnostic) = match &turn.result {
            TurnResult::Stopped(ash_core::StopReason::MaxTokens) => (
                ExecutionStatus::Truncated,
                Some("Agent reached its output limit.".into()),
            ),
            TurnResult::Stopped(_) => (ExecutionStatus::Stopped, None),
            TurnResult::Failed(error) => (
                ExecutionStatus::Failed,
                Some(format!("Agent failed: {error}")),
            ),
            TurnResult::Cancelled => (
                ExecutionStatus::Cancelled,
                Some("Agent was cancelled; existing side effects were not rolled back.".into()),
            ),
            TurnResult::Truncated => (
                ExecutionStatus::Truncated,
                Some("Agent output was truncated.".into()),
            ),
        };
        if let Some(diagnostic) = diagnostic {
            return Self::diagnostic(
                sender,
                status,
                match last {
                    Some(text) => format!("{diagnostic}\nLast text: {text}"),
                    None => diagnostic,
                },
            );
        }
        match last {
            Some(message) => Self {
                message_id: SessionId::new().to_string(),
                sender_id: sender,
                source: MessageSource::Agent,
                message,
                status,
            },
            None => Self::diagnostic(sender, status, "Agent stopped without a text message."),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatEntry {
    pub message_id: String,
    pub source: MessageSource,
    pub sender_id: SessionId,
    pub receiver_id: Option<SessionId>,
    pub kind: String,
    pub message: String,
    pub delivery_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Agent(SessionId),
    Group(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingState {
    Running,
    Unread,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingTarget {
    Agent { agent_id: SessionId },
    Group { group_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWork {
    pub owner_id: SessionId,
    #[serde(flatten)]
    pub target: PendingTarget,
    pub state: PendingState,
}

pub fn pending_notice(pending: &[PendingWork]) -> Option<String> {
    if pending.is_empty() {
        return None;
    }
    let details = pending
        .iter()
        .map(|work| {
            let target = match &work.target {
                PendingTarget::Agent { agent_id } => format!("agent_id={agent_id}"),
                PendingTarget::Group { group_id } => format!("group_id={group_id}"),
            };
            let state = match work.state {
                PendingState::Running => "running",
                PendingState::Unread => "stopped with an unread result",
            };
            format!("{target}: {state}; owner_id={}", work.owner_id)
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("Uncollected collaboration work:\n{details}\nA running target may still be working or waiting; this snapshot does not imply a deadlock. Completion results are delivered automatically when the owner continues."))
}

impl Tree {
    pub fn target(&self, agent: SessionId) -> Option<Target> {
        self.nodes.get(&agent).map(|node| {
            node.group_id
                .as_ref()
                .map_or(Target::Agent(agent), |group| Target::Group(group.clone()))
        })
    }

    pub fn line(&self, target: &Target) -> &Line {
        match target {
            Target::Agent(id) => &self.nodes[id].line,
            Target::Group(id) => &self.groups[id].line,
        }
    }

    pub fn line_mut(&mut self, target: &Target) -> &mut Line {
        match target {
            Target::Agent(id) => &mut self.nodes.get_mut(id).expect("validated target").line,
            Target::Group(id) => &mut self.groups.get_mut(id).expect("validated target").line,
        }
    }
}
