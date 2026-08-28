use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
};

use crate::snapshot::{SubagentSnapshot, SubagentState, SubagentTreeSnapshot};
use ash_agent::{Agent, Runtime, Session, Turn};
use ash_core::{
    define_tool, define_tool_with_timeout, Message, SessionId, Tool, ToolContext, ToolError,
    ToolTimeout, TurnId, TurnResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex, Notify};

const MAX_AGENT_NAME_CHARS: usize = 64;
const COLLABORATION_TOOL_NAMES: [&str; 5] = [
    "agent",
    "message_agent",
    "list_agents",
    "remove_agent",
    "wait_agent",
];

#[derive(Clone)]
pub struct AgentControl {
    inner: Arc<ControlInner>,
}

struct ControlInner {
    state: Mutex<ControlState>,
    updates: Notify,
    runtime: Runtime,
    definition: Agent,
    subagent_tx: watch::Sender<Vec<SubagentTreeSnapshot>>,
}

#[derive(Default)]
struct ControlState {
    groups: BTreeMap<SessionId, Arc<Mutex<AgentGroup>>>,
}

#[derive(Default)]
struct AgentGroup {
    agents: HashMap<String, AgentEntry>,
}

struct AgentEntry {
    session: Session,
    pending_turns: usize,
    unread: VecDeque<TurnCompletion>,
}

struct TurnCompletion {
    turn_id: TurnId,
    result: TurnResult,
    final_message: Option<String>,
}

#[derive(Debug, Serialize)]
struct AgentCompletion {
    name: String,
    turn_id: TurnId,
    result: TurnResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_message: Option<String>,
}

impl AgentEntry {
    fn snapshot(&self, name: &str) -> SubagentSnapshot {
        SubagentSnapshot {
            name: name.to_string(),
            state: if self.pending_turns == 0 {
                SubagentState::Idle
            } else {
                SubagentState::Running
            },
            usage: self.session.stats().total_usage(),
        }
    }
}

impl AgentGroup {
    fn snapshots(&self) -> Vec<SubagentSnapshot> {
        let mut agents = self
            .agents
            .iter()
            .map(|(name, entry)| entry.snapshot(name))
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

    fn has_pending(&self) -> bool {
        self.agents.values().any(|entry| entry.pending_turns > 0)
    }

    fn drain_unread(&mut self) -> Vec<AgentCompletion> {
        self.agents
            .iter_mut()
            .flat_map(|(name, entry)| {
                entry.unread.drain(..).map(|completion| AgentCompletion {
                    name: name.clone(),
                    turn_id: completion.turn_id,
                    result: completion.result,
                    final_message: completion.final_message,
                })
            })
            .collect()
    }
}

impl ControlInner {
    async fn group(&self, root_id: SessionId) -> Option<Arc<Mutex<AgentGroup>>> {
        self.state.lock().await.groups.get(&root_id).cloned()
    }

    async fn group_or_insert(&self, root_id: SessionId) -> Arc<Mutex<AgentGroup>> {
        Arc::clone(
            self.state
                .lock()
                .await
                .groups
                .entry(root_id)
                .or_insert_with(|| Arc::new(Mutex::new(AgentGroup::default()))),
        )
    }

    fn publish(&self, root_id: SessionId, agents: Vec<SubagentSnapshot>) {
        self.subagent_tx.send_modify(|trees| {
            match (
                trees.binary_search_by_key(&root_id, |tree| tree.root_id),
                agents.is_empty(),
            ) {
                (Ok(index), true) => {
                    trees.remove(index);
                }
                (Ok(index), false) => trees[index].agents = agents,
                (Err(_), true) => {}
                (Err(index), false) => {
                    trees.insert(index, SubagentTreeSnapshot { root_id, agents });
                }
            }
        });
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AgentArgs {
    /// Name for the new agent.
    name: String,
    /// Initial message for the new agent.
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MessageAgentArgs {
    /// Name of an existing agent.
    name: String,
    /// Follow-up message for the agent.
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListAgentsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveAgentArgs {
    /// Name of an existing idle agent to remove.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitAgentArgs {}

impl AgentControl {
    fn new(runtime: Runtime, definition: Agent) -> Self {
        let (subagent_tx, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(ControlInner {
                state: Mutex::new(ControlState::default()),
                updates: Notify::new(),
                runtime,
                definition,
                subagent_tx,
            }),
        }
    }

    /// Subscribe to root-scoped projections of all agents owned by this controller.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Vec<SubagentTreeSnapshot>> {
        self.inner.subagent_tx.subscribe()
    }

    /// Build the model-visible collaboration tools.
    ///
    /// # Errors
    ///
    /// Returns `ToolError` when a tool definition cannot be created.
    pub fn tools(&self) -> Result<Vec<Arc<dyn Tool>>, ToolError> {
        let create = self.clone();
        let agent = define_tool_with_timeout(
            "agent",
            "Create a named child agent, submit its initial message, and return the accepted turn id.",
            ToolTimeout::Disabled,
            move |context, args: AgentArgs| {
                let control = create.clone();
                async move { control.create(context, args).await }
            },
        )?;

        let message = self.clone();
        let message_agent = define_tool_with_timeout(
            "message_agent",
            "Submit a follow-up message to an existing child agent. Busy agents queue turns in submission order.",
            ToolTimeout::Disabled,
            move |context, args: MessageAgentArgs| {
                let control = message.clone();
                async move { control.message(context, args).await }
            },
        )?;

        let wait = self.clone();
        let wait_agent = define_tool_with_timeout(
            "wait_agent",
            "Return unread child turn results, waiting for the next completion when work is pending.",
            ToolTimeout::Disabled,
            move |context, _: WaitAgentArgs| {
                let control = wait.clone();
                async move { control.wait(context).await }
            },
        )?;

        let list = self.clone();
        let list_agents = define_tool(
            "list_agents",
            "List active child agents with their idle/running state and current usage.",
            move |context, _: ListAgentsArgs| {
                let control = list.clone();
                async move { control.list(context).await }
            },
        )?;

        let remove = self.clone();
        let remove_agent = define_tool(
            "remove_agent",
            "Remove an idle child agent after all unread results have been consumed.",
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
        let name = normalize_name(&args.name)?;
        validate_message(&args.message)?;
        let identity = context.identity;
        let root_id = identity.root_id;
        let group = self.inner.group_or_insert(root_id).await;

        let (turn, turn_id, session_id, stats) = {
            let mut group = group.lock().await;
            if group.agents.contains_key(&name) {
                return Err(ToolError::Execution(format!(
                    "agent name already used: {name}"
                )));
            }
            let session = self
                .inner
                .runtime
                .start_child(&self.inner.definition, identity);
            let session_id = session.id();
            let stats = session.subscribe_stats();
            let turn = submit(&session, args.message)?;
            let turn_id = turn.id();
            group.agents.insert(
                name.clone(),
                AgentEntry {
                    session,
                    pending_turns: 1,
                    unread: VecDeque::new(),
                },
            );
            self.inner.publish(root_id, group.snapshots());
            (turn, turn_id, session_id, stats)
        };

        self.forward_stats(root_id, session_id, stats);
        self.forward_completion(root_id, name.clone(), turn);
        json_output(&AcceptedTurn {
            name,
            turn_id,
            accepted: true,
        })
    }

    async fn message(
        &self,
        context: ToolContext,
        args: MessageAgentArgs,
    ) -> Result<String, ToolError> {
        let name = normalize_name(&args.name)?;
        validate_message(&args.message)?;
        let root_id = context.identity.root_id;
        let group = self
            .inner
            .group(root_id)
            .await
            .ok_or_else(|| ToolError::Execution(format!("agent not found: {name}")))?;

        let (turn, turn_id) = {
            let mut group = group.lock().await;
            let entry = group
                .agents
                .get_mut(&name)
                .ok_or_else(|| ToolError::Execution(format!("agent not found: {name}")))?;
            let turn = submit(&entry.session, args.message)?;
            let turn_id = turn.id();
            entry.pending_turns = entry.pending_turns.saturating_add(1);
            self.inner.publish(root_id, group.snapshots());
            (turn, turn_id)
        };

        self.forward_completion(root_id, name.clone(), turn);
        json_output(&AcceptedTurn {
            name,
            turn_id,
            accepted: true,
        })
    }

    async fn list(&self, context: ToolContext) -> Result<String, ToolError> {
        let agents = match self.inner.group(context.identity.root_id).await {
            Some(group) => group.lock().await.snapshots(),
            None => Vec::new(),
        };
        json_output(&AgentList { agents })
    }

    async fn remove(
        &self,
        context: ToolContext,
        args: RemoveAgentArgs,
    ) -> Result<String, ToolError> {
        let name = normalize_name(&args.name)?;
        let group = self
            .inner
            .group(context.identity.root_id)
            .await
            .ok_or_else(|| ToolError::Execution(format!("agent not found: {name}")))?;
        {
            let mut group = group.lock().await;
            let entry = group
                .agents
                .get(&name)
                .ok_or_else(|| ToolError::Execution(format!("agent not found: {name}")))?;
            if entry.pending_turns > 0 {
                return Err(ToolError::Execution(format!(
                    "agent is still running: {name}"
                )));
            }
            if !entry.unread.is_empty() {
                return Err(ToolError::Execution(format!(
                    "agent has unread results: {name}"
                )));
            }
            group.agents.remove(&name);
            self.inner
                .publish(context.identity.root_id, group.snapshots());
        }
        json_output(&RemovedAgent {
            name,
            removed: true,
        })
    }

    async fn wait(&self, context: ToolContext) -> Result<String, ToolError> {
        let root_id = context.identity.root_id;
        loop {
            let notified = self.inner.updates.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let output = match self.inner.group(root_id).await {
                Some(group) => {
                    let mut group = group.lock().await;
                    let completions = group.drain_unread();
                    (!completions.is_empty() || !group.has_pending())
                        .then(|| wait_output(completions))
                }
                None => Some(wait_output(Vec::new())),
            };
            if let Some(output) = output {
                return output;
            }

            tokio::select! {
                () = context.cancellation.cancelled() => return Err(ToolError::Cancelled),
                () = &mut notified => {}
            }
        }
    }

    fn forward_completion(&self, root_id: SessionId, name: String, turn: Turn) {
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let turn_id = turn.id();
            let (result, final_message) = match turn.wait().await {
                Ok(view) => (view.result, final_assistant_message(&view.messages)),
                Err(error) => (TurnResult::Failed(error.to_string()), None),
            };
            let Some(inner) = inner.upgrade() else {
                return;
            };
            let Some(group) = inner.group(root_id).await else {
                return;
            };
            {
                let mut group = group.lock().await;
                let Some(entry) = group.agents.get_mut(&name) else {
                    return;
                };
                entry.unread.push_back(TurnCompletion {
                    turn_id,
                    result,
                    final_message,
                });
                entry.pending_turns = entry.pending_turns.saturating_sub(1);
                inner.publish(root_id, group.snapshots());
            }
            inner.updates.notify_waiters();
        });
    }

    fn forward_stats(
        &self,
        root_id: SessionId,
        session_id: SessionId,
        mut stats: watch::Receiver<ash_core::SessionStats>,
    ) {
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut usage = stats.borrow_and_update().total_usage();
            while stats.changed().await.is_ok() {
                let next_usage = stats.borrow_and_update().total_usage();
                if next_usage == usage {
                    continue;
                }
                usage = next_usage;
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let Some(group) = inner.group(root_id).await else {
                    return;
                };
                let group = group.lock().await;
                let still_present = group
                    .agents
                    .values()
                    .any(|entry| entry.session.id() == session_id);
                if !still_present {
                    return;
                }
                inner.publish(root_id, group.snapshots());
            }
        });
    }
}

fn submit(session: &Session, message: String) -> Result<Turn, ToolError> {
    session
        .try_submit(message)
        .map_err(|error| ToolError::Execution(error.to_string()))
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
    let control = AgentControl::new(runtime, base.clone());
    let agent = base.pushing_tools(control.tools()?);
    Ok((agent, control))
}

fn normalize_name(name: &str) -> Result<String, ToolError> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_AGENT_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(ToolError::Execution(
            "name must be non-empty, contain no control characters, and be at most 64 characters"
                .to_string(),
        ));
    }
    Ok(name.to_string())
}

fn validate_message(message: &str) -> Result<(), ToolError> {
    if message.trim().is_empty() {
        Err(ToolError::Execution("message cannot be empty".to_string()))
    } else {
        Ok(())
    }
}

fn final_assistant_message(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(Message::visible_text)
}

fn wait_output(completions: Vec<AgentCompletion>) -> Result<String, ToolError> {
    json_output(&WaitResult { completions })
}

#[derive(Debug, Serialize)]
struct AcceptedTurn {
    name: String,
    turn_id: TurnId,
    accepted: bool,
}

#[derive(Debug, Serialize)]
struct AgentList {
    agents: Vec<SubagentSnapshot>,
}

#[derive(Debug, Serialize)]
struct RemovedAgent {
    name: String,
    removed: bool,
}

#[derive(Debug, Serialize)]
struct WaitResult {
    completions: Vec<AgentCompletion>,
}

fn json_output(value: &impl Serialize) -> Result<String, ToolError> {
    serde_json::to_string_pretty(value)
        .map_err(|error| ToolError::Execution(format!("failed to serialize agent result: {error}")))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex as StdMutex,
        },
        time::Duration,
    };

    use ash_core::{
        CancellationToken, ContentBlock, ModelClient, ModelEvent, ModelId, ModelRequest,
        ModelStream, ModelUsage, SessionIdentity, StopReason,
    };
    use futures::StreamExt as _;
    use tempfile::TempDir;

    use super::*;

    struct MockModel {
        responses: StdMutex<VecDeque<Vec<ModelEvent>>>,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl MockModel {
        fn new(responses: impl IntoIterator<Item = Vec<ModelEvent>>) -> Self {
            Self {
                responses: StdMutex::new(responses.into_iter().collect()),
                requests: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }

    impl ModelClient for MockModel {
        fn stream(&self, request: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            self.requests.lock().unwrap().push(request);
            let events = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    struct GateModel {
        release: Arc<tokio::sync::Notify>,
    }

    impl ModelClient for GateModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            let release = Arc::clone(&self.release);
            Ok(Box::pin(
                futures::stream::once(async move {
                    release.notified().await;
                    Ok(ModelEvent::Text("released".to_string()))
                })
                .chain(futures::stream::iter([Ok(ModelEvent::Stop(
                    StopReason::EndTurn,
                ))])),
            ))
        }
    }

    struct ToolUsageGateModel {
        calls: AtomicUsize,
        start: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl ModelClient for ToolUsageGateModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let start = Arc::clone(&self.start);
                return Ok(Box::pin(
                    futures::stream::once(async move {
                        start.notified().await;
                        Ok(ModelEvent::Usage(ModelUsage {
                            input_tokens: 120,
                            output_tokens: 25,
                        }))
                    })
                    .chain(futures::stream::iter([
                        Ok(ModelEvent::ToolCall {
                            id: ash_core::ToolCallId::from_provider("call"),
                            name: "missing".to_string(),
                            arguments: serde_json::json!({}),
                        }),
                        Ok(ModelEvent::Stop(StopReason::EndTurn)),
                    ])),
                ));
            }

            let release = Arc::clone(&self.release);
            Ok(Box::pin(futures::stream::once(async move {
                release.notified().await;
                Ok(ModelEvent::Stop(StopReason::EndTurn))
            })))
        }
    }

    fn definition() -> Agent {
        Agent::new(ModelId::new("test-model"), Vec::new())
    }

    fn control(model: Arc<dyn ModelClient>, directory: &TempDir) -> AgentControl {
        AgentControl::new(
            Runtime::new(model).with_session_directory(directory.path()),
            definition(),
        )
    }

    fn identity() -> SessionIdentity {
        SessionIdentity::root(SessionId::new())
    }

    fn context(identity: SessionIdentity) -> ToolContext {
        ToolContext {
            identity,
            cancellation: CancellationToken::new(),
            deadline: None,
        }
    }

    fn completion_names(output: &str) -> Vec<String> {
        serde_json::from_str::<serde_json::Value>(output).unwrap()["completions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn publishing_one_root_never_waits_for_another_root() {
        let directory = TempDir::new().unwrap();
        let control = control(Arc::new(MockModel::new([])), &directory);
        let blocked_root = SessionId::new();
        let other_root = SessionId::new();
        let blocked = control.inner.group_or_insert(blocked_root).await;
        let _blocked = blocked.lock().await;

        control.inner.publish(other_root, Vec::new());
    }

    #[test]
    fn names_are_trimmed_unicode_addresses_with_a_small_validation_rule() {
        assert_eq!(normalize_name("  研究  ").unwrap(), "研究");
        assert!(normalize_name("").is_err());
        assert!(normalize_name("bad\nname").is_err());
        assert!(normalize_name(&"x".repeat(65)).is_err());
        assert!(normalize_name("name with spaces").is_ok());
    }

    #[test]
    fn collaboration_installation_adds_only_tools_and_rejects_duplicates() {
        let directory = TempDir::new().unwrap();
        let runtime =
            Runtime::new(Arc::new(MockModel::new([]))).with_session_directory(directory.path());
        let base = definition().with_system_prompt("base");

        let (installed, control) = install_collaboration(base.clone(), runtime.clone()).unwrap();

        assert_eq!(installed.system_prompt(), Some("base"));
        assert_eq!(control.inner.definition.system_prompt(), Some("base"));
        assert_eq!(control.inner.definition.tools().len(), 0);
        assert_eq!(
            installed
                .tools()
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>(),
            COLLABORATION_TOOL_NAMES
        );
        assert!(install_collaboration(installed, runtime).is_err());
    }

    #[tokio::test]
    async fn create_returns_acceptance_and_wait_drains_the_entry_result() {
        let directory = TempDir::new().unwrap();
        let control = control(
            Arc::new(MockModel::new([vec![
                ModelEvent::Text("done".to_string()),
                ModelEvent::Stop(StopReason::EndTurn),
            ]])),
            &directory,
        );
        let identity = identity();

        let accepted = control
            .create(
                context(identity),
                AgentArgs {
                    name: " worker ".to_string(),
                    message: "inspect".to_string(),
                },
            )
            .await
            .unwrap();
        let accepted: serde_json::Value = serde_json::from_str(&accepted).unwrap();
        assert_eq!(accepted["name"], "worker");
        assert_eq!(accepted["accepted"], true);

        let output = control.wait(context(identity)).await.unwrap();
        assert_eq!(completion_names(&output), ["worker"]);
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["completions"][0]["final_message"], "done");

        let empty = control.wait(context(identity)).await.unwrap();
        assert!(completion_names(&empty).is_empty());
    }

    #[tokio::test]
    async fn child_snapshots_follow_live_protocol_usage_and_tool_counts() {
        let directory = TempDir::new().unwrap();
        let start = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let model = Arc::new(ToolUsageGateModel {
            calls: AtomicUsize::new(0),
            start: Arc::clone(&start),
            release: Arc::clone(&release),
        });
        let runtime = Runtime::new(model).with_session_directory(directory.path());
        let control = AgentControl::new(runtime, definition());
        let identity = identity();
        let mut snapshots = control.subscribe();

        control
            .create(
                context(identity),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "inspect".to_string(),
                },
            )
            .await
            .unwrap();
        start.notify_one();

        let live = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                snapshots.changed().await.unwrap();
                let current = snapshots.borrow_and_update();
                let Some(agent) = current
                    .iter()
                    .find(|tree| tree.root_id == identity.root_id)
                    .and_then(|tree| tree.agents.iter().find(|agent| agent.name == "worker"))
                else {
                    continue;
                };
                if agent.usage.tool_calls == 1 {
                    break agent.clone();
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(live.state, SubagentState::Running);
        assert_eq!(
            live.usage,
            ash_core::Usage {
                input_tokens: 120,
                output_tokens: 25,
                tool_calls: 1,
            }
        );

        release.notify_one();
        let output = control.wait(context(identity)).await.unwrap();
        assert_eq!(completion_names(&output), ["worker"]);
    }

    #[tokio::test]
    async fn wait_drains_unread_results_from_every_agent_entry() {
        let directory = TempDir::new().unwrap();
        let control = control(
            Arc::new(MockModel::new([
                vec![ModelEvent::Stop(StopReason::EndTurn)],
                vec![ModelEvent::Stop(StopReason::EndTurn)],
            ])),
            &directory,
        );
        let identity = identity();

        for name in ["alpha", "beta"] {
            control
                .create(
                    context(identity),
                    AgentArgs {
                        name: name.to_string(),
                        message: "task".to_string(),
                    },
                )
                .await
                .unwrap();
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let notified = control.inner.updates.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let group = control.inner.group(identity.root_id).await.unwrap();
                let unread = group
                    .lock()
                    .await
                    .agents
                    .values()
                    .map(|entry| entry.unread.len())
                    .sum::<usize>();
                if unread == 2 {
                    break;
                }
                notified.await;
            }
        })
        .await
        .unwrap();

        let mut names = completion_names(&control.wait(context(identity)).await.unwrap());
        names.sort();
        assert_eq!(names, ["alpha", "beta"]);
        assert!(completion_names(&control.wait(context(identity)).await.unwrap()).is_empty());
    }

    #[tokio::test]
    async fn follow_up_is_accepted_while_the_previous_turn_is_running() {
        let directory = TempDir::new().unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let control = control(
            Arc::new(GateModel {
                release: Arc::clone(&release),
            }),
            &directory,
        );
        let identity = identity();

        let first = control
            .create(
                context(identity),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "first".to_string(),
                },
            )
            .await
            .unwrap();
        let second = control
            .message(
                context(identity),
                MessageAgentArgs {
                    name: "worker".to_string(),
                    message: "second".to_string(),
                },
            )
            .await
            .unwrap();
        let first: serde_json::Value = serde_json::from_str(&first).unwrap();
        let second: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_ne!(first["turn_id"], second["turn_id"]);
        let group = control.inner.group(identity.root_id).await.unwrap();
        assert_eq!(group.lock().await.agents["worker"].pending_turns, 2);

        release.notify_one();
        let first_completion = control.wait(context(identity)).await.unwrap();
        assert_eq!(completion_names(&first_completion), ["worker"]);
        release.notify_one();
        let second_completion = control.wait(context(identity)).await.unwrap();
        assert_eq!(completion_names(&second_completion), ["worker"]);
    }

    #[tokio::test]
    async fn follow_up_reuses_the_child_session_history() {
        let directory = TempDir::new().unwrap();
        let model = Arc::new(MockModel::new([
            vec![
                ModelEvent::Text("first answer".to_string()),
                ModelEvent::Stop(StopReason::EndTurn),
            ],
            vec![
                ModelEvent::Text("second answer".to_string()),
                ModelEvent::Stop(StopReason::EndTurn),
            ],
        ]));
        let requests = Arc::clone(&model.requests);
        let control = control(model, &directory);
        let identity = identity();

        control
            .create(
                context(identity),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "first".to_string(),
                },
            )
            .await
            .unwrap();
        control.wait(context(identity)).await.unwrap();
        control
            .message(
                context(identity),
                MessageAgentArgs {
                    name: "worker".to_string(),
                    message: "second".to_string(),
                },
            )
            .await
            .unwrap();
        control.wait(context(identity)).await.unwrap();

        let requests = requests.lock().unwrap();
        let prompts = requests[1]
            .messages
            .iter()
            .filter_map(Message::user_turn_text)
            .collect::<Vec<_>>();
        assert_eq!(prompts, ["first", "second"]);
    }

    #[tokio::test]
    async fn cancelling_wait_does_not_cancel_or_consume_child_work() {
        let directory = TempDir::new().unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let control = control(
            Arc::new(GateModel {
                release: Arc::clone(&release),
            }),
            &directory,
        );
        let identity = identity();

        control
            .create(
                context(identity),
                AgentArgs {
                    name: "worker".to_string(),
                    message: "blocked".to_string(),
                },
            )
            .await
            .unwrap();

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = control
            .wait(ToolContext {
                identity,
                cancellation: cancelled,
                deadline: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Cancelled));
        {
            let group = control.inner.group(identity.root_id).await.unwrap();
            let group = group.lock().await;
            let entry = &group.agents["worker"];
            assert_eq!(entry.pending_turns, 1);
            assert!(entry.unread.is_empty());
        }

        release.notify_one();
        let output = control.wait(context(identity)).await.unwrap();
        assert_eq!(completion_names(&output), ["worker"]);
    }

    #[tokio::test]
    async fn remove_requires_consumed_results_then_allows_name_reuse() {
        let directory = TempDir::new().unwrap();
        let control = control(
            Arc::new(MockModel::new([
                vec![ModelEvent::Stop(StopReason::EndTurn)],
                vec![ModelEvent::Stop(StopReason::EndTurn)],
            ])),
            &directory,
        );
        let identity = identity();
        let create = || AgentArgs {
            name: "worker".to_string(),
            message: "task".to_string(),
        };

        control.create(context(identity), create()).await.unwrap();
        assert!(control
            .remove(
                context(identity),
                RemoveAgentArgs {
                    name: "worker".to_string(),
                },
            )
            .await
            .is_err());

        control.wait(context(identity)).await.unwrap();
        control
            .remove(
                context(identity),
                RemoveAgentArgs {
                    name: "worker".to_string(),
                },
            )
            .await
            .unwrap();
        control.create(context(identity), create()).await.unwrap();
        let group = control.inner.group(identity.root_id).await.unwrap();
        assert_eq!(group.lock().await.agents.len(), 1);
    }

    #[tokio::test]
    async fn roots_have_independent_name_scopes_and_wait_mailboxes() {
        let directory = TempDir::new().unwrap();
        let control = control(
            Arc::new(MockModel::new([
                vec![ModelEvent::Stop(StopReason::EndTurn)],
                vec![ModelEvent::Stop(StopReason::EndTurn)],
            ])),
            &directory,
        );
        let first = identity();
        let second = identity();

        for root in [&first, &second] {
            control
                .create(
                    context(*root),
                    AgentArgs {
                        name: "worker".to_string(),
                        message: "task".to_string(),
                    },
                )
                .await
                .unwrap();
        }

        assert_eq!(
            completion_names(&control.wait(context(first)).await.unwrap()),
            ["worker"]
        );
        let first_list: serde_json::Value =
            serde_json::from_str(&control.list(context(first)).await.unwrap()).unwrap();
        let second_list: serde_json::Value =
            serde_json::from_str(&control.list(context(second)).await.unwrap()).unwrap();
        assert_eq!(first_list["agents"].as_array().unwrap().len(), 1);
        assert_eq!(second_list["agents"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn final_message_uses_the_last_nonempty_assistant_text() {
        let messages = vec![
            Message::assistant_text("first"),
            Message::assistant(vec![
                ContentBlock::Thought {
                    text: "thinking".to_string(),
                    elapsed_seconds: 1,
                },
                ContentBlock::Text("final".to_string()),
            ]),
        ];
        assert_eq!(final_assistant_message(&messages).as_deref(), Some("final"));
    }
}
