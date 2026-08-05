use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use ash_core::{
    CancellationToken, Event, EventKind, ForkPoint, Message, MessageId, StopReason, ThreadId,
    ThreadView, TurnId, TurnResult, TurnView,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::BroadcastStream;

use crate::context::estimate_request_tokens;
use crate::engine::{compact_with_adapter, run_agent_turn_persisted, TurnExecution};
use crate::{
    AcceptedInput, ContextCheckpoint, Input, LogEntry, OpenedThread, RunConfig, Runtime,
    SharedThreadStore, ThreadAppender, ThreadLog, ThreadMetadata, TurnContext,
};

const EMPTY_INPUT_ERROR: &str = "thread input cannot be empty";
const INACTIVE_TURN_ERROR: &str = "the target turn is not active";

#[derive(Clone)]
pub struct Thread {
    id: ThreadId,
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<Event>,
}

pub struct Turn {
    thread_id: ThreadId,
    id: TurnId,
    commands: mpsc::Sender<Command>,
    cancellation: CancellationToken,
    completion: oneshot::Receiver<Result<StopReason, ash_core::AshError>>,
}

struct QueuedTurn {
    id: TurnId,
    inputs: Vec<Input>,
    cancellation: CancellationToken,
    completion: Option<oneshot::Sender<Result<StopReason, ash_core::AshError>>>,
}

#[derive(Default)]
struct ActorQueues {
    turns: VecDeque<QueuedTurn>,
    commands: VecDeque<Command>,
    inbox: Vec<Input>,
}

enum Command {
    Submit(QueuedTurn),
    Notify(Input),
    Steer {
        turn_id: TurnId,
        input: Input,
        reply: oneshot::Sender<Result<(), ash_core::AshError>>,
    },
    Rollback(oneshot::Sender<Result<Option<String>, ash_core::AshError>>),
    Compact(oneshot::Sender<Result<ContextCompaction, ash_core::AshError>>),
    Messages(oneshot::Sender<Vec<Message>>),
    View(oneshot::Sender<ThreadView>),
    ForkPoints(oneshot::Sender<Vec<ForkPoint>>),
    Fork {
        message_id: MessageId,
        reply: oneshot::Sender<Result<Option<Fork>, ash_core::AshError>>,
    },
}

/// Result of routing a command: either fully handled (queued immediately) or
/// handed to the turn-aware dispatcher.
enum RouteOutcome {
    /// Queued immediately (submit/notify), independent of turn state.
    Handled,
    /// A steer for the active turn.
    Steer {
        turn_id: TurnId,
        input: Input,
        reply: oneshot::Sender<Result<(), ash_core::AshError>>,
    },
    /// Deferred until the thread is idle.
    Deferred(Command),
}

impl Command {
    fn route(self, queues: &mut ActorQueues) -> RouteOutcome {
        match self {
            Command::Submit(turn) => {
                queues.turns.push_back(turn);
                RouteOutcome::Handled
            }
            Command::Notify(input) => {
                queues.inbox.push(input);
                RouteOutcome::Handled
            }
            Command::Steer {
                turn_id,
                input,
                reply,
            } => RouteOutcome::Steer {
                turn_id,
                input,
                reply,
            },
            Command::Rollback(_)
            | Command::Compact(_)
            | Command::Messages(_)
            | Command::View(_)
            | Command::ForkPoints(_)
            | Command::Fork { .. } => RouteOutcome::Deferred(self),
        }
    }
}

impl Thread {
    pub(crate) fn spawn(state: ThreadState) -> Self {
        let id = state.id();
        let (commands, command_rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(256);
        tokio::spawn(run_thread(state, command_rx, events.clone()));
        Self {
            id,
            commands,
            events,
        }
    }

    pub fn id(&self) -> ThreadId {
        self.id
    }

    pub fn events(&self) -> BroadcastStream<Event> {
        BroadcastStream::new(self.events.subscribe())
    }

    pub async fn submit(&self, input: impl Into<Input>) -> Result<Turn, ash_core::AshError> {
        let input = input.into();
        ensure_nonempty(&input)?;
        let (completion_tx, completion) = oneshot::channel();
        let queued = queued_turn(input, Some(completion_tx));
        let id = queued.id;
        let cancellation = queued.cancellation.clone();
        self.commands
            .send(Command::Submit(queued))
            .await
            .map_err(|_| thread_closed())?;
        Ok(Turn {
            thread_id: self.id,
            id,
            commands: self.commands.clone(),
            cancellation,
            completion,
        })
    }

    /// Enqueue a standalone turn without retaining a turn handle.
    pub async fn enqueue(&self, input: impl Into<Input>) -> Result<TurnId, ash_core::AshError> {
        let input = input.into();
        ensure_nonempty(&input)?;
        let queued = queued_turn(input, None);
        let id = queued.id;
        self.commands
            .send(Command::Submit(queued))
            .await
            .map_err(|_| thread_closed())?;
        Ok(id)
    }

    /// Attach input to the next submitted turn without starting work by itself.
    pub async fn notify(&self, input: impl Into<Input>) -> Result<(), ash_core::AshError> {
        let input = input.into();
        ensure_nonempty(&input)?;
        self.commands
            .send(Command::Notify(input))
            .await
            .map_err(|_| thread_closed())
    }

    pub async fn messages(&self) -> Result<Vec<Message>, ash_core::AshError> {
        self.ask(Command::Messages).await
    }

    /// Full projected state: history, model context, and turn views.
    pub async fn view(&self) -> Result<ThreadView, ash_core::AshError> {
        self.ask(Command::View).await
    }

    pub async fn fork_points(&self) -> Result<Vec<ForkPoint>, ash_core::AshError> {
        self.ask(Command::ForkPoints).await
    }

    pub async fn rollback(&self) -> Result<Option<String>, ash_core::AshError> {
        self.ask(Command::Rollback).await?
    }

    pub async fn fork_at(&self, message_id: MessageId) -> Result<Option<Fork>, ash_core::AshError> {
        self.ask(|reply| Command::Fork { message_id, reply })
            .await?
    }

    pub async fn compact(&self) -> Result<ContextCompaction, ash_core::AshError> {
        self.ask(Command::Compact).await?
    }

    async fn ask<T>(
        &self,
        make_command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, ash_core::AshError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(make_command(reply))
            .await
            .map_err(|_| thread_closed())?;
        result.await.map_err(|_| thread_closed())
    }
}

impl Turn {
    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    pub fn id(&self) -> TurnId {
        self.id
    }

    pub fn interrupt(&self) {
        self.cancellation.cancel();
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub async fn steer(&self, input: impl Into<Input>) -> Result<(), ash_core::AshError> {
        let input = input.into();
        ensure_nonempty(&input)?;
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Steer {
                turn_id: self.id,
                input,
                reply,
            })
            .await
            .map_err(|_| thread_closed())?;
        result.await.map_err(|_| thread_closed())?
    }

    pub async fn wait(self) -> Result<StopReason, ash_core::AshError> {
        self.completion.await.map_err(|_| thread_closed())?
    }
}

async fn run_thread(
    mut state: ThreadState,
    mut commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<Event>,
) {
    let mut queues = ActorQueues::default();
    let mut sequence = 0_u64;
    loop {
        if let Some(command) = queues.commands.pop_front() {
            dispatch_idle_command(command, &mut state, &mut queues).await;
            continue;
        }
        if let Some(turn) = queues.turns.pop_front() {
            run_turn(
                &mut state,
                turn,
                &mut queues,
                &mut commands,
                &events,
                &mut sequence,
            )
            .await;
            continue;
        }
        let Some(command) = commands.recv().await else {
            break;
        };
        dispatch_idle_command(command, &mut state, &mut queues).await;
    }
}

async fn run_turn(
    state: &mut ThreadState,
    turn: QueuedTurn,
    queues: &mut ActorQueues,
    commands: &mut mpsc::Receiver<Command>,
    events: &broadcast::Sender<Event>,
    sequence: &mut u64,
) {
    let QueuedTurn {
        id,
        mut inputs,
        cancellation,
        completion,
    } = turn;
    if !queues.inbox.is_empty() {
        queues.inbox.append(&mut inputs);
        inputs = std::mem::take(&mut queues.inbox);
    }
    let thread_id = state.id();
    publish(events, thread_id, Some(id), sequence, EventKind::TurnStart);
    let (payload_tx, mut payload_rx) = mpsc::channel(64);
    let (steer_tx, steer_rx) = mpsc::unbounded_channel();
    let execution = state.submit_inputs(id, inputs, steer_rx, payload_tx, cancellation.clone());
    let mut execution = Box::pin(execution);

    let mut commands_open = true;
    let result = loop {
        tokio::select! {
            result = &mut execution => break result,
            payload = payload_rx.recv() => {
                if let Some(kind) = payload {
                    publish(events, thread_id, Some(id), sequence, kind);
                }
            }
            command = commands.recv(), if commands_open => {
                let Some(command) = command else {
                    cancellation.cancel();
                    commands_open = false;
                    continue;
                };
                dispatch_active_command(command, queues, id, &steer_tx);
            }
        }
    };
    while let Ok(kind) = payload_rx.try_recv() {
        publish(events, thread_id, Some(id), sequence, kind);
    }
    drop(execution);
    let view = match &result {
        Ok(view) => view.clone(),
        Err(error) => {
            publish(
                events,
                thread_id,
                Some(id),
                sequence,
                EventKind::Error(error.to_string()),
            );
            // `submit_inputs` can fail before writing anything (empty input,
            // duplicate idempotency key), so look up this exact turn.
            state.log.turn_view(id).unwrap_or_else(|| TurnView {
                id,
                result: TurnResult::Failed(error.to_string()),
                messages: Vec::new(),
                usage: None,
                context_tokens: None,
            })
        }
    };
    publish(events, thread_id, Some(id), sequence, EventKind::Turn(view));
    if let Some(completion) = completion {
        let _ = completion.send(result.map(|view| match view.result {
            TurnResult::Completed(reason) => reason,
            TurnResult::Failed(_) | TurnResult::Interrupted(_) => StopReason::Aborted,
        }));
    }
}

/// Route a command while a turn is active. Never touches `ThreadState` (the
/// turn's execution already borrows it), so this stays synchronous.
fn dispatch_active_command(
    command: Command,
    queues: &mut ActorQueues,
    active: TurnId,
    steer: &mpsc::UnboundedSender<Input>,
) {
    match command.route(queues) {
        RouteOutcome::Handled => {}
        RouteOutcome::Steer {
            turn_id,
            input,
            reply,
        } => handle_steer(turn_id, input, reply, Some(active), Some(steer)),
        RouteOutcome::Deferred(command) => queues.commands.push_back(command),
    }
}

/// Route a command while the thread is idle.
async fn dispatch_idle_command(
    command: Command,
    state: &mut ThreadState,
    queues: &mut ActorQueues,
) {
    match command.route(queues) {
        RouteOutcome::Handled => {}
        RouteOutcome::Steer {
            turn_id,
            input,
            reply,
        } => handle_steer(turn_id, input, reply, None, None),
        RouteOutcome::Deferred(command) => handle_idle_command(command, state).await,
    }
}

fn handle_steer(
    turn_id: TurnId,
    input: Input,
    reply: oneshot::Sender<Result<(), ash_core::AshError>>,
    active: Option<TurnId>,
    steer: Option<&mpsc::UnboundedSender<Input>>,
) {
    match (active, steer) {
        (Some(active), Some(steer)) if turn_id == active => {
            let result = steer.send(input).map_err(|_| thread_closed());
            let _ = reply.send(result);
        }
        _ => reject_steer(reply),
    }
}

fn reject_steer(reply: oneshot::Sender<Result<(), ash_core::AshError>>) {
    let _ = reply.send(Err(ash_core::AshError::Config(
        INACTIVE_TURN_ERROR.to_string(),
    )));
}

async fn handle_idle_command(command: Command, state: &mut ThreadState) {
    match command {
        Command::Rollback(reply) => {
            let _ = reply.send(state.rollback_last_turn().await);
        }
        Command::Compact(reply) => {
            let _ = reply.send(state.compact().await);
        }
        Command::Messages(reply) => {
            let _ = reply.send(state.messages());
        }
        Command::View(reply) => {
            let _ = reply.send(state.view());
        }
        Command::ForkPoints(reply) => {
            let _ = reply.send(state.fork_points());
        }
        Command::Fork { message_id, reply } => {
            let result = state.fork_at(message_id).await.map(|forked| {
                forked.map(|(state, data)| Fork {
                    thread: Thread::spawn(state),
                    messages: data.messages,
                    model: data.model,
                    protocol: data.protocol,
                    working_dir: data.working_dir,
                    prompt: data.prompt,
                })
            });
            let _ = reply.send(result);
        }
        // `Command::route` only hands Deferred commands to this handler, so
        // any other variant here is a routing bug, not a user error.
        _ => unreachable!("idle dispatch only hands Deferred commands here"),
    }
}

fn publish(
    events: &broadcast::Sender<Event>,
    thread_id: ThreadId,
    turn_id: Option<TurnId>,
    sequence: &mut u64,
    kind: EventKind,
) {
    *sequence = sequence.saturating_add(1);
    let _ = events.send(Event {
        thread_id,
        turn_id,
        sequence: *sequence,
        timestamp: chrono::Utc::now().to_rfc3339(),
        kind,
    });
}

fn thread_closed() -> ash_core::AshError {
    ash_core::AshError::Config("thread runtime has stopped".to_string())
}

fn ensure_nonempty(input: &Input) -> Result<(), ash_core::AshError> {
    if input.is_empty() {
        return Err(ash_core::AshError::Config(EMPTY_INPUT_ERROR.to_string()));
    }
    Ok(())
}

fn queued_turn(
    input: Input,
    completion: Option<oneshot::Sender<Result<StopReason, ash_core::AshError>>>,
) -> QueuedTurn {
    QueuedTurn {
        id: TurnId::new(),
        inputs: vec![input],
        cancellation: CancellationToken::new(),
        completion,
    }
}

pub struct Fork {
    pub thread: Thread,
    pub messages: Vec<Message>,
    pub model: String,
    pub protocol: String,
    pub working_dir: PathBuf,
    pub prompt: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextCompaction {
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub dropped_messages: usize,
}

pub struct ThreadState {
    id: ThreadId,
    config: RunConfig,
    runtime: Runtime,
    log: ThreadLog,
    metadata: ThreadMetadata,
    store: SharedThreadStore,
    /// Open append-only handle to this thread's file. Initialized lazily so
    /// `ThreadState::new` stays synchronous; every write goes through it
    /// without re-scanning or re-reading the file.
    writer: Option<Arc<tokio::sync::Mutex<Box<dyn ThreadAppender>>>>,
}

impl ThreadState {
    pub(crate) fn new(config: RunConfig, runtime: Runtime) -> Self {
        let id = ThreadId::new();
        let metadata = thread_metadata(&config, id);
        let store = runtime.thread_store_handle();
        Self {
            id,
            config,
            runtime,
            log: ThreadLog::new(),
            metadata,
            store,
            writer: None,
        }
    }

    pub fn id(&self) -> ThreadId {
        self.id
    }

    pub fn messages(&self) -> Vec<Message> {
        self.log.messages()
    }

    pub fn view(&self) -> ThreadView {
        let mut view = self.log.view();
        let tools = self
            .config
            .tools
            .iter()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        view.context_tokens = u64::try_from(estimate_request_tokens(
            self.config.system_prompt.as_deref(),
            &view.context,
            &tools,
        ))
        .ok();
        view
    }

    pub(crate) async fn resume(&mut self, thread_id: ThreadId) -> Result<bool, ash_core::AshError> {
        let Some(opened) = self.store.open(thread_id).await? else {
            return Ok(false);
        };
        if opened.thread.metadata.kind == crate::ThreadKind::Subagent {
            return Ok(false);
        }
        self.restore(opened);
        Ok(true)
    }

    pub(crate) async fn seed(&mut self, messages: Vec<Message>) -> Result<(), ash_core::AshError> {
        let entries = messages
            .into_iter()
            .map(LogEntry::Message)
            .collect::<Vec<_>>();
        self.append(&entries).await
    }

    pub fn fork_points(&self) -> Vec<ForkPoint> {
        self.log
            .messages()
            .iter()
            .rev()
            .filter_map(|message| {
                message.user_turn_text().map(|prompt| ForkPoint {
                    message_id: message.id,
                    prompt,
                })
            })
            .collect()
    }

    async fn fork_at(
        &mut self,
        message_id: MessageId,
    ) -> Result<Option<(ThreadState, ForkData)>, ash_core::AshError> {
        let Some((turn_start, prompt)) =
            self.log
                .messages()
                .iter()
                .enumerate()
                .find_map(|(index, message)| {
                    (message.id == message_id)
                        .then(|| message.user_turn_text().map(|prompt| (index, prompt)))?
                })
        else {
            return Ok(None);
        };

        let messages = self.log.messages()[..turn_start].to_vec();
        let config = self.config.clone();
        let runtime = self.runtime.clone();
        let model = config.model.as_str().to_string();
        let protocol = runtime.model_backend().to_string();
        let working_dir = config.working_dir.clone();

        let mut state = ThreadState::new(config, runtime);
        state.seed(messages.clone()).await?;

        let details = ForkData {
            messages,
            model,
            protocol,
            working_dir,
            prompt,
        };
        Ok(Some((state, details)))
    }

    fn restore(&mut self, opened: OpenedThread) {
        self.id = opened.thread.metadata.thread_id;
        self.metadata = opened.thread.metadata;
        self.log = opened.thread.log;
        self.writer = Some(Arc::new(tokio::sync::Mutex::new(opened.writer)));
    }

    async fn submit_inputs(
        &mut self,
        turn_id: TurnId,
        inputs: Vec<Input>,
        steering: mpsc::UnboundedReceiver<Input>,
        events: mpsc::Sender<EventKind>,
        cancel: CancellationToken,
    ) -> Result<TurnView, ash_core::AshError> {
        if inputs.is_empty() {
            return Err(ash_core::AshError::Config(EMPTY_INPUT_ERROR.to_string()));
        }
        let turn_inputs = inputs.clone();
        let mut keys = HashSet::new();
        for input in &inputs {
            ensure_nonempty(input)?;
            if let Some(key) = input.idempotency_key.as_deref() {
                if self.log.contains_idempotency_key(key) || !keys.insert(key.to_string()) {
                    return Err(ash_core::AshError::Config(format!(
                        "duplicate agent input idempotency key: {key}"
                    )));
                }
            }
        }
        let mut accepted = Vec::with_capacity(inputs.len() + 1);
        accepted.push(LogEntry::TurnStart(turn_id));
        for input in inputs {
            let message = Message::user_content(input.content.clone());
            accepted.push(LogEntry::Input(AcceptedInput {
                turn_id,
                input,
                message,
            }));
        }
        self.append(&accepted).await?;
        let turn_context = TurnContext {
            thread_id: self.id,
            turn_id,
            inputs: turn_inputs,
            messages: self.log.messages(),
            metadata: self.config.metadata.clone(),
        };
        let patch = match self.runtime.prepare_turn(&turn_context).await {
            Ok(patch) => patch,
            Err(error) => {
                let _ = self
                    .append(&[LogEntry::TurnEnd {
                        id: turn_id,
                        result: TurnResult::Failed(error.to_string()),
                        usage: None,
                    }])
                    .await;
                return Err(error);
            }
        };
        let mut model_context = self.log.model_context();
        let mut turn_config = self.config.clone();
        turn_config.tools.extend(patch.tools);
        let writer = self.writer().await?;
        let mut persistence = crate::store::ThreadPersistence::new(writer);
        let execution = TurnExecution::new(
            self.id,
            turn_id,
            events.clone(),
            cancel,
            steering,
            patch.context,
        );
        let engine_result = run_agent_turn_persisted(
            self.runtime.model(),
            &turn_config,
            &mut model_context,
            execution,
            &mut persistence,
        )
        .await;
        let (turn_result, usage) = match &engine_result {
            Ok((reason, usage)) => (TurnResult::Completed(reason.clone()), *usage),
            Err(error) => (TurnResult::Failed(error.to_string()), None),
        };
        let tools = self
            .config
            .tools
            .iter()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        let context_tokens = u64::try_from(estimate_request_tokens(
            self.config.system_prompt.as_deref(),
            &model_context,
            &tools,
        ))
        .ok();
        // Project the turn's terminal entry to compute the view the extension
        // observes, without mutating the committed log.
        let mut view = {
            let mut projected = self.log.clone();
            for entry in persistence.pending() {
                projected.push(entry.clone());
            }
            projected.push(LogEntry::TurnEnd {
                id: turn_id,
                result: turn_result,
                usage,
            });
            let mut view = projected
                .turn_view(turn_id)
                .expect("a projected turn end must produce its turn view");
            view.context_tokens = context_tokens;
            view
        };
        let extension_error = self.runtime.complete_turn(&turn_context, &view).await.err();
        // An extension failure only replaces a successful engine result; it
        // must not mask an engine error.
        if let (true, Some(error)) = (engine_result.is_ok(), &extension_error) {
            view.result = TurnResult::Failed(error.to_string());
        }
        // Commit point: buffer the turn's terminal entry, then write all
        // buffered messages plus the turn end in one write+flush. On crash
        // before this point the turn has no `TurnEnd`, so the projection
        // marks it `Interrupted` and its partial output is never exposed.
        persistence.stage(&[LogEntry::TurnEnd {
            id: turn_id,
            result: view.result.clone(),
            usage: view.usage,
        }]);
        let appended = match persistence.commit().await {
            Ok(appended) => appended,
            // A commit failure is only fatal when nothing else already failed.
            Err(error) if engine_result.is_ok() && extension_error.is_none() => {
                return Err(error);
            }
            Err(_) => Vec::new(),
        };
        // Replay what this turn appended into the in-memory log instead of
        // reloading the thread from disk (which scans the whole directory and
        // visibly delays the turn-completed event after the last delta).
        drop(persistence);
        for entry in appended {
            self.log.push(entry);
        }
        if let Some(error) = engine_result.err().or(extension_error) {
            return Err(error);
        }
        let mut committed = self
            .log
            .turn_view(turn_id)
            .expect("a committed turn end must produce its turn view");
        committed.context_tokens = context_tokens;
        Ok(committed)
    }

    pub async fn rollback_last_turn(&mut self) -> Result<Option<String>, ash_core::AshError> {
        let messages = self.log.messages();
        let Some((turn_start, prompt)) = last_user_turn(&messages) else {
            return Ok(None);
        };
        self.append(&[LogEntry::Rollback]).await?;
        debug_assert_eq!(self.log.messages().len(), turn_start);
        Ok(Some(prompt))
    }

    pub async fn compact(&mut self) -> Result<ContextCompaction, ash_core::AshError> {
        let model = self.runtime.model_client();
        self.compact_using(model.as_ref(), &CancellationToken::new())
            .await
    }

    async fn compact_using(
        &mut self,
        model: &dyn ash_core::ModelClient,
        cancel: &CancellationToken,
    ) -> Result<ContextCompaction, ash_core::AshError> {
        let tools = self
            .config
            .tools
            .iter()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        let model_context = self.log.model_context();
        let before_tokens =
            estimate_request_tokens(self.config.system_prompt.as_deref(), &model_context, &tools);
        let Some(compacted) =
            compact_with_adapter(&self.config, &model_context, model, cancel).await?
        else {
            return Ok(ContextCompaction {
                before_tokens,
                after_tokens: before_tokens,
                dropped_messages: 0,
            });
        };
        let checkpoint = ContextCheckpoint::from_model_context(&compacted.messages)?;
        self.append(&[LogEntry::Checkpoint(checkpoint)]).await?;
        Ok(ContextCompaction {
            before_tokens: compacted.before_tokens,
            after_tokens: compacted.after_tokens,
            dropped_messages: compacted.dropped_messages,
        })
    }

    async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError> {
        if entries.is_empty() {
            return Ok(());
        }
        let writer = self.writer().await?;
        writer.lock().await.append(entries).await?;
        for entry in entries {
            self.log.push(entry.clone());
        }
        Ok(())
    }

    async fn writer(
        &mut self,
    ) -> Result<Arc<tokio::sync::Mutex<Box<dyn ThreadAppender>>>, ash_core::AshError> {
        if let Some(writer) = &self.writer {
            return Ok(Arc::clone(writer));
        }
        let writer = Arc::new(tokio::sync::Mutex::new(
            self.store.open_writer(self.metadata).await?,
        ));
        self.writer = Some(Arc::clone(&writer));
        Ok(writer)
    }
}

struct ForkData {
    messages: Vec<Message>,
    model: String,
    protocol: String,
    working_dir: PathBuf,
    prompt: String,
}

fn thread_metadata(config: &RunConfig, thread_id: ThreadId) -> ThreadMetadata {
    ThreadMetadata {
        thread_id,
        kind: config.kind,
    }
}

fn last_user_turn(messages: &[Message]) -> Option<(usize, String)> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| message.user_turn_text().map(|prompt| (index, prompt)))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    use super::*;
    use crate::agent::RetryBackoff;
    use ash_core::{
        Content, ContentBlock, MessageContent, MessageId, ModelClient, ModelEvent, ModelId,
        ModelRequest, ModelStream, Role, ToolCallId,
    };
    use futures::StreamExt;
    use tempfile::TempDir;

    struct MockAdapter {
        responses: Mutex<VecDeque<Vec<ModelEvent>>>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    struct DelayedAdapter {
        calls: AtomicUsize,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl ModelClient for DelayedAdapter {
        fn stream(&self, request: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            self.requests.lock().unwrap().push(request);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Ok(Box::pin(futures::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(ModelEvent::Stop(StopReason::EndTurn))
                })))
            } else {
                Ok(Box::pin(futures::stream::iter([
                    Ok(ModelEvent::Text("done".to_string())),
                    Ok(ModelEvent::Stop(StopReason::EndTurn)),
                ])))
            }
        }
    }

    struct GoalExtension {
        completed: Arc<AtomicUsize>,
    }

    struct FailingPrepareExtension;

    #[async_trait::async_trait]
    impl crate::Extension for FailingPrepareExtension {
        async fn prepare(
            &self,
            _turn: &TurnContext,
        ) -> Result<crate::TurnPatch, ash_core::AshError> {
            Err(ash_core::AshError::Config("prepare failed".to_string()))
        }
    }

    #[async_trait::async_trait]
    impl crate::Extension for GoalExtension {
        async fn prepare(
            &self,
            turn: &TurnContext,
        ) -> Result<crate::TurnPatch, ash_core::AshError> {
            assert_eq!(turn.metadata.get("goal"), Some(&serde_json::json!("ship")));
            Ok(crate::TurnPatch {
                context: vec![Message::system("active goal: ship")],
                tools: Vec::new(),
            })
        }

        async fn complete(
            &self,
            _turn: &TurnContext,
            view: &ash_core::TurnView,
        ) -> Result<(), ash_core::AshError> {
            assert!(matches!(
                &view.result,
                ash_core::TurnResult::Completed(StopReason::EndTurn)
            ));
            assert!(view
                .messages
                .iter()
                .any(|message| matches!(message.content, MessageContent::Assistant(_))));
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl ModelClient for MockAdapter {
        fn stream(&self, request: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            self.requests.lock().unwrap().push(request);
            let items = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(items.into_iter().map(Ok))))
        }
    }

    fn config(working_dir: PathBuf) -> RunConfig {
        RunConfig {
            system_prompt: Some("current prompt".to_string()),
            tools: Vec::new(),
            model: ModelId::new("current-model"),
            max_turns: 10,
            working_dir,
            max_context_tokens: 1000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(5),
            agent_path: "/root".to_string(),
            tree_id: None,
            kind: crate::ThreadKind::Root,
            metadata: serde_json::Map::new(),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        }
    }

    fn runtime() -> Runtime {
        Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::new()),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            "test",
        )
    }

    fn runtime_in(directory: &std::path::Path) -> Runtime {
        runtime().with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory)))
    }

    async fn thread_with_messages(
        config: RunConfig,
        runtime: Runtime,
        messages: &[Message],
    ) -> ThreadState {
        let mut state = ThreadState::new(config, runtime);
        state.seed(messages.to_vec()).await.unwrap();
        state
    }

    #[test]
    fn finds_the_latest_real_user_turn_after_tool_results() {
        let tool_id = ToolCallId::from_provider("call");
        let messages = vec![
            Message::user("first"),
            Message::assistant_text("first answer"),
            Message::user("second"),
            Message {
                id: MessageId::new(),
                role: Role::Assistant,
                content: MessageContent::Assistant(vec![ContentBlock::ToolCall {
                    id: tool_id.clone(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                }]),
            },
            Message {
                id: MessageId::new(),
                role: Role::User,
                content: MessageContent::ToolResult {
                    id: tool_id,
                    result: Ok("done".to_string()),
                    attachments: Vec::new(),
                },
            },
        ];

        let (turn_start, prompt) = last_user_turn(&messages).unwrap();
        assert_eq!(turn_start, 2);
        assert_eq!(prompt, "second");
    }

    #[test]
    fn fork_points_list_real_user_prompts_newest_first() {
        let tool_id = ToolCallId::from_provider("call");
        let first = Message::user("first");
        let second = Message::user("second\nline");
        let mut state = ThreadState::new(config(PathBuf::from(".")), runtime());
        state.log = ThreadLog::from_messages(vec![
            first.clone(),
            Message::assistant_text("answer"),
            Message {
                id: MessageId::new(),
                role: Role::User,
                content: MessageContent::ToolResult {
                    id: tool_id,
                    result: Ok("result".to_string()),
                    attachments: Vec::new(),
                },
            },
            second.clone(),
        ]);

        assert_eq!(
            state.fork_points(),
            vec![
                ForkPoint {
                    message_id: second.id,
                    prompt: "second\nline".to_string(),
                },
                ForkPoint {
                    message_id: first.id,
                    prompt: "first".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn fork_creates_a_new_thread_before_the_selected_prompt() {
        let directory = TempDir::new().unwrap();
        let config = config(directory.path().to_path_buf());
        let runtime = runtime_in(directory.path());
        let first = Message::user("first");
        let answer = Message::assistant_text("first answer");
        let expected_ids = [first.id, answer.id];
        let selected = Message::user("try another direction");
        let later = Message::assistant_text("second answer");
        let messages = vec![first.clone(), answer.clone(), selected.clone(), later];
        let mut state = thread_with_messages(config, runtime.clone(), &messages).await;
        let original_id = state.id();

        let (fork_state, forked) = state.fork_at(selected.id).await.unwrap().unwrap();

        assert_eq!(state.id(), original_id);
        assert_ne!(fork_state.id(), original_id);
        assert_eq!(fork_state.messages().len(), 2);
        assert_eq!(
            fork_state
                .messages()
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            expected_ids
        );
        assert_eq!(
            forked
                .messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            expected_ids
        );
        assert_eq!(forked.prompt, "try another direction");
        let original = runtime
            .thread_store_handle()
            .load(original_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.log.messages().len(), 4);
        let stored_fork = fork_state
            .store
            .load(fork_state.id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_fork
                .log
                .messages()
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            expected_ids
        );
    }

    #[tokio::test]
    async fn resume_keeps_the_current_runtime_configuration() {
        let directory = TempDir::new().unwrap();
        let current_dir = directory.path().join("current");
        tokio::fs::create_dir(&current_dir).await.unwrap();
        let runtime = runtime_in(directory.path());
        let mut state = ThreadState::new(config(current_dir.clone()), runtime.clone());
        let saved_id = ThreadId::new();
        let saved_message = Message::user("saved question");
        runtime
            .thread_store_handle()
            .create(
                thread_metadata(&config(directory.path().join("old")), saved_id),
                &[LogEntry::Message(saved_message)],
            )
            .await
            .unwrap();

        assert!(state.resume(saved_id).await.unwrap());

        assert_eq!(state.id(), saved_id);
        assert_eq!(state.config.model.as_str(), "current-model");
        assert_eq!(state.runtime.model_backend(), "test");
        assert_eq!(
            state.config.system_prompt.as_deref(),
            Some("current prompt")
        );
        assert_eq!(state.config.working_dir, current_dir);
        assert_eq!(state.messages().len(), 1);
    }

    #[tokio::test]
    async fn rollback_keeps_memory_when_persistence_fails() {
        let directory = TempDir::new().unwrap();
        let mut state = ThreadState::new(config(directory.path().to_path_buf()), runtime());
        let message = Message::user("unpersisted");
        state.log.push(LogEntry::Message(message));

        let result = state.rollback_last_turn().await;

        assert!(result.is_err());
        assert_eq!(state.log.messages().len(), 1);
        assert_eq!(state.log.model_context().len(), 1);
    }

    #[tokio::test]
    async fn submit_keeps_memory_clean_when_persistence_fails() {
        let directory = TempDir::new().unwrap();
        let blocked_parent = directory.path().join("not-a-directory");
        tokio::fs::write(&blocked_parent, b"file").await.unwrap();
        let config = config(directory.path().to_path_buf());
        let runtime = runtime_in(&blocked_parent);
        let mut state = ThreadState::new(config, runtime);
        let (events, mut received) = mpsc::channel(4);
        let (_, steering) = mpsc::unbounded_channel();

        let result = state
            .submit_inputs(
                TurnId::new(),
                vec![Input::user("unpersisted")],
                steering,
                events,
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_err());
        assert!(state.log.messages().is_empty());
        assert!(state.log.model_context().is_empty());
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn manual_compaction_preserves_full_history_and_updates_model_context() {
        let directory = TempDir::new().unwrap();
        let config = config(directory.path().to_path_buf());
        let runtime = runtime_in(directory.path());
        let messages = vec![
            Message::user(&format!("old request {}", "x".repeat(10_000))),
            Message::assistant_text("old answer"),
            Message::user("middle request"),
            Message::assistant_text("middle answer"),
            Message::user("recent request"),
            Message::assistant_text("recent answer"),
        ];
        let mut state = thread_with_messages(config, runtime, &messages).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                ModelEvent::Text("condensed facts".to_string()),
                ModelEvent::Stop(StopReason::EndTurn),
            ]])),
            requests: requests.clone(),
        };

        let result = state
            .compact_using(&adapter, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.dropped_messages, 2);
        assert_eq!(state.log.messages().len(), 6);
        assert_eq!(state.log.model_context().len(), 5);
        assert!(result.after_tokens < result.before_tokens);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].tools.is_empty());
        }
        let stored = state.store.load(state.id()).await.unwrap().unwrap();
        assert_eq!(stored.log.messages().len(), 6);
        assert_eq!(stored.log.model_context().len(), 5);
        assert!(matches!(
            &stored.log.messages()[0].content,
            MessageContent::User(contents)
                if matches!(contents.as_slice(), [Content::Text(text)] if text.starts_with("old request"))
        ));
    }

    #[tokio::test]
    async fn thread_serializes_submitted_turns_in_acceptance_order() {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            Arc::new(DelayedAdapter {
                calls: AtomicUsize::new(0),
                requests: requests.clone(),
            }),
            "test",
        )
        .with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory.path())));
        let thread = Thread::spawn(ThreadState::new(config(directory.path().into()), runtime));

        let first = thread.submit("first").await.unwrap();
        let second = thread.submit("second").await.unwrap();
        first.wait().await.unwrap();
        second.wait().await.unwrap();

        let messages = thread.messages().await.unwrap();
        let prompts = messages
            .iter()
            .filter_map(Message::user_turn_text)
            .collect::<Vec<_>>();
        assert_eq!(prompts, ["first", "second"]);
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn public_input_apis_share_one_empty_input_error() {
        let directory = TempDir::new().unwrap();
        let runtime = Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::from([vec![ModelEvent::Stop(
                    StopReason::EndTurn,
                )]])),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            "test",
        )
        .with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory.path())));
        let thread = Thread::spawn(ThreadState::new(config(directory.path().into()), runtime));

        let submit_error = thread.submit("").await.err().unwrap();
        let enqueue_error = thread.enqueue("").await.unwrap_err();
        let notify_error = thread.notify("").await.unwrap_err();
        let turn = thread.submit("work").await.unwrap();
        let steer_error = turn.steer("").await.unwrap_err();

        for error in [submit_error, enqueue_error, notify_error, steer_error] {
            assert!(
                matches!(error, ash_core::AshError::Config(message) if message == EMPTY_INPUT_ERROR)
            );
        }
        turn.wait().await.unwrap();
    }

    #[tokio::test]
    async fn emitted_turn_messages_match_the_durable_projection() {
        let directory = TempDir::new().unwrap();
        let runtime = Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::from([vec![
                    ModelEvent::Text("done".to_string()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ]])),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            "test",
        )
        .with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory.path())));
        let thread = Thread::spawn(ThreadState::new(config(directory.path().into()), runtime));
        let mut events = thread.events();

        thread.submit("work").await.unwrap().wait().await.unwrap();
        let emitted = loop {
            let event = events.next().await.unwrap().unwrap();
            if let EventKind::Turn(view) = event.kind {
                break view;
            }
        };
        let projected = thread.view().await.unwrap().turns.pop().unwrap();

        assert_eq!(emitted.id, projected.id);
        assert_eq!(emitted.messages, projected.messages);
        assert_eq!(emitted.result, projected.result);
        assert_eq!(emitted.usage, projected.usage);
    }

    #[tokio::test]
    async fn notify_joins_the_next_turn_without_starting_one() {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::from([vec![ModelEvent::Stop(
                    StopReason::EndTurn,
                )]])),
                requests,
            }),
            "test",
        )
        .with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory.path())));
        let thread = Thread::spawn(ThreadState::new(config(directory.path().into()), runtime));

        thread
            .notify(Input::from_text(crate::InputSource::Agent, "note"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(thread.messages().await.unwrap().is_empty());
        thread.submit("task").await.unwrap().wait().await.unwrap();

        let prompts = thread
            .messages()
            .await
            .unwrap()
            .iter()
            .filter_map(Message::user_turn_text)
            .collect::<Vec<_>>();
        assert_eq!(prompts, ["note", "task"]);
    }

    #[tokio::test]
    async fn enqueue_creates_a_trackable_fire_and_forget_turn() {
        let directory = TempDir::new().unwrap();
        let runtime = Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::from([vec![ModelEvent::Stop(
                    StopReason::EndTurn,
                )]])),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            "test",
        )
        .with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory.path())));
        let thread = Thread::spawn(ThreadState::new(config(directory.path().into()), runtime));
        let mut events = thread.events();

        let turn_id = thread
            .enqueue(Input::from_text(crate::InputSource::Heartbeat, "check"))
            .await
            .unwrap();
        let completed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.next().await.unwrap().unwrap();
                if matches!(event.kind, EventKind::Turn(_)) {
                    break event;
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(completed.turn_id, Some(turn_id));
    }

    #[tokio::test]
    async fn steer_joins_the_active_turn_before_its_next_model_call() {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            Arc::new(DelayedAdapter {
                calls: AtomicUsize::new(0),
                requests: requests.clone(),
            }),
            "test",
        )
        .with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory.path())));
        let thread = Thread::spawn(ThreadState::new(config(directory.path().into()), runtime));
        let turn = thread.submit("first").await.unwrap();

        tokio::time::sleep(Duration::from_millis(5)).await;
        turn.steer("updated direction").await.unwrap();
        turn.wait().await.unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1]
            .messages
            .iter()
            .any(|message| message.user_turn_text().as_deref() == Some("updated direction")));
    }

    #[tokio::test]
    async fn duplicate_keys_in_one_turn_are_rejected_before_persistence() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime_in(directory.path());
        let mut state = ThreadState::new(config(directory.path().into()), runtime);
        let mut first = Input::user("first");
        first.idempotency_key = Some("same".to_string());
        let mut second = Input::user("second");
        second.idempotency_key = Some("same".to_string());
        let (_, steering) = mpsc::unbounded_channel();
        let (events, _) = mpsc::channel(4);

        let error = state
            .submit_inputs(
                TurnId::new(),
                vec![first, second],
                steering,
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("duplicate agent input"));
        assert!(state.log.entries().is_empty());
        assert!(state.store.load(state.id()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn runtime_extensions_add_turn_context_and_observe_completion() {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let completed = Arc::new(AtomicUsize::new(0));
        let runtime = Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::from([vec![
                    ModelEvent::Text("done".to_string()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ]])),
                requests: requests.clone(),
            }),
            "test",
        )
        .with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory.path())))
        .with_extension(Arc::new(GoalExtension {
            completed: completed.clone(),
        }));
        let mut run_config = config(directory.path().into());
        run_config
            .metadata
            .insert("goal".to_string(), serde_json::json!("ship"));
        let thread = Thread::spawn(ThreadState::new(run_config, runtime));

        thread.submit("work").await.unwrap().wait().await.unwrap();

        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert!(requests.lock().unwrap()[0]
            .messages
            .iter()
            .any(|message| message.role == Role::System));
    }

    #[tokio::test]
    async fn failed_turn_that_wrote_nothing_emits_its_own_failed_view() {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::from([vec![ModelEvent::Stop(
                    StopReason::EndTurn,
                )]])),
                requests: requests.clone(),
            }),
            "test",
        )
        .with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory.path())))
        .with_extension(Arc::new(FailingPrepareExtension));
        let thread = Thread::spawn(ThreadState::new(config(directory.path().into()), runtime));
        let mut events = thread.events();

        // `submit_inputs` persists `TurnStart` + the input, then `prepare_turn`
        // fails. The turn settles with its own `TurnEnd(Failed)`; the emitted
        // view must be this turn's failure, not a previous turn's stale view.
        thread
            .submit("work")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap_err();

        // Drain events for a bounded time; `events.next()` would block forever
        // after the turn settles because no further events are emitted.
        let mut saw_turn = false;
        while let Ok(Some(Ok(event))) =
            tokio::time::timeout(Duration::from_millis(200), events.next()).await
        {
            if let EventKind::Turn(view) = event.kind {
                saw_turn = true;
                assert!(matches!(view.result, TurnResult::Failed(_)));
                assert!(!view.messages.is_empty());
            }
        }
        assert!(saw_turn);
    }

    #[tokio::test]
    async fn extension_observes_the_full_turn_message_set() {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let completed = Arc::new(AtomicUsize::new(0));
        let runtime = Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::from([vec![
                    ModelEvent::Text("done".to_string()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ]])),
                requests: requests.clone(),
            }),
            "test",
        )
        .with_thread_store(Arc::new(crate::JsonlThreadStore::new(directory.path())))
        .with_extension(Arc::new(GoalExtension {
            completed: completed.clone(),
        }));
        let mut run_config = config(directory.path().into());
        run_config
            .metadata
            .insert("goal".to_string(), serde_json::json!("ship"));
        let thread = Thread::spawn(ThreadState::new(run_config, runtime));

        thread.submit("work").await.unwrap().wait().await.unwrap();

        // The extension sees the accepted user input plus the assistant reply.
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }
}
