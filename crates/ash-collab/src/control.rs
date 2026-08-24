use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use crate::snapshot::{SubagentSnapshot, SubagentState, SubagentTreeSnapshot};
use ash_agent::{Agent, Input, InputSource, Runtime, Session, SessionOptions, Turn};
use ash_core::{
    define_tool, define_tool_with_timeout, is_valid_segment, AgentPath, CancellationToken,
    ContentBlock, Message, MessageContent, SessionId, Tool, ToolContext, ToolError, ToolTimeout,
    TurnId, TurnResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch, Mutex, Notify};

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;
const COLLABORATION_TOOL_NAMES: [&str; 5] = [
    "agent",
    "message_agent",
    "list_agents",
    "remove_agent",
    "wait_agent",
];
/// Explorer projection is deliberately an allowlist: unknown custom and MCP
/// tools are not assumed to be read-only.
const EXPLORER_TOOL_NAMES: [&str; 5] = ["read", "glob", "grep", "webfetch", "skill"];

const COLLABORATION_INSTRUCTIONS: &str = "You are the main agent. Before non-trivial work, identify independent workstreams. Delegate concrete, bounded work that benefits from separate execution. Keep simple tasks, immediate blockers, and tightly coupled work local. Reuse an existing agent with `message_agent` for related follow-ups. Review agent results before using them. Child agents share the workspace and cannot delegate further.";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolPolicy {
    Inherit,
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

struct AgentProfile {
    name: ProfileName,
    description: &'static str,
    prompt: &'static str,
    tools: ToolPolicy,
}

impl AgentProfile {
    const fn for_name(name: ProfileName) -> Self {
        match name {
            ProfileName::Default => Self {
                name,
                description: "General-purpose agent for self-contained work.",
                prompt: "Handle the assignment directly and return a concise, evidence-backed result.",
                tools: ToolPolicy::Inherit,
            },
            ProfileName::Explorer => Self {
                name,
                description: "Use for focused, independent, read-only codebase investigation.",
                prompt: "Answer through read-only inspection. Return concrete findings with relevant paths and symbols.",
                tools: ToolPolicy::Allow(&EXPLORER_TOOL_NAMES),
            },
            ProfileName::Worker => Self {
                name,
                description: "Use for bounded implementation, testing, and refactoring.",
                prompt: "Execute the assignment, preserve unrelated changes, verify the result, and report changed files.",
                tools: ToolPolicy::Inherit,
            },
        }
    }

    fn apply(self, agent: Agent) -> Agent {
        let prompt = [agent.system_prompt().unwrap_or_default(), self.prompt]
            .into_iter()
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.tools.apply(agent.with_system_prompt(prompt))
    }

    fn descriptions() -> String {
        ProfileName::ALL
            .into_iter()
            .map(Self::for_name)
            .map(|profile| format!("{}: {}", profile.name.as_str(), profile.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone)]
pub struct AgentControl {
    inner: Arc<ControlInner>,
}

struct ControlInner {
    state: Mutex<ControlState>,
    updates: Notify,
    shutdown: CancellationToken,
    runtime: Runtime,
    definition: Agent,
    options: SessionOptions,
    subagent_tx: watch::Sender<Vec<SubagentTreeSnapshot>>,
}

impl Drop for ControlInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[derive(Default)]
struct ControlState {
    trees: HashMap<SessionId, AgentTree>,
}

#[derive(Default)]
struct AgentTree {
    agents: HashMap<AgentPath, AgentEntry>,
    removed: HashSet<AgentPath>,
    completions: VecDeque<QueuedCompletion>,
}

struct AgentEntry {
    name: String,
    profile: ProfileName,
    session: Session,
    turns: VecDeque<TrackedTurn>,
    completion_tx: mpsc::UnboundedSender<TurnObservation>,
    last_message: String,
}

struct TrackedTurn {
    id: TurnId,
    cancellation: CancellationToken,
    message: String,
}

struct TurnObservation {
    turn: Turn,
    response: Option<oneshot::Sender<CompletionDelivery>>,
}

struct CompletionDelivery {
    completion: AgentCompletion,
    acknowledge: oneshot::Sender<()>,
}

struct QueuedCompletion {
    path: AgentPath,
    completion: AgentCompletion,
}

#[derive(Debug, Clone, Serialize)]
struct AgentCompletion {
    name: String,
    profile: ProfileName,
    message: String,
    result: TurnResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_message: Option<String>,
}

impl AgentEntry {
    fn snapshot(&self) -> SubagentSnapshot {
        SubagentSnapshot {
            name: self.name.clone(),
            profile: self.profile.as_str().to_string(),
            state: if self.turns.is_empty() {
                SubagentState::Idle
            } else {
                SubagentState::Running
            },
            usage: self.session.stats().total_usage(),
            last_message: self.last_message.clone(),
        }
    }

    fn cancel_pending(&self) {
        self.turns
            .iter()
            .for_each(|turn| turn.cancellation.cancel());
    }

    fn finish(&mut self, turn_id: TurnId) -> Option<TrackedTurn> {
        if self.turns.front()?.id == turn_id {
            self.turns.pop_front()
        } else {
            None
        }
    }
}

impl AgentTree {
    fn snapshots(&self) -> Vec<SubagentSnapshot> {
        let mut agents = self
            .agents
            .values()
            .map(AgentEntry::snapshot)
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| {
            right
                .state
                .is_active()
                .cmp(&left.state.is_active())
                .then_with(|| left.name.cmp(&right.name))
        });
        agents
    }
}

impl ControlState {
    fn snapshots(&self) -> Vec<SubagentTreeSnapshot> {
        let mut trees = self
            .trees
            .iter()
            .map(|(root_id, tree)| SubagentTreeSnapshot {
                root_id: *root_id,
                agents: tree.snapshots(),
            })
            .collect::<Vec<_>>();
        trees.sort_by_key(|tree| tree.root_id.to_string());
        trees
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AgentArgs {
    /// Stable name using lowercase letters, digits, underscores, or hyphens.
    name: String,
    /// Initial message for the new agent.
    message: String,
    /// Optional profile: default, explorer, or worker.
    profile: Option<ProfileName>,
    /// Wait for this turn and return its result. Defaults to true.
    #[serde(default = "default_true")]
    wait: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MessageAgentArgs {
    /// Name of an existing agent.
    name: String,
    /// Follow-up message for the agent.
    message: String,
    /// Cancel all unfinished turns before submitting this message.
    #[serde(default)]
    interrupt: bool,
    /// Wait for this turn and return its result. Defaults to true.
    #[serde(default = "default_true")]
    wait: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListAgentsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveAgentArgs {
    /// Name of an existing agent to detach.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitAgentArgs {
    /// Maximum wait in milliseconds. Use 0 for an immediate snapshot. Defaults to 10000 and is capped at 3600000.
    timeout_ms: Option<u64>,
}

const fn default_true() -> bool {
    true
}

impl AgentControl {
    fn new(runtime: Runtime, definition: Agent, options: SessionOptions) -> Self {
        let (subagent_tx, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(ControlInner {
                state: Mutex::new(ControlState::default()),
                updates: Notify::new(),
                shutdown: CancellationToken::new(),
                runtime,
                definition,
                options,
                subagent_tx,
            }),
        }
    }

    /// Subscribe to root-scoped projections of all agents owned by this controller.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Vec<SubagentTreeSnapshot>> {
        self.inner.subagent_tx.subscribe()
    }

    async fn publish(&self) {
        let state = self.inner.state.lock().await;
        self.inner.subagent_tx.send_replace(state.snapshots());
    }

    /// Build the model-visible collaboration tools.
    ///
    /// # Errors
    ///
    /// Returns `ToolError` when a tool definition cannot be created.
    pub fn tools(&self) -> Result<Vec<Arc<dyn Tool>>, ToolError> {
        let create = self.clone();
        let description = format!(
            "Create a named leaf agent and send its initial message. Use this proactively for a concrete, bounded workstream that can be investigated or implemented independently. With `wait=true`, return the result in this call; with `wait=false`, submit it in the background for a later `wait_agent` call. Include enough context and the expected output. Names remain available for follow-up messages.\n\nAvailable profiles:\n{}",
            AgentProfile::descriptions()
        );
        let agent = define_tool_with_timeout(
            "agent",
            &description,
            ToolTimeout::Disabled,
            move |context, args: AgentArgs| {
                let control = create.clone();
                async move { control.create(context, args).await }
            },
        )?;

        let message = self.clone();
        let message_agent = define_tool_with_timeout(
            "message_agent",
            "Send a follow-up message to an existing named agent. Busy agents queue turns in submission order. Set `interrupt=true` to cancel unfinished work first. With `wait=true`, return the result in this call; with `wait=false`, submit it in the background for `wait_agent`.",
            ToolTimeout::Disabled,
            move |context, args: MessageAgentArgs| {
                let control = message.clone();
                async move { control.message(context, args).await }
            },
        )?;

        let wait = self.clone();
        let wait_agent = define_tool_with_timeout(
            "wait_agent",
            "Wait for unread agent-turn results. Returns every result completed since the previous wait, plus the current idle/running snapshot.",
            ToolTimeout::Disabled,
            move |context, args: WaitAgentArgs| {
                let control = wait.clone();
                async move { control.wait(context, args).await }
            },
        )?;

        let list = self.clone();
        let list_agents = define_tool(
            "list_agents",
            "List active child agents with their state, current session usage, and latest task.",
            move |context, _: ListAgentsArgs| {
                let control = list.clone();
                async move { control.list(context).await }
            },
        )?;

        let remove = self.clone();
        let remove_agent = define_tool(
            "remove_agent",
            "Detach a named child agent and cancel its unfinished turns. Its persisted session is retained, but the name cannot be reused in this agent tree.",
            move |context, args: RemoveAgentArgs| {
                let control = remove.clone();
                async move { control.remove(context, args).await }
            },
        )?;

        Ok(vec![
            agent,
            message_agent,
            list_agents,
            remove_agent,
            wait_agent,
        ])
    }

    async fn create(&self, context: ToolContext, args: AgentArgs) -> Result<String, ToolError> {
        validate_name(&args.name)?;
        validate_message(&args.message)?;

        let identity = context.session.identity;
        let path = identity
            .path
            .join(&args.name)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let profile = args.profile.unwrap_or(ProfileName::Default);
        let result = {
            // Holding this lock across child construction is intentional: it is
            // the creation reservation, and keeps duplicate names impossible
            // without a second reservation state machine.
            let mut state = self.inner.state.lock().await;
            let tree = state.trees.entry(identity.root_id).or_default();
            if tree.agents.contains_key(&path) || tree.removed.contains(&path) {
                return Err(ToolError::Execution(format!(
                    "agent name already used: {}",
                    args.name
                )));
            }

            let child = AgentProfile::for_name(profile).apply(self.inner.definition.clone());
            let session = self
                .inner
                .runtime
                .start_child(
                    &child,
                    &self.inner.options,
                    &identity,
                    &args.name,
                    Vec::new(),
                )
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            self.spawn_stats_pump(identity.root_id, path.clone(), &session);
            let (completion_tx, observations) = mpsc::unbounded_channel();
            let Submission {
                tracked,
                observation,
                result,
            } = submit(&session, args.message.clone(), args.wait).await?;
            self.spawn_completion_pump(identity.root_id, path.clone(), observations);
            completion_tx
                .send(observation)
                .map_err(|_| ToolError::Execution("agent completion pump stopped".to_string()))?;
            tree.agents.insert(
                path,
                AgentEntry {
                    name: args.name.clone(),
                    profile,
                    session,
                    turns: VecDeque::from([tracked]),
                    completion_tx,
                    last_message: args.message,
                },
            );
            result
        };

        self.publish().await;
        self.submission_result(&args.name, profile, result).await
    }

    async fn message(
        &self,
        context: ToolContext,
        args: MessageAgentArgs,
    ) -> Result<String, ToolError> {
        validate_name(&args.name)?;
        validate_message(&args.message)?;

        let identity = context.session.identity;
        let path = identity
            .path
            .join(&args.name)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let (profile, result) = {
            let mut state = self.inner.state.lock().await;
            let entry = state
                .trees
                .get_mut(&identity.root_id)
                .and_then(|tree| tree.agents.get_mut(&path))
                .ok_or_else(|| ToolError::Execution(format!("agent not found: {}", args.name)))?;
            if args.interrupt {
                entry.cancel_pending();
            }
            let submission = submit(&entry.session, args.message.clone(), args.wait).await?;
            entry.turns.push_back(submission.tracked);
            entry
                .completion_tx
                .send(submission.observation)
                .map_err(|_| ToolError::Execution("agent completion pump stopped".to_string()))?;
            entry.last_message = args.message;
            (entry.profile, submission.result)
        };

        self.publish().await;
        self.submission_result(&args.name, profile, result).await
    }

    async fn submission_result(
        &self,
        name: &str,
        profile: ProfileName,
        result: Option<oneshot::Receiver<CompletionDelivery>>,
    ) -> Result<String, ToolError> {
        match result {
            Some(result) => {
                let delivery = result.await.map_err(|_| {
                    ToolError::Execution(format!("agent stopped before replying: {name}"))
                })?;
                let output = json_output(&delivery.completion)?;
                let _ = delivery.acknowledge.send(());
                Ok(output)
            }
            None => json_output(&serde_json::json!({
                "name": name,
                "profile": profile,
                "accepted": true,
            })),
        }
    }

    async fn list(&self, context: ToolContext) -> Result<String, ToolError> {
        let root_id = context.session.identity.root_id;
        let agents = self
            .inner
            .state
            .lock()
            .await
            .trees
            .get(&root_id)
            .map_or_else(Vec::new, AgentTree::snapshots);
        json_output(&serde_json::json!({ "agents": agents }))
    }

    async fn remove(
        &self,
        context: ToolContext,
        args: RemoveAgentArgs,
    ) -> Result<String, ToolError> {
        validate_name(&args.name)?;
        let identity = context.session.identity;
        let path = identity
            .path
            .join(&args.name)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let entry =
            {
                let mut state = self.inner.state.lock().await;
                let tree = state.trees.get_mut(&identity.root_id).ok_or_else(|| {
                    ToolError::Execution(format!("agent not found: {}", args.name))
                })?;
                let entry = tree.agents.remove(&path).ok_or_else(|| {
                    ToolError::Execution(format!("agent not found: {}", args.name))
                })?;
                tree.removed.insert(path.clone());
                tree.completions.retain(|queued| queued.path != path);
                entry
            };
        entry.cancel_pending();
        drop(entry);
        self.publish().await;
        self.inner.updates.notify_waiters();
        json_output(&serde_json::json!({
            "name": args.name,
            "removed": true,
        }))
    }

    async fn wait(&self, context: ToolContext, args: WaitAgentArgs) -> Result<String, ToolError> {
        let max_timeout_ms = context.deadline.map_or(MAX_WAIT_TIMEOUT_MS, |deadline| {
            u64::try_from(
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
            .min(MAX_WAIT_TIMEOUT_MS)
        });
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
            .min(max_timeout_ms);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let root_id = context.session.identity.root_id;

        loop {
            let notified = self.inner.updates.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let output = {
                let mut state = self.inner.state.lock().await;
                let tree = state.trees.entry(root_id).or_default();
                let all_idle = tree.agents.values().all(|agent| agent.turns.is_empty());
                (!tree.completions.is_empty() || all_idle || timeout_ms == 0).then(|| {
                    wait_output(
                        tree.completions
                            .drain(..)
                            .map(|queued| queued.completion)
                            .collect(),
                        tree.snapshots(),
                        false,
                    )
                })
            };
            if let Some(output) = output {
                return output;
            }

            let timed_out = tokio::select! {
                () = tokio::time::sleep_until(deadline) => true,
                () = &mut notified => false,
            };
            if timed_out {
                let mut state = self.inner.state.lock().await;
                let tree = state.trees.entry(root_id).or_default();
                let completions = tree
                    .completions
                    .drain(..)
                    .map(|queued| queued.completion)
                    .collect::<Vec<_>>();
                let timed_out = completions.is_empty();
                return wait_output(completions, tree.snapshots(), timed_out);
            }
        }
    }

    fn spawn_completion_pump(
        &self,
        root_id: SessionId,
        path: AgentPath,
        mut observations: mpsc::UnboundedReceiver<TurnObservation>,
    ) {
        let control = Arc::downgrade(&self.inner);
        let shutdown = self.inner.shutdown.clone();
        tokio::spawn(async move {
            while let Some(observation) = observations.recv().await {
                let TurnObservation { turn, response } = observation;
                let turn_id = turn.id();
                let settled = tokio::select! {
                    () = shutdown.cancelled() => break,
                    result = turn.wait() => result,
                };
                let (result, final_message) = match settled {
                    Ok(view) => (view.result, final_assistant_message(&view.messages)),
                    Err(error) => (TurnResult::Failed(error.to_string()), None),
                };
                let Some(inner) = control.upgrade() else {
                    break;
                };
                let control = Self { inner };
                if !control
                    .settle_turn(root_id, &path, turn_id, result, final_message, response)
                    .await
                {
                    break;
                }
            }
        });
    }

    fn spawn_stats_pump(&self, root_id: SessionId, path: AgentPath, session: &Session) {
        let control = Arc::downgrade(&self.inner);
        let shutdown = self.inner.shutdown.clone();
        let mut stats = session.subscribe_stats();
        tokio::spawn(async move {
            let mut usage = stats.borrow().total_usage();
            loop {
                let changed = tokio::select! {
                    () = shutdown.cancelled() => break,
                    changed = stats.changed() => changed,
                };
                if changed.is_err() {
                    break;
                }
                let next_usage = stats.borrow_and_update().total_usage();
                if usage == next_usage {
                    continue;
                }
                usage = next_usage;
                let Some(inner) = control.upgrade() else {
                    break;
                };
                let control = Self { inner };
                if !control.publish_agent_stats(root_id, &path).await {
                    break;
                }
            }
        });
    }

    async fn publish_agent_stats(&self, root_id: SessionId, path: &AgentPath) -> bool {
        let state = self.inner.state.lock().await;
        if state
            .trees
            .get(&root_id)
            .is_none_or(|tree| !tree.agents.contains_key(path))
        {
            return false;
        }
        self.inner.subagent_tx.send_replace(state.snapshots());
        true
    }

    async fn settle_turn(
        &self,
        root_id: SessionId,
        path: &AgentPath,
        turn_id: TurnId,
        result: TurnResult,
        final_message: Option<String>,
        response: Option<oneshot::Sender<CompletionDelivery>>,
    ) -> bool {
        let delivery = {
            let mut state = self.inner.state.lock().await;
            let Some(tree) = state.trees.get_mut(&root_id) else {
                return false;
            };
            let Some(entry) = tree.agents.get_mut(path) else {
                return false;
            };
            let Some(tracked) = entry.finish(turn_id) else {
                tracing::error!(%turn_id, agent = %entry.name, "agent turns settled out of order");
                return false;
            };
            let completion = AgentCompletion {
                name: entry.name.clone(),
                profile: entry.profile,
                message: tracked.message,
                result,
                final_message,
            };
            match response {
                Some(response) => Some((completion, response)),
                None => {
                    tree.completions.push_back(QueuedCompletion {
                        path: path.clone(),
                        completion,
                    });
                    None
                }
            }
        };

        self.publish().await;
        if let Some((completion, response)) = delivery {
            self.deliver_completion(root_id, path.clone(), completion, response)
                .await;
        } else {
            self.inner.updates.notify_waiters();
        }
        true
    }

    async fn deliver_completion(
        &self,
        root_id: SessionId,
        path: AgentPath,
        completion: AgentCompletion,
        response: oneshot::Sender<CompletionDelivery>,
    ) {
        let fallback = completion.clone();
        let (acknowledge, consumed) = oneshot::channel();
        if response
            .send(CompletionDelivery {
                completion,
                acknowledge,
            })
            .is_ok()
            && consumed.await.is_ok()
        {
            return;
        }
        self.queue_completion(root_id, path, fallback).await;
    }

    async fn queue_completion(
        &self,
        root_id: SessionId,
        path: AgentPath,
        completion: AgentCompletion,
    ) {
        let mut state = self.inner.state.lock().await;
        let Some(tree) = state.trees.get_mut(&root_id) else {
            return;
        };
        if !tree.agents.contains_key(&path) {
            return;
        }
        tree.completions
            .push_back(QueuedCompletion { path, completion });
        drop(state);
        self.inner.updates.notify_waiters();
    }
}

struct Submission {
    tracked: TrackedTurn,
    observation: TurnObservation,
    result: Option<oneshot::Receiver<CompletionDelivery>>,
}

async fn submit(session: &Session, message: String, wait: bool) -> Result<Submission, ToolError> {
    let turn = session
        .submit(Input::from_text(InputSource::Agent, message.clone()))
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let (response, result) = if wait {
        let (sender, receiver) = oneshot::channel();
        (Some(sender), Some(receiver))
    } else {
        (None, None)
    };
    let tracked = TrackedTurn {
        id: turn.id(),
        cancellation: turn.cancellation_token(),
        message,
    };
    Ok(Submission {
        tracked,
        observation: TurnObservation { turn, response },
        result,
    })
}

/// Install collaboration on the main agent. Children derive from the clean
/// base agent, so they never inherit collaboration tools.
///
/// # Errors
///
/// Returns `ToolError` when collaboration is already installed or a tool
/// definition cannot be created.
pub fn install_collaboration(
    base: Agent,
    options: SessionOptions,
    runtime: Runtime,
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
    let control = AgentControl::new(runtime, base.clone(), options);
    let agent = with_collaboration_instructions(base).pushing_tools(control.tools()?);
    Ok((agent, control))
}

fn with_collaboration_instructions(agent: Agent) -> Agent {
    let prompt = agent
        .system_prompt()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .map_or_else(
            || COLLABORATION_INSTRUCTIONS.to_string(),
            |prompt| format!("{prompt}\n\n{COLLABORATION_INSTRUCTIONS}"),
        );
    agent.with_system_prompt(prompt)
}

fn validate_name(name: &str) -> Result<(), ToolError> {
    if is_valid_segment(name) {
        Ok(())
    } else {
        Err(ToolError::Execution(
            "name must use lowercase letters, digits, underscores, or hyphens and be at most 64 characters"
                .to_string(),
        ))
    }
}

fn validate_message(message: &str) -> Result<(), ToolError> {
    if message.trim().is_empty() {
        Err(ToolError::Execution("message cannot be empty".to_string()))
    } else {
        Ok(())
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

fn wait_output(
    completions: Vec<AgentCompletion>,
    agents: Vec<SubagentSnapshot>,
    timed_out: bool,
) -> Result<String, ToolError> {
    json_output(&serde_json::json!({
        "completions": completions,
        "agents": agents,
        "timed_out": timed_out,
    }))
}

fn json_output(value: &impl Serialize) -> Result<String, ToolError> {
    serde_json::to_string_pretty(value)
        .map_err(|error| ToolError::Execution(format!("failed to serialize agent result: {error}")))
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex as StdMutex,
        },
    };

    use ash_core::{
        ModelClient, ModelEvent, ModelId, ModelRequest, ModelStream, SessionIdentity,
        SessionToolContext, StopReason,
    };
    use futures::StreamExt as _;

    use super::*;

    struct TestModel {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    struct BlockingThenDoneModel {
        calls: AtomicUsize,
        started: Arc<Notify>,
    }

    struct StreamingModel;

    impl ModelClient for TestModel {
        fn stream(&self, request: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            self.requests.lock().unwrap().push(request);
            Ok(Box::pin(futures::stream::iter([
                Ok(ModelEvent::Text("done".to_string())),
                Ok(ModelEvent::Stop(StopReason::EndTurn)),
            ])))
        }
    }

    impl ModelClient for BlockingThenDoneModel {
        fn stream(&self, _: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.started.notify_one();
                Ok(Box::pin(futures::stream::pending()))
            } else {
                Ok(Box::pin(futures::stream::iter([
                    Ok(ModelEvent::Text("replacement done".to_string())),
                    Ok(ModelEvent::Stop(StopReason::EndTurn)),
                ])))
            }
        }
    }

    impl ModelClient for StreamingModel {
        fn stream(&self, _: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            Ok(Box::pin(
                futures::stream::iter([Ok(ModelEvent::Text("live progress".to_string()))])
                    .chain(futures::stream::pending()),
            ))
        }
    }

    #[derive(Deserialize, JsonSchema)]
    struct NoArgs {}

    fn named_tool(name: &str) -> Arc<dyn Tool> {
        define_tool(name, "test", |_, _: NoArgs| async { Ok("ok") }).unwrap()
    }

    fn agent() -> Agent {
        Agent::new(ModelId::new("test"), Vec::new()).with_system_prompt("base")
    }

    fn options() -> SessionOptions {
        SessionOptions {
            working_dir: PathBuf::from("."),
            tool_timeout: Duration::from_secs(2),
        }
    }

    fn context(identity: SessionIdentity) -> ToolContext {
        ToolContext {
            session_id: identity.id,
            turn_id: TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: Some(std::time::Instant::now() + Duration::from_secs(2)),
            session: SessionToolContext {
                identity,
                messages: Vec::new(),
            },
        }
    }

    fn control() -> (
        AgentControl,
        tempfile::TempDir,
        Arc<StdMutex<Vec<ModelRequest>>>,
    ) {
        let directory = tempfile::TempDir::new().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = Runtime::new(
            Arc::new(TestModel {
                requests: Arc::clone(&requests),
            }),
            "test",
        )
        .with_session_store(Arc::new(ash_agent::JsonlSessionStore::new(
            directory.path(),
        )));
        (
            AgentControl::new(runtime, agent(), options()),
            directory,
            requests,
        )
    }

    fn tool_names(agent: &Agent) -> Vec<&str> {
        agent.tools().iter().map(|tool| tool.name()).collect()
    }

    #[test]
    fn explorer_uses_an_explicit_read_only_allowlist() {
        let tools = ["read", "glob", "grep", "webfetch", "skill", "write", "bash"]
            .into_iter()
            .map(named_tool)
            .collect();
        let projected =
            AgentProfile::for_name(ProfileName::Explorer).apply(agent().with_tools(tools));

        assert_eq!(
            tool_names(&projected),
            ["read", "glob", "grep", "webfetch", "skill"]
        );
    }

    #[test]
    fn installation_exposes_only_the_minimal_tools() {
        let (control, _directory, _) = control();
        let installed =
            with_collaboration_instructions(agent()).pushing_tools(control.tools().unwrap());

        assert_eq!(
            tool_names(&installed),
            [
                "agent",
                "message_agent",
                "list_agents",
                "remove_agent",
                "wait_agent"
            ]
        );
        assert_eq!(
            installed
                .tools()
                .iter()
                .map(|tool| tool.timeout())
                .collect::<Vec<_>>(),
            [
                ToolTimeout::Disabled,
                ToolTimeout::Disabled,
                ToolTimeout::Session,
                ToolTimeout::Session,
                ToolTimeout::Disabled,
            ]
        );
        assert_eq!(tool_names(&control.inner.definition), Vec::<&str>::new());
        let prompt = installed.system_prompt().unwrap();
        assert!(prompt.contains("Before non-trivial work"));
        assert!(!prompt.contains("wait=false"));
        assert!(installed.tools()[0]
            .description()
            .contains("Use this proactively"));
        assert!(installed.tools()[0]
            .description()
            .contains("With `wait=true`"));
    }

    #[test]
    fn schemas_keep_creation_and_messaging_separate() {
        let (control, _directory, _) = control();
        let tools = control.tools().unwrap();
        let create = tools[0].definition().parameters_schema;
        let message = tools[1].definition().parameters_schema;

        assert!(create["properties"].get("message").is_some());
        assert!(create["properties"].get("task").is_none());
        assert!(create["properties"].get("profile").is_some());
        assert!(create["properties"].get("interrupt").is_none());
        assert!(message["properties"].get("message").is_some());
        assert!(message["properties"].get("interrupt").is_some());
        assert!(message["properties"].get("profile").is_none());

        let args: AgentArgs = serde_json::from_value(serde_json::json!({
            "name": "research",
            "message": "inspect"
        }))
        .unwrap();
        assert!(args.wait);
    }

    #[tokio::test]
    async fn synchronous_creation_returns_and_consumes_the_result() {
        let (control, _directory, _) = control();
        let identity = SessionIdentity::root(SessionId::new());

        let output = control
            .create(
                context(identity.clone()),
                AgentArgs {
                    name: "be-resource-product".to_string(),
                    message: "inspect".to_string(),
                    profile: Some(ProfileName::Explorer),
                    wait: true,
                },
            )
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["name"], "be-resource-product");
        assert_eq!(output["final_message"], "done");

        let mut state = control.inner.state.lock().await;
        let tree = state.trees.get_mut(&identity.root_id).unwrap();
        assert!(tree.completions.is_empty());
        assert_eq!(tree.snapshots()[0].state, SubagentState::Idle);
    }

    #[tokio::test]
    async fn background_creation_is_collected_by_wait() {
        let (control, _directory, _) = control();
        let identity = SessionIdentity::root(SessionId::new());

        control
            .create(
                context(identity.clone()),
                AgentArgs {
                    name: "research".to_string(),
                    message: "inspect".to_string(),
                    profile: None,
                    wait: false,
                },
            )
            .await
            .unwrap();
        let output = control
            .wait(
                context(identity),
                WaitAgentArgs {
                    timeout_ms: Some(1_000),
                },
            )
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(output["completions"][0]["name"], "research");
        assert_eq!(output["completions"][0]["final_message"], "done");
        assert_eq!(output["timed_out"], false);
    }

    #[tokio::test]
    async fn root_projections_remain_isolated() {
        let (control, _directory, _) = control();
        let first = SessionIdentity::root(SessionId::new());
        let second = SessionIdentity::root(SessionId::new());

        for (identity, name) in [(&first, "first"), (&second, "second")] {
            control
                .create(
                    context(identity.clone()),
                    AgentArgs {
                        name: name.to_string(),
                        message: "inspect".to_string(),
                        profile: None,
                        wait: true,
                    },
                )
                .await
                .unwrap();
        }

        let snapshots = control.subscribe();
        let trees = snapshots.borrow();
        assert_eq!(trees.len(), 2);
        assert_eq!(
            trees
                .iter()
                .find(|tree| tree.root_id == first.root_id)
                .unwrap()
                .agents[0]
                .name,
            "first"
        );
        assert_eq!(
            trees
                .iter()
                .find(|tree| tree.root_id == second.root_id)
                .unwrap()
                .agents[0]
                .name,
            "second"
        );
    }

    #[tokio::test]
    async fn background_results_preserve_agent_submission_order() {
        let (control, _directory, _) = control();
        let identity = SessionIdentity::root(SessionId::new());
        control
            .create(
                context(identity.clone()),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "initial".to_string(),
                    profile: None,
                    wait: true,
                },
            )
            .await
            .unwrap();

        for message in ["first", "second"] {
            control
                .message(
                    context(identity.clone()),
                    MessageAgentArgs {
                        name: "worker".to_string(),
                        message: message.to_string(),
                        interrupt: false,
                        wait: false,
                    },
                )
                .await
                .unwrap();
        }

        let mut completed = Vec::new();
        for _ in 0..2 {
            if completed.len() == 2 {
                break;
            }
            let output = control
                .wait(
                    context(identity.clone()),
                    WaitAgentArgs {
                        timeout_ms: Some(1_000),
                    },
                )
                .await
                .unwrap();
            let output: serde_json::Value = serde_json::from_str(&output).unwrap();
            completed.extend(
                output["completions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|completion| completion["message"].as_str().unwrap().to_string()),
            );
        }

        assert_eq!(completed, ["first", "second"]);
    }

    #[tokio::test]
    async fn unacknowledged_synchronous_delivery_becomes_unread() {
        let (control, _directory, _) = control();
        let identity = SessionIdentity::root(SessionId::new());
        let root_id = identity.root_id;
        control
            .create(
                context(identity.clone()),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "initial".to_string(),
                    profile: None,
                    wait: true,
                },
            )
            .await
            .unwrap();
        let path = identity.path.join("worker").unwrap();
        let completion = AgentCompletion {
            name: "worker".to_string(),
            profile: ProfileName::Default,
            message: "inspect".to_string(),
            result: TurnResult::Completed(StopReason::EndTurn),
            final_message: Some("done".to_string()),
        };
        let (response, result) = oneshot::channel();
        let delivery = {
            let control = control.clone();
            tokio::spawn(async move {
                control
                    .deliver_completion(root_id, path, completion, response)
                    .await;
            })
        };

        let delivered = result.await.unwrap();
        drop(delivered);
        delivery.await.unwrap();

        let state = control.inner.state.lock().await;
        assert_eq!(state.trees[&root_id].completions.len(), 1);
    }

    #[tokio::test]
    async fn dropping_the_controller_stops_idle_completion_pumps() {
        let (control, _directory, _) = control();
        let identity = SessionIdentity::root(SessionId::new());
        control
            .create(
                context(identity),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "inspect".to_string(),
                    profile: None,
                    wait: true,
                },
            )
            .await
            .unwrap();
        let inner = Arc::downgrade(&control.inner);

        drop(control);

        tokio::time::timeout(Duration::from_secs(1), async {
            while inner.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completion pump retained the controller");
    }

    #[tokio::test]
    async fn follow_up_reuses_the_agents_session() {
        let (control, _directory, requests) = control();
        let identity = SessionIdentity::root(SessionId::new());
        control
            .create(
                context(identity.clone()),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "first".to_string(),
                    profile: None,
                    wait: true,
                },
            )
            .await
            .unwrap();
        control
            .message(
                context(identity),
                MessageAgentArgs {
                    name: "worker".to_string(),
                    message: "second".to_string(),
                    interrupt: false,
                    wait: true,
                },
            )
            .await
            .unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.len() > requests[0].messages.len());
    }

    #[tokio::test]
    async fn concurrent_duplicate_names_are_rejected_atomically() {
        let (control, _directory, _) = control();
        let identity = SessionIdentity::root(SessionId::new());
        let args = || AgentArgs {
            name: "same".to_string(),
            message: "work".to_string(),
            profile: None,
            wait: true,
        };

        let (first, second) = tokio::join!(
            control.create(context(identity.clone()), args()),
            control.create(context(identity), args()),
        );
        let outcomes = [first, second];

        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(outcomes
            .iter()
            .filter_map(|result| result.as_ref().err())
            .any(|error| error.to_string().contains("agent name already used: same")));
    }

    #[tokio::test]
    async fn list_reports_usage_and_remove_tombstones_the_name() {
        let (control, directory, _) = control();
        let identity = SessionIdentity::root(SessionId::new());
        control
            .create(
                context(identity.clone()),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "inspect usage".to_string(),
                    profile: None,
                    wait: true,
                },
            )
            .await
            .unwrap();

        let listed = control.list(context(identity.clone())).await.unwrap();
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        assert_eq!(listed["agents"][0]["name"], "worker");
        assert_eq!(listed["agents"][0]["state"], "idle");
        assert!(
            listed["agents"][0]["usage"]["input_tokens"]
                .as_u64()
                .unwrap()
                > 0
        );
        let child_id = {
            let state = control.inner.state.lock().await;
            let entry =
                &state.trees[&identity.root_id].agents[&identity.path.join("worker").unwrap()];
            assert!(entry.session.stats().active_turn.is_none());
            assert_eq!(entry.snapshot().usage, entry.session.stats().total_usage());
            entry.session.id()
        };

        control
            .remove(
                context(identity.clone()),
                RemoveAgentArgs {
                    name: "worker".to_string(),
                },
            )
            .await
            .unwrap();
        let listed = control.list(context(identity.clone())).await.unwrap();
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        assert!(listed["agents"].as_array().unwrap().is_empty());
        assert!(directory.path().join(format!("{child_id}.jsonl")).exists());

        let error = control
            .create(
                context(identity),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "reuse".to_string(),
                    profile: None,
                    wait: true,
                },
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("agent name already used: worker"));
    }

    #[tokio::test]
    async fn snapshots_include_active_turn_progress_before_it_settles() {
        let directory = tempfile::TempDir::new().unwrap();
        let runtime = Runtime::new(Arc::new(StreamingModel), "test").with_session_store(Arc::new(
            ash_agent::JsonlSessionStore::new(directory.path()),
        ));
        let control = AgentControl::new(runtime, agent(), options());
        let identity = SessionIdentity::root(SessionId::new());
        let mut snapshots = control.subscribe();

        control
            .create(
                context(identity.clone()),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "stream usage".to_string(),
                    profile: None,
                    wait: false,
                },
            )
            .await
            .unwrap();

        let live = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = snapshots
                    .borrow_and_update()
                    .iter()
                    .find(|tree| tree.root_id == identity.root_id)
                    .and_then(|tree| tree.agents.first())
                    .cloned();
                if snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.usage.output_tokens > 0)
                {
                    break snapshot.expect("live snapshot");
                }
                snapshots.changed().await.expect("snapshot sender");
            }
        })
        .await
        .expect("active usage update");

        assert_eq!(live.state, SubagentState::Running);
        assert!(live.usage.input_tokens > 0);
        assert!(live.usage.output_tokens > 0);
        let state = control.inner.state.lock().await;
        let entry = &state.trees[&identity.root_id].agents[&identity.path.join("worker").unwrap()];
        assert_eq!(
            entry.session.stats().settled_usage,
            ash_core::Usage::default()
        );
        assert!(entry.session.stats().active_turn.is_some());
        assert_eq!(entry.snapshot().usage, entry.session.stats().total_usage());
    }

    #[tokio::test]
    async fn removing_running_agent_drops_its_late_completion_and_snapshot() {
        let directory = tempfile::TempDir::new().unwrap();
        let started = Arc::new(Notify::new());
        let runtime = Runtime::new(
            Arc::new(BlockingThenDoneModel {
                calls: AtomicUsize::new(0),
                started: Arc::clone(&started),
            }),
            "test",
        )
        .with_session_store(Arc::new(ash_agent::JsonlSessionStore::new(
            directory.path(),
        )));
        let control = AgentControl::new(runtime, agent(), options());
        let identity = SessionIdentity::root(SessionId::new());
        let snapshots = control.subscribe();
        control
            .create(
                context(identity.clone()),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "block".to_string(),
                    profile: None,
                    wait: false,
                },
            )
            .await
            .unwrap();
        started.notified().await;

        control
            .remove(
                context(identity.clone()),
                RemoveAgentArgs {
                    name: "worker".to_string(),
                },
            )
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(snapshots
            .borrow()
            .iter()
            .find(|tree| tree.root_id == identity.root_id)
            .is_none_or(|tree| tree.agents.is_empty()));
        let output = control
            .wait(
                context(identity),
                WaitAgentArgs {
                    timeout_ms: Some(0),
                },
            )
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(output["agents"].as_array().unwrap().is_empty());
        assert!(output["completions"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn interrupt_replaces_work_without_losing_its_background_result() {
        let directory = tempfile::TempDir::new().unwrap();
        let started = Arc::new(Notify::new());
        let runtime = Runtime::new(
            Arc::new(BlockingThenDoneModel {
                calls: AtomicUsize::new(0),
                started: Arc::clone(&started),
            }),
            "test",
        )
        .with_session_store(Arc::new(ash_agent::JsonlSessionStore::new(
            directory.path(),
        )));
        let control = AgentControl::new(runtime, agent(), options());
        let identity = SessionIdentity::root(SessionId::new());

        control
            .create(
                context(identity.clone()),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "old direction".to_string(),
                    profile: None,
                    wait: false,
                },
            )
            .await
            .unwrap();
        started.notified().await;

        let replacement = control
            .message(
                context(identity.clone()),
                MessageAgentArgs {
                    name: "worker".to_string(),
                    message: "new direction".to_string(),
                    interrupt: true,
                    wait: true,
                },
            )
            .await
            .unwrap();
        let replacement: serde_json::Value = serde_json::from_str(&replacement).unwrap();
        assert_eq!(replacement["final_message"], "replacement done");

        let interrupted = control
            .wait(
                context(identity),
                WaitAgentArgs {
                    timeout_ms: Some(1_000),
                },
            )
            .await
            .unwrap();
        let interrupted: serde_json::Value = serde_json::from_str(&interrupted).unwrap();
        assert_eq!(
            interrupted["completions"][0]["result"]["Completed"],
            "Aborted"
        );
    }
}
