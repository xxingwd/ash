use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use crate::snapshot::{SubagentSnapshot, SubagentState};
use ash_agent::{Agent, Input, InputSource, Runtime, Session, SessionOptions, Turn};
use ash_core::{
    define_tool, is_valid_segment, AgentPath, CancellationToken, ContentBlock, Message,
    MessageContent, ModelId, SessionId, SessionIdentity, StopReason, Tool, ToolContext, ToolError,
    TurnResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex, Notify};

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;
/// Tools whose public contracts are read-only. Explorer projection is an
/// allowlist: unknown custom and MCP tools are excluded unless their behavior
/// is represented by one of these canonical tool names.
const EXPLORER_TOOL_NAMES: [&str; 5] = ["read", "glob", "grep", "webfetch", "skill"];

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

/// Schema-facing name of one built-in profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProfileName {
    Default,
    Explorer,
    Worker,
}

impl std::str::FromStr for ProfileName {
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

/// How a profile filters the inherited tool set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolPolicy {
    /// Keep every inherited business tool.
    Inherit,
    /// Keep only the named inherited tools.
    Allow(&'static [&'static str]),
}

impl ToolPolicy {
    fn apply(self, agent: Agent) -> Agent {
        match self {
            Self::Inherit => agent,
            Self::Allow(names) => {
                let tools = agent
                    .tools()
                    .iter()
                    .filter(|tool| names.contains(&tool.name()))
                    .cloned()
                    .collect();
                agent.with_tools(tools)
            }
        }
    }
}

/// Data-driven description of one class of child session: a name, a
/// description for the parent's tool documentation, a prompt overlay, and a
/// tool policy. The session execution engine stays untouched when a new
/// profile is added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentProfile {
    pub name: &'static str,
    pub description: &'static str,
    pub prompt_overlay: &'static str,
    pub tool_policy: ToolPolicy,
    /// Optional model override; `None` inherits the parent's model.
    pub model: Option<ModelId>,
    /// Optional turn limit; `None` inherits the parent's limit.
    pub max_turns: Option<u32>,
}

impl AgentProfile {
    const fn builtin(
        name: &'static str,
        description: &'static str,
        prompt_overlay: &'static str,
        tool_policy: ToolPolicy,
    ) -> Self {
        Self {
            name,
            description,
            prompt_overlay,
            tool_policy,
            model: None,
            max_turns: None,
        }
    }

    const fn default() -> Self {
        Self::builtin(
            "default",
            "General-purpose agent for a self-contained task that inherits the current configuration.",
            "Handle the assigned task directly. Stay within its scope and return a concise, evidence-backed result to the parent agent.",
            ToolPolicy::Inherit,
        )
    }

    const fn explorer() -> Self {
        Self::builtin(
            "explorer",
            "Use whenever a specific, well-scoped codebase question can be answered independently. Explorers are fast, read-only, and authoritative. Spawn multiple explorers in the same round for distinct questions; reuse an existing explorer for related follow-ups.",
            "Answer the assigned codebase question through read-only inspection. Do not edit files. Return concrete findings with relevant paths and symbols, and do not broaden the investigation beyond the question.",
            ToolPolicy::Allow(&EXPLORER_TOOL_NAMES),
        )
    }

    const fn worker() -> Self {
        Self::builtin(
            "worker",
            "Prefer for bounded implementation and production work such as features, fixes, tests, and refactors. Assign explicit file or module ownership, keep write scopes disjoint, and remind workers that the workspace is shared.",
            "Execute the assigned implementation or production task. Respect the stated file or module ownership, preserve unrelated workspace changes, verify your work, and report changed files plus validation results.",
            ToolPolicy::Inherit,
        )
    }

    const fn for_name(name: ProfileName) -> Self {
        match name {
            ProfileName::Default => Self::default(),
            ProfileName::Explorer => Self::explorer(),
            ProfileName::Worker => Self::worker(),
        }
    }

    const fn available_profiles_description() -> &'static str {
        r"Optional type name for the new agent. If omitted, `default` is used.
Available roles:
default: General-purpose agent for a self-contained task that inherits the current configuration.
explorer: Use whenever a specific, well-scoped codebase question can be answered independently. Explorers are fast, read-only, and authoritative. Spawn multiple explorers in the same round for distinct questions; reuse an existing explorer for related follow-ups.
worker: Prefer for bounded implementation and production work such as features, fixes, tests, and refactors. Assign explicit file or module ownership, keep write scopes disjoint, and remind workers that the workspace is shared."
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionSnapshot {
    pub session_id: SessionId,
    pub task_name: String,
    pub agent_type: &'static str,
    pub status: SubagentState,
    pub last_task_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<SessionSnapshot> for SubagentSnapshot {
    fn from(snapshot: SessionSnapshot) -> Self {
        Self {
            task_name: snapshot.task_name,
            agent_type: snapshot.agent_type.to_string(),
            state: snapshot.status,
            last_task_message: snapshot.last_task_message,
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
    spawner: Arc<dyn ChildSessionFactory>,
    subagent_tx: watch::Sender<Vec<SubagentSnapshot>>,
}

struct SpawnReservation {
    inner: Arc<ControlInner>,
    root_id: SessionId,
    task_name: Option<String>,
}

/// Remove a pending spawn reservation from the shared state. Used by the
/// reservation's release path, its `Drop` impl, and the successful spawn path
/// once the child agent has been registered.
async fn release_spawn(inner: &Arc<ControlInner>, root_id: SessionId, task_name: &str) {
    let mut state = inner.state.lock().await;
    if let Some(tree) = state.trees.get_mut(&root_id) {
        tree.pending_spawns.remove(task_name);
    }
}

impl SpawnReservation {
    const fn new(inner: Arc<ControlInner>, root_id: SessionId, task_name: String) -> Self {
        Self {
            inner,
            root_id,
            task_name: Some(task_name),
        }
    }

    async fn release(&mut self) {
        let Some(task_name) = self.task_name.take() else {
            return;
        };
        release_spawn(&self.inner, self.root_id, &task_name).await;
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
        let root_id = self.root_id;
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    release_spawn(&inner, root_id, &task_name).await;
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

pub(crate) struct ChildSessionSpec {
    pub runtime: Runtime,
    pub agent: Agent,
    pub options: SessionOptions,
    pub history: Vec<Message>,
}

pub(crate) trait ChildSessionFactory: Send + Sync {
    fn create(
        &self,
        profile: &AgentProfile,
        history: Vec<Message>,
    ) -> Result<ChildSessionSpec, ToolError>;
}

struct InheritedSessionFactory {
    runtime: Runtime,
    definition: Agent,
    scope: SessionOptions,
}

impl ChildSessionFactory for InheritedSessionFactory {
    fn create(
        &self,
        profile: &AgentProfile,
        history: Vec<Message>,
    ) -> Result<ChildSessionSpec, ToolError> {
        let definition = self
            .definition
            .clone()
            .with_system_prompt(profile_system_prompt(
                self.definition.system_prompt(),
                profile,
            ));
        Ok(ChildSessionSpec {
            runtime: self.runtime.clone(),
            agent: definition,
            options: self.scope.clone(),
            history,
        })
    }
}

#[derive(Default)]
struct ControlState {
    /// One tree per root session, keyed by the root session id.
    trees: HashMap<SessionId, AgentTreeState>,
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
    task_path: AgentPath,
    profile: AgentProfile,
    /// The durable session backing this child. Turn ordering is entirely the
    /// session actor's job; the controller never queues turns itself.
    session: Session,
    /// Turns submitted to `session` that have not yet settled, including any
    /// currently running turn. A queued follow-up keeps this above zero, so a
    /// child with pending work holds its concurrency slot.
    active_turns: usize,
    /// Cancellation handle of the currently running turn, if any.
    active_cancel: Option<CancellationToken>,
    /// Latest terminal outcome, present only when `active_turns` reached zero.
    terminal: Option<ChildState>,
    completion_revision: Option<u64>,
    last_task_message: String,
}

/// Terminal outcome of a child agent, reached only after every accepted turn
/// (including queued follow-ups) has settled.
enum ChildState {
    Completed(Option<String>),
    Interrupted(Option<String>),
    Errored {
        final_message: Option<String>,
        error: String,
    },
}

impl ChildState {
    const fn status(&self) -> SubagentState {
        match self {
            Self::Completed(_) => SubagentState::Completed,
            Self::Interrupted(_) => SubagentState::Interrupted,
            Self::Errored { .. } => SubagentState::Errored,
        }
    }

    fn final_message(&self) -> Option<&str> {
        match self {
            Self::Completed(message) | Self::Interrupted(message) => message.as_deref(),
            Self::Errored { final_message, .. } => final_message.as_deref(),
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Self::Errored { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl ChildRecord {
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            session_id: self.id,
            task_name: self.task_path.to_string(),
            agent_type: self.profile.name,
            status: self.status(),
            last_task_message: self.last_task_message.clone(),
            final_message: self
                .terminal
                .as_ref()
                .and_then(ChildState::final_message)
                .map(str::to_string),
            error: self
                .terminal
                .as_ref()
                .and_then(ChildState::error)
                .map(str::to_string),
        }
    }

    fn status(&self) -> SubagentState {
        if self.active_turns > 0 {
            SubagentState::Running
        } else {
            self.terminal
                .as_ref()
                .map_or(SubagentState::Pending, ChildState::status)
        }
    }

    /// Whether the child is busy enough to hold a concurrency slot. A child
    /// with queued follow-ups keeps its slot until they all settle.
    fn holds_slot(&self) -> bool {
        self.active_turns > 0
    }
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

    fn needs_new_slot(self, record: &ChildRecord) -> bool {
        self.triggers_turn() && !record.holds_slot()
    }
}

fn sorted_snapshots<'a>(records: impl Iterator<Item = &'a ChildRecord>) -> Vec<SessionSnapshot> {
    let mut agents = records.map(ChildRecord::snapshot).collect::<Vec<_>>();
    agents.sort_by(|left, right| left.task_name.cmp(&right.task_name));
    agents
}

impl ControlState {
    fn active_count(&self, root_id: SessionId) -> usize {
        self.trees
            .get(&root_id)
            .map_or(0, AgentTreeState::active_count)
    }

    fn snapshots(&self, root_id: SessionId, path_prefix: Option<&str>) -> Vec<SessionSnapshot> {
        sorted_snapshots(
            self.trees
                .get(&root_id)
                .into_iter()
                .flat_map(|tree| tree.agents.values())
                .filter(|record| agent_path_matches(record, path_prefix)),
        )
    }

    fn wait_snapshot(
        &mut self,
        root_id: SessionId,
        waiter: &str,
        path_prefix: Option<&str>,
    ) -> WaitSnapshot {
        self.trees
            .get_mut(&root_id)
            .map_or_else(WaitSnapshot::empty, |tree| {
                tree.wait_snapshot(waiter, path_prefix)
            })
    }
}

impl AgentTreeState {
    fn active_count(&self) -> usize {
        self.agents
            .values()
            .filter(|record| record.holds_slot())
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
                .any(|record| record.task_path.as_str() == task_name)
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
            .filter(|record| !record.holds_slot())
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

    /// Fold one settled turn into a child record. The child reaches terminal
    /// state (and bumps the completion revision) only after every accepted
    /// turn, including queued follow-ups, has settled, so waiters never
    /// observe an intermediate completion. Returns whether it went terminal.
    fn settle_turn(&mut self, session_id: SessionId, terminal: ChildState) -> bool {
        let Some(record) = self.agents.get_mut(&session_id) else {
            return false;
        };
        record.active_turns = record.active_turns.saturating_sub(1);
        record.active_cancel = None;
        if record.active_turns > 0 {
            return false;
        }
        record.terminal = Some(terminal);
        self.next_completion_revision = self.next_completion_revision.saturating_add(1);
        record.completion_revision = Some(self.next_completion_revision);
        true
    }
}

struct WaitSnapshot {
    agents: Vec<SessionSnapshot>,
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
    agent_type: Option<ProfileName>,
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
        spawner: Arc<dyn ChildSessionFactory>,
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
            AgentProfile::available_profiles_description()
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
        let profile = AgentProfile::for_name(args.agent_type.unwrap_or(ProfileName::Default));
        let fork_mode = ForkMode::from_arg(args.fork_turns)?;
        let parent = context.session.identity.clone();
        let task_path = parent
            .path
            .join(&args.task_name)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let task_name = task_path.to_string();
        let mut reservation = {
            let mut state = self.inner.state.lock().await;
            state
                .trees
                .entry(parent.root_id)
                .or_default()
                .reserve_spawn(&task_name, self.inner.max_concurrent_children)?;
            drop(state);
            SpawnReservation::new(Arc::clone(&self.inner), parent.root_id, task_name.clone())
        };
        let messages = fork_messages(&context.session.messages, fork_mode);
        let session = match self
            .prepare_child(profile.clone(), &parent, args.task_name.as_str(), messages)
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
            parent.root_id,
            id,
            task_path.clone(),
            profile.clone(),
            args.message.clone(),
            session.clone(),
        )
        .await;
        release_spawn(&self.inner, parent.root_id, &task_name).await;
        reservation.commit();

        self.submit_and_watch(&session, parent.root_id, &task_name, args.message.clone())
            .await?;
        self.inner.updates.notify_waiters();
        self.publish().await;

        json_output(&serde_json::json!({
            "agent_id": id,
            "task_name": task_name,
            "agent_type": profile.name,
        }))
    }

    /// Build the child agent and start its runtime. Controller state is not
    /// touched, so the caller decides how to release the spawn reservation on
    /// failure.
    async fn prepare_child(
        &self,
        profile: AgentProfile,
        parent: &SessionIdentity,
        segment: &str,
        history: Vec<Message>,
    ) -> Result<Session, ToolError> {
        let child = self.inner.spawner.create(&profile, history)?;
        let agent = apply_profile(child.agent, profile);
        child
            .runtime
            .start_child(&agent, &child.options, parent, segment, child.history)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))
    }

    /// Register the spawned session as a child agent in the shared state.
    async fn register_child(
        &self,
        root_id: SessionId,
        id: SessionId,
        task_path: AgentPath,
        profile: AgentProfile,
        last_task_message: String,
        session: Session,
    ) {
        let mut state = self.inner.state.lock().await;
        let tree = state.trees.entry(root_id).or_default();
        tree.agents.insert(
            id,
            ChildRecord {
                id,
                task_path,
                profile,
                session,
                active_turns: 0,
                active_cancel: None,
                terminal: None,
                completion_revision: None,
                last_task_message,
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
        let (target, session) = {
            let mut state = self.inner.state.lock().await;
            let active_count = state.active_count(context.session.identity.root_id);
            let record = resolve_target_mut(
                &mut state,
                context.session.identity.root_id,
                context.session.identity.path.as_str(),
                &args.target,
            )?;
            if let Some(max) = self.inner.max_concurrent_children {
                if delivery.needs_new_slot(record) && active_count >= max {
                    return Err(ToolError::Execution(format!(
                        "maximum of {max} concurrent sub-agents reached"
                    )));
                }
            }
            record.last_task_message = args.message.clone();
            (record.task_path.to_string(), record.session.clone())
        };
        if delivery.triggers_turn() {
            self.submit_and_watch(
                &session,
                context.session.identity.root_id,
                &target,
                args.message.clone(),
            )
            .await?;
        } else {
            session
                .notify(Input::from_text(InputSource::Agent, args.message))
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
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
                context.session.identity.root_id,
                context.session.identity.path.as_str(),
                &args.target,
            )?;
            let result = (
                record.task_path.to_string(),
                record.status(),
                record.active_cancel.clone(),
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
                .snapshots(
                    context.session.identity.root_id,
                    args.path_prefix.as_deref(),
                )
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
                context.session.identity.root_id,
                context.session.identity.path.as_str(),
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
                    context.session.identity.root_id,
                    context.session.identity.path.as_str(),
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
        root_id: SessionId,
        path_prefix: Option<&str>,
    ) -> Vec<SessionSnapshot> {
        self.inner
            .state
            .lock()
            .await
            .snapshots(root_id, path_prefix)
    }

    /// Submit a turn to a child session and spawn a task that only observes
    /// its completion. Turn ordering is left entirely to the session actor;
    /// the controller never re-queues.
    ///
    /// # Errors
    ///
    /// Returns `ToolError` when the session rejects the turn.
    async fn submit_and_watch(
        &self,
        session: &Session,
        root_id: SessionId,
        target: &str,
        input: String,
    ) -> Result<(), ToolError> {
        let turn = session
            .submit(Input::from_text(InputSource::Agent, input))
            .await
            .map_err(|error| ToolError::Execution(format!("{target}: {error}")))?;
        let cancel = turn.cancellation_token();
        {
            let mut state = self.inner.state.lock().await;
            let Some(record) = state
                .trees
                .get_mut(&root_id)
                .and_then(|tree| tree.agents.get_mut(&session.id()))
            else {
                return Err(ToolError::Execution(format!(
                    "agent is no longer available: {target}"
                )));
            };
            record.active_turns = record.active_turns.saturating_add(1);
            record.active_cancel = Some(cancel);
            record.completion_revision = None;
            drop(state);
        }
        let watcher = self.clone();
        let session_id = session.id();
        tokio::spawn(async move {
            watcher.observe_turn(root_id, session_id, turn).await;
        });
        Ok(())
    }

    /// Observe one submitted turn to its settlement and fold the outcome into
    /// the child's projection. The session actor owns the turn lifecycle; this
    /// task only reports it.
    async fn observe_turn(&self, root_id: SessionId, session_id: SessionId, turn: Turn) {
        let result = turn.wait().await;
        let (result, final_message) = match result {
            Ok(view) => {
                let final_message = final_assistant_message(&view.messages);
                (Ok(view.result), final_message)
            }
            Err(error) => (Err(error), None),
        };
        let terminal = match result {
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
        let settled = state
            .trees
            .get_mut(&root_id)
            .is_some_and(|tree| tree.settle_turn(session_id, terminal));
        if !settled {
            return;
        }
        drop(state);
        self.inner.updates.notify_waiters();
        self.publish().await;
    }
}

/// Add collaboration capabilities to the main agent and return the controller.
///
/// # Errors
///
/// Returns `ToolError` when a tool cannot be defined.
pub fn install_collaboration(
    base: Agent,
    options: SessionOptions,
    runtime: Runtime,
    max_concurrent_children: Option<usize>,
) -> Result<(Agent, AgentControl), ToolError> {
    let spawner = Arc::new(InheritedSessionFactory {
        runtime,
        definition: base.clone(),
        scope: options,
    });
    let control = AgentControl::new(max_concurrent_children, spawner);
    let agent = with_multi_agent_instructions(base).pushing_tools(control.tools()?);
    Ok((agent, control))
}

fn with_multi_agent_instructions(agent: Agent) -> Agent {
    let prompt = agent
        .system_prompt()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .map_or_else(
            || MULTI_AGENT_INSTRUCTIONS.to_string(),
            |prompt| format!("{prompt}\n\n{MULTI_AGENT_INSTRUCTIONS}"),
        );
    agent.with_system_prompt(prompt)
}

fn validate_task_name(task_name: &str) -> Result<(), ToolError> {
    if is_valid_segment(task_name) {
        Ok(())
    } else {
        Err(ToolError::Execution(
            "task_name must contain only lowercase letters, digits, and underscores and be at most 64 characters"
                .to_string(),
        ))
    }
}

fn agent_path_matches(record: &ChildRecord, path_prefix: Option<&str>) -> bool {
    path_prefix.is_none_or(|prefix| {
        let prefix = format!("/{}", prefix.trim_matches('/'));
        record.task_path.under(&prefix)
    })
}

fn resolve_target_mut<'a>(
    state: &'a mut ControlState,
    root_id: SessionId,
    current_agent_path: &str,
    target: &str,
) -> Result<&'a mut ChildRecord, ToolError> {
    let tree = state.trees.get_mut(&root_id).ok_or_else(|| {
        ToolError::Execution("no sub-agents exist in the current session".to_string())
    })?;
    if let Ok(session_id) = SessionId::from_str(target) {
        return tree
            .agents
            .get_mut(&session_id)
            .ok_or_else(|| ToolError::Execution(format!("sub-agent not found: {target}")));
    }

    let current_path = current_agent_path.trim_end_matches('/');
    let relative_path = format!("{current_path}/{}", target.trim_matches('/'));
    let mut matches = tree
        .agents
        .values_mut()
        .filter(|record| {
            record.task_path.as_str() == target
                || record.task_path.as_str() == relative_path
                || record.task_path.segments().last() == Some(target)
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

/// Apply a profile's tool policy and overrides to an inherited agent.
fn apply_profile(agent: Agent, profile: AgentProfile) -> Agent {
    let agent = profile.tool_policy.apply(agent);
    let agent = match profile.model {
        Some(model) => agent.with_model(model),
        None => agent,
    };
    match profile.max_turns {
        Some(max_turns) => agent.with_max_turns(max_turns),
        None => agent,
    }
}

fn profile_system_prompt(base_prompt: Option<&str>, profile: &AgentProfile) -> String {
    [base_prompt.unwrap_or_default(), profile.prompt_overlay]
        .into_iter()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
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

    use ash_core::{ModelClient, ModelId, SessionToolContext};
    use ash_protocol::{create_adapter, Protocol, ProviderConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    struct RejectingFactory;

    impl ChildSessionFactory for RejectingFactory {
        fn create(
            &self,
            _profile: &AgentProfile,
            _history: Vec<Message>,
        ) -> Result<ChildSessionSpec, ToolError> {
            Err(ToolError::Execution(
                "spawn is not used by this test".to_string(),
            ))
        }
    }

    fn state_only_control() -> AgentControl {
        AgentControl::new(None, Arc::new(RejectingFactory))
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
        }
    }

    fn root_identity() -> SessionIdentity {
        SessionIdentity::root(SessionId::new())
    }

    fn make_context(identity: SessionIdentity) -> ToolContext {
        ToolContext {
            session_id: identity.id,
            turn_id: ash_core::TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + Duration::from_secs(5),
            session: SessionToolContext {
                identity,
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
    ) -> (AgentControl, tempfile::TempDir, Runtime) {
        // Keep test session files out of the real data directory
        // (`~/.ash/sessions`); the returned TempDir stays alive for
        // the whole test so spawned sub-agents keep a valid store.
        let directory = tempfile::TempDir::new().expect("create temp session directory");
        let runtime = Runtime::new(model, "test").with_session_store(Arc::new(
            ash_agent::JsonlSessionStore::new(directory.path()),
        ));
        let agent = make_agent().with_max_turns(max_turns);
        let options = make_options();
        let spawner = Arc::new(InheritedSessionFactory {
            runtime: runtime.clone(),
            definition: agent,
            scope: options,
        });
        (AgentControl::new(None, spawner), directory, runtime)
    }

    /// Start a real child session under `root_id` so state-only tests hold a
    /// live `Session` handle.
    async fn start_child_session(
        runtime: &Runtime,
        root_id: SessionId,
        task_name: &str,
    ) -> Session {
        runtime
            .start_child(
                &make_agent(),
                &make_options(),
                &SessionIdentity::root(root_id),
                task_name,
                Vec::new(),
            )
            .await
            .unwrap()
    }

    /// Insert a child record backed by a live session into the tree.
    async fn insert_child(
        control: &AgentControl,
        runtime: &Runtime,
        root_id: SessionId,
        task_name: &str,
        profile: AgentProfile,
        terminal: Option<ChildState>,
        completion_revision: Option<u64>,
    ) -> Session {
        let session = start_child_session(runtime, root_id, task_name).await;
        let task_path = AgentPath::root().join(task_name).unwrap();
        let mut state = control.inner.state.lock().await;
        state.trees.entry(root_id).or_default().agents.insert(
            session.id(),
            ChildRecord {
                id: session.id(),
                task_path,
                profile,
                session: session.clone(),
                active_turns: 0,
                active_cancel: None,
                terminal,
                completion_revision,
                last_task_message: "initial".to_string(),
            },
        );
        drop(state);
        session
    }

    #[test]
    fn exposes_the_codex_0_144_3_builtin_roles() {
        assert_eq!(
            ProfileName::from_str("default").unwrap(),
            ProfileName::Default
        );
        assert_eq!(
            ProfileName::from_str("explorer").unwrap(),
            ProfileName::Explorer
        );
        assert_eq!(
            ProfileName::from_str("worker").unwrap(),
            ProfileName::Worker
        );
        assert!(ProfileName::from_str("awaiter").is_err());
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
        let agent = apply_profile(make_agent().with_tools(inherited), AgentProfile::explorer());

        assert_eq!(
            tool_names(&agent),
            ["read", "glob", "grep", "webfetch", "skill"]
        );
        assert!(!tool_names(&agent).contains(&"custom_mutator"));
        assert!(!tool_names(&agent).contains(&"spawn_agent"));
    }

    #[test]
    fn profile_model_and_turn_overrides_apply_without_touching_the_engine() {
        let profile = AgentProfile {
            model: Some(ModelId::new("override-model")),
            max_turns: Some(7),
            ..AgentProfile::default()
        };

        let agent = apply_profile(make_agent().with_max_turns(100), profile);

        assert_eq!(agent.model().as_str(), "override-model");
        assert_eq!(agent.max_turns(), 7);
    }

    #[test]
    fn default_and_worker_keep_only_inherited_tools() {
        for profile in [AgentProfile::default(), AgentProfile::worker()] {
            let inherited = ["read", "write", "custom_tool"]
                .into_iter()
                .map(named_tool)
                .collect();
            let agent = apply_profile(make_agent().with_tools(inherited), profile.clone());

            assert_eq!(
                tool_names(&agent),
                ["read", "write", "custom_tool"],
                "{profile:?}"
            );
        }
    }

    #[test]
    fn installs_collaboration_tools_and_prompt_on_the_main_agent() {
        let options = make_options();
        let runtime = Runtime::new(make_model(None), "test");
        let (agent, _) = install_collaboration(make_agent(), options, runtime, None).unwrap();
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
    }

    #[test]
    fn spawned_agents_derive_from_the_clean_base() {
        let base = make_agent().with_tools(vec![named_tool("read"), named_tool("write")]);
        let options = make_options();
        let runtime = Runtime::new(make_model(None), "test");
        let (main, control) = install_collaboration(base, options, runtime, None).unwrap();

        let child = control
            .inner
            .spawner
            .create(&AgentProfile::default(), Vec::new())
            .unwrap();
        let child = apply_profile(child.agent, AgentProfile::default());

        assert_eq!(
            tool_names(&main),
            [
                "read",
                "write",
                "spawn_agent",
                "message_agent",
                "interrupt_agent",
                "wait_agent",
            ]
        );
        assert_eq!(tool_names(&child), ["read", "write"]);
        assert!(main.system_prompt().unwrap().contains("<multi_agent_mode>"));
        assert!(!child
            .system_prompt()
            .unwrap()
            .contains("<multi_agent_mode>"));
        assert!(!child
            .system_prompt()
            .unwrap()
            .contains("<subagent_context>"));
        assert!(child
            .system_prompt()
            .unwrap()
            .contains("Handle the assigned task directly"));
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
        let context = make_context(root_identity());

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
        let control = AgentControl::new(Some(0), Arc::new(RejectingFactory));
        let error = control
            .spawn(
                make_context(root_identity()),
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
        let root_id = SessionId::new();
        control
            .inner
            .state
            .lock()
            .await
            .trees
            .entry(root_id)
            .or_default()
            .pending_spawns
            .insert("/root/inspect".to_string());

        let error = control
            .spawn(
                make_context(SessionIdentity::root(root_id)),
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

    #[tokio::test]
    async fn settlement_waits_for_every_accepted_followup_before_going_terminal() {
        let (control, _directory, runtime) = make_control(make_model(None), 1);
        let root_id = SessionId::new();
        let session = insert_child(
            &control,
            &runtime,
            root_id,
            "inspect",
            AgentProfile::explorer(),
            None,
            None,
        )
        .await;
        let agent_id = session.id();
        let mut tree = control.inner.state.lock().await;
        let tree = tree.trees.get_mut(&root_id).unwrap();

        // Initial turn plus two accepted follow-ups are all unsettled.
        let record = tree.agents.get_mut(&agent_id).unwrap();
        record.active_turns = 3;
        // Intermediate settlements never surface as terminal: the first two
        // complete turns keep the agent busy.
        assert!(!tree.settle_turn(
            agent_id,
            ChildState::Completed(Some("initial result".to_string())),
        ));
        assert!(!tree.settle_turn(
            agent_id,
            ChildState::Completed(Some("first follow-up".to_string())),
        ));
        assert!(tree.settle_turn(
            agent_id,
            ChildState::Completed(Some("final result".to_string())),
        ));

        let record = tree.agents.get(&agent_id).unwrap();
        assert_eq!(record.active_turns, 0);
        assert!(matches!(
            &record.terminal,
            Some(ChildState::Completed(message)) if message.as_deref() == Some("final result")
        ));
        assert_eq!(record.completion_revision, Some(1));
    }

    #[tokio::test]
    async fn message_agent_rejects_an_unknown_target() {
        let (control, _directory, runtime) = make_control(make_model(None), 1);
        let root_id = SessionId::new();
        insert_child(
            &control,
            &runtime,
            root_id,
            "inspect",
            AgentProfile::explorer(),
            Some(ChildState::Completed(Some("done".to_string()))),
            Some(1),
        )
        .await;

        let result = control
            .message_agent(
                &make_context(SessionIdentity::root(root_id)),
                MessageAgentArgs {
                    target: "/root/missing".to_string(),
                    message: "follow up".to_string(),
                    start_turn: Some(true),
                },
            )
            .await;

        assert!(result.is_err());
        let agents = control.snapshots(root_id, None).await;
        assert_eq!(agents[0].status, SubagentState::Completed);
        assert_eq!(agents[0].last_task_message, "initial");
    }

    #[tokio::test]
    async fn message_agent_can_queue_guidance_without_starting_a_turn() {
        let (control, _directory, runtime) = make_control(make_model(None), 1);
        let root_id = SessionId::new();
        let session = insert_child(
            &control,
            &runtime,
            root_id,
            "inspect",
            AgentProfile::explorer(),
            Some(ChildState::Completed(Some("done".to_string()))),
            Some(1),
        )
        .await;

        let result = control
            .message_agent(
                &make_context(SessionIdentity::root(root_id)),
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
        // Notify stages guidance but must not start a turn.
        assert!(session.view().await.unwrap().messages.is_empty());
        let agents = control.snapshots(root_id, None).await;
        assert_eq!(agents[0].status, SubagentState::Completed);
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
        let (control, _directory, runtime) = make_control(make_model(None), 1);
        let root = SessionId::new();
        let other_root = SessionId::new();
        insert_child(
            &control,
            &runtime,
            root,
            "inspect",
            AgentProfile::explorer(),
            Some(ChildState::Completed(Some("done".to_string()))),
            None,
        )
        .await;

        assert_eq!(control.snapshots(root, None).await.len(), 1);
        assert!(control.snapshots(other_root, None).await.is_empty());
    }

    #[tokio::test]
    async fn wait_returns_each_completion_only_once() {
        let (control, _directory, runtime) = make_control(make_model(None), 1);
        let root_id = SessionId::new();
        insert_child(
            &control,
            &runtime,
            root_id,
            "completed",
            AgentProfile::explorer(),
            Some(ChildState::Completed(Some("first result".to_string()))),
            Some(1),
        )
        .await;
        let running_id = insert_child(
            &control,
            &runtime,
            root_id,
            "running",
            AgentProfile::worker(),
            None,
            None,
        )
        .await
        .id();
        {
            let mut state = control.inner.state.lock().await;
            let tree = state.trees.get_mut(&root_id).unwrap();
            tree.next_completion_revision = 1;
            tree.agents.get_mut(&running_id).unwrap().active_turns = 1;
            drop(state);
        }

        let context = make_context(SessionIdentity::root(root_id));
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
            assert!(state.trees.get_mut(&root_id).unwrap().settle_turn(
                running_id,
                ChildState::Completed(Some("second result".to_string())),
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
        let (control, _directory, runtime) = make_control(make_model(None), 1);
        let root_id = SessionId::new();
        insert_child(
            &control,
            &runtime,
            root_id,
            "completed",
            AgentProfile::explorer(),
            Some(ChildState::Completed(Some("result".to_string()))),
            Some(1),
        )
        .await;
        {
            let mut state = control.inner.state.lock().await;
            let tree = state.trees.get_mut(&root_id).unwrap();
            tree.next_completion_revision = 1;
            drop(state);
        }

        let context = make_context(SessionIdentity::root(root_id));
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
    async fn interrupt_cancels_only_the_active_turn_and_keeps_the_session_usable() {
        let (control, _directory, runtime) = make_control(make_model(None), 1);
        let root_id = SessionId::new();
        let session = insert_child(
            &control,
            &runtime,
            root_id,
            "inspect",
            AgentProfile::explorer(),
            None,
            None,
        )
        .await;
        let token = CancellationToken::new();
        {
            let mut state = control.inner.state.lock().await;
            let record = state
                .trees
                .get_mut(&root_id)
                .unwrap()
                .agents
                .get_mut(&session.id())
                .unwrap();
            record.active_turns = 1;
            record.active_cancel = Some(token.clone());
            drop(state);
        }

        let result = control
            .interrupt(
                &make_context(SessionIdentity::root(root_id)),
                InterruptAgentArgs {
                    target: "/root/inspect".to_string(),
                },
            )
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["previous_status"], "running");
        assert!(token.is_cancelled());

        // Interrupt must not close the session: a follow-up still lands.
        control
            .message_agent(
                &make_context(SessionIdentity::root(root_id)),
                MessageAgentArgs {
                    target: "/root/inspect".to_string(),
                    message: "continue".to_string(),
                    start_turn: Some(true),
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn runs_a_spawned_agent_and_captures_its_final_message() {
        let (base_url, server) = mock_model_server(&["child done", "followup done"]).await;
        let (control, _directory, _runtime) = make_control(make_model(Some(base_url)), 2);
        let root_id = SessionId::new();
        let context = ToolContext {
            session_id: root_id,
            turn_id: ash_core::TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + Duration::from_secs(5),
            session: SessionToolContext {
                identity: SessionIdentity::root(root_id),
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
                    agent_type: Some(ProfileName::Explorer),
                    fork_turns: Some(ForkTurnsArg::Keyword(ForkTurnsKeyword::All)),
                },
            )
            .await
            .unwrap();

        let completed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let agents = control.snapshots(root_id, None).await;
                if agents
                    .first()
                    .is_some_and(|agent| agent.status == SubagentState::Completed)
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
                let agents = control.snapshots(root_id, None).await;
                if agents.first().is_some_and(|agent| {
                    agent.status == SubagentState::Completed
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
        assert!(!requests[0].contains("<subagent_context>"));
        assert!(requests[0].contains("read-only inspection"));
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
        let (control, _directory, _runtime) = make_control(make_model(Some(base_url)), 1);
        let root_id = SessionId::new();
        let mut snapshots = control.subscribe();
        let context = ToolContext {
            session_id: root_id,
            turn_id: ash_core::TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + Duration::from_secs(5),
            session: SessionToolContext {
                identity: SessionIdentity::root(root_id),
                messages: vec![Message::user("delegate this")],
            },
        };
        control
            .spawn(
                context,
                SpawnAgentArgs {
                    task_name: "inspect".to_string(),
                    message: "Inspect the parser.".to_string(),
                    agent_type: Some(ProfileName::Explorer),
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
        let (control, _directory, _runtime) = make_control(make_model(Some(base_url)), 1);
        let root_id = SessionId::new();
        let context = ToolContext {
            session_id: root_id,
            turn_id: ash_core::TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + Duration::from_secs(5),
            session: SessionToolContext {
                identity: SessionIdentity::root(root_id),
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
                let agents = control.snapshots(root_id, None).await;
                if agents.len() == 4
                    && agents
                        .iter()
                        .all(|agent| agent.status == SubagentState::Completed)
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
