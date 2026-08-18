use std::{
    collections::{HashMap, HashSet, VecDeque},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use crate::snapshot::{SubagentSnapshot, SubagentState};
use ash_agent::{Agent, Input, InputSource, Runtime, Session, SessionOptions, Turn};
use ash_core::{
    define_tool, is_valid_segment, AgentPath, CancellationToken, ContentBlock, Message,
    MessageContent, SessionId, SessionIdentity, StopReason, Tool, ToolContext, ToolError, TurnId,
    TurnResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex, Notify};

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;
const COLLABORATION_TOOL_NAMES: [&str; 4] = [
    "spawn_agent",
    "message_agent",
    "interrupt_agent",
    "wait_agent",
];
/// Tools whose public contracts are read-only. Explorer projection is an
/// allowlist: unknown custom and MCP tools are excluded unless their behavior
/// is represented by one of these canonical tool names.
const EXPLORER_TOOL_NAMES: [&str; 5] = ["read", "glob", "grep", "webfetch", "skill"];

const MULTI_AGENT_INSTRUCTIONS: &str = r"<multi_agent_mode>
You are the main agent. Before starting non-trivial work, check for independent workstreams. When two or more tasks can proceed independently, delegate them in parallel; keep one-path or tightly coupled work local. Sub-agents share the workspace and cannot delegate further.

- Handle simple tasks and immediate blockers yourself.
- Assign concrete, bounded tasks with enough context and non-overlapping write scopes.
- Continue useful non-overlapping work while sub-agents run; do not duplicate delegated work.
- Review sub-agent results before using them.
</multi_agent_mode>";

/// Schema-facing name of one built-in profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ProfileName {
    Default,
    Explorer,
    Worker,
}

impl ProfileName {
    const ALL: [Self; 3] = [Self::Default, Self::Explorer, Self::Worker];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Explorer => "explorer",
            Self::Worker => "worker",
        }
    }
}

impl FromStr for ProfileName {
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
enum ToolPolicy {
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
struct AgentProfile {
    name: ProfileName,
    description: &'static str,
    prompt_overlay: &'static str,
    tool_policy: ToolPolicy,
}

impl AgentProfile {
    const fn builtin(
        name: ProfileName,
        description: &'static str,
        prompt_overlay: &'static str,
        tool_policy: ToolPolicy,
    ) -> Self {
        Self {
            name,
            description,
            prompt_overlay,
            tool_policy,
        }
    }

    const fn default() -> Self {
        Self::builtin(
            ProfileName::Default,
            "General-purpose agent for a self-contained task that inherits the current configuration.",
            "Handle the assigned task directly. Stay within its scope and return a concise, evidence-backed result to the parent agent.",
            ToolPolicy::Inherit,
        )
    }

    const fn explorer() -> Self {
        Self::builtin(
            ProfileName::Explorer,
            "Use whenever a specific, well-scoped codebase question can be answered independently. Explorers are fast, read-only, and authoritative. Spawn multiple explorers in the same round for distinct questions; reuse an existing explorer for related follow-ups.",
            "Answer the assigned codebase question through read-only inspection. Do not edit files. Return concrete findings with relevant paths and symbols, and do not broaden the investigation beyond the question.",
            ToolPolicy::Allow(&EXPLORER_TOOL_NAMES),
        )
    }

    const fn worker() -> Self {
        Self::builtin(
            ProfileName::Worker,
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

    fn apply(&self, agent: Agent) -> Agent {
        let prompt = [
            agent.system_prompt().unwrap_or_default(),
            self.prompt_overlay,
        ]
        .into_iter()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
        self.tool_policy.apply(agent.with_system_prompt(prompt))
    }

    fn available_profiles_description() -> String {
        let roles = ProfileName::ALL
            .into_iter()
            .map(Self::for_name)
            .map(|profile| format!("{}: {}", profile.name.as_str(), profile.description))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Optional type name for the new agent. If omitted, `default` is used.\nAvailable roles:\n{roles}"
        )
    }
}

#[derive(Debug, Clone, Serialize)]
struct SessionSnapshot {
    session_id: SessionId,
    task_path: String,
    agent_type: ProfileName,
    status: SubagentState,
    last_task_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl From<SessionSnapshot> for SubagentSnapshot {
    fn from(snapshot: SessionSnapshot) -> Self {
        Self {
            task_path: snapshot.task_path,
            agent_type: snapshot.agent_type.as_str().to_string(),
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
    runtime: Runtime,
    definition: Agent,
    scope: SessionOptions,
    subagent_tx: watch::Sender<Vec<SubagentSnapshot>>,
}

struct SlotReservation {
    inner: Arc<ControlInner>,
    root_id: SessionId,
    key: Option<SlotReservationKey>,
}

enum SlotReservationKey {
    Spawn(AgentPath),
    Followup(SessionId),
}

async fn release_slot(inner: &Arc<ControlInner>, root_id: SessionId, key: SlotReservationKey) {
    let mut state = inner.state.lock().await;
    if let Some(tree) = state.trees.get_mut(&root_id) {
        tree.release_slot(&key);
    }
}

impl SlotReservation {
    const fn new(inner: Arc<ControlInner>, root_id: SessionId, key: SlotReservationKey) -> Self {
        Self {
            inner,
            root_id,
            key: Some(key),
        }
    }

    async fn release(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        release_slot(&self.inner, self.root_id, key).await;
    }

    fn commit(mut self) {
        self.key = None;
    }
}

impl Drop for SlotReservation {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        let root_id = self.root_id;
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    release_slot(&inner, root_id, key).await;
                });
            }
            Err(_) => {
                tracing::warn!(
                    "slot reservation dropped outside a tokio runtime; the slot may leak"
                );
            }
        }
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
    pending_spawns: HashSet<AgentPath>,
    pending_followups: HashMap<SessionId, usize>,
    next_completion_revision: u64,
    wait_cursors: HashMap<WaitCursorKey, u64>,
}

struct ChildRecord {
    id: SessionId,
    task_path: AgentPath,
    profile: ProfileName,
    /// The durable session backing this child. Turn ordering is entirely the
    /// session actor's job; the controller never queues turns itself.
    session: Session,
    lifecycle: ChildLifecycle,
    completion_revision: Option<u64>,
    last_task_message: String,
}

enum ChildLifecycle {
    Running(ActiveTurns),
    Idle(ChildState),
}

/// Non-empty turns accepted by a child session, in submission order. Settled
/// outcomes stay queued until every earlier turn settles.
struct ActiveTurns {
    turns: VecDeque<TrackedTurn>,
}

impl ActiveTurns {
    fn new(turn: TrackedTurn) -> Self {
        Self {
            turns: VecDeque::from([turn]),
        }
    }

    fn push(&mut self, turn: TrackedTurn) {
        self.turns.push_back(turn);
    }

    fn active_cancellation(&self) -> Option<CancellationToken> {
        self.turns.front().map(|turn| turn.cancellation.clone())
    }

    fn settle(&mut self, turn_id: TurnId, outcome: ChildState) -> Option<ChildState> {
        let turn = self.turns.iter_mut().find(|turn| turn.id == turn_id)?;
        if turn.outcome.is_some() {
            return None;
        }
        turn.outcome = Some(outcome);

        let mut latest = None;
        while self
            .turns
            .front()
            .is_some_and(|turn| turn.outcome.is_some())
        {
            latest = self.turns.pop_front().and_then(|turn| turn.outcome);
        }
        self.turns.is_empty().then_some(latest).flatten()
    }
}

struct TrackedTurn {
    id: TurnId,
    cancellation: CancellationToken,
    outcome: Option<ChildState>,
}

impl TrackedTurn {
    fn new(turn: &Turn) -> Self {
        Self {
            id: turn.id(),
            cancellation: turn.cancellation_token(),
            outcome: None,
        }
    }

    #[cfg(test)]
    fn test(id: TurnId, cancellation: CancellationToken) -> Self {
        Self {
            id,
            cancellation,
            outcome: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum WaitScope {
    All,
    Agents(Vec<SessionId>),
}

impl WaitScope {
    fn includes(&self, record: &ChildRecord) -> bool {
        match self {
            Self::All => true,
            Self::Agents(ids) => ids.contains(&record.id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WaitCursorKey {
    waiter: AgentPath,
    scope: WaitScope,
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
        let terminal = match &self.lifecycle {
            ChildLifecycle::Running(_) => None,
            ChildLifecycle::Idle(terminal) => Some(terminal),
        };
        SessionSnapshot {
            session_id: self.id,
            task_path: self.task_path.to_string(),
            agent_type: self.profile,
            status: self.status(),
            last_task_message: self.last_task_message.clone(),
            final_message: terminal
                .and_then(ChildState::final_message)
                .map(str::to_string),
            error: terminal.and_then(ChildState::error).map(str::to_string),
        }
    }

    fn status(&self) -> SubagentState {
        match &self.lifecycle {
            ChildLifecycle::Running(_) => SubagentState::Running,
            ChildLifecycle::Idle(terminal) => terminal.status(),
        }
    }

    /// Whether the child is busy enough to hold a concurrency slot. A child
    /// with queued follow-ups keeps its slot until they all settle.
    fn holds_slot(&self) -> bool {
        matches!(self.lifecycle, ChildLifecycle::Running(_))
    }

    fn track(&mut self, turn: &Turn) {
        let turn = TrackedTurn::new(turn);
        match &mut self.lifecycle {
            ChildLifecycle::Running(turns) => turns.push(turn),
            ChildLifecycle::Idle(_) => {
                self.lifecycle = ChildLifecycle::Running(ActiveTurns::new(turn));
            }
        }
        self.completion_revision = None;
    }

    fn active_cancellation(&self) -> Option<CancellationToken> {
        match &self.lifecycle {
            ChildLifecycle::Running(turns) => turns.active_cancellation(),
            ChildLifecycle::Idle(_) => None,
        }
    }

    fn settle(&mut self, turn_id: TurnId, outcome: ChildState) -> bool {
        let terminal = match &mut self.lifecycle {
            ChildLifecycle::Running(turns) => turns.settle(turn_id, outcome),
            ChildLifecycle::Idle(_) => None,
        };
        let Some(terminal) = terminal else {
            return false;
        };
        self.lifecycle = ChildLifecycle::Idle(terminal);
        true
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
}

fn sorted_snapshots<'a>(records: impl Iterator<Item = &'a ChildRecord>) -> Vec<SessionSnapshot> {
    let mut agents = records.map(ChildRecord::snapshot).collect::<Vec<_>>();
    agents.sort_by(|left, right| left.task_path.cmp(&right.task_path));
    agents
}

impl ControlState {
    fn snapshots(&self, root_id: SessionId, scope: &WaitScope) -> Vec<SessionSnapshot> {
        sorted_snapshots(
            self.trees
                .get(&root_id)
                .into_iter()
                .flat_map(|tree| tree.agents.values())
                .filter(|record| scope.includes(record)),
        )
    }

    fn wait_snapshot(
        &mut self,
        root_id: SessionId,
        waiter: &AgentPath,
        scope: &WaitScope,
    ) -> WaitSnapshot {
        self.trees
            .get_mut(&root_id)
            .map_or_else(WaitSnapshot::empty, |tree| {
                tree.wait_snapshot(waiter, scope)
            })
    }

    fn wait_scope(
        &self,
        root_id: SessionId,
        current_path: &AgentPath,
        targets: Option<&[String]>,
    ) -> Result<WaitScope, ToolError> {
        let Some(targets) = targets else {
            return Ok(WaitScope::All);
        };
        if targets.is_empty() {
            return Err(ToolError::Execution(
                "wait_agent targets cannot be empty".to_string(),
            ));
        }
        let tree = self.trees.get(&root_id).ok_or_else(no_subagents)?;
        let mut ids = targets
            .iter()
            .map(|target| resolve_target_id(tree, current_path, target))
            .collect::<Result<Vec<_>, _>>()?;
        ids.sort_by_key(SessionId::to_string);
        ids.dedup();
        Ok(WaitScope::Agents(ids))
    }
}

impl AgentTreeState {
    fn active_count(&self) -> usize {
        let running = self
            .agents
            .values()
            .filter(|record| record.holds_slot())
            .count();
        let pending_followups = self
            .pending_followups
            .keys()
            .filter(|id| !self.agents.get(id).is_some_and(ChildRecord::holds_slot))
            .count();
        running
            .saturating_add(self.pending_spawns.len())
            .saturating_add(pending_followups)
    }

    fn reserve_spawn(
        &mut self,
        task_path: &AgentPath,
        max: Option<usize>,
    ) -> Result<(), ToolError> {
        if let Some(max) = max {
            if self.active_count() >= max {
                return Err(ToolError::Execution(format!(
                    "maximum of {max} concurrent sub-agents reached"
                )));
            }
        }
        if self.pending_spawns.contains(task_path)
            || self
                .agents
                .values()
                .any(|record| record.task_path == *task_path)
        {
            return Err(ToolError::Execution(format!(
                "agent task path already exists: {task_path}; use message_agent with start_turn=true to reuse it"
            )));
        }
        self.pending_spawns.insert(task_path.clone());
        Ok(())
    }

    fn reserve_followup(&mut self, id: SessionId, max: Option<usize>) -> Result<(), ToolError> {
        let record = self
            .agents
            .get(&id)
            .ok_or_else(|| ToolError::Execution(format!("sub-agent not found: {id}")))?;
        let needs_new_slot = !record.holds_slot() && !self.pending_followups.contains_key(&id);
        if let Some(max) = max {
            if needs_new_slot && self.active_count() >= max {
                return Err(ToolError::Execution(format!(
                    "maximum of {max} concurrent sub-agents reached"
                )));
            }
        }
        self.pending_followups
            .entry(id)
            .and_modify(|count| *count = count.saturating_add(1))
            .or_insert(1);
        Ok(())
    }

    fn release_slot(&mut self, key: &SlotReservationKey) {
        match key {
            SlotReservationKey::Spawn(task_path) => {
                self.pending_spawns.remove(task_path);
            }
            SlotReservationKey::Followup(id) => {
                let remove = self.pending_followups.get_mut(id).is_some_and(|count| {
                    *count = count.saturating_sub(1);
                    *count == 0
                });
                if remove {
                    self.pending_followups.remove(id);
                }
            }
        }
    }

    fn wait_snapshot(&mut self, waiter: &AgentPath, scope: &WaitScope) -> WaitSnapshot {
        let waiter_key = WaitCursorKey {
            waiter: waiter.clone(),
            scope: scope.clone(),
        };
        let cursor = self.wait_cursors.get(&waiter_key).copied().unwrap_or(0);
        let completion_revision = self
            .agents
            .values()
            .filter(|record| scope.includes(record))
            .filter(|record| !record.holds_slot())
            .filter_map(|record| record.completion_revision)
            .filter(|revision| *revision > cursor)
            .max();
        if let Some(revision) = completion_revision {
            self.wait_cursors.insert(waiter_key, revision);
        }
        let agents = sorted_snapshots(self.agents.values().filter(|record| scope.includes(record)));
        WaitSnapshot {
            agents,
            has_update: completion_revision.is_some(),
        }
    }

    /// Fold one settled turn into a child record. The child reaches terminal
    /// state (and bumps the completion revision) only after every accepted
    /// turn, including queued follow-ups, has settled, so waiters never
    /// observe an intermediate completion. Returns whether it went terminal.
    fn settle_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        terminal: ChildState,
    ) -> bool {
        let Some(record) = self.agents.get_mut(&session_id) else {
            return false;
        };
        if !record.settle(turn_id, terminal) {
            return false;
        }
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
    #[serde(default)]
    start_turn: bool,
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
    /// Agent ids, canonical task paths, or unambiguous task names to wait for. Omit to wait for all agents.
    targets: Option<Vec<String>>,
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
            None | Some(ForkTurnsArg::Keyword(ForkTurnsKeyword::All)) => Ok(Self::All),
            Some(ForkTurnsArg::Keyword(ForkTurnsKeyword::None)) => Ok(Self::None),
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
        runtime: Runtime,
        definition: Agent,
        scope: SessionOptions,
    ) -> Self {
        let (subagent_tx, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(ControlInner {
                state: Mutex::new(ControlState::default()),
                updates: Notify::new(),
                max_concurrent_children,
                runtime,
                definition,
                scope,
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
            "Spawn a leaf agent for a concrete, bounded task. Use this proactively for independent workstreams that can proceed without blocking the main agent. Include enough context and the expected output in `message`.\n\n{}",
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
            "Wait for updates from selected agents, or all agents when `targets` is omitted. Set `timeout_ms` to 0 to return the current snapshot immediately. Completed updates include final messages for review.",
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
        let mut reservation = {
            let mut state = self.inner.state.lock().await;
            state
                .trees
                .entry(parent.root_id)
                .or_default()
                .reserve_spawn(&task_path, self.inner.max_concurrent_children)?;
            drop(state);
            SlotReservation::new(
                Arc::clone(&self.inner),
                parent.root_id,
                SlotReservationKey::Spawn(task_path.clone()),
            )
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

        let turn = match Self::submit_turn(&session, task_path.as_str(), args.message.clone()).await
        {
            Ok(turn) => turn,
            Err(error) => {
                reservation.release().await;
                return Err(error);
            }
        };
        self.register_child(
            parent.root_id,
            &task_path,
            ChildRecord {
                id,
                task_path: task_path.clone(),
                profile: profile.name,
                session,
                lifecycle: ChildLifecycle::Running(ActiveTurns::new(TrackedTurn::new(&turn))),
                completion_revision: None,
                last_task_message: args.message.clone(),
            },
        )
        .await;
        reservation.commit();

        self.watch_turn(parent.root_id, id, turn);
        self.inner.updates.notify_waiters();
        self.publish().await;

        json_output(&serde_json::json!({
            "agent_id": id,
            "task_path": task_path,
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
        let agent = profile.apply(self.inner.definition.clone());
        self.inner
            .runtime
            .start_child(&agent, &self.inner.scope, parent, segment, history)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))
    }

    /// Register the spawned session as a child agent in the shared state.
    async fn register_child(&self, root_id: SessionId, task_path: &AgentPath, child: ChildRecord) {
        let mut state = self.inner.state.lock().await;
        let tree = state.trees.entry(root_id).or_default();
        tree.pending_spawns.remove(task_path);
        tree.agents.insert(child.id, child);
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
        let delivery = if args.start_turn {
            MessageDelivery::Followup
        } else {
            MessageDelivery::Queue
        };
        let root_id = context.session.identity.root_id;
        let (target, session, reservation) = {
            let mut state = self.inner.state.lock().await;
            let tree = state.trees.get_mut(&root_id).ok_or_else(no_subagents)?;
            let id = resolve_target_id(tree, &context.session.identity.path, &args.target)?;
            let record = tree
                .agents
                .get(&id)
                .ok_or_else(|| ToolError::Execution(format!("sub-agent not found: {id}")))?;
            let target = record.task_path.to_string();
            let session = record.session.clone();
            let reservation = if delivery.triggers_turn() {
                tree.reserve_followup(id, self.inner.max_concurrent_children)?;
                Some(SlotReservation::new(
                    Arc::clone(&self.inner),
                    root_id,
                    SlotReservationKey::Followup(id),
                ))
            } else {
                None
            };
            (target, session, reservation)
        };
        if delivery.triggers_turn() {
            let Some(mut reservation) = reservation else {
                return Err(ToolError::Execution(
                    "follow-up slot was not reserved".to_string(),
                ));
            };
            let turn = match Self::submit_turn(&session, &target, args.message.clone()).await {
                Ok(turn) => turn,
                Err(error) => {
                    reservation.release().await;
                    return Err(error);
                }
            };
            let tracked = {
                let mut state = self.inner.state.lock().await;
                let tracked = state.trees.get_mut(&root_id).is_some_and(|tree| {
                    let tracked = tree.agents.get_mut(&session.id()).is_some_and(|record| {
                        record.last_task_message.clone_from(&args.message);
                        record.track(&turn);
                        true
                    });
                    tree.release_slot(&SlotReservationKey::Followup(session.id()));
                    tracked
                });
                tracked
            };
            reservation.commit();
            if !tracked {
                turn.cancellation_token().cancel();
                return Err(ToolError::Execution(format!(
                    "agent is no longer available: {target}"
                )));
            }
            self.watch_turn(root_id, session.id(), turn);
        } else {
            session
                .notify(Input::from_text(InputSource::Agent, args.message.clone()))
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            let mut state = self.inner.state.lock().await;
            let record = state
                .trees
                .get_mut(&root_id)
                .and_then(|tree| tree.agents.get_mut(&session.id()))
                .ok_or_else(|| {
                    ToolError::Execution(format!("agent is no longer available: {target}"))
                })?;
            record.last_task_message = args.message;
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
                &context.session.identity.path,
                &args.target,
            )?;
            let result = (
                record.task_path.to_string(),
                record.status(),
                record.active_cancellation(),
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
        let scope = self.inner.state.lock().await.wait_scope(
            context.session.identity.root_id,
            &context.session.identity.path,
            args.targets.as_deref(),
        )?;
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
                .scoped_snapshots(context.session.identity.root_id, &scope)
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
                &context.session.identity.path,
                &scope,
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
                    &context.session.identity.path,
                    &scope,
                );
                return json_output(&serde_json::json!({
                    "agents": snapshot.agents,
                    "timed_out": !snapshot.has_update,
                }));
            }
        }
    }

    #[cfg(test)]
    async fn snapshots(&self, root_id: SessionId) -> Vec<SessionSnapshot> {
        self.scoped_snapshots(root_id, &WaitScope::All).await
    }

    async fn scoped_snapshots(
        &self,
        root_id: SessionId,
        scope: &WaitScope,
    ) -> Vec<SessionSnapshot> {
        self.inner.state.lock().await.snapshots(root_id, scope)
    }

    async fn submit_turn(
        session: &Session,
        target: &str,
        input: String,
    ) -> Result<Turn, ToolError> {
        session
            .submit(Input::from_text(InputSource::Agent, input))
            .await
            .map_err(|error| ToolError::Execution(format!("{target}: {error}")))
    }

    fn watch_turn(&self, root_id: SessionId, session_id: SessionId, turn: Turn) {
        let watcher = self.clone();
        tokio::spawn(async move {
            watcher.observe_turn(root_id, session_id, turn).await;
        });
    }

    /// Observe one submitted turn to its settlement and fold the outcome into
    /// the child's projection. The session actor owns the turn lifecycle; this
    /// task only reports it.
    async fn observe_turn(&self, root_id: SessionId, session_id: SessionId, turn: Turn) {
        let turn_id = turn.id();
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
            .is_some_and(|tree| tree.settle_turn(session_id, turn_id, terminal));
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
/// The base agent must be clean: collaboration tools and
/// `<multi_agent_mode>` instructions are installed exactly once, and every
/// child derives from the same unmodified base.
///
/// # Errors
///
/// Returns `ToolError` when the base agent already carries collaboration
/// tools, or when a tool cannot be defined.
pub fn install_collaboration(
    base: Agent,
    options: SessionOptions,
    runtime: Runtime,
    max_concurrent_children: Option<usize>,
) -> Result<(Agent, AgentControl), ToolError> {
    if base
        .tools()
        .iter()
        .any(|tool| COLLABORATION_TOOL_NAMES.contains(&tool.name()))
    {
        return Err(ToolError::Execution(
            "collaboration tools are already installed on this agent".to_string(),
        ));
    }
    let control = AgentControl::new(max_concurrent_children, runtime, base.clone(), options);
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

fn no_subagents() -> ToolError {
    ToolError::Execution("no sub-agents exist in the current session".to_string())
}

fn resolve_target_id(
    tree: &AgentTreeState,
    current_agent_path: &AgentPath,
    target: &str,
) -> Result<SessionId, ToolError> {
    if let Ok(session_id) = SessionId::from_str(target) {
        return tree
            .agents
            .contains_key(&session_id)
            .then_some(session_id)
            .ok_or_else(|| ToolError::Execution(format!("sub-agent not found: {target}")));
    }

    let relative_path = format!(
        "{}/{}",
        current_agent_path.as_str().trim_end_matches('/'),
        target.trim_matches('/')
    );
    let matches = tree
        .agents
        .values()
        .filter(|record| {
            record.task_path.as_str() == target
                || record.task_path.as_str() == relative_path
                || record.task_path.segments().last() == Some(target)
        })
        .map(|record| record.id)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(ToolError::Execution(format!(
            "sub-agent not found: {target}"
        ))),
        [id] => Ok(*id),
        _ => Err(ToolError::Execution(format!(
            "ambiguous sub-agent target: {target}; use its canonical task path or id"
        ))),
    }
}

fn resolve_target_mut<'a>(
    state: &'a mut ControlState,
    root_id: SessionId,
    current_agent_path: &AgentPath,
    target: &str,
) -> Result<&'a mut ChildRecord, ToolError> {
    let tree = state.trees.get_mut(&root_id).ok_or_else(no_subagents)?;
    let id = resolve_target_id(tree, current_agent_path, target)?;
    tree.agents
        .get_mut(&id)
        .ok_or_else(|| ToolError::Execution(format!("sub-agent not found: {target}")))
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

    fn state_only_control() -> AgentControl {
        AgentControl::new(
            None,
            Runtime::new(make_model(None), "test"),
            make_agent(),
            make_options(),
        )
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
        (
            AgentControl::new(None, runtime.clone(), agent, options),
            directory,
            runtime,
        )
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
        let lifecycle = terminal.map_or_else(
            || {
                ChildLifecycle::Running(ActiveTurns::new(TrackedTurn::test(
                    TurnId::new(),
                    CancellationToken::new(),
                )))
            },
            ChildLifecycle::Idle,
        );
        let mut state = control.inner.state.lock().await;
        state.trees.entry(root_id).or_default().agents.insert(
            session.id(),
            ChildRecord {
                id: session.id(),
                task_path,
                profile: profile.name,
                session: session.clone(),
                lifecycle,
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
        let agent = AgentProfile::explorer().apply(make_agent().with_tools(inherited));

        assert_eq!(
            tool_names(&agent),
            ["read", "glob", "grep", "webfetch", "skill"]
        );
        assert!(!tool_names(&agent).contains(&"custom_mutator"));
        assert!(!tool_names(&agent).contains(&"spawn_agent"));
    }

    #[test]
    fn default_and_worker_keep_only_inherited_tools() {
        for profile in [AgentProfile::default(), AgentProfile::worker()] {
            let inherited = ["read", "write", "custom_tool"]
                .into_iter()
                .map(named_tool)
                .collect();
            let agent = profile.apply(make_agent().with_tools(inherited));

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
        assert!(spawn.description.contains("leaf agent"));
        assert!(spawn.description.contains("independent workstreams"));
        assert!(spawn.description.contains("explorer"));
        assert!(spawn.description.contains("worker"));
        assert!(!spawn.description.contains("Runtime -> Session -> Turn"));
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
        for property in ["timeout_ms", "targets"] {
            assert!(wait.parameters_schema["properties"].get(property).is_some());
        }
        assert!(agent
            .system_prompt()
            .unwrap()
            .contains("<multi_agent_mode>"));
        assert!(agent
            .system_prompt()
            .unwrap()
            .contains("Before starting non-trivial work"));
    }

    #[test]
    fn rejects_installing_collaboration_on_an_already_enhanced_agent() {
        let options = make_options();
        let runtime = Runtime::new(make_model(None), "test");
        let (agent, _) = install_collaboration(make_agent(), options, runtime, None).unwrap();

        let error = install_collaboration(
            agent,
            make_options(),
            Runtime::new(make_model(None), "test"),
            None,
        )
        .map(|_| ())
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("collaboration tools are already installed"));
    }

    #[test]
    fn spawned_agents_derive_from_the_clean_base() {
        let base = make_agent().with_tools(vec![named_tool("read"), named_tool("write")]);
        let options = make_options();
        let runtime = Runtime::new(make_model(None), "test");
        let (main, control) = install_collaboration(base, options, runtime, None).unwrap();

        let child = AgentProfile::default().apply(control.inner.definition.clone());

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
    async fn failed_spawn_releases_the_task_path_reservation() {
        let blocked_directory = tempfile::NamedTempFile::new().unwrap();
        let runtime = Runtime::new(make_model(None), "test").with_session_store(Arc::new(
            ash_agent::JsonlSessionStore::new(blocked_directory.path()),
        ));
        let control = AgentControl::new(None, runtime, make_agent(), make_options());
        let mut context = make_context(root_identity());
        context.session.messages.push(Message::user("context"));
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
            assert!(error.to_string().contains("File exists"));
        }
    }

    #[tokio::test]
    async fn concurrency_limit_rejects_before_creating_a_child() {
        let control = AgentControl::new(
            Some(0),
            Runtime::new(make_model(None), "test"),
            make_agent(),
            make_options(),
        );
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
    async fn followup_reservations_enforce_the_limit_per_agent() {
        let (control, _directory, runtime) = make_control(make_model(None), 1);
        let root_id = SessionId::new();
        let first = insert_child(
            &control,
            &runtime,
            root_id,
            "first",
            AgentProfile::default(),
            Some(ChildState::Completed(None)),
            Some(1),
        )
        .await;
        let second = insert_child(
            &control,
            &runtime,
            root_id,
            "second",
            AgentProfile::default(),
            Some(ChildState::Completed(None)),
            Some(2),
        )
        .await;
        let mut state = control.inner.state.lock().await;
        let tree = state.trees.get_mut(&root_id).unwrap();

        tree.reserve_followup(first.id(), Some(1)).unwrap();
        let error = tree.reserve_followup(second.id(), Some(1)).unwrap_err();
        assert!(error
            .to_string()
            .contains("maximum of 1 concurrent sub-agents reached"));

        tree.reserve_followup(first.id(), Some(1)).unwrap();
        assert_eq!(tree.active_count(), 1);
        assert_eq!(tree.pending_followups.get(&first.id()), Some(&2));

        tree.release_slot(&SlotReservationKey::Followup(first.id()));
        assert_eq!(tree.active_count(), 1);
        tree.release_slot(&SlotReservationKey::Followup(first.id()));
        assert_eq!(tree.active_count(), 0);
        assert!(!tree.pending_followups.contains_key(&first.id()));

        tree.reserve_followup(second.id(), Some(1)).unwrap();
    }

    #[tokio::test]
    async fn duplicate_reservation_rejects_before_creating_a_child() {
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
            .insert(AgentPath::root().join("inspect").unwrap());

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

        assert!(error.to_string().contains("task path already exists"));
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
        let turn_ids = [TurnId::new(), TurnId::new(), TurnId::new()];
        let record = tree.agents.get_mut(&agent_id).unwrap();
        record.lifecycle = ChildLifecycle::Running(ActiveTurns {
            turns: turn_ids
                .iter()
                .map(|id| TrackedTurn::test(*id, CancellationToken::new()))
                .collect(),
        });
        // Observer tasks may acquire the controller lock out of order. The
        // second outcome remains buffered until the first turn settles.
        assert!(!tree.settle_turn(
            agent_id,
            turn_ids[1],
            ChildState::Completed(Some("first follow-up".to_string())),
        ));
        assert!(!tree.settle_turn(
            agent_id,
            turn_ids[0],
            ChildState::Completed(Some("initial result".to_string())),
        ));
        assert!(tree.settle_turn(
            agent_id,
            turn_ids[2],
            ChildState::Completed(Some("final result".to_string())),
        ));

        let record = tree.agents.get(&agent_id).unwrap();
        assert!(matches!(
            &record.lifecycle,
            ChildLifecycle::Idle(ChildState::Completed(message))
                if message.as_deref() == Some("final result")
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
                    start_turn: true,
                },
            )
            .await;

        assert!(result.is_err());
        let agents = control.snapshots(root_id).await;
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
                    start_turn: false,
                },
            )
            .await
            .unwrap();

        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(!result["turn_triggered"].as_bool().unwrap());
        // Notify stages guidance but must not start a turn.
        assert!(session.view().await.unwrap().messages.is_empty());
        let agents = control.snapshots(root_id).await;
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

        assert_eq!(control.snapshots(root).await.len(), 1);
        assert!(control.snapshots(other_root).await.is_empty());
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
        let running_turn_id = TurnId::new();
        {
            let mut state = control.inner.state.lock().await;
            let tree = state.trees.get_mut(&root_id).unwrap();
            tree.next_completion_revision = 1;
            tree.agents.get_mut(&running_id).unwrap().lifecycle = ChildLifecycle::Running(
                ActiveTurns::new(TrackedTurn::test(running_turn_id, CancellationToken::new())),
            );
            drop(state);
        }

        let context = make_context(SessionIdentity::root(root_id));
        let first = control
            .wait(
                &context,
                WaitAgentArgs {
                    timeout_ms: Some(100),
                    targets: None,
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
                        targets: None,
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
                running_turn_id,
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
                    targets: None,
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
                    targets: None,
                },
            )
            .await
            .unwrap();
        let completion: serde_json::Value = serde_json::from_str(&completion).unwrap();
        assert!(!completion["timed_out"].as_bool().unwrap());
        assert_eq!(completion["agents"][0]["final_message"], "result");
    }

    #[tokio::test]
    async fn wait_accepts_multiple_explicit_targets() {
        let (control, _directory, runtime) = make_control(make_model(None), 1);
        let root_id = SessionId::new();
        let first = insert_child(
            &control,
            &runtime,
            root_id,
            "first",
            AgentProfile::explorer(),
            Some(ChildState::Completed(Some("first result".to_string()))),
            Some(1),
        )
        .await;
        insert_child(
            &control,
            &runtime,
            root_id,
            "second",
            AgentProfile::worker(),
            Some(ChildState::Completed(Some("second result".to_string()))),
            Some(2),
        )
        .await;
        insert_child(
            &control,
            &runtime,
            root_id,
            "excluded",
            AgentProfile::default(),
            Some(ChildState::Completed(Some("excluded result".to_string()))),
            Some(3),
        )
        .await;

        let result = control
            .wait(
                &make_context(SessionIdentity::root(root_id)),
                WaitAgentArgs {
                    timeout_ms: Some(0),
                    targets: Some(vec![
                        first.id().to_string(),
                        "first".to_string(),
                        "/root/second".to_string(),
                    ]),
                },
            )
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(result["agents"].as_array().unwrap().len(), 2);
        assert_eq!(result["agents"][0]["task_path"], "/root/first");
        assert_eq!(result["agents"][1]["task_path"], "/root/second");
    }

    #[tokio::test]
    async fn wait_rejects_an_empty_target_list() {
        let control = state_only_control();
        let root_id = SessionId::new();

        let error = control
            .wait(
                &make_context(SessionIdentity::root(root_id)),
                WaitAgentArgs {
                    timeout_ms: Some(0),
                    targets: Some(Vec::new()),
                },
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("targets cannot be empty"));
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
        let active_token = CancellationToken::new();
        let queued_token = CancellationToken::new();
        let active_turn_id = TurnId::new();
        let queued_turn_id = TurnId::new();
        {
            let mut state = control.inner.state.lock().await;
            let record = state
                .trees
                .get_mut(&root_id)
                .unwrap()
                .agents
                .get_mut(&session.id())
                .unwrap();
            record.lifecycle = ChildLifecycle::Running(ActiveTurns {
                turns: VecDeque::from([
                    TrackedTurn::test(active_turn_id, active_token.clone()),
                    TrackedTurn::test(queued_turn_id, queued_token.clone()),
                ]),
            });
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
        assert!(active_token.is_cancelled());
        assert!(!queued_token.is_cancelled());

        {
            let mut state = control.inner.state.lock().await;
            assert!(!state.trees.get_mut(&root_id).unwrap().settle_turn(
                session.id(),
                active_turn_id,
                ChildState::Interrupted(None),
            ));
        }
        control
            .interrupt(
                &make_context(SessionIdentity::root(root_id)),
                InterruptAgentArgs {
                    target: "/root/inspect".to_string(),
                },
            )
            .await
            .unwrap();
        assert!(queued_token.is_cancelled());
        {
            let mut state = control.inner.state.lock().await;
            assert!(state.trees.get_mut(&root_id).unwrap().settle_turn(
                session.id(),
                queued_turn_id,
                ChildState::Interrupted(None),
            ));
        }

        // Interrupt must not close the session: a follow-up still lands.
        control
            .message_agent(
                &make_context(SessionIdentity::root(root_id)),
                MessageAgentArgs {
                    target: "/root/inspect".to_string(),
                    message: "continue".to_string(),
                    start_turn: true,
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
                let agents = control.snapshots(root_id).await;
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
                    start_turn: true,
                },
            )
            .await
            .unwrap();
        let followed_up = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let agents = control.snapshots(root_id).await;
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
                        snapshot.task_path == "/root/inspect"
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
                let agents = control.snapshots(root_id).await;
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
