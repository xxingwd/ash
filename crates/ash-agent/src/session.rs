use std::{collections::VecDeque, sync::Arc};

use ash_core::{
    AshError, CancellationToken, Conversation, Input, SessionError, SessionEvent, SessionId,
    SessionIdentity, Step, Turn, TurnId, TurnResult,
};
use tokio::sync::{broadcast, mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::BroadcastStream;

use crate::{
    context::estimate_request_tokens,
    engine::{compact, run_turn, EngineOutcome, StepCommitter, TurnChannels},
    jsonl::{JsonlSessionStore, OpenedSession, SessionWriter, TurnStart},
    Agent, Runtime,
};

const EMPTY_INPUT_ERROR: &str = "session input cannot be empty";
const MAX_PENDING_TURNS: usize = 64;

#[derive(Clone)]
pub struct Session {
    identity: SessionIdentity,
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<SessionEvent>,
    capacity: Arc<Semaphore>,
}

pub struct TurnHandle {
    session_id: SessionId,
    id: TurnId,
    cancellation: CancellationToken,
    completion: oneshot::Receiver<Result<Arc<Turn>, AshError>>,
}

pub struct ForkedSession {
    pub session: Session,
    pub input: Input,
}

struct QueuedTurn {
    id: TurnId,
    input: Input,
    cancellation: CancellationToken,
    completion: oneshot::Sender<Result<Arc<Turn>, AshError>>,
    _permit: OwnedSemaphorePermit,
}

enum Command {
    Submit(QueuedTurn),
    View(oneshot::Sender<Result<Conversation, AshError>>),
    Undo(oneshot::Sender<Result<Option<ForkedSession>, AshError>>),
    Fork {
        turn_id: TurnId,
        reply: oneshot::Sender<Result<Option<ForkedSession>, AshError>>,
    },
    Compact {
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<bool, AshError>>,
    },
}

impl Session {
    pub(crate) fn spawn(state: SessionActorState) -> Self {
        let identity = state.identity;
        let (commands, command_rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(256);
        let actor_events = events.clone();
        let capacity = Arc::new(Semaphore::new(MAX_PENDING_TURNS));
        let actor_capacity = Arc::clone(&capacity);
        tokio::spawn(async move {
            if let Err(panic) = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                run_session(state, command_rx, actor_events),
            ))
            .await
            {
                tracing::error!(
                    session_id = %identity.id(),
                    message = %panic_payload(panic.as_ref()),
                    "session actor panicked"
                );
            }
            actor_capacity.close();
        });
        Self {
            identity,
            commands,
            events,
            capacity,
        }
    }

    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.identity.id()
    }

    #[must_use]
    pub const fn identity(&self) -> SessionIdentity {
        self.identity
    }

    #[must_use]
    pub fn events(&self) -> BroadcastStream<SessionEvent> {
        BroadcastStream::new(self.events.subscribe())
    }

    pub async fn submit(&self, input: impl Into<Input>) -> Result<TurnHandle, AshError> {
        let input = input.into();
        ensure_nonempty(&input)?;
        let permit = Arc::clone(&self.capacity)
            .acquire_owned()
            .await
            .map_err(|_| closed())?;
        let (command, handle) = self.prepare_submission(input, permit);
        self.commands.send(command).await.map_err(|_| closed())?;
        Ok(handle)
    }

    pub fn try_submit(&self, input: impl Into<Input>) -> Result<TurnHandle, AshError> {
        let input = input.into();
        ensure_nonempty(&input)?;
        let permit =
            Arc::clone(&self.capacity)
                .try_acquire_owned()
                .map_err(|error| match error {
                    tokio::sync::TryAcquireError::NoPermits => SessionError::QueueFull,
                    tokio::sync::TryAcquireError::Closed => SessionError::Closed,
                })?;
        let (command, handle) = self.prepare_submission(input, permit);
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SessionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => SessionError::Closed,
            })?;
        Ok(handle)
    }

    pub async fn conversation(&self) -> Result<Conversation, AshError> {
        self.ask(Command::View).await
    }

    pub async fn undo(&self) -> Result<Option<ForkedSession>, AshError> {
        self.ask(Command::Undo).await
    }

    pub async fn fork_at(&self, turn_id: TurnId) -> Result<Option<ForkedSession>, AshError> {
        self.ask(|reply| Command::Fork { turn_id, reply }).await
    }

    pub async fn compact(&self) -> Result<bool, AshError> {
        self.compact_with_cancellation(CancellationToken::new())
            .await
    }

    pub async fn compact_with_cancellation(
        &self,
        cancellation: CancellationToken,
    ) -> Result<bool, AshError> {
        self.ask(|reply| Command::Compact {
            cancellation,
            reply,
        })
        .await
    }

    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, AshError>>) -> Command,
    ) -> Result<T, AshError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| closed())?;
        result.await.map_err(|_| closed())?
    }

    fn prepare_submission(
        &self,
        input: Input,
        permit: OwnedSemaphorePermit,
    ) -> (Command, TurnHandle) {
        let id = TurnId::new();
        let cancellation = CancellationToken::new();
        let (completion_tx, completion) = oneshot::channel();
        (
            Command::Submit(QueuedTurn {
                id,
                input,
                cancellation: cancellation.clone(),
                completion: completion_tx,
                _permit: permit,
            }),
            TurnHandle {
                session_id: self.id(),
                id,
                cancellation,
                completion,
            },
        )
    }
}

impl TurnHandle {
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[must_use]
    pub const fn id(&self) -> TurnId {
        self.id
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub async fn wait(self) -> Result<Arc<Turn>, AshError> {
        self.completion.await.map_err(|_| closed())?
    }
}

async fn run_session(
    mut state: SessionActorState,
    mut commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<SessionEvent>,
) {
    let mut queue = VecDeque::new();
    loop {
        if let Some(turn) = queue.pop_front() {
            run_queued_turn(&mut state, turn, &mut queue, &mut commands, &events).await;
            continue;
        }
        let Some(command) = commands.recv().await else {
            break;
        };
        dispatch_idle(command, &mut state, &mut queue).await;
    }
}

async fn run_queued_turn(
    state: &mut SessionActorState,
    queued: QueuedTurn,
    queue: &mut VecDeque<QueuedTurn>,
    commands: &mut mpsc::Receiver<Command>,
    events: &broadcast::Sender<SessionEvent>,
) {
    let QueuedTurn {
        id,
        input,
        cancellation,
        completion,
        _permit,
    } = queued;
    publish(events, SessionEvent::Started(id));
    let (live_tx, mut live_rx) = mpsc::channel(64);
    let (committer, mut commits) = StepCommitter::channel();
    let model = state.runtime.model_handle();
    let agent = state.agent.clone();
    let conversation = state.conversation.clone();
    let execution = run_turn(
        model.as_ref(),
        &agent,
        &conversation,
        id,
        input.clone(),
        TurnChannels::new(live_tx, committer),
        ash_core::ToolContext {
            identity: state.identity,
            cancellation: cancellation.clone(),
            deadline: None,
        },
    );
    tokio::pin!(execution);
    let mut committed_steps = 0;
    let mut commit_error = None;
    let outcome = loop {
        tokio::select! {
            biased;
            outcome = &mut execution => break outcome,
            commit = commits.recv(), if commit_error.is_none() => {
                let Some(commit) = commit else {
                    continue;
                };
                while let Ok(event) = live_rx.try_recv() {
                    publish(events, event);
                }
                let start = (committed_steps == 0).then(|| TurnStart {
                    id,
                    input: input.clone(),
                });
                match state
                    .commit_step(start, Arc::clone(&commit.step))
                    .await
                {
                    Ok(()) => {
                        publish(
                            events,
                            SessionEvent::StepCommitted {
                                turn_id: id,
                                index: committed_steps,
                                step: commit.step,
                            },
                        );
                        committed_steps += 1;
                        let _ = commit.reply.send(Ok(()));
                    }
                    Err(error) => {
                        commit_error = Some(error.to_string());
                        let _ = commit.reply.send(Err(error));
                    }
                }
            }
            event = live_rx.recv() => {
                if let Some(event) = event {
                    publish(events, event);
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    cancellation.cancel();
                    continue;
                };
                dispatch_active(command, queue);
            }
        }
    };
    while let Ok(event) = live_rx.try_recv() {
        publish(events, event);
    }
    while let Ok(command) = commands.try_recv() {
        dispatch_active(command, queue);
    }

    let discard = matches!(outcome.turn.result, TurnResult::Cancelled)
        && !outcome.turn.has_tools()
        && queue.is_empty();
    let result = if let Some(error) = commit_error {
        publish(
            events,
            SessionEvent::Discarded {
                turn_id: id,
                error: Some(error.clone()),
            },
        );
        Err(outcome.error.unwrap_or_else(|| AshError::Config(error)))
    } else if discard {
        publish(
            events,
            SessionEvent::Discarded {
                turn_id: id,
                error: None,
            },
        );
        Err(AshError::Cancelled)
    } else {
        commit_outcome(state, outcome, committed_steps > 0, events).await
    };
    let _ = completion.send(result);
}

async fn commit_outcome(
    state: &mut SessionActorState,
    outcome: EngineOutcome,
    start_written: bool,
    events: &broadcast::Sender<SessionEvent>,
) -> Result<Arc<Turn>, AshError> {
    let turn = Arc::clone(&outcome.turn);
    let summary = outcome.summary;
    if let Err(error) = state
        .commit_turn(Arc::clone(&turn), summary.clone(), start_written)
        .await
    {
        publish(
            events,
            SessionEvent::Discarded {
                turn_id: turn.id,
                error: Some(error.to_string()),
            },
        );
        return Err(error);
    }
    publish(
        events,
        SessionEvent::Finished {
            turn: Arc::clone(&turn),
            summary,
        },
    );
    match outcome.error {
        Some(error) => Err(error),
        None => Ok(turn),
    }
}

fn dispatch_active(command: Command, queue: &mut VecDeque<QueuedTurn>) {
    match command {
        Command::Submit(turn) => queue.push_back(turn),
        Command::View(reply) => reject_busy(reply),
        Command::Undo(reply) => reject_busy(reply),
        Command::Fork { reply, .. } => reject_busy(reply),
        Command::Compact { reply, .. } => reject_busy(reply),
    }
}

async fn dispatch_idle(
    command: Command,
    state: &mut SessionActorState,
    queue: &mut VecDeque<QueuedTurn>,
) {
    match command {
        Command::Submit(turn) => queue.push_back(turn),
        Command::View(reply) => {
            let _ = reply.send(Ok(state.conversation.clone()));
        }
        Command::Undo(reply) => {
            let result = state.undo().await.map(|fork| {
                fork.map(|(state, input)| ForkedSession {
                    session: Session::spawn(state),
                    input,
                })
            });
            let _ = reply.send(result);
        }
        Command::Fork { turn_id, reply } => {
            let result = state.fork_at(turn_id).await.map(|fork| {
                fork.map(|(state, input)| ForkedSession {
                    session: Session::spawn(state),
                    input,
                })
            });
            let _ = reply.send(result);
        }
        Command::Compact {
            cancellation,
            mut reply,
        } => {
            let operation = state.compact(&cancellation);
            tokio::pin!(operation);
            tokio::select! {
                biased;
                () = reply.closed() => {
                    cancellation.cancel();
                    let _ = operation.await;
                }
                result = &mut operation => {
                    let _ = reply.send(result);
                }
            }
        }
    }
}

fn reject_busy<T>(reply: oneshot::Sender<Result<T, AshError>>) {
    let _ = reply.send(Err(SessionError::Busy.into()));
}

fn publish(events: &broadcast::Sender<SessionEvent>, event: SessionEvent) {
    match &event {
        SessionEvent::Started(turn_id) => tracing::info!(%turn_id, "turn started"),
        SessionEvent::Retrying { turn_id } => tracing::info!(%turn_id, "model response retrying"),
        SessionEvent::Text { turn_id, text } => {
            tracing::debug!(%turn_id, chars = text.len(), "text streamed")
        }
        SessionEvent::Thought { turn_id, text } => {
            tracing::debug!(%turn_id, chars = text.len(), "thought streamed")
        }
        SessionEvent::Activity { turn_id, activity } => tracing::debug!(
            %turn_id,
            input_tokens = activity.stats.input_tokens,
            output_tokens = activity.stats.output_tokens,
            generation_ms = activity.stats.generation_ms,
            completed_tool_calls = activity.completed_tool_calls,
            "turn activity"
        ),
        SessionEvent::Context {
            turn_id,
            tokens,
            limit,
        } => tracing::debug!(%turn_id, tokens, limit, "context estimated"),
        SessionEvent::ToolStarted {
            turn_id, id, name, ..
        } => {
            tracing::info!(%turn_id, %id, tool = %name, "tool started")
        }
        SessionEvent::ToolFinished {
            turn_id,
            id,
            result,
        } => match result {
            Ok(output) => tracing::info!(%turn_id, %id, chars = output.len(), "tool finished"),
            Err(_) => tracing::warn!(%turn_id, %id, "tool failed"),
        },
        SessionEvent::StepCommitted {
            turn_id,
            index,
            step,
        } => tracing::info!(
            %turn_id,
            index,
            items = step.items.len(),
            "turn step committed"
        ),
        SessionEvent::Finished { turn, summary } => tracing::info!(
            turn_id = %turn.id,
            tools = turn.completed_tool_calls(),
            result = turn_result_label(&turn.result),
            compacted = summary.is_some(),
            "turn finished"
        ),
        SessionEvent::Discarded { turn_id, error } if error.is_some() => {
            tracing::warn!(%turn_id, "turn discarded with an error")
        }
        SessionEvent::Discarded { turn_id, .. } => tracing::info!(%turn_id, "turn discarded"),
    }
    let _ = events.send(event);
}

fn turn_result_label(result: &TurnResult) -> &'static str {
    match result {
        TurnResult::Stopped(_) => "stopped",
        TurnResult::Cancelled => "cancelled",
        TurnResult::Truncated => "truncated",
        TurnResult::Failed(_) => "failed",
    }
}

pub(crate) struct SessionActorState {
    identity: SessionIdentity,
    agent: Agent,
    runtime: Runtime,
    conversation: Conversation,
    store: Arc<JsonlSessionStore>,
    writer: Option<SessionWriter>,
}

impl SessionActorState {
    pub(crate) fn new(agent: Agent, runtime: Runtime) -> Self {
        Self::with_identity(agent, runtime, SessionIdentity::root(SessionId::new()))
    }

    pub(crate) fn new_child(agent: Agent, runtime: Runtime, parent: SessionIdentity) -> Self {
        Self::with_identity(agent, runtime, parent.child())
    }

    fn with_identity(agent: Agent, runtime: Runtime, identity: SessionIdentity) -> Self {
        let store = runtime.session_store_handle();
        Self {
            identity,
            agent,
            runtime,
            conversation: Conversation::new(),
            store,
            writer: None,
        }
    }

    pub(crate) async fn resume(&mut self, session_id: SessionId) -> Result<bool, AshError> {
        let Some(opened) = self.store.open(session_id).await? else {
            return Ok(false);
        };
        if !opened.session.identity.is_root() {
            return Err(SessionError::ChildSession.into());
        }
        self.restore(opened);
        Ok(true)
    }

    fn restore(&mut self, opened: OpenedSession) {
        self.identity = opened.session.identity;
        self.conversation = opened.session.conversation;
        self.writer = Some(opened.writer);
    }

    async fn commit_turn(
        &mut self,
        turn: Arc<Turn>,
        summary: Option<String>,
        start_written: bool,
    ) -> Result<(), AshError> {
        let writer = self.writer().await?;
        writer
            .finish_turn(
                (!start_written).then(|| TurnStart {
                    id: turn.id,
                    input: turn.input.clone(),
                }),
                turn.result.clone(),
                turn.stats,
                summary.clone(),
            )
            .await?;
        self.conversation.push(turn, summary);
        Ok(())
    }

    async fn commit_step(
        &mut self,
        start: Option<TurnStart>,
        step: Arc<Step>,
    ) -> Result<(), AshError> {
        self.writer().await?.commit_step(start, step).await
    }

    async fn compact(&mut self, cancellation: &CancellationToken) -> Result<bool, AshError> {
        if cancellation.is_cancelled() {
            return Err(AshError::Cancelled);
        }
        let tools = self.agent.tool_definitions();
        let before = estimate_request_tokens(
            self.agent.system_prompt(),
            &self.conversation.context(),
            &tools,
        );
        let Some((summary, _stats)) = compact(
            self.runtime.model(),
            &self.agent,
            &self.conversation,
            cancellation,
        )
        .await?
        else {
            return Ok(false);
        };
        let after = estimate_request_tokens(
            self.agent.system_prompt(),
            &self.conversation.context_with_summary(&summary),
            &tools,
        );
        if after >= before {
            return Ok(false);
        }
        self.writer().await?.checkpoint(summary.clone()).await?;
        self.conversation.compact(summary);
        Ok(true)
    }

    async fn undo(&self) -> Result<Option<(Self, Input)>, AshError> {
        if !self.identity.is_root() {
            return Err(SessionError::ChildSession.into());
        }
        let Some(last) = self.conversation.turns().last() else {
            return Ok(None);
        };
        self.fork_at(last.id).await
    }

    async fn fork_at(&self, turn_id: TurnId) -> Result<Option<(Self, Input)>, AshError> {
        if !self.identity.is_root() {
            return Err(SessionError::ChildSession.into());
        }
        let Some((conversation, input)) = self.conversation.before(turn_id) else {
            return Ok(None);
        };
        let mut state = Self::new(self.agent.clone(), self.runtime.clone());
        if conversation.summary().is_some() || !conversation.turns().is_empty() {
            let mut writer = state.store.open_new(state.identity).await?;
            writer.seed(&conversation, &input).await?;
            state.writer = Some(writer);
        }
        state.conversation = conversation;
        Ok(Some((state, input)))
    }

    async fn writer(&mut self) -> Result<&mut SessionWriter, AshError> {
        if self.writer.is_none() {
            self.writer = Some(self.store.open_new(self.identity).await?);
        }
        self.writer.as_mut().ok_or_else(closed)
    }
}

fn ensure_nonempty(input: &Input) -> Result<(), AshError> {
    if input.is_empty() {
        return Err(AshError::Config(EMPTY_INPUT_ERROR.to_string()));
    }
    Ok(())
}

const fn closed() -> AshError {
    AshError::Session(SessionError::Closed)
}

fn panic_payload(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| panic.downcast_ref::<String>().map(ToString::to_string))
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex as StdMutex,
        },
    };

    use ash_core::{
        define_tool, ModelEvent, ModelId, ModelRequest, ModelStream, ProtocolError, StopReason,
        TurnStats,
    };
    use futures::StreamExt;
    use tempfile::TempDir;

    use super::*;

    struct MockModel {
        responses: StdMutex<VecDeque<Vec<ModelEvent>>>,
    }

    impl MockModel {
        fn new(responses: impl IntoIterator<Item = Vec<ModelEvent>>) -> Self {
            Self {
                responses: StdMutex::new(responses.into_iter().collect()),
            }
        }
    }

    impl ash_core::ModelClient for MockModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            let events = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    struct PendingModel;

    impl ash_core::ModelClient for PendingModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    struct QueueModel {
        calls: AtomicUsize,
        started: tokio::sync::Notify,
    }

    struct StepThenPendingModel {
        calls: AtomicUsize,
    }

    impl ash_core::ModelClient for StepThenPendingModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::pin(futures::stream::iter(
                    [
                        ModelEvent::ToolCall {
                            id: ash_core::ToolCallId::from_provider("call"),
                            name: "test".to_string(),
                            arguments: serde_json::json!({}),
                        },
                        ModelEvent::Stop(StopReason::EndTurn),
                    ]
                    .into_iter()
                    .map(Ok),
                )))
            } else {
                Ok(Box::pin(futures::stream::pending()))
            }
        }
    }

    impl ash_core::ModelClient for QueueModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.started.notify_one();
                Ok(Box::pin(futures::stream::pending()))
            } else {
                Ok(Box::pin(futures::stream::iter(
                    response("second").into_iter().map(Ok),
                )))
            }
        }
    }

    fn agent() -> Agent {
        Agent::new(ModelId::new("test-model"), Vec::new())
    }

    fn runtime(model: Arc<dyn ash_core::ModelClient>, directory: &TempDir) -> Runtime {
        Runtime::new(model).with_session_directory(directory.path())
    }

    fn response(text: &str) -> Vec<ModelEvent> {
        vec![
            ModelEvent::Text(text.to_string()),
            ModelEvent::Stop(StopReason::EndTurn),
        ]
    }

    #[tokio::test]
    async fn bounds_all_accepted_turns_even_after_the_mailbox_is_drained() {
        let directory = TempDir::new().unwrap();
        let session = runtime(Arc::new(PendingModel), &directory).start(&agent());
        let mut handles = VecDeque::new();
        for index in 0..MAX_PENDING_TURNS {
            handles.push_back(session.try_submit(format!("turn {index}")).unwrap());
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            session.try_submit("overflow"),
            Err(AshError::Session(SessionError::QueueFull))
        ));
        let first = handles.pop_front().unwrap();
        first.cancellation_token().cancel();
        first.wait().await.unwrap();
        handles.push_back(session.try_submit("released slot").unwrap());
        for handle in handles {
            handle.cancellation_token().cancel();
        }
    }

    #[tokio::test]
    async fn async_submission_waits_for_capacity_without_losing_a_slot() {
        let directory = TempDir::new().unwrap();
        let session = runtime(Arc::new(PendingModel), &directory).start(&agent());
        let mut handles = VecDeque::new();
        for _ in 0..MAX_PENDING_TURNS {
            handles.push_back(session.submit("pending").await.unwrap());
        }
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            session.submit("overflow")
        )
        .await
        .is_err());
        let first = handles.pop_front().unwrap();
        first.cancellation_token().cancel();
        first.wait().await.unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(1), session.submit("next"))
            .await
            .unwrap()
            .unwrap();
        handles.push_back(next);
        for handle in handles {
            handle.cancellation_token().cancel();
        }
    }

    #[tokio::test]
    async fn cancelled_manual_compaction_preserves_history_and_releases_the_actor() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime(Arc::new(PendingModel), &directory);
        let mut state = SessionActorState::new(agent(), runtime);
        state.conversation.push(
            Arc::new(Turn {
                id: TurnId::new(),
                input: Input::user("history"),
                steps: Vec::new(),
                result: TurnResult::Stopped(StopReason::EndTurn),
                stats: Default::default(),
            }),
            None,
        );
        let session = Session::spawn(state);
        let cancellation = CancellationToken::new();
        let running = session.clone();
        let token = cancellation.clone();
        let task = tokio::spawn(async move { running.compact_with_cancellation(token).await });
        tokio::task::yield_now().await;
        cancellation.cancel();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            Err(AshError::Cancelled)
        ));
        let conversation = session.conversation().await.unwrap();
        assert_eq!(conversation.turns().len(), 1);
        assert!(conversation.summary().is_none());
    }

    #[tokio::test]
    async fn dropping_manual_compaction_cancels_the_model_wait() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime(Arc::new(PendingModel), &directory);
        let mut state = SessionActorState::new(agent(), runtime);
        state.conversation.push(
            Arc::new(Turn {
                id: TurnId::new(),
                input: Input::user("history"),
                steps: Vec::new(),
                result: TurnResult::Stopped(StopReason::EndTurn),
                stats: Default::default(),
            }),
            None,
        );
        let session = Session::spawn(state);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), session.compact())
                .await
                .is_err()
        );
        let conversation =
            tokio::time::timeout(std::time::Duration::from_secs(1), session.conversation())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(conversation.turns().len(), 1);
        assert!(conversation.summary().is_none());
    }

    #[tokio::test]
    async fn undo_forks_the_prefix_without_changing_the_original() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime(
            Arc::new(MockModel::new([response("one"), response("two")])),
            &directory,
        );
        let session = runtime.start(&agent());
        session.submit("first").await.unwrap().wait().await.unwrap();
        session
            .submit("second")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();

        let fork = session.undo().await.unwrap().unwrap();

        assert_eq!(fork.input, Input::user("second"));
        assert_eq!(fork.session.conversation().await.unwrap().turns().len(), 1);
        assert_eq!(session.conversation().await.unwrap().turns().len(), 2);
        let data = tokio::fs::read_to_string(
            directory
                .path()
                .join(format!("{}.jsonl", fork.session.id())),
        )
        .await
        .unwrap();
        let records = data
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 4);
        assert_eq!(records[1]["type"], "turn_start");
        assert_eq!(records[2]["type"], "turn_step");
        assert_eq!(records[3]["summary"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn fork_before_the_first_turn_stays_in_memory() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime(Arc::new(MockModel::new([response("one")])), &directory);
        let session = runtime.start(&agent());
        let turn = session.submit("first").await.unwrap().wait().await.unwrap();

        let fork = session.fork_at(turn.id).await.unwrap().unwrap();

        assert_eq!(fork.input, Input::user("first"));
        assert!(fork
            .session
            .conversation()
            .await
            .unwrap()
            .turns()
            .is_empty());
        assert!(!tokio::fs::try_exists(
            directory
                .path()
                .join(format!("{}.jsonl", fork.session.id()))
        )
        .await
        .unwrap());
    }

    #[tokio::test]
    async fn fork_preserves_a_checkpoint_before_the_selected_loaded_turn() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime(Arc::new(MockModel::new([])), &directory);
        let mut state = SessionActorState::new(agent(), runtime);
        state.conversation.compact("older turns".to_string());
        let selected = Arc::new(Turn {
            id: TurnId::new(),
            input: Input::user("selected"),
            steps: Vec::new(),
            result: TurnResult::Stopped(StopReason::EndTurn),
            stats: TurnStats::default(),
        });
        state.conversation.push(Arc::clone(&selected), None);

        let (fork, input) = state.fork_at(selected.id).await.unwrap().unwrap();
        let fork_id = fork.identity.id();

        assert_eq!(input, selected.input);
        assert_eq!(fork.conversation.summary(), Some("older turns"));
        assert!(fork.conversation.turns().is_empty());
        drop(fork);

        let stored = state.store.load(fork_id).await.unwrap().unwrap();
        assert_eq!(stored.conversation.summary(), Some("older turns"));
        assert!(stored.conversation.turns().is_empty());
    }

    #[tokio::test]
    async fn child_sessions_reject_fork_and_undo_even_when_empty() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime(Arc::new(MockModel::new([])), &directory);
        let child = runtime.start_child(&agent(), SessionIdentity::root(SessionId::new()));

        assert!(matches!(
            child.undo().await,
            Err(AshError::Session(SessionError::ChildSession))
        ));
        assert!(matches!(
            child.fork_at(TurnId::new()).await,
            Err(AshError::Session(SessionError::ChildSession))
        ));
    }

    #[tokio::test]
    async fn cancellation_without_tools_or_a_successor_discards_the_turn() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime(Arc::new(PendingModel), &directory);
        let session = runtime.start(&agent());
        let handle = session.submit("cancel me").await.unwrap();
        handle.cancellation_token().cancel();

        assert!(matches!(handle.wait().await, Err(AshError::Cancelled)));
        assert!(session.conversation().await.unwrap().turns().is_empty());
        assert!(
            !tokio::fs::try_exists(directory.path().join(format!("{}.jsonl", session.id())))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn cancellation_is_committed_when_a_successor_is_queued() {
        let directory = TempDir::new().unwrap();
        let model = Arc::new(QueueModel {
            calls: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
        });
        let runtime = runtime(model.clone(), &directory);
        let session = runtime.start(&agent());
        let first = session.submit("first").await.unwrap();
        model.started.notified().await;
        let second = session.submit("second").await.unwrap();
        assert!(matches!(
            session.conversation().await,
            Err(AshError::Session(SessionError::Busy))
        ));

        first.cancellation_token().cancel();
        let first = first.wait().await.unwrap();
        second.wait().await.unwrap();

        assert_eq!(first.result, TurnResult::Cancelled);
        let conversation = session.conversation().await.unwrap();
        assert_eq!(conversation.turns().len(), 2);
        assert_eq!(conversation.turns()[0].result, TurnResult::Cancelled);
    }

    #[tokio::test]
    async fn step_is_persisted_and_published_before_the_turn_finishes() {
        let directory = TempDir::new().unwrap();
        let model = Arc::new(StepThenPendingModel {
            calls: AtomicUsize::new(0),
        });
        let tool = define_tool("test", "test", |_, (): ()| async { Ok("done") }).unwrap();
        let runtime = runtime(model, &directory);
        let session = runtime.start(&Agent::new(ModelId::new("test-model"), vec![tool]));
        let mut events = session.events();
        let handle = session.submit("run").await.unwrap();

        let committed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let mut tool_finished = false;
            loop {
                match events.next().await {
                    Some(Ok(SessionEvent::ToolFinished { .. })) => tool_finished = true,
                    Some(Ok(event @ SessionEvent::StepCommitted { .. })) => {
                        assert!(tool_finished);
                        break event;
                    }
                    Some(Ok(_)) | Some(Err(_)) => {}
                    None => panic!("session event stream closed"),
                }
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            committed,
            SessionEvent::StepCommitted { index: 0, .. }
        ));

        let path = directory.path().join(format!("{}.jsonl", session.id()));
        let records = tokio::fs::read_to_string(&path).await.unwrap();
        let types = records
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["type"].clone())
            .collect::<Vec<_>>();
        assert_eq!(types, ["init", "turn_start", "turn_step"]);

        handle.cancellation_token().cancel();
        let turn = handle.wait().await.unwrap();
        assert_eq!(turn.result, TurnResult::Cancelled);
        let records = tokio::fs::read_to_string(path).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(records.lines().last().unwrap()).unwrap()
                ["type"],
            "turn_end"
        );
    }
}
