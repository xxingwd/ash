use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};

use ash_agent::{Agent, Input, InputSource, Runtime, Thread, ThreadOptions};
use ash_core::{
    define_tool, CancellationToken, ContentBlock, Message, MessageContent, StopReason, ThreadId,
    Tool, ToolContext, ToolError, TreeId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex, Notify};

const DEFAULT_MAX_CONCURRENT_CHILDREN: usize = 3;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;
const COLLABORATION_TOOL_NAMES: [&str; 6] = [
    "spawn_agent",
    "send_message",
    "followup_task",
    "interrupt_agent",
    "list_agents",
    "wait_agent",
];

const MULTI_AGENT_INSTRUCTIONS: &str = r#"<multi_agent_mode>
You are one agent in a team that shares the same workspace and tools. Split work where parallelism pays; keep tightly coupled work local.

Delegation is proactive. You do not need separate permission to delegate work that is already inside the user's request.

Decision rules:
- Handle simple, one-path tasks yourself.
- When there are two or more independent questions or workstreams, usually spawn them in the same round.
- Use `explorer` for a focused, read-only codebase question that can be answered independently. Trust a completed exploration instead of repeating it.
- Prefer `worker` for a bounded implementation, fix, test, or refactor with a clear write scope.
- Use `default` for a self-contained task that does not fit the other roles.
- Keep an immediate blocker local when your very next action depends on it and delegation would only add waiting.

Operating rules:
- Give every agent a concrete task, expected output, and enough context to finish without guessing.
- For code changes, assign explicit files or modules. Write scopes must not overlap.
- The workspace is shared. Never ask an agent to revert unrelated edits; tell workers they are not alone in the codebase.
- After spawning, continue useful non-overlapping work immediately. Do not redo the delegated task.
- Use `wait_agent` only when an agent's result is required for the next step.
- Reuse context with `followup_task`; use `send_message` for guidance that should not start a new turn.
- Review returned changes before integrating them.
</multi_agent_mode>"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    Default,
    Explorer,
    Worker,
}

impl AgentRole {
    fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Explorer => "explorer",
            Self::Worker => "worker",
        }
    }

    fn available_roles_description() -> &'static str {
        r#"Optional type name for the new agent. If omitted, `default` is used.
Available roles:
default: General-purpose agent for a self-contained task that inherits the current configuration.
explorer: Use whenever a specific, well-scoped codebase question can be answered independently. Explorers are fast, read-only, and authoritative. Spawn multiple explorers in the same round for distinct questions; reuse an existing explorer for related follow-ups.
worker: Prefer for bounded implementation and production work such as features, fixes, tests, and refactors. Assign explicit file or module ownership, keep write scopes disjoint, and remind workers that the workspace is shared."#
    }

    fn child_instructions(self) -> &'static str {
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

impl FromStr for AgentRole {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "default" => Ok(Self::Default),
            "explorer" => Ok(Self::Explorer),
            "worker" => Ok(Self::Worker),
            other => Err(format!("unknown agent_type '{other}'")),
        }
    }
}

impl AgentSpawner for InheritedAgentSpawner {
    fn spawn(&self, request: SpawnRequest) -> Result<ChildAgent, ToolError> {
        let mut definition = self.definition.clone();
        definition.system_prompt = Some(subagent_system_prompt(
            self.system_prompt.as_deref(),
            request.role,
            &request.parent_path,
            &request.task_name,
        ));
        let scope = ThreadOptions {
            working_dir: self.scope.working_dir.clone(),
            tool_timeout: self.scope.tool_timeout,
            path: request.task_name,
            tree_id: Some(request.tree_id),
            metadata: self.scope.metadata.clone(),
        };
        Ok(ChildAgent {
            runtime: self.runtime.clone(),
            agent: definition,
            options: scope,
            history: request.messages,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Pending,
    Running,
    Completed,
    Interrupted,
    Errored,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSnapshot {
    pub agent_id: ThreadId,
    pub task_name: String,
    pub agent_type: AgentRole,
    pub status: AgentStatus,
    pub last_task_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct AgentControl {
    inner: Arc<ControlInner>,
}

struct ControlInner {
    state: Mutex<ControlState>,
    updates: Notify,
    max_concurrent_children: usize,
    spawner: Option<Arc<dyn AgentSpawner>>,
}

pub struct SpawnRequest {
    pub role: AgentRole,
    pub parent_path: String,
    pub task_name: String,
    pub messages: Vec<Message>,
    pub tree_id: TreeId,
}

pub struct ChildAgent {
    pub runtime: Runtime,
    pub agent: Agent,
    pub options: ThreadOptions,
    pub history: Vec<Message>,
}

pub trait AgentSpawner: Send + Sync {
    fn spawn(&self, request: SpawnRequest) -> Result<ChildAgent, ToolError>;
}

struct InheritedAgentSpawner {
    runtime: Runtime,
    system_prompt: Option<String>,
    definition: Agent,
    scope: ThreadOptions,
}

#[derive(Default)]
struct ControlState {
    threads: HashMap<TreeId, ChildSession>,
}

#[derive(Default)]
struct ChildSession {
    agents: HashMap<ThreadId, ChildRecord>,
    next_completion_revision: u64,
    wait_cursors: HashMap<String, u64>,
}

struct ChildRecord {
    id: ThreadId,
    task_name: String,
    role: AgentRole,
    state: ChildState,
    completion_revision: Option<u64>,
    last_task_message: String,
    command_tx: mpsc::Sender<ChildCommand>,
}

enum ChildState {
    Pending,
    Running(CancellationToken),
    RunningThenPending(CancellationToken),
    Completed(Option<String>),
    Interrupted(Option<String>),
    Errored {
        final_message: Option<String>,
        error: String,
    },
}

impl ChildState {
    fn status(&self) -> AgentStatus {
        match self {
            Self::Pending => AgentStatus::Pending,
            Self::Running(_) | Self::RunningThenPending(_) => AgentStatus::Running,
            Self::Completed(_) => AgentStatus::Completed,
            Self::Interrupted(_) => AgentStatus::Interrupted,
            Self::Errored { .. } => AgentStatus::Errored,
        }
    }

    fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Running(_) | Self::RunningThenPending(_)
        )
    }

    fn final_message(&self) -> Option<&str> {
        match self {
            Self::Completed(message) | Self::Interrupted(message) => message.as_deref(),
            Self::Errored { final_message, .. } => final_message.as_deref(),
            Self::Pending | Self::Running(_) | Self::RunningThenPending(_) => None,
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
            Self::Running(cancel) | Self::RunningThenPending(cancel) => Some(cancel.clone()),
            _ => None,
        }
    }

    fn queue_followup(&mut self) {
        let current = std::mem::replace(self, Self::Pending);
        *self = match current {
            Self::Running(cancel) | Self::RunningThenPending(cancel) => {
                Self::RunningThenPending(cancel)
            }
            Self::Pending | Self::Completed(_) | Self::Interrupted(_) | Self::Errored { .. } => {
                Self::Pending
            }
        };
    }

    fn finish_turn(self, completed: Self) -> Self {
        match self {
            Self::RunningThenPending(_) => Self::Pending,
            Self::Running(_)
            | Self::Pending
            | Self::Completed(_)
            | Self::Interrupted(_)
            | Self::Errored { .. } => completed,
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
        self.last_task_message.clear();
        self.last_task_message.push_str(message);
        if delivery == MessageDelivery::Followup {
            self.state.queue_followup();
            self.completion_revision = None;
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
    fn triggers_turn(self) -> bool {
        matches!(self, Self::Followup)
    }

    fn command(self, message: String) -> ChildCommand {
        match self {
            Self::Queue => ChildCommand::Queue(message),
            Self::Followup => ChildCommand::Followup(message),
        }
    }

    fn needs_new_slot(self, state: &ChildState) -> bool {
        self.triggers_turn() && !state.is_active()
    }
}

impl ControlState {
    fn active_count(&self, tree_id: TreeId) -> usize {
        self.threads
            .get(&tree_id)
            .map_or(0, ChildSession::active_count)
    }

    fn snapshots(&self, tree_id: TreeId, path_prefix: Option<&str>) -> Vec<AgentSnapshot> {
        let mut agents = self
            .threads
            .get(&tree_id)
            .into_iter()
            .flat_map(|session| session.agents.values())
            .filter(|record| path_prefix.is_none_or(|prefix| record.task_name.starts_with(prefix)))
            .map(ChildRecord::snapshot)
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.task_name.cmp(&right.task_name));
        agents
    }

    fn wait_snapshot(&mut self, tree_id: TreeId, waiter: &str) -> WaitSnapshot {
        self.threads
            .get_mut(&tree_id)
            .map_or_else(WaitSnapshot::empty, |session| session.wait_snapshot(waiter))
    }

    fn finish_turn(&mut self, tree_id: TreeId, agent_id: ThreadId, completed: ChildState) -> bool {
        self.threads
            .get_mut(&tree_id)
            .is_some_and(|session| session.finish_turn(agent_id, completed))
    }
}

impl ChildSession {
    fn active_count(&self) -> usize {
        self.agents
            .values()
            .filter(|record| record.state.is_active())
            .count()
    }

    fn wait_snapshot(&mut self, waiter: &str) -> WaitSnapshot {
        let cursor = self.wait_cursors.get(waiter).copied().unwrap_or(0);
        let completion_revision = self
            .agents
            .values()
            .filter(|record| !record.state.is_active())
            .filter_map(|record| record.completion_revision)
            .filter(|revision| *revision > cursor)
            .max();
        if let Some(revision) = completion_revision {
            self.wait_cursors.insert(waiter.to_string(), revision);
        }
        let mut agents = self
            .agents
            .values()
            .map(ChildRecord::snapshot)
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.task_name.cmp(&right.task_name));
        WaitSnapshot {
            agents,
            has_update: completion_revision.is_some(),
        }
    }

    fn finish_turn(&mut self, agent_id: ThreadId, completed: ChildState) -> bool {
        let Some(record) = self.agents.get_mut(&agent_id) else {
            return false;
        };
        let current = std::mem::replace(&mut record.state, ChildState::Pending);
        record.state = current.finish_turn(completed);
        if !record.state.is_active() {
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
    fn empty() -> Self {
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
    fork_turns: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MessageAgentArgs {
    /// Agent id, canonical task path, or unambiguous task name.
    target: String,
    /// Message text to deliver.
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InterruptAgentArgs {
    /// Agent id, canonical task path, or unambiguous task name.
    target: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListAgentsArgs {
    /// Optional canonical task-path prefix.
    path_prefix: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitAgentArgs {
    /// Maximum wait in milliseconds. Defaults to 10000 and is capped at 3600000.
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkMode {
    None,
    All,
    Last(usize),
}

impl ForkMode {
    fn parse(value: Option<&str>) -> Result<Self, ToolError> {
        let value = value.map(str::trim).filter(|value| !value.is_empty());
        match value.unwrap_or("all") {
            value if value.eq_ignore_ascii_case("none") => Ok(Self::None),
            value if value.eq_ignore_ascii_case("all") => Ok(Self::All),
            value => {
                let turns = value.parse::<usize>().map_err(|_| {
                    ToolError::Execution(
                        "fork_turns must be `none`, `all`, or a positive integer string"
                            .to_string(),
                    )
                })?;
                if turns == 0 {
                    return Err(ToolError::Execution(
                        "fork_turns must be `none`, `all`, or a positive integer string"
                            .to_string(),
                    ));
                }
                Ok(Self::Last(turns))
            }
        }
    }
}

impl Default for AgentControl {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CONCURRENT_CHILDREN)
    }
}

impl AgentControl {
    pub fn new(max_concurrent_children: usize) -> Self {
        Self::new_with_spawner(max_concurrent_children, None)
    }

    pub fn with_spawner(max_concurrent_children: usize, spawner: Arc<dyn AgentSpawner>) -> Self {
        Self::new_with_spawner(max_concurrent_children, Some(spawner))
    }

    fn new_with_spawner(
        max_concurrent_children: usize,
        spawner: Option<Arc<dyn AgentSpawner>>,
    ) -> Self {
        Self {
            inner: Arc::new(ControlInner {
                state: Mutex::new(ControlState::default()),
                updates: Notify::new(),
                max_concurrent_children,
                spawner,
            }),
        }
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let spawn = self.clone();
        let spawn_description = format!(
            "Spawn a sub-agent for a concrete, bounded task that can make progress independently alongside useful local work. Spawned agents inherit the current model, environment, AGENTS.md instructions, skills, and tools.\n\nUse this proactively when parallel work improves speed or quality:\n- Use `explorer` for an independent read-only codebase question.\n- Prefer `worker` for a bounded code change with explicit file or module ownership.\n- Use `default` for another self-contained task.\n- When two or more independent tasks are available, usually spawn them in the same round.\n- Do not spawn for a trivial task or an immediate blocker that must finish before your next action.\n- Give the agent the exact output you need; do not duplicate its work locally.\n- After spawning, continue non-overlapping work and call `wait_agent` only when its result becomes a blocker.\n\n{}",
            AgentRole::available_roles_description()
        );
        let spawn_tool = define_tool(
            "spawn_agent",
            &spawn_description,
            move |context, args: SpawnAgentArgs| {
                let control = spawn.clone();
                async move { control.spawn(context, args).await }
            },
        );

        let send = self.clone();
        let send_tool = define_tool(
            "send_message",
            "Send a message to an existing agent. The message is queued promptly and does not trigger a new turn.",
            move |context, args: MessageAgentArgs| {
                let control = send.clone();
                async move {
                    control
                        .send_message(&context, args, MessageDelivery::Queue)
                        .await
                }
            },
        );

        let followup = self.clone();
        let followup_tool = define_tool(
            "followup_task",
            "Reuse an existing agent for context-dependent work. Send a follow-up task and trigger another turn when it is idle; if it is running, queue the task for its next turn. Prefer this over spawning a replacement agent for related work.",
            move |context, args: MessageAgentArgs| {
                let control = followup.clone();
                async move {
                    control
                        .send_message(&context, args, MessageDelivery::Followup)
                        .await
                }
            },
        );

        let interrupt = self.clone();
        let interrupt_tool = define_tool(
            "interrupt_agent",
            "Interrupt an agent's current turn and return its previous status. The agent remains available for follow-up tasks.",
            move |context, args: InterruptAgentArgs| {
                let control = interrupt.clone();
                async move { control.interrupt(&context, args).await }
            },
        );

        let list = self.clone();
        let list_tool = define_tool(
            "list_agents",
            "List agents in the current root session, optionally filtered by canonical task-path prefix.",
            move |context, args: ListAgentsArgs| {
                let control = list.clone();
                async move { control.list(&context, args).await }
            },
        );

        let wait = self.clone();
        let wait_tool = define_tool(
            "wait_agent",
            "Wait for an agent update only when its result is required for your next step. Completed updates include the final message so it can be reviewed and integrated.",
            move |context, args: WaitAgentArgs| {
                let control = wait.clone();
                async move { control.wait(&context, args).await }
            },
        );

        vec![
            spawn_tool,
            send_tool,
            followup_tool,
            interrupt_tool,
            list_tool,
            wait_tool,
        ]
    }

    async fn spawn(&self, context: ToolContext, args: SpawnAgentArgs) -> Result<String, ToolError> {
        validate_task_name(&args.task_name)?;
        if args.message.trim().is_empty() {
            return Err(ToolError::Execution(
                "spawn_agent message cannot be empty".to_string(),
            ));
        }
        let role = args.agent_type.unwrap_or(AgentRole::Default);
        let fork_mode = ForkMode::parse(args.fork_turns.as_deref())?;
        let parent_path = normalized_agent_path(&context.agent.path);
        let task_name = format!("{parent_path}/{}", args.task_name);
        let (command_tx, command_rx) = mpsc::channel(32);

        let messages = fork_messages(&context.agent.messages, fork_mode);
        let spawner = self.inner.spawner.as_ref().ok_or_else(|| {
            ToolError::Execution("sub-agent spawner is not configured".to_string())
        })?;
        let mut child = spawner.spawn(SpawnRequest {
            role,
            parent_path,
            task_name: task_name.clone(),
            messages,
            tree_id: context.agent.tree_id,
        })?;
        child.agent.tools.extend(self.tools());
        let thread = child
            .runtime
            .start_with_history(child.agent, child.options, child.history)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let id = thread.id();

        {
            let mut state = self.inner.state.lock().await;
            let session = state.threads.entry(context.agent.tree_id).or_default();
            if session.active_count() >= self.inner.max_concurrent_children {
                return Err(ToolError::Execution(format!(
                    "maximum of {} concurrent sub-agents reached",
                    self.inner.max_concurrent_children
                )));
            }
            if session
                .agents
                .values()
                .any(|record| record.task_name == task_name)
            {
                return Err(ToolError::Execution(format!(
                    "agent task name already exists: {task_name}; use followup_task to reuse it"
                )));
            }
            session.agents.insert(
                id,
                ChildRecord {
                    id,
                    task_name: task_name.clone(),
                    role,
                    state: ChildState::Pending,
                    completion_revision: None,
                    last_task_message: args.message.clone(),
                    command_tx,
                },
            );
        }

        let control = self.clone();
        let tree_id = context.agent.tree_id;
        let initial_input = args.message.clone();
        tokio::spawn(async move {
            control
                .run_child(tree_id, id, thread, initial_input, command_rx)
                .await;
        });
        self.inner.updates.notify_waiters();

        json_output(&serde_json::json!({
            "agent_id": id,
            "task_name": task_name,
            "agent_type": role,
        }))
    }

    async fn send_message(
        &self,
        context: &ToolContext,
        args: MessageAgentArgs,
        delivery: MessageDelivery,
    ) -> Result<String, ToolError> {
        if args.message.trim().is_empty() {
            return Err(ToolError::Execution("message cannot be empty".to_string()));
        }
        let (target, command_tx) = {
            let mut state = self.inner.state.lock().await;
            let record = resolve_target_mut(
                &mut state,
                context.agent.tree_id,
                &context.agent.path,
                &args.target,
            )?;
            (record.task_name.clone(), record.command_tx.clone())
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
            if delivery.needs_new_slot(&record.state)
                && active_count >= self.inner.max_concurrent_children
            {
                return Err(ToolError::Execution(format!(
                    "maximum of {} concurrent sub-agents reached",
                    self.inner.max_concurrent_children
                )));
            }
            record.prepare_delivery(delivery, &args.message);
            permit.send(delivery.command(args.message));
        }
        self.inner.updates.notify_waiters();
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
            (
                record.task_name.clone(),
                record.state.status(),
                record.state.cancel(),
            )
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        json_output(&serde_json::json!({
            "target": target,
            "previous_status": previous_status,
        }))
    }

    async fn list(&self, context: &ToolContext, args: ListAgentsArgs) -> Result<String, ToolError> {
        let agents = self
            .snapshots(context.agent.tree_id, args.path_prefix.as_deref())
            .await;
        json_output(&serde_json::json!({ "agents": agents }))
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
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let notified = self.inner.updates.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = self
                .inner
                .state
                .lock()
                .await
                .wait_snapshot(context.agent.tree_id, &context.agent.path);
            if snapshot.agents.is_empty() || snapshot.has_update {
                return json_output(&serde_json::json!({
                    "agents": snapshot.agents,
                    "timed_out": false,
                }));
            }

            let timed_out = tokio::select! {
                _ = tokio::time::sleep_until(deadline) => true,
                _ = &mut notified => false,
            };
            if timed_out {
                let snapshot = self
                    .inner
                    .state
                    .lock()
                    .await
                    .wait_snapshot(context.agent.tree_id, &context.agent.path);
                return json_output(&serde_json::json!({
                    "agents": snapshot.agents,
                    "timed_out": !snapshot.has_update,
                }));
            }
        }
    }

    async fn snapshots(&self, tree_id: TreeId, path_prefix: Option<&str>) -> Vec<AgentSnapshot> {
        self.inner
            .state
            .lock()
            .await
            .snapshots(tree_id, path_prefix)
    }

    async fn run_child(
        &self,
        tree_id: TreeId,
        agent_id: ThreadId,
        thread: Thread,
        initial_input: String,
        mut command_rx: mpsc::Receiver<ChildCommand>,
    ) {
        self.run_child_turn(tree_id, agent_id, &thread, initial_input)
            .await;

        while let Some(command) = command_rx.recv().await {
            match command {
                ChildCommand::Queue(message) => {
                    let _ = thread
                        .notify(Input::from_text(InputSource::Agent, message))
                        .await;
                }
                ChildCommand::Followup(message) => {
                    self.run_child_turn(tree_id, agent_id, &thread, message)
                        .await;
                }
            }
        }
    }

    async fn run_child_turn(
        &self,
        tree_id: TreeId,
        agent_id: ThreadId,
        thread: &Thread,
        input: String,
    ) {
        let turn = match thread
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
                );
                self.inner.updates.notify_waiters();
                return;
            }
        };
        let cancel = turn.cancellation_token();
        {
            let mut state = self.inner.state.lock().await;
            let Some(record) = state
                .threads
                .get_mut(&tree_id)
                .and_then(|session| session.agents.get_mut(&agent_id))
            else {
                return;
            };
            record.state = ChildState::Running(cancel.clone());
        }
        self.inner.updates.notify_waiters();

        let result = turn.wait().await;
        let final_message = thread
            .messages()
            .await
            .ok()
            .and_then(|messages| final_assistant_message(&messages));

        let completed = match result {
            Ok(StopReason::Aborted) => ChildState::Interrupted(final_message),
            Ok(_) => ChildState::Completed(final_message),
            Err(error) => ChildState::Errored {
                final_message,
                error: error.to_string(),
            },
        };
        let mut state = self.inner.state.lock().await;
        if !state.finish_turn(tree_id, agent_id, completed) {
            return;
        }
        drop(state);
        self.inner.updates.notify_waiters();
    }
}

pub fn install_subagent_tools(agent: &mut Agent, options: ThreadOptions, runtime: Runtime) {
    if COLLABORATION_TOOL_NAMES
        .iter()
        .all(|name| agent.tools.iter().any(|tool| tool.name() == *name))
    {
        return;
    }
    agent
        .tools
        .retain(|tool| !COLLABORATION_TOOL_NAMES.contains(&tool.name()));
    let system_prompt = agent.system_prompt.get_or_insert_with(String::new);
    if !system_prompt.trim().is_empty() {
        system_prompt.push_str("\n\n");
    }
    system_prompt.push_str(MULTI_AGENT_INSTRUCTIONS);
    let spawner = Arc::new(InheritedAgentSpawner {
        runtime,
        system_prompt: agent.system_prompt.clone(),
        definition: agent.clone(),
        scope: options,
    });
    let control = AgentControl::with_spawner(DEFAULT_MAX_CONCURRENT_CHILDREN, spawner);
    agent.tools.extend(control.tools());
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

fn normalized_agent_path(agent_path: &str) -> String {
    let path = agent_path.trim().trim_end_matches('/');
    if path.is_empty() {
        "/root".to_string()
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
    let session = state.threads.get_mut(&tree_id).ok_or_else(|| {
        ToolError::Execution("no sub-agents exist in the current session".to_string())
    })?;
    if let Ok(agent_id) = ThreadId::from_str(target) {
        return session
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| ToolError::Execution(format!("sub-agent not found: {target}")));
    }

    let current_path = normalized_agent_path(current_agent_path);
    let relative_path = format!("{current_path}/{}", target.trim_matches('/'));
    let mut matches = session
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
            MessageContent::User(_) | MessageContent::Assistant(_) => None,
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

    fn test_agent() -> Agent {
        Agent {
            system_prompt: Some("base prompt".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 10,
            max_context_tokens: 200_000,
            context_policy: Arc::new(ash_agent::CodingContextPolicy),
        }
    }

    fn test_options() -> ThreadOptions {
        ThreadOptions {
            working_dir: PathBuf::from("."),
            tool_timeout: Duration::from_secs(5),
            ..ThreadOptions::default()
        }
    }

    fn test_context(tree_id: TreeId) -> ToolContext {
        ToolContext {
            thread_id: ThreadId::new(),
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

    fn test_model(base_url: Option<String>) -> Arc<dyn ModelClient> {
        create_adapter(ProviderConfig {
            protocol: Protocol::OpenaiResponses,
            api_key: "test".into(),
            base_url,
        })
    }

    fn test_control(model: Arc<dyn ModelClient>, max_turns: u32) -> AgentControl {
        let mut agent = test_agent();
        agent.max_turns = max_turns;
        let options = test_options();
        let spawner = Arc::new(InheritedAgentSpawner {
            runtime: Runtime::new(model, "test"),
            system_prompt: agent.system_prompt.clone(),
            definition: agent,
            scope: options,
        });
        AgentControl::with_spawner(DEFAULT_MAX_CONCURRENT_CHILDREN, spawner)
    }

    #[test]
    fn exposes_the_codex_0_144_3_builtin_roles() {
        assert_eq!(AgentRole::from_str("default"), Ok(AgentRole::Default));
        assert_eq!(AgentRole::from_str("explorer"), Ok(AgentRole::Explorer));
        assert_eq!(AgentRole::from_str("worker"), Ok(AgentRole::Worker));
        assert!(AgentRole::from_str("awaiter").is_err());
    }

    #[test]
    fn installs_the_codex_style_collaboration_tools_and_prompt() {
        let mut agent = test_agent();
        let options = test_options();
        let runtime = Runtime::new(test_model(None), "test");
        install_subagent_tools(&mut agent, options.clone(), runtime.clone());
        install_subagent_tools(&mut agent, options, runtime);
        let names = agent
            .tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "spawn_agent",
                "send_message",
                "followup_task",
                "interrupt_agent",
                "list_agents",
                "wait_agent",
            ]
        );
        let spawn = agent.tools[0].definition();
        assert!(spawn.description.contains("explorer"));
        assert!(spawn.description.contains("worker"));
        assert!(spawn.description.contains("same round"));
        assert!(spawn.description.contains("continue non-overlapping work"));
        for property in ["task_name", "message", "agent_type", "fork_turns"] {
            assert!(spawn.parameters_schema["properties"]
                .get(property)
                .is_some());
        }
        assert!(agent
            .system_prompt
            .as_deref()
            .unwrap()
            .contains("<multi_agent_mode>"));
        assert!(agent
            .system_prompt
            .as_deref()
            .unwrap()
            .contains("Delegation is proactive"));
        assert_eq!(
            agent
                .system_prompt
                .as_deref()
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

    #[test]
    fn queued_followup_keeps_the_current_turn_active_until_the_next_turn() {
        let mut state = ChildState::Running(CancellationToken::new());

        state.queue_followup();
        assert_eq!(state.status(), AgentStatus::Running);

        let state = state.finish_turn(ChildState::Completed(Some("first turn".to_string())));
        assert!(matches!(state, ChildState::Pending));
    }

    #[tokio::test]
    async fn failed_delivery_does_not_change_the_agent_lifecycle() {
        let control = AgentControl::default();
        let tree_id = TreeId::new();
        let agent_id = ThreadId::new();
        let (command_tx, command_rx) = mpsc::channel(1);
        drop(command_rx);
        control
            .inner
            .state
            .lock()
            .await
            .threads
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
                    completion_revision: None,
                    last_task_message: "original".to_string(),
                    command_tx,
                },
            );

        let result = control
            .send_message(
                &test_context(tree_id),
                MessageAgentArgs {
                    target: "/root/inspect".to_string(),
                    message: "follow up".to_string(),
                },
                MessageDelivery::Followup,
            )
            .await;

        assert!(result.is_err());
        let agents = control.snapshots(tree_id, None).await;
        assert_eq!(agents[0].status, AgentStatus::Completed);
        assert_eq!(agents[0].last_task_message, "original");
    }

    #[test]
    fn forks_all_none_or_recent_complete_turns() {
        let call_id = ash_core::ToolCallId::from_provider("spawn");
        let messages = vec![
            Message::user("first"),
            Message::assistant_text("first answer"),
            Message::user("second"),
            Message::assistant_text("second answer"),
            Message {
                id: ash_core::MessageId::new(),
                role: ash_core::Role::Assistant,
                content: MessageContent::Assistant(vec![ContentBlock::ToolCall {
                    id: call_id,
                    name: "spawn_agent".to_string(),
                    arguments: serde_json::json!({}),
                }]),
            },
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
            Message {
                id: ash_core::MessageId::new(),
                role: ash_core::Role::Assistant,
                content: MessageContent::Assistant(vec![
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
            },
            Message {
                id: ash_core::MessageId::new(),
                role: ash_core::Role::User,
                content: MessageContent::ToolResult {
                    id: first_call,
                    result: Ok("done".to_string()),
                    attachments: Vec::new(),
                },
            },
        ];

        let forked = fork_messages(&messages, ForkMode::All);

        assert_eq!(forked.len(), 3);
        assert!(matches!(forked[2].content, MessageContent::User(_)));

        let mut complete = messages;
        complete.push(Message {
            id: ash_core::MessageId::new(),
            role: ash_core::Role::User,
            content: MessageContent::ToolResult {
                id: second_call,
                result: Ok("spawned".to_string()),
                attachments: Vec::new(),
            },
        });
        assert_eq!(fork_messages(&complete, ForkMode::All).len(), 6);
    }

    #[tokio::test]
    async fn isolates_agent_trees_by_root_session() {
        let control = AgentControl::default();
        let root = TreeId::new();
        let other_root = TreeId::new();
        let agent_id = ThreadId::new();
        let (command_tx, _command_rx) = mpsc::channel(1);
        control
            .inner
            .state
            .lock()
            .await
            .threads
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
        let control = AgentControl::default();
        let tree_id = TreeId::new();
        let completed_id = ThreadId::new();
        let running_id = ThreadId::new();
        let (completed_tx, _completed_rx) = mpsc::channel(1);
        let (running_tx, _running_rx) = mpsc::channel(1);
        {
            let mut state = control.inner.state.lock().await;
            let session = state.threads.entry(tree_id).or_default();
            session.next_completion_revision = 1;
            session.agents.insert(
                completed_id,
                ChildRecord {
                    id: completed_id,
                    task_name: "/root/completed".to_string(),
                    role: AgentRole::Explorer,
                    state: ChildState::Completed(Some("first result".to_string())),
                    completion_revision: Some(1),
                    last_task_message: "first task".to_string(),
                    command_tx: completed_tx,
                },
            );
            session.agents.insert(
                running_id,
                ChildRecord {
                    id: running_id,
                    task_name: "/root/running".to_string(),
                    role: AgentRole::Worker,
                    state: ChildState::Running(CancellationToken::new()),
                    completion_revision: None,
                    last_task_message: "second task".to_string(),
                    command_tx: running_tx,
                },
            );
        }

        let context = test_context(tree_id);
        let first = control
            .wait(
                &context,
                WaitAgentArgs {
                    timeout_ms: Some(100),
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
            ));
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
    async fn runs_a_spawned_agent_and_captures_its_final_message() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for answer in ["child done", "followup done"] {
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

        let control = test_control(test_model(Some(format!("http://{address}"))), 2);
        let tree_id = TreeId::new();
        let context = ToolContext {
            thread_id: ThreadId::new(),
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
                    fork_turns: Some("all".to_string()),
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
            .send_message(
                &followup_context,
                MessageAgentArgs {
                    target: "/root/inspect".to_string(),
                    message: "Confirm the finding.".to_string(),
                },
                MessageDelivery::Followup,
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
}
