use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use crate::snapshot::{SubagentSnapshot, SubagentState};
use ash_agent::{Agent, Input, InputSource, Runtime, Session, SessionOptions, Turn};
use ash_core::{
    define_tool, is_valid_segment, AgentPath, CancellationToken, ContentBlock, Message,
    MessageContent, SessionId, Tool, ToolContext, ToolError, TurnId, TurnResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch, Mutex, Notify};

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;
const COLLABORATION_TOOL_NAMES: [&str; 3] = ["agent", "message_agent", "wait_agent"];
/// Explorer projection is deliberately an allowlist: unknown custom and MCP
/// tools are not assumed to be read-only.
const EXPLORER_TOOL_NAMES: [&str; 5] = ["read", "glob", "grep", "webfetch", "skill"];

const COLLABORATION_INSTRUCTIONS: &str = "You are the main agent. Use `agent` for concrete, independent work and `message_agent` for follow-up work with an existing agent. Set `wait=false` when tasks can run independently, continue useful work while they run, then use `wait_agent` when their results are needed. Child agents share the workspace, cannot delegate further, and their results must be reviewed before use.";

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
                description: "General-purpose agent that inherits the main agent's tools.",
                prompt: "Handle the assigned task directly and return a concise, evidence-backed result.",
                tools: ToolPolicy::Inherit,
            },
            ProfileName::Explorer => Self {
                name,
                description: "Read-only agent for focused codebase investigation.",
                prompt: "Answer through read-only inspection. Return concrete findings with relevant paths and symbols.",
                tools: ToolPolicy::Allow(&EXPLORER_TOOL_NAMES),
            },
            ProfileName::Worker => Self {
                name,
                description: "Agent for bounded implementation, testing, and refactoring.",
                prompt: "Execute the assigned implementation task, preserve unrelated changes, verify the result, and report changed files.",
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
    runtime: Runtime,
    definition: Agent,
    options: SessionOptions,
    subagent_tx: watch::Sender<Vec<SubagentSnapshot>>,
}

#[derive(Default)]
struct ControlState {
    trees: HashMap<SessionId, AgentTree>,
}

#[derive(Default)]
struct AgentTree {
    agents: HashMap<AgentPath, AgentEntry>,
    completions: VecDeque<AgentCompletion>,
}

struct AgentEntry {
    name: String,
    profile: ProfileName,
    session: Session,
    turns: VecDeque<TrackedTurn>,
    last_message: String,
}

struct TrackedTurn {
    id: TurnId,
    cancellation: CancellationToken,
    message: String,
    waiter: Option<oneshot::Sender<AgentCompletion>>,
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
            last_message: self.last_message.clone(),
        }
    }

    fn cancel_pending(&self) {
        self.turns
            .iter()
            .for_each(|turn| turn.cancellation.cancel());
    }

    fn finish(&mut self, turn_id: TurnId) -> Option<TrackedTurn> {
        let index = self.turns.iter().position(|turn| turn.id == turn_id)?;
        self.turns.remove(index)
    }
}

impl AgentTree {
    fn snapshots(&self) -> Vec<SubagentSnapshot> {
        let mut agents = self
            .agents
            .values()
            .map(AgentEntry::snapshot)
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.name.cmp(&right.name));
        agents
    }
}

impl ControlState {
    fn snapshots(&self) -> Vec<SubagentSnapshot> {
        let mut agents = self
            .trees
            .values()
            .flat_map(AgentTree::snapshots)
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.name.cmp(&right.name));
        agents
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AgentArgs {
    /// Stable name using lowercase letters, digits, and underscores.
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
                runtime,
                definition,
                options,
                subagent_tx,
            }),
        }
    }

    /// Subscribe to presentation snapshots of all live child agents.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Vec<SubagentSnapshot>> {
        self.inner.subagent_tx.subscribe()
    }

    async fn publish(&self) {
        let snapshots = self.inner.state.lock().await.snapshots();
        let _ = self.inner.subagent_tx.send(snapshots);
    }

    /// Build the model-visible collaboration tools.
    ///
    /// # Errors
    ///
    /// Returns `ToolError` when a tool definition cannot be created.
    pub fn tools(&self) -> Result<Vec<Arc<dyn Tool>>, ToolError> {
        let create = self.clone();
        let description = format!(
            "Create a named leaf agent and send its initial message. Names remain available for follow-up messages. Use `wait=false` for independent work that should run in the background.\n\nAvailable profiles:\n{}",
            AgentProfile::descriptions()
        );
        let agent = define_tool("agent", &description, move |context, args: AgentArgs| {
            let control = create.clone();
            async move { control.create(context, args).await }
        })?;

        let message = self.clone();
        let message_agent = define_tool(
            "message_agent",
            "Send a follow-up message to an existing named agent. Busy agents queue turns in submission order. Set `interrupt=true` to cancel unfinished work first, or `wait=false` to return immediately.",
            move |context, args: MessageAgentArgs| {
                let control = message.clone();
                async move { control.message(context, args).await }
            },
        )?;

        let wait = self.clone();
        let wait_agent = define_tool(
            "wait_agent",
            "Wait for unread results from background agent turns. Returns every result completed since the previous wait, plus the current idle/running snapshot.",
            move |context, args: WaitAgentArgs| {
                let control = wait.clone();
                async move { control.wait(context, args).await }
            },
        )?;

        Ok(vec![agent, message_agent, wait_agent])
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
        let (turn, result) = {
            // Holding this lock across child construction is intentional: it is
            // the creation reservation, and keeps duplicate names impossible
            // without a second reservation state machine.
            let mut state = self.inner.state.lock().await;
            let tree = state.trees.entry(identity.root_id).or_default();
            if tree.agents.contains_key(&path) {
                return Err(ToolError::Execution(format!(
                    "agent already exists: {}",
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
            let (turn, tracked, result) = submit(&session, args.message.clone(), args.wait).await?;
            tree.agents.insert(
                path.clone(),
                AgentEntry {
                    name: args.name.clone(),
                    profile,
                    session,
                    turns: VecDeque::from([tracked]),
                    last_message: args.message,
                },
            );
            (turn, result)
        };

        self.watch_turn(identity.root_id, path, turn);
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
        let (turn, profile, result) = {
            let mut state = self.inner.state.lock().await;
            let entry = state
                .trees
                .get_mut(&identity.root_id)
                .and_then(|tree| tree.agents.get_mut(&path))
                .ok_or_else(|| ToolError::Execution(format!("agent not found: {}", args.name)))?;
            if args.interrupt {
                entry.cancel_pending();
            }
            let (turn, tracked, result) =
                submit(&entry.session, args.message.clone(), args.wait).await?;
            entry.turns.push_back(tracked);
            entry.last_message = args.message;
            (turn, entry.profile, result)
        };

        self.watch_turn(identity.root_id, path, turn);
        self.publish().await;
        self.submission_result(&args.name, profile, result).await
    }

    async fn submission_result(
        &self,
        name: &str,
        profile: ProfileName,
        result: Option<oneshot::Receiver<AgentCompletion>>,
    ) -> Result<String, ToolError> {
        match result {
            Some(result) => result
                .await
                .map_err(|_| ToolError::Execution(format!("agent stopped before replying: {name}")))
                .and_then(|completion| json_output(&completion)),
            None => json_output(&serde_json::json!({
                "name": name,
                "profile": profile,
                "accepted": true,
            })),
        }
    }

    async fn wait(&self, context: ToolContext, args: WaitAgentArgs) -> Result<String, ToolError> {
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
                        tree.completions.drain(..).collect(),
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
                let completions = tree.completions.drain(..).collect::<Vec<_>>();
                let timed_out = completions.is_empty();
                return wait_output(completions, tree.snapshots(), timed_out);
            }
        }
    }

    fn watch_turn(&self, root_id: SessionId, path: AgentPath, turn: Turn) {
        let control = self.clone();
        tokio::spawn(async move {
            control.observe_turn(root_id, path, turn).await;
        });
    }

    async fn observe_turn(&self, root_id: SessionId, path: AgentPath, turn: Turn) {
        let turn_id = turn.id();
        let (result, final_message) = match turn.wait().await {
            Ok(view) => (view.result, final_assistant_message(&view.messages)),
            Err(error) => (TurnResult::Failed(error.to_string()), None),
        };

        let queued = {
            let mut state = self.inner.state.lock().await;
            let Some(tree) = state.trees.get_mut(&root_id) else {
                return;
            };
            let Some(entry) = tree.agents.get_mut(&path) else {
                return;
            };
            let Some(tracked) = entry.finish(turn_id) else {
                return;
            };
            let completion = AgentCompletion {
                name: entry.name.clone(),
                profile: entry.profile,
                message: tracked.message,
                result,
                final_message,
            };
            match tracked.waiter {
                Some(waiter) => match waiter.send(completion) {
                    Ok(()) => false,
                    Err(completion) => {
                        tree.completions.push_back(completion);
                        true
                    }
                },
                None => {
                    tree.completions.push_back(completion);
                    true
                }
            }
        };

        if queued {
            self.inner.updates.notify_waiters();
        }
        self.publish().await;
    }
}

async fn submit(
    session: &Session,
    message: String,
    wait: bool,
) -> Result<
    (
        Turn,
        TrackedTurn,
        Option<oneshot::Receiver<AgentCompletion>>,
    ),
    ToolError,
> {
    let turn = session
        .submit(Input::from_text(InputSource::Agent, message.clone()))
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let (waiter, result) = if wait {
        let (sender, receiver) = oneshot::channel();
        (Some(sender), Some(receiver))
    } else {
        (None, None)
    };
    let tracked = TrackedTurn {
        id: turn.id(),
        cancellation: turn.cancellation_token(),
        message,
        waiter,
    };
    Ok((turn, tracked, result))
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
            "name must contain only lowercase letters, digits, and underscores and be at most 64 characters"
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

    use super::*;

    struct TestModel {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    struct BlockingThenDoneModel {
        calls: AtomicUsize,
        started: Arc<Notify>,
    }

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
            deadline: std::time::Instant::now() + Duration::from_secs(2),
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
            ["agent", "message_agent", "wait_agent"]
        );
        assert_eq!(tool_names(&control.inner.definition), Vec::<&str>::new());
        assert!(installed
            .system_prompt()
            .unwrap()
            .contains("Set `wait=false`"));
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
                    name: "research".to_string(),
                    message: "inspect".to_string(),
                    profile: Some(ProfileName::Explorer),
                    wait: true,
                },
            )
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["name"], "research");
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
    async fn duplicate_names_are_rejected() {
        let (control, _directory, _) = control();
        let identity = SessionIdentity::root(SessionId::new());
        let args = || AgentArgs {
            name: "same".to_string(),
            message: "work".to_string(),
            profile: None,
            wait: true,
        };

        control
            .create(context(identity.clone()), args())
            .await
            .unwrap();
        let error = control.create(context(identity), args()).await.unwrap_err();

        assert!(error.to_string().contains("agent already exists: same"));
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
