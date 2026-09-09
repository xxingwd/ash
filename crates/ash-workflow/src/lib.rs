use std::{collections::BTreeMap, sync::Arc};

use ash_agent::{Agent, Runtime, TurnHandle};
use ash_core::{
    define_tool_with_timeout, AshError, CancellationToken, SessionId, SessionIdentity, Tool,
    ToolContext, ToolError, ToolTimeout, Turn, TurnResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{watch, Mutex};

mod script;
mod store;

pub use script::ScriptError;
pub use store::StoreError;

pub const WORKFLOW_TOOL_NAME: &str = "workflow";
pub const WORKFLOW_INSTRUCTIONS: &str = include_str!("../WORKFLOW.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub id: SessionId,
    pub parent_id: Option<SessionId>,
    pub label: String,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    pub id: SessionId,
    pub root_id: SessionId,
    pub revision: u64,
    pub status: WorkflowStatus,
    pub agents: Vec<AgentSnapshot>,
}

#[derive(Debug, Clone)]
pub enum AgentResult {
    Completed(Arc<Turn>),
    Failed(String),
    Cancelled,
}

impl AgentResult {
    const fn status(&self) -> AgentStatus {
        match self {
            Self::Completed(_) => AgentStatus::Completed,
            Self::Failed(_) => AgentStatus::Failed,
            Self::Cancelled => AgentStatus::Cancelled,
        }
    }

    #[must_use]
    pub fn turn(&self) -> Option<&Arc<Turn>> {
        match self {
            Self::Completed(turn) => Some(turn),
            Self::Failed(_) | Self::Cancelled => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("workflow has already started")]
    AlreadyStarted,
    #[error("workflow has finished")]
    Finished,
    #[error("workflow was cancelled")]
    Cancelled,
    #[error("workflow label cannot be empty")]
    EmptyLabel,
    #[error("workflow prompt cannot be empty")]
    EmptyPrompt,
    #[error("parent agent does not belong to this workflow: {0}")]
    UnknownParent(SessionId),
    #[error(transparent)]
    Storage(#[from] StoreError),
    #[error(transparent)]
    Agent(#[from] AshError),
}

struct AgentRun {
    result: watch::Sender<Option<AgentResult>>,
    cancellation: CancellationToken,
}

impl AgentRun {
    fn new(cancellation: CancellationToken) -> Self {
        let (result, _) = watch::channel(None);
        Self {
            result,
            cancellation,
        }
    }

    fn finish(&self, result: AgentResult) {
        self.result.send_replace(Some(result));
    }

    async fn wait(&self) -> AgentResult {
        let mut result = self.result.subscribe();
        loop {
            if let Some(result) = result.borrow_and_update().clone() {
                return result;
            }
            if result.changed().await.is_err() {
                return AgentResult::Failed("agent completion channel closed".into());
            }
        }
    }
}

#[derive(Clone)]
pub struct AgentHandle {
    id: SessionId,
    run: Arc<AgentRun>,
}

impl AgentHandle {
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    pub async fn wait(&self) -> AgentResult {
        self.run.wait().await
    }

    pub fn cancel(&self) {
        self.run.cancellation.cancel();
    }
}

struct AgentEntry {
    identity: SessionIdentity,
    snapshot: AgentSnapshot,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScriptState {
    Ready,
    Running,
    Finished,
}

struct WorkflowState {
    script: ScriptState,
    recorded: bool,
    status: WorkflowStatus,
    id: SessionId,
    root_id: SessionId,
    revision: u64,
    agents: BTreeMap<SessionId, AgentEntry>,
}

#[derive(Clone)]
pub struct Workflow {
    runtime: Runtime,
    agent: Agent,
    state: Arc<Mutex<WorkflowState>>,
    updates: watch::Sender<WorkflowSnapshot>,
    cancellation: CancellationToken,
    store: Arc<store::Store>,
}

struct RunGuard(Option<Workflow>);

impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Some(workflow) = &self.0 {
            workflow.cancel();
        }
    }
}

impl Workflow {
    #[must_use]
    pub fn new(runtime: Runtime, agent: Agent, label: impl Into<String>) -> Self {
        Self::from_parent(
            runtime,
            agent,
            SessionIdentity::root(SessionId::new()),
            label,
        )
    }

    #[must_use]
    pub fn from_parent(
        runtime: Runtime,
        agent: Agent,
        parent: SessionIdentity,
        label: impl Into<String>,
    ) -> Self {
        let id = SessionId::new();
        let root_id = parent.id();
        let root_snapshot = AgentSnapshot {
            id: root_id,
            parent_id: parent.parent_id(),
            label: label.into(),
            status: AgentStatus::Running,
        };
        let snapshot = WorkflowSnapshot {
            id,
            root_id,
            revision: 0,
            status: WorkflowStatus::Running,
            agents: vec![root_snapshot.clone()],
        };
        let (updates, _) = watch::channel(snapshot);
        let mut agents = BTreeMap::new();
        agents.insert(
            root_id,
            AgentEntry {
                identity: parent,
                snapshot: root_snapshot,
            },
        );
        let session_directory = runtime.session_directory();
        Self {
            runtime,
            agent,
            state: Arc::new(Mutex::new(WorkflowState {
                script: ScriptState::Ready,
                recorded: false,
                status: WorkflowStatus::Running,
                id,
                root_id,
                revision: 0,
                agents,
            })),
            updates,
            cancellation: CancellationToken::new(),
            store: Arc::new(store::Store::new(session_directory, root_id, id)),
        }
    }

    #[must_use]
    pub fn root_id(&self) -> SessionId {
        self.updates.borrow().root_id
    }

    #[must_use]
    pub fn snapshot(&self) -> WorkflowSnapshot {
        self.updates.borrow().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<WorkflowSnapshot> {
        self.updates.subscribe()
    }

    pub async fn run(&self, script: impl Into<String>) -> Result<serde_json::Value, ScriptError> {
        let script = script.into();
        self.record_started(Some(script.clone())).await?;
        {
            let mut state = self.state.lock().await;
            if state.script != ScriptState::Ready {
                return Err(WorkflowError::AlreadyStarted.into());
            }
            if self.cancellation.is_cancelled() {
                return Err(ScriptError::Cancelled);
            }
            state.script = ScriptState::Running;
        }
        let mut guard = RunGuard(Some(self.clone()));
        let result = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(ScriptError::Cancelled),
            result = script::run(self.clone(), script) => result,
        };
        let status = match &result {
            Ok(_) => AgentStatus::Completed,
            Err(ScriptError::Cancelled) => AgentStatus::Cancelled,
            Err(_) => AgentStatus::Failed,
        };
        let persisted_result = result.as_ref().ok().cloned();
        let persisted_error = result.as_ref().err().map(ToString::to_string);
        self.finish_root(status, persisted_result, persisted_error)
            .await;
        if result.is_ok() {
            guard.0 = None;
        }
        result
    }

    pub async fn spawn_agent(
        &self,
        parent_id: Option<SessionId>,
        label: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Result<AgentHandle, WorkflowError> {
        self.record_started(None).await?;
        let label = label.into();
        if label.trim().is_empty() {
            return Err(WorkflowError::EmptyLabel);
        }
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(WorkflowError::EmptyPrompt);
        }

        let parent_id = parent_id.unwrap_or_else(|| self.root_id());
        let (child, turn, run) = {
            let mut state = self.state.lock().await;
            if self.cancellation.is_cancelled() {
                return Err(WorkflowError::Cancelled);
            }
            if state.script == ScriptState::Finished {
                return Err(WorkflowError::Finished);
            }
            let parent = state
                .agents
                .get(&parent_id)
                .ok_or(WorkflowError::UnknownParent(parent_id))?;
            let parent_identity = parent.identity;
            let child = self.runtime.start_child(&self.agent, parent.identity);
            let child_id = child.id();
            let turn = child.try_submit(prompt.clone())?;
            if let Err(error) = self
                .store
                .append(store::Event::AgentSpawned {
                    id: child_id,
                    parent_id: parent_identity.id(),
                    label: label.clone(),
                    prompt,
                })
                .await
            {
                turn.cancellation_token().cancel();
                return Err(error.into());
            }
            let run = Arc::new(AgentRun::new(turn.cancellation_token()));
            let snapshot = AgentSnapshot {
                id: child_id,
                parent_id: Some(parent_identity.id()),
                label,
                status: AgentStatus::Running,
            };
            state.agents.insert(
                child_id,
                AgentEntry {
                    identity: child.identity(),
                    snapshot,
                },
            );
            self.publish_locked(&mut state);
            (child, turn, run)
        };

        let child_id = child.id();
        let workflow = self.clone();
        let watcher_run = Arc::clone(&run);
        tokio::spawn(async move {
            let _session = child;
            let completion = finish_turn(turn);
            tokio::pin!(completion);
            let result = tokio::select! {
                biased;
                () = workflow.cancellation.cancelled() => {
                    watcher_run.cancellation.cancel();
                    completion.await
                }
                result = &mut completion => result,
            };
            let error = match &result {
                AgentResult::Completed(_) => None,
                AgentResult::Failed(error) => Some(error.clone()),
                AgentResult::Cancelled => Some("agent cancelled".to_string()),
            };
            if let Err(error) = workflow
                .store
                .append(store::Event::AgentFinished {
                    id: child_id,
                    status: result.status(),
                    error,
                })
                .await
            {
                tracing::error!(%error, %child_id, "failed to persist workflow agent result");
            }
            workflow.finish_agent(child_id, result.status()).await;
            watcher_run.finish(result);
        });

        Ok(AgentHandle { id: child_id, run })
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
        let workflow = self.clone();
        tokio::spawn(async move {
            if let Err(error) = workflow.record_started(None).await {
                tracing::error!(%error, "failed to persist cancelled workflow start");
            }
            workflow
                .finish_root(AgentStatus::Cancelled, None, None)
                .await;
        });
    }

    async fn finish_agent(&self, id: SessionId, status: AgentStatus) {
        let mut state = self.state.lock().await;
        if let Some(entry) = state.agents.get_mut(&id) {
            if entry.snapshot.status == AgentStatus::Running {
                entry.snapshot.status = status;
                self.publish_locked(&mut state);
            }
        }
    }

    async fn finish_root(
        &self,
        status: AgentStatus,
        result: Option<serde_json::Value>,
        error: Option<String>,
    ) {
        let mut state = self.state.lock().await;
        if state.script == ScriptState::Finished {
            return;
        }
        state.script = ScriptState::Finished;
        state.status = workflow_status(status);
        let root_id = state.root_id;
        if let Some(root) = state.agents.get_mut(&root_id) {
            root.snapshot.status = status;
        }
        if let Err(error) = self
            .store
            .append(store::Event::Finished {
                status: workflow_status(status),
                result,
                error,
            })
            .await
        {
            tracing::error!(%error, "failed to persist workflow result");
        }
        self.publish_locked(&mut state);
    }

    async fn record_started(&self, script: Option<String>) -> Result<(), WorkflowError> {
        let mut state = self.state.lock().await;
        if state.recorded {
            return Ok(());
        }
        let root = state
            .agents
            .get(&state.root_id)
            .ok_or(WorkflowError::UnknownParent(state.root_id))?;
        self.store
            .append(store::Event::Started {
                label: root.snapshot.label.clone(),
                root: root.identity,
                script,
            })
            .await?;
        state.recorded = true;
        Ok(())
    }

    fn publish_locked(&self, state: &mut WorkflowState) {
        state.revision = state.revision.saturating_add(1);
        self.updates.send_replace(snapshot(state));
    }

    #[must_use]
    pub fn id(&self) -> SessionId {
        self.snapshot().id
    }

    pub async fn load_snapshot(
        runtime: &Runtime,
        root_id: SessionId,
        id: SessionId,
    ) -> Result<WorkflowSnapshot, StoreError> {
        let replay = store::Store::new(runtime.session_directory(), root_id, id)
            .replay()
            .await?;
        replay_snapshot(replay)
    }

    pub async fn list_snapshots(
        runtime: &Runtime,
        root_id: SessionId,
    ) -> Result<Vec<WorkflowSnapshot>, StoreError> {
        let mut snapshots = Vec::new();
        for id in store::Store::ids(runtime.session_directory(), root_id).await? {
            match Self::load_snapshot(runtime, root_id, id).await {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(error) => tracing::warn!(%error, %id, "skipping unreadable workflow log"),
            }
        }
        Ok(snapshots)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkflowArgs {
    /// JavaScript workflow body. It may call `agent(prompt, parent_id)`.
    script: String,
}

pub fn tool(runtime: Runtime, agent: Agent) -> Result<Arc<dyn Tool>, ToolError> {
    define_tool_with_timeout(
        WORKFLOW_TOOL_NAME,
        "Run bounded fan-out/fan-in work as JavaScript. Use this for parallel or runtime-discovered work; return a concise final result from the script.",
        ToolTimeout::Disabled,
        move |context: ToolContext, args: WorkflowArgs| {
            let runtime = runtime.clone();
            let agent = agent.clone();
            async move {
                let mut workflow = Workflow::from_parent(
                    runtime,
                    agent,
                    context.identity,
                    "workflow",
                );
                workflow.cancellation = context.cancellation.child_token();
                let result = context.run(workflow.run(args.script)).await;
                match result {
                    Ok(Ok(value)) => serde_json::to_string(&serde_json::json!({
                        "result": value,
                        "workflow": workflow.snapshot(),
                    }))
                    .map_err(|error| ToolError::Execution(error.to_string())),
                    Ok(Err(error)) => {
                        workflow.cancel();
                        Err(ToolError::Execution(error.to_string()))
                    }
                    Err(error) => {
                        workflow.cancel();
                        Err(error)
                    }
                }
            }
        },
    )
}

fn snapshot(state: &WorkflowState) -> WorkflowSnapshot {
    WorkflowSnapshot {
        id: state.id,
        root_id: state.root_id,
        revision: state.revision,
        status: state.status,
        agents: state
            .agents
            .values()
            .map(|entry| entry.snapshot.clone())
            .collect(),
    }
}

const fn workflow_status(status: AgentStatus) -> WorkflowStatus {
    match status {
        AgentStatus::Running => WorkflowStatus::Running,
        AgentStatus::Completed => WorkflowStatus::Completed,
        AgentStatus::Failed => WorkflowStatus::Failed,
        AgentStatus::Cancelled => WorkflowStatus::Cancelled,
        AgentStatus::Interrupted => WorkflowStatus::Interrupted,
    }
}

fn replay_snapshot(replay: store::Replay) -> Result<WorkflowSnapshot, StoreError> {
    let revision = u64::try_from(
        replay
            .events
            .iter()
            .filter(|(_, event)| !matches!(event, store::Event::Started { .. }))
            .count(),
    )
    .unwrap_or(u64::MAX);
    let mut agents = BTreeMap::new();
    let mut status = WorkflowStatus::Running;
    for (_, event) in replay.events {
        match event {
            store::Event::Started { label, root, .. } => {
                agents.insert(
                    root.id(),
                    AgentSnapshot {
                        id: root.id(),
                        parent_id: root.parent_id(),
                        label,
                        status: AgentStatus::Running,
                    },
                );
            }
            store::Event::AgentSpawned {
                id,
                parent_id,
                label,
                ..
            } => {
                agents.insert(
                    id,
                    AgentSnapshot {
                        id,
                        parent_id: Some(parent_id),
                        label,
                        status: AgentStatus::Running,
                    },
                );
            }
            store::Event::AgentFinished {
                id,
                status: agent_status,
                ..
            } => {
                let agent = agents.get_mut(&id).ok_or_else(|| {
                    StoreError::Corrupt(format!("agent finished before spawn: {id}"))
                })?;
                agent.status = agent_status;
            }
            store::Event::Finished {
                status: workflow_status,
                ..
            } => status = workflow_status,
        }
    }
    let root = agents.get_mut(&replay.root_id).ok_or_else(|| {
        StoreError::Corrupt(format!("workflow root is missing: {}", replay.root_id))
    })?;
    if status == WorkflowStatus::Running {
        status = WorkflowStatus::Interrupted;
    }
    root.status = match status {
        WorkflowStatus::Running => AgentStatus::Running,
        WorkflowStatus::Completed => AgentStatus::Completed,
        WorkflowStatus::Failed => AgentStatus::Failed,
        WorkflowStatus::Cancelled => AgentStatus::Cancelled,
        WorkflowStatus::Interrupted => AgentStatus::Interrupted,
    };
    agents
        .values_mut()
        .filter(|agent| agent.status == AgentStatus::Running)
        .for_each(|agent| agent.status = AgentStatus::Interrupted);
    Ok(WorkflowSnapshot {
        id: replay.id,
        root_id: replay.root_id,
        revision,
        status,
        agents: agents.into_values().collect(),
    })
}

async fn finish_turn(turn: TurnHandle) -> AgentResult {
    match turn.wait().await {
        Ok(turn) => match &turn.result {
            TurnResult::Cancelled => AgentResult::Cancelled,
            TurnResult::Failed(error) => AgentResult::Failed(error.clone()),
            TurnResult::Truncated => AgentResult::Failed("turn was truncated".to_string()),
            TurnResult::Stopped(_) => AgentResult::Completed(turn),
        },
        Err(AshError::Cancelled) => AgentResult::Cancelled,
        Err(error) => AgentResult::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash_core::{ModelClient, ModelEvent, ModelId, ModelRequest, ModelStream, ProtocolError};
    use futures::stream;

    struct Model;

    struct PendingModel;

    impl ModelClient for PendingModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            Ok(Box::pin(stream::pending()))
        }
    }

    fn test_workflow(model: impl ModelClient + 'static) -> (tempfile::TempDir, Workflow) {
        let directory = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(
            Runtime::new(Arc::new(model)).with_session_directory(directory.path()),
            Agent::new(ModelId::new("test"), Vec::new()),
            "workflow",
        );
        (directory, workflow)
    }

    fn root_status(workflow: &Workflow) -> AgentStatus {
        workflow
            .snapshot()
            .agents
            .iter()
            .find(|agent| agent.id == workflow.root_id())
            .unwrap()
            .status
    }

    fn workflow_log(directory: &tempfile::TempDir, workflow: &Workflow) -> std::path::PathBuf {
        directory
            .path()
            .join("workflows")
            .join(workflow.root_id().to_string())
            .join(format!("{}.jsonl", workflow.id()))
    }

    impl ModelClient for Model {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            Ok(Box::pin(stream::iter([
                Ok(ModelEvent::Text("done".to_string())),
                Ok(ModelEvent::Stop(ash_core::StopReason::EndTurn)),
            ])))
        }
    }

    #[tokio::test]
    async fn creates_nested_agents_and_publishes_parent_ids() {
        let (_directory, workflow) = test_workflow(Model);
        let root_child = workflow
            .spawn_agent(None, "root-child", "start")
            .await
            .expect("root child");
        let nested = workflow
            .spawn_agent(Some(root_child.id()), "nested", "continue")
            .await
            .expect("nested child");

        assert_eq!(workflow.snapshot().agents.len(), 3);
        let snapshot = workflow.snapshot();
        assert_eq!(
            snapshot
                .agents
                .iter()
                .find(|agent| agent.id == root_child.id())
                .and_then(|agent| agent.parent_id),
            Some(workflow.root_id())
        );
        assert_eq!(
            snapshot
                .agents
                .iter()
                .find(|agent| agent.id == nested.id())
                .and_then(|agent| agent.parent_id),
            Some(root_child.id())
        );
        assert!(matches!(root_child.wait().await, AgentResult::Completed(_)));
        assert!(matches!(nested.wait().await, AgentResult::Completed(_)));
    }

    #[tokio::test]
    async fn runs_javascript_with_nested_agent_ids() {
        let (directory, workflow) = test_workflow(Model);
        let value = workflow
            .run(
                r#"
                const parent = await agent("parent");
                const child = await agent("child", parent.id);
                return { parent: parent.output, child: child.output };
                "#,
            )
            .await
            .expect("script");

        assert_eq!(value["parent"], "done");
        assert_eq!(value["child"], "done");
        let snapshot = workflow.snapshot();
        assert_eq!(snapshot.agents.len(), 3);
        assert_eq!(
            snapshot
                .agents
                .iter()
                .find(|agent| agent.id == workflow.root_id())
                .map(|agent| agent.status),
            Some(AgentStatus::Completed)
        );
        let restored =
            Workflow::load_snapshot(&workflow.runtime, workflow.root_id(), workflow.id())
                .await
                .unwrap();
        assert_eq!(restored, snapshot);
        assert_eq!(
            Workflow::list_snapshots(&workflow.runtime, workflow.root_id())
                .await
                .unwrap(),
            vec![snapshot]
        );
        let records = tokio::fs::read_to_string(workflow_log(&directory, &workflow))
            .await
            .unwrap();
        assert_eq!(records.lines().count(), 7);
    }

    #[tokio::test]
    async fn rejects_unknown_parent() {
        let (_directory, workflow) = test_workflow(Model);
        let result = workflow
            .spawn_agent(Some(SessionId::new()), "child", "run")
            .await;
        assert!(matches!(result, Err(WorkflowError::UnknownParent(_))));
    }

    #[tokio::test]
    async fn rejects_repeated_scripts_without_changing_the_completed_result() {
        let (_directory, workflow) = test_workflow(Model);
        assert_eq!(workflow.run("return 1;").await.unwrap(), 1);
        assert!(matches!(
            workflow.run("throw new Error('failure');").await,
            Err(ScriptError::Workflow(WorkflowError::AlreadyStarted))
        ));
        assert_eq!(root_status(&workflow), AgentStatus::Completed);
        assert!(matches!(
            workflow.spawn_agent(None, "late", "run").await,
            Err(WorkflowError::Finished)
        ));
    }

    #[tokio::test]
    async fn cancellation_reaches_existing_children_and_rejects_new_work() {
        let (_directory, workflow) = test_workflow(PendingModel);
        let child = workflow.spawn_agent(None, "pending", "run").await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), child.wait())
                .await
                .is_err()
        );
        workflow.cancel();
        assert!(matches!(
            workflow.spawn_agent(None, "late", "run").await,
            Err(WorkflowError::Cancelled)
        ));
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), child.wait())
                .await
                .unwrap(),
            AgentResult::Cancelled
        ));
        assert_eq!(
            workflow
                .snapshot()
                .agents
                .iter()
                .find(|agent| agent.id == child.id())
                .unwrap()
                .status,
            AgentStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_scripts_that_never_settle_or_yield() {
        for script in ["await new Promise(() => {});", "while (true) {}"] {
            let (_directory, workflow) = test_workflow(Model);
            let running = workflow.clone();
            let task = tokio::spawn(async move { running.run(script).await });
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            workflow.cancel();
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(result, Err(ScriptError::Cancelled)));
            assert_eq!(root_status(&workflow), AgentStatus::Cancelled);
        }
    }

    #[tokio::test]
    async fn dropping_a_script_cancels_its_workflow() {
        let (_directory, workflow) = test_workflow(Model);
        let running = workflow.clone();
        let mut updates = workflow.subscribe();
        let task = tokio::spawn(async move { running.run("await new Promise(() => {});").await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(matches!(
            workflow.run("return 2;").await,
            Err(ScriptError::Workflow(WorkflowError::AlreadyStarted))
        ));
        task.abort();
        let _ = task.await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while root_status(&workflow) != AgentStatus::Cancelled {
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            workflow.spawn_agent(None, "late", "run").await,
            Err(WorkflowError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn cold_snapshot_marks_unfinished_work_as_interrupted() {
        let (_directory, workflow) = test_workflow(PendingModel);
        let child = workflow.spawn_agent(None, "pending", "run").await.unwrap();

        let restored =
            Workflow::load_snapshot(&workflow.runtime, workflow.root_id(), workflow.id())
                .await
                .unwrap();

        assert_eq!(restored.status, WorkflowStatus::Interrupted);
        assert!(restored
            .agents
            .iter()
            .all(|agent| agent.status == AgentStatus::Interrupted));
        workflow.cancel();
        assert!(matches!(child.wait().await, AgentResult::Cancelled));
    }

    #[tokio::test]
    async fn cold_snapshot_ignores_an_incomplete_jsonl_tail() {
        use tokio::io::AsyncWriteExt as _;

        let (directory, workflow) = test_workflow(Model);
        workflow.run("return 1;").await.unwrap();
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(workflow_log(&directory, &workflow))
            .await
            .unwrap();
        file.write_all(br#"{"type":"event""#).await.unwrap();
        file.flush().await.unwrap();

        let restored =
            Workflow::load_snapshot(&workflow.runtime, workflow.root_id(), workflow.id())
                .await
                .unwrap();

        assert_eq!(restored.status, WorkflowStatus::Completed);
        assert_eq!(restored.agents.len(), 1);
    }
}
