use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use crate::snapshot::{SubagentSnapshot, SubagentState};
use ash_agent::{Agent, Input, InputSource, Runtime, Session, SessionKind, SessionOptions};
use ash_core::{
    define_tool, CancellationToken, ContentBlock, Message, MessageContent, SessionId, StopReason,
    Tool, ToolContext, ToolError, TreeId, TurnResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch, Mutex, Notify};

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;
const EXPOSED_COLLABORATION_TOOL_NAMES: [&str; 4] = [
    "spawn_agent",
    "message_agent",
    "interrupt_agent",
    "wait_agent",
];

const LEGACY_COLLABORATION_TOOL_NAMES: [&str; 3] = ["send_message", "followup_task", "list_agents"];

/// Tools whose public contracts are read-only. Explorer projection is an
/// allowlist: unknown custom and MCP tools are excluded unless their behavior
/// is represented by one of these canonical tool names.
const EXPLORER_TOOL_NAMES: [&str; 5] = ["read", "glob", "grep", "webfetch", "skill"];

/// Every collaboration tool name, derived from the exposed and legacy sets so
/// that adding a new tool touches exactly one place.
fn all_collaboration_tool_names() -> Vec<&'static str> {
    EXPOSED_COLLABORATION_TOOL_NAMES
        .iter()
        .chain(LEGACY_COLLABORATION_TOOL_NAMES.iter())
        .copied()
        .collect()
}

const MULTI_AGENT_INSTRUCTIONS: &str = r"<multi_agent_mode>
You are one agent in a team that shares the same workspace and tools. Split work where parallelism pays; keep tightly coupled work local.

Delegation is available when it simplifies the work. You do not need separate permission to delegate work that is already inside the user's request.

Decision rules:
- Handle simple, one-path tasks yourself.
- When there are two or more independent questions or workstreams, multiple sub-agents may run at the same time.
- Use `explorer` for a focused, read-only codebase question that can be answered independently. Trust a completed exploration instead of repeating it.
- Prefer `worker` for a bounded implementation, fix, test, or refactor with a clear write scope.
- Use `default` for a self-contained task that does not fit the other roles.
- Keep an immediate blocker local when your very next action depends on it and delegation would only add waiting.

Operating rules:
- Give every agent a concrete task, expected output, and enough context to finish without guessing.
- For code changes, assign explicit files or modules. Write scopes must not overlap.
- The workspace is shared. Never ask an agent to revert unrelated edits; tell workers they are not alone in the codebase.
- After spawning, continue useful non-overlapping work immediately when available. Do not redo the delegated task.
- Use `wait_agent` with `timeout_ms: 0` to inspect current status, or with a positive timeout when an agent's result is required for the next step.
- Reuse context with `message_agent` and `start_turn: true`; use `message_agent` with `start_turn: false` for guidance that should not start a new turn.
- Interrupt a sub-agent only from the parent when the current turn is stale, wrong, or blocking the plan.
- Review returned changes before integrating them.
</multi_agent_mode>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentRole {
    Default,
    Explorer,
    Worker,
}

impl AgentRole {
    const fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Explorer => "explorer",
            Self::Worker => "worker",
        }
    }

    const fn available_roles_description() -> &'static str {
        r"Optional type name for the new agent. If omitted, `default` is used.
Available roles:
default: General-purpose agent for a self-contained task that inherits the current configuration.
explorer: Use whenever a specific, well-scoped codebase question can be answered independently. Explorers are fast, read-only, and authoritative. Spawn multiple explorers in the same round for distinct questions; reuse an existing explorer for related follow-ups.
worker: Prefer for bounded implementation and production work such as features, fixes, tests, and refactors. Assign explicit file or module ownership, keep write scopes disjoint, and remind workers that the workspace is shared."
    }

    const fn child_instructions(self) -> &'static str {
        match self {
            Self::Default => {
                "Handle the assigned task directly. Stay within its scope, delegate independent subparts when that creates real parallel progress, and return a concise, evidence-backed result to the parent agent."
            }
            Self::Explorer => {
                "Answer the assigned codebase question through read-only inspection. Do not edit files. Return concrete findings with relevant paths and symbols, and do not broaden the investigation beyond the question."
            }
            Self::Worker => {
                "Execute the assigned implementation or production task. Respect the stated file or module ownership, preserve unrelated workspace changes, coordinate independent side questions through sub-agents when useful, verify your work, and report changed files plus validation results."
            }
        }
    }
}

#[cfg(test)]
impl FromStr for AgentRole {
    type Err = ToolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "default" => Ok(Self::Default),
            "explorer" => Ok(Self::Explorer),
            "worker" => Ok(Self::Worker),
            other => Err(ToolError::Execution(format!(
                "unknown agent_type '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentStatus {
    Pending,
    Running,
    Completed,
    Interrupted,
    Errored,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AgentSnapshot {
    pub agent_id: SessionId,
    pub task_name: String,
    pub agent_type: AgentRole,
    pub status: AgentStatus,
    pub last_task_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<AgentSnapshot> for SubagentSnapshot {
    fn from(snapshot: AgentSnapshot) -> Self {
        Self {
            task_name: snapshot.task_name,
            agent_type: snapshot.agent_type.name().to_string(),
            state: snapshot.status.into(),
            last_task_message: snapshot.last_task_message,
        }
    }
}

impl From<AgentStatus> for SubagentState {
    fn from(status: AgentStatus) -> Self {
        match status {
            AgentStatus::Pending => Self::Pending,
            AgentStatus::Running => Self::Running,
            AgentStatus::Completed => Self::Completed,
            AgentStatus::Interrupted => Self::Interrupted,
            AgentStatus::Errored => Self::Errored,
        }
    }
}

#[derive(Clone)]
pub struct AgentControl {
    inner: Arc<ControlInner>,
}

struct ControlInner {
    state: Mutex<ControlState>,
    updates: Notify,
    max_concurrent_children: Option<usize>,
    spawner: Arc<dyn AgentSpawner>,
    subagent_tx: watch::Sender<Vec<SubagentSnapshot>>,
}

struct SpawnReservation {
    inner: Arc<ControlInner>,
    tree_id: TreeId,
    task_name: Option<String>,
}

/// Remove a pending spawn reservation from the shared state. Used by the
/// reservation's release path, its `Drop` impl, and the successful spawn path
/// once the child agent has been registered.
async fn release_spawn(inner: &Arc<ControlInner>, tree_id: TreeId, task_name: &str) {
    let mut state = inner.state.lock().await;
    if let Some(tree) = state.trees.get_mut(&tree_id) {
        tree.pending_spawns.remove(task_name);
    }
}

impl SpawnReservation {
    const fn new(inner: Arc<ControlInner>, tree_id: TreeId, task_name: String) -> Self {
        Self {
            inner,
            tree_id,
            task_name: Some(task_name),
        }
    }

    async fn release(&mut self) {
        let Some(task_name) = self.task_name.take() else {
            return;
        };
        release_spawn(&self.inner, self.tree_id, &task_name).await;
    }

    fn commit(mut self) {
        self.task_name = None;
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        let Some(task_name) = self.task_name.take() else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        let tree_id = self.tree_id;
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    release_spawn(&inner, tree_id, &task_name).await;
                });
            }
            Err(_) => {
                tracing::warn!(
                    %task_name,
                    "spawn reservation dropped outside a tokio runtime; the task name may leak"
                );
            }
        }
    }
}

pub(crate) struct SpawnRequest {
    pub role: AgentRole,
    pub parent_path: String,
    pub task_name: String,
    pub messages: Vec<Message>,
    pub tree_id: TreeId,
}

pub(crate) struct ChildAgent {
    pub runtime: Runtime,
    pub agent: Agent,
    pub options: SessionOptions,
    pub history: Vec<Message>,
}

pub(crate) trait AgentSpawner: Send + Sync {
    fn spawn(&self, request: SpawnRequest) -> Result<ChildAgent, ToolError>;
}

struct InheritedAgentSpawner {
    runtime: Runtime,
    system_prompt: Option<String>,
    definition: Agent,
    scope: SessionOptions,
}

impl AgentSpawner for InheritedAgentSpawner {
    fn spawn(&self, request: SpawnRequest) -> Result<ChildAgent, ToolError> {
        let definition = self
            .definition
            .clone()
            .with_system_prompt(subagent_system_prompt(
                self.system_prompt.as_deref(),
                request.role,
                &request.parent_path,
                &request.task_name,
            ));
        let scope = SessionOptions {
            working_dir: self.scope.working_dir.clone(),
            tool_timeout: self.scope.tool_timeout,
            path: request.task_name,
            tree_id: Some(request.tree_id),
            kind: SessionKind::Subagent,
        };
        Ok(ChildAgent {
            runtime: self.runtime.clone(),
            agent: definition,
            options: scope,
            history: request.messages,
        })
    }
}

#[derive(Default)]
struct ControlState {
    trees: HashMap<TreeId, AgentTreeState>,
}

#[derive(Default)]
struct AgentTreeState {
    agents: HashMap<SessionId, ChildRecord>,
    pending_spawns: HashSet<String>,
    next_completion_revision: u64,
    wait_cursors: HashMap<String, u64>,
}

struct ChildRecord {
    id: SessionId,
    task_name: String,
    role: AgentRole,
    state: ChildState,
    /// Follow-up turns accepted by the controller but not yet settled. This
    /// includes the currently running follow-up, if any.
    pending_followups: usize,
    completion_revision: Option<u64>,
    last_task_message: String,
    command_tx: mpsc::Sender<ChildCommand>,
}

enum ChildState {
    Pending,
    Running(CancellationToken),
    Completed(Option<String>),
    Interrupted(Option<String>),
    Errored {
        final_message: Option<String>,
        error: String,
    },
}

impl ChildState {
    const fn status(&self) -> AgentStatus {
        match self {
            Self::Pending => AgentStatus::Pending,
            Self::Running(_) => AgentStatus::Running,
            Self::Completed(_) => AgentStatus::Completed,
            Self::Interrupted(_) => AgentStatus::Interrupted,
            Self::Errored { .. } => AgentStatus::Errored,
        }
    }

    const fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running(_))
    }

    fn final_message(&self) -> Option<&str> {
        match self {
            Self::Completed(message) | Self::Interrupted(message) => message.as_deref(),
            Self::Errored { final_message, .. } => final_message.as_deref(),
            Self::Pending | Self::Running(_) => None,
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Self::Errored { error, .. } => Some(error),
            _ => None,
        }
    }

    fn cancel(&self) -> Option<CancellationToken> {
        match self {
            Self::Running(cancel) => Some(cancel.clone()),
            _ => None,
        }
    }
}

impl ChildRecord {
    fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            agent_id: self.id,
            task_name: self.task_name.clone(),
            agent_type: self.role,
            status: self.state.status(),
            last_task_message: self.last_task_message.clone(),
            final_message: self.state.final_message().map(str::to_string),
            error: self.state.error().map(str::to_string),
        }
    }

    fn prepare_delivery(&mut self, delivery: MessageDelivery, message: &str) {
        self.last_task_message = message.to_string();
        if delivery == MessageDelivery::Followup {
            self.pending_followups = self.pending_followups.saturating_add(1);
            if !matches!(self.state, ChildState::Running(_)) {
                self.state = ChildState::Pending;
            }
            self.completion_revision = None;
        }
    }

    /// Settle one turn and report whether the agent is now terminal. A queued
    /// follow-up keeps the record pending until every accepted follow-up has
    /// finished, so waiters never observe an intermediate completion.
    fn finish_turn(&mut self, completed: ChildState, was_followup: bool) -> bool {
        if was_followup {
            self.pending_followups = self.pending_followups.saturating_sub(1);
        }
        if self.pending_followups == 0 {
            self.state = completed;
            true
        } else {
            self.state = ChildState::Pending;
            false
        }
    }
}

enum ChildCommand {
    Queue(String),
    Followup(String),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MessageDelivery {
    Queue,
    Followup,
}

impl MessageDelivery {
    const fn triggers_turn(self) -> bool {
        matches!(self, Self::Followup)
    }

    const fn command(self, message: String) -> ChildCommand {
        match self {
            Self::Queue => ChildCommand::Queue(message),
            Self::Followup => ChildCommand::Followup(message),
        }
    }

    const fn needs_new_slot(self, state: &ChildState) -> bool {
        self.triggers_turn() && !state.is_active()
    }
}

fn sorted_snapshots<'a>(records: impl Iterator<Item = &'a ChildRecord>) -> Vec<AgentSnapshot> {
    let mut agents = records.map(ChildRecord::snapshot).collect::<Vec<_>>();
    agents.sort_by(|left, right| left.task_name.cmp(&right.task_name));
    agents
}

impl ControlState {
    fn active_count(&self, tree_id: TreeId) -> usize {
        self.trees
            .get(&tree_id)
            .map_or(0, AgentTreeState::active_count)
    }

    fn snapshots(&self, tree_id: TreeId, path_prefix: Option<&str>) -> Vec<AgentSnapshot> {
        sorted_snapshots(
            self.trees
                .get(&tree_id)
                .into_iter()
                .flat_map(|tree| tree.agents.values())
                .filter(|record| agent_path_matches(record, path_prefix)),
        )
    }

    fn wait_snapshot(
        &mut self,
        tree_id: TreeId,
        waiter: &str,
        path_prefix: Option<&str>,
    ) -> WaitSnapshot {
        self.trees
            .get_mut(&tree_id)
            .map_or_else(WaitSnapshot::empty, |tree| {
                tree.wait_snapshot(waiter, path_prefix)
            })
    }

    fn finish_turn(
        &mut self,
        tree_id: TreeId,
        agent_id: SessionId,
        completed: ChildState,
        was_followup: bool,
    ) -> bool {
        self.trees
            .get_mut(&tree_id)
            .is_some_and(|tree| tree.finish_turn(agent_id, completed, was_followup))
    }
}

impl AgentTreeState {
    fn active_count(&self) -> usize {
        self.agents
            .values()
            .filter(|record| record.state.is_active())
            .count()
            .saturating_add(self.pending_spawns.len())
    }

    fn reserve_spawn(&mut self, task_name: &str, max: Option<usize>) -> Result<(), ToolError> {
        if let Some(max) = max {
            if self.active_count() >= max {
                return Err(ToolError::Execution(format!(
                    "maximum of {max} concurrent sub-agents reached"
                )));
            }
        }
        if self.pending_spawns.contains(task_name)
            || self
                .agents
                .values()
                .any(|record| record.task_name == task_name)
        {
            return Err(ToolError::Execution(format!(
                "agent task name already exists: {task_name}; use message_agent with start_turn=true to reuse it"
            )));
        }
        self.pending_spawns.insert(task_name.to_string());
        Ok(())
    }

    fn wait_snapshot(&mut self, waiter: &str, path_prefix: Option<&str>) -> WaitSnapshot {
        let waiter_key = path_prefix.map_or_else(
            || waiter.to_string(),
            |prefix| format!("{waiter}\n{prefix}"),
        );
        let cursor = self.wait_cursors.get(&waiter_key).copied().unwrap_or(0);
        let completion_revision = self
            .agents
            .values()
            .filter(|record| agent_path_matches(record, path_prefix))
            .filter(|record| !record.state.is_active())
            .filter_map(|record| record.completion_revision)
            .filter(|revision| *revision > cursor)
            .max();
        if let Some(revision) = completion_revision {
            self.wait_cursors.insert(waiter_key, revision);
        }
        let agents = sorted_snapshots(
            self.agents
                .values()
                .filter(|record| agent_path_matches(record, path_prefix)),
        );
        WaitSnapshot {
            agents,
            has_update: completion_revision.is_some(),
        }
    }

    fn finish_turn(
        &mut self,
        agent_id: SessionId,
        completed: ChildState,
        was_followup: bool,
    ) -> bool {
        let Some(record) = self.agents.get_mut(&agent_id) else {
            return false;
        };
        if record.finish_turn(completed, was_followup) {
            self.next_completion_revision = self.next_completion_revision.saturating_add(1);
            record.completion_revision = Some(self.next_completion_revision);
        }
        true
    }
}

struct WaitSnapshot {
    agents: Vec<AgentSnapshot>,
    has_update: bool,
}

impl WaitSnapshot {
    const fn empty() -> Self {
        Self {
            agents: Vec::new(),
            has_update: false,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SpawnAgentArgs {
    /// Task name using lowercase letters, digits, and underscores.
    task_name: String,
    /// Initial plain-text task for the new agent.
    message: String,
    /// Optional role: default, explorer, or worker.
    agent_type: Option<AgentRole>,
    /// Context to fork: none, all, or a positive number of recent turns.
    #[serde(default)]
    fork_turns: Option<ForkTurnsArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MessageAgentArgs {
    /// Agent id, canonical task path, or unambiguous task name.
    target: String,
    /// Message text to deliver.
    message: String,
    /// Start a follow-up turn after delivery. If false or omitted, the message is queued as guidance only.
    start_turn: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InterruptAgentArgs {
    /// Agent id, canonical task path, or unambiguous task name.
    target: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitAgentArgs {
    /// Maximum wait in milliseconds. Use 0 to return the current snapshot immediately. Defaults to 10000 and is capped at 3600000.
    timeout_ms: Option<u64>,
    /// Optional canonical task-path prefix.
    path_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(untagged)]
enum ForkTurnsArg {
    Keyword(ForkTurnsKeyword),
    Last(usize),
    LastText(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ForkTurnsKeyword {
    None,
    All,
}

const INVALID_FORK_TURNS_MESSAGE: &str = "fork_turns must be `none`, `all`, or a positive integer";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkMode {
    None,
    All,
    Last(usize),
}

impl ForkMode {
    fn from_arg(value: Option<ForkTurnsArg>) -> Result<Self, ToolError> {
        match value {
            None => Ok(Self::All),
            Some(ForkTurnsArg::Keyword(ForkTurnsKeyword::None)) => Ok(Self::None),
            Some(ForkTurnsArg::Keyword(ForkTurnsKeyword::All)) => Ok(Self::All),
            Some(ForkTurnsArg::Last(turns)) => Self::last(turns),
            Some(ForkTurnsArg::LastText(text)) => {
                let turns = text
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| ToolError::Execution(INVALID_FORK_TURNS_MESSAGE.to_string()))?;
                Self::last(turns)
            }
        }
    }

    fn last(turns: usize) -> Result<Self, ToolError> {
        if turns == 0 {
            Err(ToolError::Execution(INVALID_FORK_TURNS_MESSAGE.to_string()))
        } else {
            Ok(Self::Last(turns))
        }
    }
}

impl AgentControl {
    pub(crate) fn new(
        max_concurrent_children: Option<usize>,
        spawner: Arc<dyn AgentSpawner>,
    ) -> Self {
        let (subagent_tx, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(ControlInner {
                state: Mutex::new(ControlState::default()),
                updates: Notify::new(),
                max_concurrent_children,
                spawner,
                subagent_tx,
            }),
        }
    }

    /// Subscribe to display-oriented snapshots of every sub-agent managed by
    /// this control. The receiver is updated whenever a sub-agent is spawned,
    /// transitions state, or receives a follow-up message.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Vec<SubagentSnapshot>> {
        self.inner.subagent_tx.subscribe()
    }

    /// Publish the current sub-agent snapshots to subscribers.
    async fn publish(&self) {
        let state = self.inner.state.lock().await;
        let snapshots = state
            .trees
            .values()
            .flat_map(|tree| tree.agents.values())
            .map(ChildRecord::snapshot)
            .map(SubagentSnapshot::from)
            .collect();
        drop(state);
        let _ = self.inner.subagent_tx.send(snapshots);
    }

    /// Build the collaboration tools (spawn, message, wait, interrupt).
    ///
    /// # Errors
    ///
    /// Returns `ToolError` when a tool cannot be defined.
    pub fn tools(&self) -> Result<Vec<Arc<dyn Tool>>, ToolError> {
        let spawn = self.clone();
        let spawn_description = format!(
            "Spawn a sub-agent for a concrete, bounded task that can make progress independently. Spawned agents use the same Runtime -> Session -> Turn execution pipeline as the parent and inherit the current model, environment, AGENTS.md instructions, skills, and tools.\n\nUse this when a separate agent makes the plan simpler or can answer an independent question:\n- Use `explorer` for an independent read-only codebase question.\n- Prefer `worker` for a bounded code change with explicit file or module ownership.\n- Use `default` for another self-contained task.\n- Multiple sub-agents are supported, but do not spawn for trivial tasks or immediate blockers.\n- Give the agent the exact output you need; do not duplicate its work locally.\n- After spawning, continue non-overlapping work when available and call `wait_agent` only when its result becomes relevant.\n\n{}",
            AgentRole::available_roles_description()
        );
        let spawn_tool = define_tool(
            "spawn_agent",
            &spawn_description,
            move |context, args: SpawnAgentArgs| {
                let control = spawn.clone();
                async move { control.spawn(context, args).await }
            },
        )?;

        let message = self.clone();
        let message_tool = define_tool(
            "message_agent",
            "Send a message to an existing agent. Set `start_turn` to true for a follow-up turn; leave it false to queue guidance without starting work.",
            move |context, args: MessageAgentArgs| {
                let control = message.clone();
                async move { control.message_agent(&context, args).await }
            },
        )?;

        let interrupt = self.clone();
        let interrupt_tool = define_tool(
            "interrupt_agent",
            "Interrupt an agent's current turn and return its previous status. The agent remains available for follow-up tasks.",
            move |context, args: InterruptAgentArgs| {
                let control = interrupt.clone();
                async move { control.interrupt(&context, args).await }
            },
        )?;

        let wait = self.clone();
        let wait_tool = define_tool(
            "wait_agent",
            "Wait for an agent update, or set `timeout_ms` to 0 to return the current snapshot immediately. Completed updates include final messages for review.",
            move |context, args: WaitAgentArgs| {
                let control = wait.clone();
                async move { control.wait(&context, args).await }
            },
        )?;

        Ok(vec![spawn_tool, message_tool, interrupt_tool, wait_tool])
    }

    async fn spawn(&self, context: ToolContext, args: SpawnAgentArgs) -> Result<String, ToolError> {
        validate_task_name(&args.task_name)?;
        if args.message.trim().is_empty() {
            return Err(ToolError::Execution(
                "spawn_agent message cannot be empty".to_string(),
            ));
        }
        let role = args.agent_type.unwrap_or(AgentRole::Default);
        let fork_mode = ForkMode::from_arg(args.fork_turns)?;
        let parent_path = normalized_agent_path(&context.agent.path);
        let task_name = format!("{parent_path}/{}", args.task_name);
        let mut reservation = {
            let mut state = self.inner.state.lock().await;
            state
                .trees
                .entry(context.agent.tree_id)
                .or_default()
                .reserve_spawn(&task_name, self.inner.max_concurrent_children)?;
            drop(state);
            SpawnReservation::new(
                Arc::clone(&self.inner),
                context.agent.tree_id,
                task_name.clone(),
            )
        };
        let (command_tx, command_rx) = mpsc::channel(32);

        let messages = fork_messages(&context.agent.messages, fork_mode);
        let session = match self
            .prepare_child(SpawnRequest {
                role,
                parent_path,
                task_name: task_name.clone(),
                messages,
                tree_id: context.agent.tree_id,
            })
            .await
        {
            Ok(session) => session,
            Err(error) => {
                reservation.release().await;
                return Err(error);
            }
        };
        let id = session.id();

        self.register_child(
            context.agent.tree_id,
            id,
            &task_name,
            role,
            args.message.clone(),
            command_tx,
        )
        .await;
        release_spawn(&self.inner, context.agent.tree_id, &task_name).await;
        reservation.commit();

        let control = self.clone();
        let tree_id = context.agent.tree_id;
        let initial_input = args.message.clone();
        tokio::spawn(async move {
            control
                .run_child(tree_id, id, session, initial_input, command_rx)
                .await;
        });
        self.inner.updates.notify_waiters();
        self.publish().await;

        json_output(&serde_json::json!({
            "agent_id": id,
            "task_name": task_name,
            "agent_type": role,
        }))
    }

    /// Build the child agent and start its runtime. Controller state is not
    /// touched, so the caller decides how to release the spawn reservation on
    /// failure.
    async fn prepare_child(&self, request: SpawnRequest) -> Result<Session, ToolError> {
        let role = request.role;
        let child = self.inner.spawner.spawn(request)?;
        let agent = Self::finalize_child_agent(child.agent, role, self.tools()?);
        child
            .runtime
            .start_with_history(&agent, &child.options, child.history)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))
    }

    fn finalize_child_agent(
        agent: Agent,
        role: AgentRole,
        collaboration_tools: Vec<Arc<dyn Tool>>,
    ) -> Agent {
        match role {
            AgentRole::Explorer => {
                let tools = agent
                    .tools()
                    .iter()
                    .filter(|tool| EXPLORER_TOOL_NAMES.contains(&tool.name()))
                    .cloned()
                    .collect();
                agent.with_tools(tools)
            }
            AgentRole::Default | AgentRole::Worker => agent.pushing_tools(collaboration_tools),
        }
    }

    /// Register the spawned session as a pending child agent in the shared
    /// state.
    async fn register_child(
        &self,
        tree_id: TreeId,
        id: SessionId,
        task_name: &str,
        role: AgentRole,
        last_task_message: String,
        command_tx: mpsc::Sender<ChildCommand>,
    ) {
        let mut state = self.inner.state.lock().await;
        let tree = state.trees.entry(tree_id).or_default();
        tree.agents.insert(
            id,
            ChildRecord {
                id,
                task_name: task_name.to_string(),
                role,
                state: ChildState::Pending,
                pending_followups: 0,
                completion_revision: None,
                last_task_message,
                command_tx,
            },
        );
        drop(state);
    }

    async fn message_agent(
        &self,
        context: &ToolContext,
        args: MessageAgentArgs,
    ) -> Result<String, ToolError> {
        if args.message.trim().is_empty() {
            return Err(ToolError::Execution("message cannot be empty".to_string()));
        }
        let delivery = if args.start_turn.unwrap_or(false) {
            MessageDelivery::Followup
        } else {
            MessageDelivery::Queue
        };
        let (target, command_tx) = {
            let mut state = self.inner.state.lock().await;
            let record = resolve_target_mut(
                &mut state,
                context.agent.tree_id,
                &context.agent.path,
                &args.target,
            )?;
            let result = (record.task_name.clone(), record.command_tx.clone());
            drop(state);
            result
        };
        let permit = command_tx
            .reserve()
            .await
            .map_err(|_| ToolError::Execution(format!("agent is no longer available: {target}")))?;
        {
            let mut state = self.inner.state.lock().await;
            let active_count = state.active_count(context.agent.tree_id);
            let record = resolve_target_mut(
                &mut state,
                context.agent.tree_id,
                &context.agent.path,
                &target,
            )?;
            if let Some(max) = self.inner.max_concurrent_children {
                if delivery.needs_new_slot(&record.state) && active_count >= max {
                    return Err(ToolError::Execution(format!(
                        "maximum of {max} concurrent sub-agents reached"
                    )));
                }
            }
            record.prepare_delivery(delivery, &args.message);
            permit.send(delivery.command(args.message));
            drop(state);
        }
        self.inner.updates.notify_waiters();
        self.publish().await;
        json_output(&serde_json::json!({
            "target": target,
            "queued": true,
            "turn_triggered": delivery.triggers_turn(),
        }))
    }

    async fn interrupt(
        &self,
        context: &ToolContext,
        args: InterruptAgentArgs,
    ) -> Result<String, ToolError> {
        let (target, previous_status, cancel) = {
            let mut state = self.inner.state.lock().await;
            let record = resolve_target_mut(
                &mut state,
                context.agent.tree_id,
                &context.agent.path,
                &args.target,
            )?;
            let result = (
                record.task_name.clone(),
                record.state.status(),
                record.state.cancel(),
            );
            drop(state);
            result
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        json_output(&serde_json::json!({
            "target": target,
            "previous_status": previous_status,
        }))
    }

    async fn wait(&self, context: &ToolContext, args: WaitAgentArgs) -> Result<String, ToolError> {
        let remaining = context
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        let max_timeout_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .min(MAX_WAIT_TIMEOUT_MS);
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
            .min(max_timeout_ms);
        if timeout_ms == 0 {
            let agents = self
                .snapshots(context.agent.tree_id, args.path_prefix.as_deref())
                .await;
            return json_output(&serde_json::json!({
                "agents": agents,
                "timed_out": false,
            }));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let notified = self.inner.updates.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = self.inner.state.lock().await.wait_snapshot(
                context.agent.tree_id,
                &context.agent.path,
                args.path_prefix.as_deref(),
            );
            if snapshot.agents.is_empty() || snapshot.has_update {
                return json_output(&serde_json::json!({
                    "agents": snapshot.agents,
                    "timed_out": false,
                }));
            }

            let timed_out = tokio::select! {
                () = tokio::time::sleep_until(deadline) => true,
                () = &mut notified => false,
            };
            if timed_out {
                let snapshot = self.inner.state.lock().await.wait_snapshot(
                    context.agent.tree_id,
                    &context.agent.path,
                    args.path_prefix.as_deref(),
                );
                return json_output(&serde_json::json!({
                    "agents": snapshot.agents,
                    "timed_out": !snapshot.has_update,
                }));
            }
        }
    }

    pub(crate) async fn snapshots(
        &self,
        tree_id: TreeId,
        path_prefix: Option<&str>,
    ) -> Vec<AgentSnapshot> {
        self.inner
            .state
            .lock()
            .await
            .snapshots(tree_id, path_prefix)
    }

    async fn run_child(
        &self,
        tree_id: TreeId,
        agent_id: SessionId,
        session: Session,
        initial_input: String,
        mut command_rx: mpsc::Receiver<ChildCommand>,
    ) {
        self.run_child_turn(tree_id, agent_id, &session, initial_input, false)
            .await;

        while let Some(command) = command_rx.recv().await {
            match command {
                ChildCommand::Queue(message) => {
                    let _ = session
                        .notify(Input::from_text(InputSource::Agent, message))
                        .await;
                }
                ChildCommand::Followup(message) => {
                    self.run_child_turn(tree_id, agent_id, &session, message, true)
                        .await;
                }
            }
        }
    }

    async fn run_child_turn(
        &self,
        tree_id: TreeId,
        agent_id: SessionId,
        session: &Session,
        input: String,
        is_followup: bool,
    ) {
        let turn = match session
            .submit(Input::from_text(InputSource::Agent, input))
            .await
        {
            Ok(turn) => turn,
            Err(error) => {
                let mut state = self.inner.state.lock().await;
                let _ = state.finish_turn(
                    tree_id,
                    agent_id,
                    ChildState::Errored {
                        final_message: None,
                        error: error.to_string(),
                    },
                    is_followup,
                );
                drop(state);
                self.inner.updates.notify_waiters();
                self.publish().await;
                return;
            }
        };
        let cancel = turn.cancellation_token();
        {
            let mut state = self.inner.state.lock().await;
            let Some(record) = state
                .trees
                .get_mut(&tree_id)
                .and_then(|tree| tree.agents.get_mut(&agent_id))
            else {
                return;
            };
            record.state = ChildState::Running(cancel.clone());
            drop(state);
        }
        self.inner.updates.notify_waiters();
        self.publish().await;

        let result = turn.wait().await;
        let (result, final_message) = match result {
            Ok(view) => {
                let final_message = final_assistant_message(&view.messages);
                (Ok(view.result), final_message)
            }
            Err(error) => (Err(error), None),
        };

        let completed = match result {
            Ok(TurnResult::Completed(StopReason::Aborted) | TurnResult::Interrupted(_)) => {
                ChildState::Interrupted(final_message)
            }
            Ok(TurnResult::Completed(_)) => ChildState::Completed(final_message),
            Ok(TurnResult::Failed(error)) => ChildState::Errored {
                final_message,
                error,
            },
            Err(error) => ChildState::Errored {
                final_message,
                error: error.to_string(),
            },
        };
        let mut state = self.inner.state.lock().await;
        if !state.finish_turn(tree_id, agent_id, completed, is_followup) {
            return;
        }
        drop(state);
        self.inner.updates.notify_waiters();
        self.publish().await;
    }
}

/// Install the collaboration tools on an agent and return the controller.
///
/// # Errors
///
/// Returns `ToolError` when a tool cannot be defined.
pub fn install_subagent_tools(
    agent: Agent,
    options: SessionOptions,
    runtime: Runtime,
    max_concurrent_children: Option<usize>,
) -> Result<(Agent, Option<Arc<AgentControl>>), ToolError> {
    let has_exposed_tools = EXPOSED_COLLABORATION_TOOL_NAMES
        .iter()
        .all(|name| agent.tools().iter().any(|tool| tool.name() == *name));
    let has_legacy_tools = LEGACY_COLLABORATION_TOOL_NAMES
        .iter()
        .any(|name| agent.tools().iter().any(|tool| tool.name() == *name));
    if has_exposed_tools && !has_legacy_tools {
        return Ok((with_multi_agent_instructions(agent), None));
    }
    let agent = agent.without_tools(&all_collaboration_tool_names());
    let agent = with_multi_agent_instructions(agent);
    let spawner = Arc::new(InheritedAgentSpawner {
        runtime,
        system_prompt: agent.system_prompt().map(str::to_string),
        definition: agent.clone(),
        scope: options,
    });
    let control = AgentControl::new(max_concurrent_children, spawner);
    let agent = agent.pushing_tools(control.tools()?);
    Ok((agent, Some(Arc::new(control))))
}

fn with_multi_agent_instructions(agent: Agent) -> Agent {
    let mut prompt = agent.system_prompt().unwrap_or_default().to_string();
    if prompt.contains("<multi_agent_mode>") {
        return agent;
    }
    if !prompt.trim().is_empty() {
        prompt.push_str("\n\n");
    }
    prompt.push_str(MULTI_AGENT_INSTRUCTIONS);
    agent.with_system_prompt(prompt)
}

fn validate_task_name(task_name: &str) -> Result<(), ToolError> {
    let valid = !task_name.is_empty()
        && task_name.len() <= 64
        && task_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(ToolError::Execution(
            "task_name must contain only lowercase letters, digits, and underscores and be at most 64 characters"
                .to_string(),
        ))
    }
}

fn agent_path_matches(record: &ChildRecord, path_prefix: Option<&str>) -> bool {
    path_prefix.is_none_or(|prefix| record.task_name.starts_with(prefix))
}

/// The canonical path of the root agent; every sub-agent path is derived from
/// it. The value mirrors the CLI's default working directory, so a sub-agent
/// spawned without an explicit parent path lives under `/root`.
const ROOT_AGENT_PATH: &str = "/root";

fn normalized_agent_path(agent_path: &str) -> String {
    let path = agent_path.trim().trim_end_matches('/');
    if path.is_empty() {
        ROOT_AGENT_PATH.to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn resolve_target_mut<'a>(
    state: &'a mut ControlState,
    tree_id: TreeId,
    current_agent_path: &str,
    target: &str,
) -> Result<&'a mut ChildRecord, ToolError> {
    let tree = state.trees.get_mut(&tree_id).ok_or_else(|| {
        ToolError::Execution("no sub-agents exist in the current session".to_string())
    })?;
    if let Ok(agent_id) = SessionId::from_str(target) {
        return tree
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| ToolError::Execution(format!("sub-agent not found: {target}")));
    }

    let current_path = normalized_agent_path(current_agent_path);
    let relative_path = format!("{current_path}/{}", target.trim_matches('/'));
    let mut matches = tree
        .agents
        .values_mut()
        .filter(|record| {
            record.task_name == target
                || record.task_name == relative_path
                || record.task_name.rsplit('/').next() == Some(target)
        })
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(ToolError::Execution(format!(
            "sub-agent not found: {target}"
        ))),
        1 => Ok(matches.remove(0)),
        _ => Err(ToolError::Execution(format!(
            "ambiguous sub-agent target: {target}; use its canonical task path or id"
        ))),
    }
}

fn fork_messages(messages: &[Message], mode: ForkMode) -> Vec<Message> {
    let end = complete_history_end(messages);
    let messages = &messages[..end];
    // `start` always satisfies `start <= end`: `end` comes from
    // `complete_history_end` (a user boundary or the full length) while
    // `start` points at a user message inside that range (or 0), so the
    // slices below cannot panic.
    let start = match mode {
        ForkMode::None => return Vec::new(),
        ForkMode::All => 0,
        ForkMode::Last(turns) => messages
            .iter()
            .enumerate()
            .filter(|(_, message)| matches!(message.content, MessageContent::User(_)))
            .map(|(index, _)| index)
            .rev()
            .nth(turns.saturating_sub(1))
            .unwrap_or(0),
    };
    messages[start..].to_vec()
}

fn complete_history_end(messages: &[Message]) -> usize {
    let Some((call_index, call_ids)) =
        messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| {
                let MessageContent::Assistant(blocks) = &message.content else {
                    return None;
                };
                let ids = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall { id, .. } => Some(id),
                        ContentBlock::Text(_) | ContentBlock::Thought { .. } => None,
                    })
                    .collect::<Vec<_>>();
                (!ids.is_empty()).then_some((index, ids))
            })
    else {
        return messages.len();
    };
    let result_ids = messages[call_index + 1..]
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::ToolResult { id, .. } => Some(id),
            MessageContent::User(_) | MessageContent::System(_) | MessageContent::Assistant(_) => {
                None
            }
        })
        .collect::<Vec<_>>();

    if call_ids.iter().all(|call_id| result_ids.contains(call_id)) {
        messages.len()
    } else {
        call_index
    }
}

fn subagent_system_prompt(
    base_prompt: Option<&str>,
    role: AgentRole,
    parent_path: &str,
    task_name: &str,
) -> String {
    let context = format!(
        "<subagent_context>\nYou are `{task_name}`, a `{}` sub-agent spawned by `{parent_path}`. You share the same workspace with the parent and other agents.\n\n{}\n</subagent_context>",
        role.name(),
        role.child_instructions()
    );
    match base_prompt
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
    {
        Some(base_prompt) => format!("{base_prompt}\n\n{context}"),
        None => context,
    }
}

fn final_assistant_message(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        let MessageContent::Assistant(blocks) = &message.content else {
            return None;
        };
        let text = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) if !text.trim().is_empty() => Some(text.as_str()),
                ContentBlock::Text(_)
                | ContentBlock::Thought { .. }
                | ContentBlock::ToolCall { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    })
}

fn json_output(value: &serde_json::Value) -> Result<String, ToolError> {
    serde_json::to_string_pretty(value)
        .map_err(|error| ToolError::Execution(format!("failed to serialize agent result: {error}")))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ash_core::{AgentToolContext, ModelClient, ModelId};
    use ash_protocol::{create_adapter, Protocol, ProviderConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    struct RejectingSpawner;

    impl AgentSpawner for RejectingSpawner {
        fn spawn(&self, _request: SpawnRequest) -> Result<ChildAgent, ToolError> {
            Err(ToolError::Execution(
                "spawn is not used by this test".to_string(),
            ))
        }
    }

    fn state_only_control() -> AgentControl {
        AgentControl::new(None, Arc::new(RejectingSpawner))
    }

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct NoToolArgs {}

    fn named_tool(name: &str) -> Arc<dyn Tool> {
        define_tool(name, "test tool", |_context, _args: NoToolArgs| async {
            Ok::<_, ToolError>(String::new())
        })
        .unwrap()
    }

    fn tool_names(agent: &Agent) -> Vec<&str> {
        agent.tools().iter().map(|tool| tool.name()).collect()
    }

    fn make_agent() -> Agent {
        Agent::new(ModelId::new("test-model"), Vec::new())
            .with_system_prompt("base prompt")
            .with_max_turns(10)
            .with_max_context_tokens(200_000)
    }

    fn make_options() -> SessionOptions {
        SessionOptions {
            working_dir: PathBuf::from("."),
            tool_timeout: Duration::from_secs(5),
            ..SessionOptions::default()
        }
    }

    fn make_context(tree_id: TreeId) -> ToolContext {
        ToolContext {
            session_id: SessionId::new(),
            turn_id: ash_core::TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + Duration::from_secs(5),
            agent: AgentToolContext {
                tree_id,
                path: "/root".to_string(),
                messages: Vec::new(),
            },
        }
    }

    fn make_model(base_url: Option<String>) -> Arc<dyn ModelClient> {
        create_adapter(ProviderConfig {
            protocol: Protocol::Responses,
            api_key: "test".into(),
            base_url,
        })
    }

    fn make_control(
        model: Arc<dyn ModelClient>,
        max_turns: u32,
    ) -> (AgentControl, tempfile::TempDir) {
        // Keep test session files out of the real data directory
        // (`~/.local/share/ash/sessions`); the returned TempDir stays alive for
        // the whole test so spawned sub-agents keep a valid store.
        let directory = tempfile::TempDir::new().expect("create temp session directory");
        let runtime = Runtime::new(model, "test").with_session_store(Arc::new(
            ash_agent::JsonlSessionStore::new(directory.path()),
        ));
        let agent = make_agent().with_max_turns(max_turns);
        let options = make_options();
        let spawner = Arc::new(InheritedAgentSpawner {
            runtime,
            system_prompt: agent.system_prompt().map(str::to_string),
            definition: agent,
            scope: options,
        });
        (AgentControl::new(None, spawner), directory)
    }

    #[test]
    fn exposes_the_codex_0_144_3_builtin_roles() {
        assert_eq!(AgentRole::from_str("default").unwrap(), AgentRole::Default);
        assert_eq!(
            AgentRole::from_str("explorer").unwrap(),
            AgentRole::Explorer
        );
        assert_eq!(AgentRole::from_str("worker").unwrap(), AgentRole::Worker);
        assert!(AgentRole::from_str("awaiter").is_err());
    }

    #[test]
    fn explorer_final_tools_are_an_explicit_read_only_allowlist() {
        let inherited = [
            "read",
            "glob",
            "grep",
            "webfetch",
            "skill",
            "write",
            "edit",
            "bash",
            "custom_mutator",
        ]
        .into_iter()
        .map(named_tool)
        .collect();
        let collaboration = EXPOSED_COLLABORATION_TOOL_NAMES
            .into_iter()
            .map(named_tool)
            .collect();
        let agent = AgentControl::finalize_child_agent(
            make_agent().with_tools(inherited),
            AgentRole::Explorer,
            collaboration,
        );

        assert_eq!(
            tool_names(&agent),
            ["read", "glob", "grep", "webfetch", "skill"]
        );
        assert!(!tool_names(&agent).contains(&"custom_mutator"));
        assert!(!tool_names(&agent).contains(&"spawn_agent"));
    }

    #[test]
    fn default_and_worker_keep_inherited_and_collaboration_tools() {
        for role in [AgentRole::Default, AgentRole::Worker] {
            let inherited = ["read", "write", "custom_tool"]
                .into_iter()
                .map(named_tool)
                .collect();
            let collaboration = EXPOSED_COLLABORATION_TOOL_NAMES
                .into_iter()
                .map(named_tool)
                .collect();
            let agent = AgentControl::finalize_child_agent(
                make_agent().with_tools(inherited),
                role,
                collaboration,
            );

            assert_eq!(
                tool_names(&agent),
                [
                    "read",
                    "write",
                    "custom_tool",
                    "spawn_agent",
                    "message_agent",
                    "interrupt_agent",
                    "wait_agent",
                ],
                "{role:?}"
            );
        }
    }

    #[test]
    fn installs_the_codex_style_collaboration_tools_and_prompt() {
        let options = make_options();
        let runtime = Runtime::new(make_model(None), "test");
        let (agent, _) =
            install_subagent_tools(make_agent(), options.clone(), runtime.clone(), None).unwrap();
        let (agent, _) = install_subagent_tools(agent, options, runtime, None).unwrap();
        let names = agent
            .tools()
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "spawn_agent",
                "message_agent",
                "interrupt_agent",
                "wait_agent",
            ]
        );
        let spawn = agent.tools()[0].definition();
        assert!(spawn.description.contains("explorer"));
        assert!(spawn.description.contains("worker"));
        assert!(spawn.description.contains("Runtime -> Session -> Turn"));
        assert!(spawn.description.contains("continue non-overlapping work"));
        for property in ["task_name", "message", "agent_type", "fork_turns"] {
            assert!(spawn.parameters_schema["properties"]
                .get(property)
                .is_some());
        }
        let message = agent.tools()[1].definition();
        for property in ["target", "message", "start_turn"] {
            assert!(message.parameters_schema["properties"]
                .get(property)
                .is_some());
        }
        let wait = agent.tools()[3].definition();
        for property in ["timeout_ms", "path_prefix"] {
            assert!(wait.parameters_schema["properties"].get(property).is_some());
        }
        assert!(agent
            .system_prompt()
            .unwrap()
            .contains("<multi_agent_mode>"));
        assert!(agent
            .system_prompt()
            .unwrap()
            .contains("Delegation is available"));
        assert_eq!(
            agent
                .system_prompt()
                .unwrap()
                .matches("<multi_agent_mode>")
                .count(),
            1
        );
    }

    #[test]
    fn validates_canonical_task_name_segments() {
        assert!(validate_task_name("parser_tests").is_ok());
        assert!(validate_task_name("worker2").is_ok());
        assert!(validate_task_name("Parser").is_err());
        assert!(validate_task_name("parser-tests").is_err());
        assert!(validate_task_name("").is_err());
    }

    #[tokio::test]
    async fn failed_spawn_releases_the_task_name_reservation() {
        let control = state_only_control();
        let context = make_context(TreeId::new());

        for _ in 0..2 {
            let error = control
                .spawn(
                    context.clone(),
                    SpawnAgentArgs {
                        task_name: "inspect".to_string(),
                        message: "Inspect the parser.".to_string(),
                        agent_type: None,
                        fork_turns: None,
                    },
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("spawn is not used"));
        }
    }

    #[tokio::test]
    async fn concurrency_limit_rejects_before_calling_the_spawner() {
        let control = AgentControl::new(Some(0), Arc::new(RejectingSpawner));
        let error = control
            .spawn(
                make_context(TreeId::new()),
                SpawnAgentArgs {
                    task_name: "inspect".to_string(),
                    message: "Inspect the parser.".to_string(),
                    agent_type: None,
                    fork_turns: None,
                },
            )
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("maximum of 0 concurrent sub-agents reached"));
    }

    #[tokio::test]
    async fn duplicate_reservation_rejects_before_calling_the_spawner() {
        let control = state_only_control();
        let tree_id = TreeId::new();
        control
            .inner
            .state
            .lock()
            .await
            .trees
            .entry(tree_id)
            .or_default()
            .pending_spawns
            .insert("/root/inspect".to_string());

        let error = control
            .spawn(
                make_context(tree_id),
                SpawnAgentArgs {
                    task_name: "inspect".to_string(),
                    message: "Inspect the parser.".to_string(),
                    agent_type: None,
                    fork_turns: None,
                },
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("task name already exists"));
    }

    #[test]
    fn multiple_queued_followups_publish_completion_only_after_the_last_turn() {
        let agent_id = SessionId::new();
        let (command_tx, _command_rx) = mpsc::channel(2);
        let mut record = ChildRecord {
            id: agent_id,
            task_name: "/root/inspect".to_string(),
            role: AgentRole::Explorer,
            state: ChildState::Running(CancellationToken::new()),
            pending_followups: 0,
            completion_revision: None,
            last_task_message: "initial".to_string(),
            command_tx,
        };
        record.prepare_delivery(MessageDelivery::Followup, "first follow-up");
        record.prepare_delivery(MessageDelivery::Followup, "second follow-up");
        let mut tree = AgentTreeState::default();
        tree.agents.insert(agent_id, record);

        assert!(tree.finish_turn(
            agent_id,
            ChildState::Completed(Some("initial result".to_string())),
            false,
        ));
        let record = tree.agents.get_mut(&agent_id).unwrap();
        assert_eq!(record.pending_followups, 2);
        assert!(matches!(record.state, ChildState::Pending));
        assert_eq!(record.completion_revision, None);

        record.state = ChildState::Running(CancellationToken::new());
        assert!(tree.finish_turn(
            agent_id,
            ChildState::Completed(Some("first result".to_string())),
            true,
        ));
        let record = tree.agents.get_mut(&agent_id).unwrap();
        assert_eq!(record.pending_followups, 1);
        assert!(matches!(record.state, ChildState::Pending));
        assert_eq!(record.completion_revision, None);

        record.state = ChildState::Running(CancellationToken::new());
        assert!(tree.finish_turn(
            agent_id,
            ChildState::Completed(Some("final result".to_string())),
            true,
        ));
        let record = tree.agents.get(&agent_id).unwrap();
        assert_eq!(record.pending_followups, 0);
        assert!(matches!(
            &record.state,
            ChildState::Completed(Some(message)) if message == "final result"
        ));
        assert_eq!(record.completion_revision, Some(1));
    }

    #[tokio::test]
    async fn failed_delivery_does_not_change_the_agent_lifecycle() {
        let control = state_only_control();
        let tree_id = TreeId::new();
        let agent_id = SessionId::new();
        let (command_tx, command_rx) = mpsc::channel(1);
        drop(command_rx);
        control
            .inner
            .state
            .lock()
            .await
            .trees
            .entry(tree_id)
            .or_default()
            .agents
            .insert(
                agent_id,
                ChildRecord {
                    id: agent_id,
                    task_name: "/root/inspect".to_string(),
                    role: AgentRole::Explorer,
                    state: ChildState::Completed(Some("done".to_string())),
                    pending_followups: 0,
                    completion_revision: None,
                    last_task_message: "original".to_string(),
                    command_tx,
                },
            );

        let result = control
            .message_agent(
                &make_context(tree_id),
                MessageAgentArgs {
                    target: "/root/inspect".to_string(),
                    message: "follow up".to_string(),
                    start_turn: Some(true),
                },
            )
            .await;

        assert!(result.is_err());
        let agents = control.snapshots(tree_id, None).await;
        assert_eq!(agents[0].status, AgentStatus::Completed);
        assert_eq!(agents[0].last_task_message, "original");
    }

    #[tokio::test]
    async fn message_agent_can_queue_guidance_without_starting_a_turn() {
        let control = state_only_control();
        let tree_id = TreeId::new();
        let agent_id = SessionId::new();
        let (command_tx, mut command_rx) = mpsc::channel(1);
        control
            .inner
            .state
            .lock()
            .await
            .trees
            .entry(tree_id)
            .or_default()
            .agents
            .insert(
                agent_id,
                ChildRecord {
                    id: agent_id,
                    task_name: "/root/inspect".to_string(),
                    role: AgentRole::Explorer,
                    state: ChildState::Completed(Some("done".to_string())),
                    pending_followups: 0,
                    completion_revision: Some(1),
                    last_task_message: "original".to_string(),
                    command_tx,
                },
            );

        let result = control
            .message_agent(
                &make_context(tree_id),
                MessageAgentArgs {
                    target: "/root/inspect".to_string(),
                    message: "keep this in mind".to_string(),
                    start_turn: None,
                },
            )
            .await
            .unwrap();

        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(!result["turn_triggered"].as_bool().unwrap());
        assert!(matches!(
            command_rx.recv().await.unwrap(),
            ChildCommand::Queue(message) if message == "keep this in mind"
        ));
        let agents = control.snapshots(tree_id, None).await;
        assert_eq!(agents[0].status, AgentStatus::Completed);
        assert_eq!(agents[0].last_task_message, "keep this in mind");
    }

    #[test]
    fn forks_all_none_or_recent_complete_turns() {
        let call_id = ash_core::ToolCallId::from_provider("spawn");
        let messages = vec![
            Message::user("first"),
            Message::assistant_text("first answer"),
            Message::user("second"),
            Message::assistant_text("second answer"),
            Message::assistant(vec![ContentBlock::ToolCall {
                id: call_id,
                name: "spawn_agent".to_string(),
                arguments: serde_json::json!({}),
            }]),
        ];

        assert!(fork_messages(&messages, ForkMode::None).is_empty());
        assert_eq!(fork_messages(&messages, ForkMode::All).len(), 4);
        let recent = fork_messages(&messages, ForkMode::Last(1));
        assert_eq!(recent.len(), 2);
        assert!(matches!(recent[0].content, MessageContent::User(_)));
    }

    #[test]
    fn excludes_an_entire_tool_group_until_every_result_exists() {
        let first_call = ash_core::ToolCallId::from_provider("first");
        let second_call = ash_core::ToolCallId::from_provider("second");
        let messages = vec![
            Message::user("first turn"),
            Message::assistant_text("first answer"),
            Message::user("second turn"),
            Message::assistant(vec![
                ContentBlock::ToolCall {
                    id: first_call.clone(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                },
                ContentBlock::ToolCall {
                    id: second_call.clone(),
                    name: "spawn_agent".to_string(),
                    arguments: serde_json::json!({}),
                },
            ]),
            Message::tool_result(first_call, Ok("done".to_string()), Vec::new()),
        ];

        let forked = fork_messages(&messages, ForkMode::All);

        assert_eq!(forked.len(), 3);
        assert!(matches!(forked[2].content, MessageContent::User(_)));

        let mut complete = messages;
        complete.push(Message::tool_result(
            second_call,
            Ok("spawned".to_string()),
            Vec::new(),
        ));
        assert_eq!(fork_messages(&complete, ForkMode::All).len(), 6);
    }

    #[tokio::test]
    async fn isolates_agent_trees_by_root_session() {
        let control = state_only_control();
        let root = TreeId::new();
        let other_root = TreeId::new();
        let agent_id = SessionId::new();
        let (command_tx, _command_rx) = mpsc::channel(1);
        control
            .inner
            .state
            .lock()
            .await
            .trees
            .entry(root)
            .or_default()
            .agents
            .insert(
                agent_id,
                ChildRecord {
                    id: agent_id,
                    task_name: "/root/inspect".to_string(),
                    role: AgentRole::Explorer,
                    state: ChildState::Completed(Some("done".to_string())),
                    pending_followups: 0,
                    completion_revision: None,
                    last_task_message: "inspect".to_string(),
                    command_tx,
                },
            );

        assert_eq!(control.snapshots(root, None).await.len(), 1);
        assert!(control.snapshots(other_root, None).await.is_empty());
    }

    #[tokio::test]
    async fn wait_returns_each_completion_only_once() {
        let control = state_only_control();
        let tree_id = TreeId::new();
        let completed_id = SessionId::new();
        let running_id = SessionId::new();
        let (completed_tx, _completed_rx) = mpsc::channel(1);
        let (running_tx, _running_rx) = mpsc::channel(1);
        {
            let mut state = control.inner.state.lock().await;
            let tree = state.trees.entry(tree_id).or_default();
            tree.next_completion_revision = 1;
            tree.agents.insert(
                completed_id,
                ChildRecord {
                    id: completed_id,
                    task_name: "/root/completed".to_string(),
                    role: AgentRole::Explorer,
                    state: ChildState::Completed(Some("first result".to_string())),
                    pending_followups: 0,
                    completion_revision: Some(1),
                    last_task_message: "first task".to_string(),
                    command_tx: completed_tx,
                },
            );
            tree.agents.insert(
                running_id,
                ChildRecord {
                    id: running_id,
                    task_name: "/root/running".to_string(),
                    role: AgentRole::Worker,
                    state: ChildState::Running(CancellationToken::new()),
                    pending_followups: 0,
                    completion_revision: None,
                    last_task_message: "second task".to_string(),
                    command_tx: running_tx,
                },
            );
            drop(state);
        }

        let context = make_context(tree_id);
        let first = control
            .wait(
                &context,
                WaitAgentArgs {
                    timeout_ms: Some(100),
                    path_prefix: None,
                },
            )
            .await
            .unwrap();
        assert!(
            !serde_json::from_str::<serde_json::Value>(&first).unwrap()["timed_out"]
                .as_bool()
                .unwrap()
        );

        let waiting_control = control.clone();
        let waiting_context = context.clone();
        let waiting = tokio::spawn(async move {
            waiting_control
                .wait(
                    &waiting_context,
                    WaitAgentArgs {
                        timeout_ms: Some(1_000),
                        path_prefix: None,
                    },
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());

        {
            let mut state = control.inner.state.lock().await;
            assert!(state.finish_turn(
                tree_id,
                running_id,
                ChildState::Completed(Some("second result".to_string())),
                false,
            ));
            drop(state);
        }
        control.inner.updates.notify_waiters();

        let second = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let second: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert!(!second["timed_out"].as_bool().unwrap());
        assert_eq!(second["agents"][1]["final_message"], "second result");
    }

    #[tokio::test]
    async fn zero_timeout_wait_lists_without_advancing_completion_cursor() {
        let control = state_only_control();
        let tree_id = TreeId::new();
        let agent_id = SessionId::new();
        let (command_tx, _command_rx) = mpsc::channel(1);
        {
            let mut state = control.inner.state.lock().await;
            let tree = state.trees.entry(tree_id).or_default();
            tree.next_completion_revision = 1;
            tree.agents.insert(
                agent_id,
                ChildRecord {
                    id: agent_id,
                    task_name: "/root/completed".to_string(),
                    role: AgentRole::Explorer,
                    state: ChildState::Completed(Some("result".to_string())),
                    pending_followups: 0,
                    completion_revision: Some(1),
                    last_task_message: "task".to_string(),
                    command_tx,
                },
            );
            drop(state);
        }

        let context = make_context(tree_id);
        let snapshot = control
            .wait(
                &context,
                WaitAgentArgs {
                    timeout_ms: Some(0),
                    path_prefix: None,
                },
            )
            .await
            .unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
        assert!(!snapshot["timed_out"].as_bool().unwrap());
        assert_eq!(snapshot["agents"][0]["final_message"], "result");

        let completion = control
            .wait(
                &context,
                WaitAgentArgs {
                    timeout_ms: Some(100),
                    path_prefix: None,
                },
            )
            .await
            .unwrap();
        let completion: serde_json::Value = serde_json::from_str(&completion).unwrap();
        assert!(!completion["timed_out"].as_bool().unwrap());
        assert_eq!(completion["agents"][0]["final_message"], "result");
    }

    #[tokio::test]
    async fn runs_a_spawned_agent_and_captures_its_final_message() {
        let (base_url, server) = mock_model_server(&["child done", "followup done"]).await;
        let (control, _directory) = make_control(make_model(Some(base_url)), 2);
        let tree_id = TreeId::new();
        let context = ToolContext {
            session_id: SessionId::new(),
            turn_id: ash_core::TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + Duration::from_secs(5),
            agent: AgentToolContext {
                tree_id,
                path: "/root".to_string(),
                messages: vec![Message::user("delegate this")],
            },
        };
        let followup_context = context.clone();
        control
            .spawn(
                context,
                SpawnAgentArgs {
                    task_name: "inspect".to_string(),
                    message: "Inspect the parser.".to_string(),
                    agent_type: Some(AgentRole::Explorer),
                    fork_turns: Some(ForkTurnsArg::Keyword(ForkTurnsKeyword::All)),
                },
            )
            .await
            .unwrap();

        let completed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let agents = control.snapshots(tree_id, None).await;
                if agents
                    .first()
                    .is_some_and(|agent| agent.status == AgentStatus::Completed)
                {
                    break agents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(completed[0].final_message.as_deref(), Some("child done"));

        control
            .message_agent(
                &followup_context,
                MessageAgentArgs {
                    target: "/root/inspect".to_string(),
                    message: "Confirm the finding.".to_string(),
                    start_turn: Some(true),
                },
            )
            .await
            .unwrap();
        let followed_up = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let agents = control.snapshots(tree_id, None).await;
                if agents.first().is_some_and(|agent| {
                    agent.status == AgentStatus::Completed
                        && agent.final_message.as_deref() == Some("followup done")
                }) {
                    break agents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            followed_up[0].final_message.as_deref(),
            Some("followup done")
        );

        let requests = server.await.unwrap();
        assert!(requests[0].contains("<subagent_context>"));
        assert!(requests[0].contains("explorer"));
        assert!(requests[0].contains("Inspect the parser."));
        assert!(requests[1].contains("child done"));
        assert!(requests[1].contains("Confirm the finding."));
    }

    /// Answers each accepted connection with a single completed model response
    /// and returns the base URL plus a handle to the collected request bodies.
    async fn mock_model_server(
        responses: &[&str],
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let (address_tx, address_rx) = tokio::sync::oneshot::channel();
        let responses = responses
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>();
        let server = tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let _ = address_tx.send(address);
            let mut requests = Vec::new();
            for answer in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let Some(headers_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if request.len() >= headers_end + 4 + content_length {
                        break;
                    }
                }
                let body = format!(
                    "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{answer}\"}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}}}\n\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                requests.push(String::from_utf8(request).unwrap());
            }
            requests
        });
        let address = address_rx.await.unwrap();
        (format!("http://{address}"), server)
    }

    #[tokio::test]
    async fn subscribers_receive_subagent_snapshot_updates() {
        let (base_url, _server) = mock_model_server(&["child done"]).await;
        let (control, _directory) = make_control(make_model(Some(base_url)), 1);
        let tree_id = TreeId::new();
        let mut snapshots = control.subscribe();
        let context = ToolContext {
            session_id: SessionId::new(),
            turn_id: ash_core::TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + Duration::from_secs(5),
            agent: AgentToolContext {
                tree_id,
                path: "/root".to_string(),
                messages: vec![Message::user("delegate this")],
            },
        };
        control
            .spawn(
                context,
                SpawnAgentArgs {
                    task_name: "inspect".to_string(),
                    message: "Inspect the parser.".to_string(),
                    agent_type: Some(AgentRole::Explorer),
                    fork_turns: None,
                },
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.has_changed().unwrap_or(false) {
                    let latest = snapshots.borrow_and_update();
                    if latest.iter().any(|snapshot| {
                        snapshot.task_name == "/root/inspect"
                            && snapshot.agent_type == "explorer"
                            && snapshot.state == SubagentState::Completed
                    }) {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("sub-agent snapshots never reached Completed");
    }

    #[tokio::test]
    async fn default_configuration_allows_more_than_three_concurrent_subagents() {
        let (base_url, _server) = mock_model_server(&["a", "b", "c", "d"]).await;
        let (control, _directory) = make_control(make_model(Some(base_url)), 1);
        let tree_id = TreeId::new();
        let context = ToolContext {
            session_id: SessionId::new(),
            turn_id: ash_core::TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + Duration::from_secs(5),
            agent: AgentToolContext {
                tree_id,
                path: "/root".to_string(),
                messages: vec![Message::user("delegate this")],
            },
        };
        for index in 0..4 {
            control
                .spawn(
                    context.clone(),
                    SpawnAgentArgs {
                        task_name: format!("worker_{index}"),
                        message: format!("Do task {index}."),
                        agent_type: None,
                        fork_turns: None,
                    },
                )
                .await
                .expect("spawning a fourth concurrent sub-agent should not be limited by default");
        }

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let agents = control.snapshots(tree_id, None).await;
                if agents.len() == 4
                    && agents
                        .iter()
                        .all(|agent| agent.status == AgentStatus::Completed)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("all four sub-agents should complete");
    }
}
