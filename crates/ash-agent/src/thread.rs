use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
};

use ash_core::{
    CancellationToken, Content, EventKind, ForkPoint, Message, MessageContent, MessageId,
    StopReason, ThreadId, TurnId,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::BroadcastStream;

use crate::context::estimate_request_tokens;
use crate::engine::{compact_with_adapter, run_agent_turn_persisted, TurnExecution};
use crate::{
    runtime::Event, AcceptedInput, ContextCheckpoint, Input, Record, RunConfig, Runtime,
    SharedThreadStore, ThreadLog, ThreadMetadata, TurnContext, TurnOutcome, TurnStatus, Version,
};

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
    ForkPoints(oneshot::Sender<Vec<ForkPoint>>),
    Fork {
        message_id: MessageId,
        reply: oneshot::Sender<Result<Option<Fork>, ash_core::AshError>>,
    },
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
        if input.is_empty() {
            return Err(ash_core::AshError::Config(
                "thread input cannot be empty".to_string(),
            ));
        }
        let id = TurnId::new();
        let cancellation = CancellationToken::new();
        let (completion_tx, completion) = oneshot::channel();
        self.commands
            .send(Command::Submit(QueuedTurn {
                id,
                inputs: vec![input],
                cancellation: cancellation.clone(),
                completion: Some(completion_tx),
            }))
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
        if input.is_empty() {
            return Err(ash_core::AshError::Config(
                "thread input cannot be empty".to_string(),
            ));
        }
        let id = TurnId::new();
        self.commands
            .send(Command::Submit(QueuedTurn {
                id,
                inputs: vec![input],
                cancellation: CancellationToken::new(),
                completion: None,
            }))
            .await
            .map_err(|_| thread_closed())?;
        Ok(id)
    }

    /// Attach input to the next submitted turn without starting work by itself.
    pub async fn notify(&self, input: impl Into<Input>) -> Result<(), ash_core::AshError> {
        let input = input.into();
        if input.is_empty() {
            return Err(ash_core::AshError::Config(
                "thread input cannot be empty".to_string(),
            ));
        }
        self.commands
            .send(Command::Notify(input))
            .await
            .map_err(|_| thread_closed())
    }

    pub async fn messages(&self) -> Result<Vec<Message>, ash_core::AshError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Messages(reply))
            .await
            .map_err(|_| thread_closed())?;
        result.await.map_err(|_| thread_closed())
    }

    pub async fn fork_points(&self) -> Result<Vec<ForkPoint>, ash_core::AshError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::ForkPoints(reply))
            .await
            .map_err(|_| thread_closed())?;
        result.await.map_err(|_| thread_closed())
    }

    pub async fn rollback(&self) -> Result<Option<String>, ash_core::AshError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Rollback(reply))
            .await
            .map_err(|_| thread_closed())?;
        result.await.map_err(|_| thread_closed())?
    }

    pub async fn fork_at(&self, message_id: MessageId) -> Result<Option<Fork>, ash_core::AshError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Fork { message_id, reply })
            .await
            .map_err(|_| thread_closed())?;
        result.await.map_err(|_| thread_closed())?
    }

    pub async fn compact(&self) -> Result<ContextCompaction, ash_core::AshError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Compact(reply))
            .await
            .map_err(|_| thread_closed())?;
        result.await.map_err(|_| thread_closed())?
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
        if input.is_empty() {
            return Err(ash_core::AshError::Config(
                "steering input cannot be empty".to_string(),
            ));
        }
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
            handle_idle_command(&mut state, command, &mut queues).await;
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
        handle_idle_command(&mut state, command, &mut queues).await;
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
    publish(
        events,
        thread_id,
        Some(id),
        sequence,
        EventKind::TurnStarted,
    );
    let (payload_tx, mut payload_rx) = mpsc::channel(64);
    let (steer_tx, steer_rx) = mpsc::unbounded_channel();
    let execution = state.submit_inputs(id, inputs, steer_rx, payload_tx, cancellation.clone());
    tokio::pin!(execution);

    let result = loop {
        tokio::select! {
            result = &mut execution => break result,
            payload = payload_rx.recv() => {
                if let Some(kind) = payload {
                    publish(events, thread_id, Some(id), sequence, kind);
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    cancellation.cancel();
                    continue;
                };
                handle_active_command(command, id, queues, &steer_tx);
            }
        }
    };
    while let Ok(kind) = payload_rx.try_recv() {
        publish(events, thread_id, Some(id), sequence, kind);
    }
    if let Err(error) = &result {
        publish(
            events,
            thread_id,
            Some(id),
            sequence,
            EventKind::Error(error.to_string()),
        );
    }
    publish(
        events,
        thread_id,
        Some(id),
        sequence,
        EventKind::TurnCompleted {
            reason: result.as_ref().cloned().unwrap_or(StopReason::Aborted),
        },
    );
    if let Some(completion) = completion {
        let _ = completion.send(result);
    }
}

async fn handle_idle_command(state: &mut ThreadState, command: Command, queues: &mut ActorQueues) {
    match command {
        Command::Submit(turn) => queues.turns.push_back(turn),
        Command::Notify(input) => queues.inbox.push(input),
        Command::Steer { reply, .. } => {
            let _ = reply.send(Err(ash_core::AshError::Config(
                "the target turn is no longer active".to_string(),
            )));
        }
        Command::Rollback(reply) => {
            let _ = reply.send(state.rollback_last_turn().await);
        }
        Command::Compact(reply) => {
            let _ = reply.send(state.compact().await);
        }
        Command::Messages(reply) => {
            let _ = reply.send(state.messages());
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
    }
}

fn handle_active_command(
    command: Command,
    active: TurnId,
    queues: &mut ActorQueues,
    steer: &mpsc::UnboundedSender<Input>,
) {
    match command {
        Command::Submit(turn) => queues.turns.push_back(turn),
        Command::Notify(input) => queues.inbox.push(input),
        Command::Steer {
            turn_id,
            input,
            reply,
        } if turn_id == active => {
            let result = steer.send(input).map_err(|_| thread_closed());
            let _ = reply.send(result);
        }
        Command::Steer { reply, .. } => {
            let _ = reply.send(Err(ash_core::AshError::Config(
                "the target turn is not active".to_string(),
            )));
        }
        Command::Rollback(reply) => {
            queues.commands.push_back(Command::Rollback(reply));
        }
        Command::Compact(reply) => {
            queues.commands.push_back(Command::Compact(reply));
        }
        Command::Messages(reply) => {
            queues.commands.push_back(Command::Messages(reply));
        }
        Command::ForkPoints(reply) => {
            queues.commands.push_back(Command::ForkPoints(reply));
        }
        command @ Command::Fork { .. } => {
            queues.commands.push_back(command);
        }
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
    version: Version,
    persisted: bool,
}

impl ThreadState {
    pub(crate) fn new(config: RunConfig, runtime: Runtime) -> Self {
        let id = ThreadId::new();
        let metadata = thread_metadata(&config, &runtime, id);
        let store = runtime.thread_store_handle();
        Self {
            id,
            config,
            runtime,
            log: ThreadLog::new(),
            metadata,
            store,
            version: Version::initial(),
            persisted: false,
        }
    }

    pub fn id(&self) -> ThreadId {
        self.id
    }

    pub fn messages(&self) -> Vec<Message> {
        self.log.messages()
    }

    pub(crate) async fn resume(&mut self, thread_id: ThreadId) -> Result<bool, ash_core::AshError> {
        let Some(stored) = self.store.load(thread_id).await? else {
            return Ok(false);
        };
        self.restore(stored);
        Ok(true)
    }

    pub(crate) async fn seed(&mut self, messages: Vec<Message>) -> Result<(), ash_core::AshError> {
        let records = messages
            .into_iter()
            .map(Record::Message)
            .collect::<Vec<_>>();
        self.append(&records).await
    }

    pub fn fork_points(&self) -> Vec<ForkPoint> {
        self.log
            .messages()
            .iter()
            .rev()
            .filter_map(|message| {
                user_prompt(message).map(|prompt| ForkPoint {
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
                        .then(|| user_prompt(message).map(|prompt| (index, prompt)))
                        .flatten()
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

    fn restore(&mut self, stored: crate::StoredThread) {
        self.id = stored.metadata.thread_id;
        self.metadata.thread_id = self.id;
        self.log = stored.log;
        self.version = stored.version;
        self.persisted = true;
    }

    async fn submit_inputs(
        &mut self,
        turn_id: TurnId,
        inputs: Vec<Input>,
        steering: mpsc::UnboundedReceiver<Input>,
        events: mpsc::Sender<EventKind>,
        cancel: CancellationToken,
    ) -> Result<StopReason, ash_core::AshError> {
        let turn_inputs = inputs.clone();
        let mut keys = HashSet::new();
        for input in &inputs {
            if input.is_empty() {
                return Err(ash_core::AshError::Config(
                    "agent input content cannot be empty".to_string(),
                ));
            }
            if let Some(key) = input.idempotency_key.as_deref() {
                if self.log.contains_idempotency_key(key) || !keys.insert(key.to_string()) {
                    return Err(ash_core::AshError::Config(format!(
                        "duplicate agent input idempotency key: {key}"
                    )));
                }
            }
        }
        if inputs.is_empty() {
            return Err(ash_core::AshError::Config(
                "a turn requires at least one input".to_string(),
            ));
        }
        let mut accepted = Vec::with_capacity(inputs.len() + 1);
        accepted.push(Record::TurnStarted { turn_id });
        accepted.extend(inputs.into_iter().map(|input| {
            let message = Message::user_content(input.content.clone());
            Record::InputAccepted(AcceptedInput {
                turn_id,
                input,
                message,
            })
        }));
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
                    .append(&[Record::TurnFailed {
                        turn_id,
                        error: error.to_string(),
                    }])
                    .await;
                return Err(error);
            }
        };
        let mut model_context = self.log.model_context();
        model_context.extend(patch.context);
        let mut turn_config = self.config.clone();
        turn_config.tools.extend(patch.tools);
        let mut persistence =
            crate::store::ThreadPersistence::new(self.store.clone(), self.id, self.version);
        let execution = TurnExecution::new(self.id, turn_id, events.clone(), cancel, steering);
        let mut result = run_agent_turn_persisted(
            self.runtime.model(),
            &turn_config,
            &mut model_context,
            execution,
            &mut persistence,
        )
        .await;
        self.version = persistence.version();
        drop(persistence);
        let completed_messages = match self.store.load(self.id).await {
            Ok(Some(stored)) => stored.log.messages(),
            Ok(None) => {
                let error = thread_closed();
                if result.is_ok() {
                    result = Err(error);
                }
                self.log.messages()
            }
            Err(error) => {
                if result.is_ok() {
                    result = Err(error);
                }
                self.log.messages()
            }
        };
        let status = match &result {
            Ok(reason) => TurnStatus::Completed(reason.clone()),
            Err(error) => TurnStatus::Failed(error.to_string()),
        };
        let outcome = TurnOutcome {
            status,
            messages: completed_messages,
        };
        if let Err(error) = self.runtime.complete_turn(&turn_context, &outcome).await {
            if result.is_ok() {
                result = Err(error);
            }
        }
        let terminal = match &result {
            Ok(reason) => Record::TurnCompleted {
                turn_id,
                reason: reason.clone(),
            },
            Err(error) => Record::TurnFailed {
                turn_id,
                error: error.to_string(),
            },
        };
        let terminal_result = self.append(&[terminal]).await;
        match self.store.load(self.id).await {
            Ok(Some(stored)) => {
                self.log = stored.log;
                self.version = stored.version;
            }
            Err(error) if result.is_ok() => return Err(error),
            Ok(None) if result.is_ok() => return Err(thread_closed()),
            Ok(None) | Err(_) => {}
        }
        if let Err(error) = terminal_result {
            if result.is_ok() {
                return Err(error);
            }
        }
        result
    }

    pub async fn rollback_last_turn(&mut self) -> Result<Option<String>, ash_core::AshError> {
        let messages = self.log.messages();
        let Some((turn_start, prompt)) = last_user_turn(&messages) else {
            return Ok(None);
        };
        self.append(&[Record::TurnRolledBack]).await?;
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
        self.append(&[Record::ContextCheckpoint(checkpoint)])
            .await?;
        Ok(ContextCompaction {
            before_tokens: compacted.before_tokens,
            after_tokens: compacted.after_tokens,
            dropped_messages: compacted.dropped_messages,
        })
    }

    async fn append(&mut self, records: &[Record]) -> Result<(), ash_core::AshError> {
        if records.is_empty() {
            return Ok(());
        }
        self.version = if self.persisted {
            self.store.append(self.id, self.version, records).await?
        } else {
            let version = self.store.create(self.metadata.clone(), records).await?;
            self.persisted = true;
            version
        };
        for record in records {
            self.log.push(record.clone());
        }
        Ok(())
    }
}

struct ForkData {
    messages: Vec<Message>,
    model: String,
    protocol: String,
    working_dir: PathBuf,
    prompt: String,
}

fn thread_metadata(config: &RunConfig, runtime: &Runtime, thread_id: ThreadId) -> ThreadMetadata {
    ThreadMetadata {
        thread_id,
        model_backend: runtime.model_backend().to_string(),
        model: config.model.clone(),
        working_dir: config.working_dir.clone(),
        system_prompt: config.system_prompt.clone(),
        max_turns: config.max_turns,
        max_context_tokens: config.max_context_tokens,
        max_tool_duration: config.max_tool_duration,
    }
}

fn last_user_turn(messages: &[Message]) -> Option<(usize, String)> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| user_prompt(message).map(|prompt| (index, prompt)))
}

fn user_prompt(message: &Message) -> Option<String> {
    let MessageContent::User(contents) = &message.content else {
        return None;
    };
    Some(
        contents
            .iter()
            .map(|content| match content {
                Content::Text(text) => text.clone(),
                Content::Image { media_type, .. } => format!("[image: {media_type}]"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
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

    use ash_core::{
        ContentBlock, MessageContent, MessageId, ModelClient as ProtocolAdapter, ModelId,
        ModelRequest as LlmRequest, ModelStream as ProtocolStream, ModelStreamEvent as StreamItem,
        Role, ToolCallId,
    };
    use futures::StreamExt;
    use tempfile::TempDir;

    use super::*;

    struct MockAdapter {
        responses: Mutex<VecDeque<Vec<StreamItem>>>,
        requests: Arc<Mutex<Vec<LlmRequest>>>,
    }

    struct DelayedAdapter {
        calls: AtomicUsize,
        requests: Arc<Mutex<Vec<LlmRequest>>>,
    }

    impl ProtocolAdapter for DelayedAdapter {
        fn stream(&self, request: LlmRequest) -> Result<ProtocolStream, ash_core::ProtocolError> {
            self.requests.lock().unwrap().push(request);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Ok(Box::pin(futures::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(StreamItem::Stop(StopReason::EndTurn))
                })))
            } else {
                Ok(Box::pin(futures::stream::iter([
                    Ok(StreamItem::TextDelta("done".to_string())),
                    Ok(StreamItem::Stop(StopReason::EndTurn)),
                ])))
            }
        }
    }

    struct GoalExtension {
        completed: Arc<AtomicUsize>,
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
            outcome: &TurnOutcome,
        ) -> Result<(), ash_core::AshError> {
            assert!(matches!(&outcome.status, TurnStatus::Completed(_)));
            assert!(outcome
                .messages
                .iter()
                .any(|message| matches!(message.content, MessageContent::Assistant(_))));
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl ProtocolAdapter for MockAdapter {
        fn stream(&self, request: LlmRequest) -> Result<ProtocolStream, ash_core::ProtocolError> {
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
            context_policy: Arc::new(crate::CodingContextPolicy),
            max_tool_duration: Duration::from_secs(5),
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
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
                thread_metadata(&config(directory.path().join("old")), &runtime, saved_id),
                &[Record::Message(saved_message)],
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
        state.log.push(Record::Message(message));

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
                StreamItem::TextDelta("condensed facts".to_string()),
                StreamItem::Stop(StopReason::EndTurn),
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
        let prompts = messages.iter().filter_map(user_prompt).collect::<Vec<_>>();
        assert_eq!(prompts, ["first", "second"]);
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn notify_joins_the_next_turn_without_starting_one() {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::from([vec![StreamItem::Stop(
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
            .filter_map(user_prompt)
            .collect::<Vec<_>>();
        assert_eq!(prompts, ["note", "task"]);
    }

    #[tokio::test]
    async fn enqueue_creates_a_trackable_fire_and_forget_turn() {
        let directory = TempDir::new().unwrap();
        let runtime = Runtime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::from([vec![StreamItem::Stop(
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
                if matches!(event.kind, EventKind::TurnCompleted { .. }) {
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
            .any(|message| user_prompt(message).as_deref() == Some("updated direction")));
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
                    StreamItem::TextDelta("done".to_string()),
                    StreamItem::Stop(StopReason::EndTurn),
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
}
