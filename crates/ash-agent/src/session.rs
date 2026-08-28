use std::{collections::VecDeque, sync::Arc};

use ash_core::{
    ActiveTurnStats, CancellationToken, ForkPoint, Message, MessageId, SessionEvent,
    SessionEventKind, SessionId, SessionIdentity, SessionStats, SessionView, TurnId, TurnResult,
    TurnStats, TurnView,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_stream::wrappers::BroadcastStream;

use crate::context::estimate_request_tokens;
use crate::engine::{compact_context, run_agent_turn};
use crate::jsonl::{JsonlSessionStore, OpenedSession, SessionWriter};
use crate::log::{ContextCheckpoint, LogEntry, SessionLog};
use crate::{Agent, ContextUpdate, Input, Runtime};

const EMPTY_INPUT_ERROR: &str = "session input cannot be empty";

#[derive(Clone)]
pub struct Session {
    identity: SessionIdentity,
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<SessionEvent>,
    stats: watch::Receiver<SessionStats>,
}

pub struct Turn {
    session_id: SessionId,
    id: TurnId,
    cancellation: CancellationToken,
    completion: oneshot::Receiver<Result<TurnView, ash_core::AshError>>,
}

struct QueuedTurn {
    id: TurnId,
    input: Input,
    cancellation: CancellationToken,
    completion: Option<oneshot::Sender<Result<TurnView, ash_core::AshError>>>,
}

#[derive(Default)]
struct ActorQueues {
    turns: VecDeque<QueuedTurn>,
}

enum Command {
    Submit(QueuedTurn),
    Rollback(oneshot::Sender<Result<Option<String>, ash_core::AshError>>),
    Compact {
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<ContextUpdate, ash_core::AshError>>,
    },
    View(oneshot::Sender<Result<SessionView, ash_core::AshError>>),
    ForkPoints(oneshot::Sender<Result<Vec<ForkPoint>, ash_core::AshError>>),
    Fork {
        message_id: MessageId,
        reply: oneshot::Sender<Result<Option<ForkedSession>, ash_core::AshError>>,
    },
}

impl Session {
    pub(crate) fn spawn(state: SessionActorState) -> Self {
        let id = state.id();
        let identity = state.identity();
        let initial_stats = state.stats();
        let (commands, command_rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(256);
        let (stats_tx, stats) = watch::channel(initial_stats);
        let events_clone = events.clone();
        tokio::spawn(async move {
            // Keep actor panics visible: the JoinHandle is dropped here, so a
            // panic inside the actor would otherwise disappear with it.
            if let Err(panic) = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                run_session(state, command_rx, events_clone, stats_tx),
            ))
            .await
            {
                tracing::error!(
                    %id,
                    message = %panic_payload(panic.as_ref()),
                    "session actor panicked"
                );
            }
        });
        Self {
            identity,
            commands,
            events,
            stats,
        }
    }

    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.identity.id
    }

    #[must_use]
    pub const fn identity(&self) -> SessionIdentity {
        self.identity
    }

    #[must_use]
    pub fn events(&self) -> BroadcastStream<SessionEvent> {
        BroadcastStream::new(self.events.subscribe())
    }

    #[must_use]
    pub fn stats(&self) -> SessionStats {
        *self.stats.borrow()
    }

    #[must_use]
    pub fn subscribe_stats(&self) -> watch::Receiver<SessionStats> {
        self.stats.clone()
    }

    /// Submit a new turn and return a handle to it.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the input is empty or the session runtime has
    /// stopped.
    pub async fn submit(&self, input: impl Into<Input>) -> Result<Turn, ash_core::AshError> {
        let (command, turn) = self.prepare_submission(input.into())?;
        self.commands
            .send(command)
            .await
            .map_err(|_| session_closed())?;
        Ok(turn)
    }

    /// Submit immediately without waiting for queue capacity.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the input is empty, the queue is full, or the
    /// session runtime has stopped.
    pub fn try_submit(&self, input: impl Into<Input>) -> Result<Turn, ash_core::AshError> {
        let (command, turn) = self.prepare_submission(input.into())?;
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => queue_full(),
                mpsc::error::TrySendError::Closed(_) => session_closed(),
            })?;
        Ok(turn)
    }

    /// Full projected state: history, model context, and turn views.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the session runtime has stopped.
    pub async fn view(&self) -> Result<SessionView, ash_core::AshError> {
        self.ask(Command::View).await
    }

    /// Forkable submission points in this session.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the session runtime has stopped.
    pub async fn fork_points(&self) -> Result<Vec<ForkPoint>, ash_core::AshError> {
        self.ask(Command::ForkPoints).await
    }

    /// Roll back the latest settled turn and return its prompt.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the session runtime has stopped.
    pub async fn rollback(&self) -> Result<Option<String>, ash_core::AshError> {
        self.ask(Command::Rollback).await
    }

    /// Fork the session from the message with the given id.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the session runtime has stopped.
    pub async fn fork_at(
        &self,
        message_id: MessageId,
    ) -> Result<Option<ForkedSession>, ash_core::AshError> {
        self.ask(|reply| Command::Fork { message_id, reply }).await
    }

    /// Compact model context, keeping full history durable.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the session runtime has stopped.
    pub async fn compact(&self) -> Result<ContextUpdate, ash_core::AshError> {
        let cancellation = CancellationToken::new();
        self.ask(|reply| Command::Compact {
            cancellation,
            reply,
        })
        .await
    }

    async fn ask<T>(
        &self,
        make_command: impl FnOnce(oneshot::Sender<Result<T, ash_core::AshError>>) -> Command,
    ) -> Result<T, ash_core::AshError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(make_command(reply))
            .await
            .map_err(|_| session_closed())?;
        result.await.map_err(|_| session_closed())?
    }

    fn prepare_submission(&self, input: Input) -> Result<(Command, Turn), ash_core::AshError> {
        ensure_nonempty(&input)?;
        let (completion_tx, completion) = oneshot::channel();
        let queued = queued_turn(input, Some(completion_tx));
        let turn = Turn {
            session_id: self.identity.id,
            id: queued.id,
            cancellation: queued.cancellation.clone(),
            completion,
        };
        Ok((Command::Submit(queued), turn))
    }
}

impl Turn {
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

    /// Wait for this turn to settle and return its canonical completed view.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the turn ends without a completion view.
    pub async fn wait(self) -> Result<TurnView, ash_core::AshError> {
        self.completion.await.map_err(|_| session_closed())?
    }
}

async fn run_session(
    mut state: SessionActorState,
    mut commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<SessionEvent>,
    stats: watch::Sender<SessionStats>,
) {
    let mut queues = ActorQueues::default();
    let mut sequence = 0_u64;
    loop {
        if let Some(turn) = queues.turns.pop_front() {
            run_turn(
                &mut state,
                turn,
                &mut queues,
                &mut commands,
                &events,
                &mut sequence,
                &stats,
            )
            .await;
            continue;
        }
        let Some(command) = commands.recv().await else {
            break;
        };
        dispatch_idle_command(
            command,
            &mut state,
            &mut queues,
            &events,
            &mut sequence,
            &stats,
        )
        .await;
    }
}

async fn run_turn(
    state: &mut SessionActorState,
    turn: QueuedTurn,
    queues: &mut ActorQueues,
    commands: &mut mpsc::Receiver<Command>,
    events: &broadcast::Sender<SessionEvent>,
    sequence: &mut u64,
    stats: &watch::Sender<SessionStats>,
) {
    let QueuedTurn {
        id,
        input,
        cancellation,
        completion,
    } = turn;
    let session_id = state.id();
    let (payload_tx, mut payload_rx) = mpsc::channel(64);
    let execution = state.submit_input(id, input, payload_tx, cancellation.clone());
    let mut execution = Box::pin(execution);

    let mut commands_open = true;
    let result = loop {
        tokio::select! {
            biased;
            result = &mut execution => break result,
            payload = payload_rx.recv() => {
                if let Some(kind) = payload {
                    apply_turn_stats(stats, id, &kind);
                    publish(events, session_id, Some(id), sequence, kind);
                }
            }
            command = commands.recv(), if commands_open => {
                let Some(command) = command else {
                    cancellation.cancel();
                    commands_open = false;
                    continue;
                };
                dispatch_active_command(command, queues);
            }
        }
    };
    while let Ok(kind) = payload_rx.try_recv() {
        apply_turn_stats(stats, id, &kind);
        publish(events, session_id, Some(id), sequence, kind);
    }
    drop(execution);
    let view = match &result {
        Ok(view) => view.clone(),
        Err(error) => {
            // Submit can fail before writing anything (empty input), so look
            // up this exact turn and synthesize a failed view if it never
            // entered the log.
            state.log.turn_view(id).unwrap_or_else(|| TurnView {
                id,
                result: TurnResult::Failed(error.to_string()),
                messages: Vec::new(),
                stats: ash_core::TurnStats::default(),
            })
        }
    };
    let previous = *stats.borrow();
    let next = state.stats();
    if previous != next {
        stats.send_replace(next);
    }
    if previous.context_tokens != next.context_tokens {
        if let Some(tokens) = next.context_tokens {
            publish(
                events,
                session_id,
                Some(id),
                sequence,
                SessionEventKind::ContextChanged { tokens },
            );
        }
    }
    let completed = view.clone();
    publish(
        events,
        session_id,
        Some(id),
        sequence,
        SessionEventKind::TurnCompleted(view),
    );
    if let Some(completion) = completion {
        let _ = completion.send(Ok(completed));
    }
}

fn apply_turn_stats(
    stats: &watch::Sender<SessionStats>,
    turn_id: TurnId,
    event: &SessionEventKind,
) {
    let mut next = *stats.borrow();
    match event {
        SessionEventKind::TurnStarted => {
            next.active_turn = Some(ActiveTurnStats {
                turn_id,
                stats: TurnStats::default(),
            });
        }
        SessionEventKind::TurnProgress(turn_stats) => {
            next.active_turn = Some(ActiveTurnStats {
                turn_id,
                stats: *turn_stats,
            });
        }
        SessionEventKind::ContextChanged { tokens } => next.context_tokens = Some(*tokens),
        SessionEventKind::Live(_)
        | SessionEventKind::TurnCompleted(_)
        | SessionEventKind::ContextCompacted(_) => return,
    }
    if *stats.borrow() != next {
        stats.send_replace(next);
    }
}

/// Route a command while a turn is active. Never touches `SessionActorState` (the
/// turn's execution already borrows it), so this stays synchronous.
fn dispatch_active_command(command: Command, queues: &mut ActorQueues) {
    match command {
        Command::Submit(turn) => queues.turns.push_back(turn),
        Command::Rollback(reply) => reject_busy(reply),
        Command::Compact { reply, .. } => reject_busy(reply),
        Command::View(reply) => reject_busy(reply),
        Command::ForkPoints(reply) => reject_busy(reply),
        Command::Fork { reply, .. } => reject_busy(reply),
    }
}

/// Route a command while the session is idle.
async fn dispatch_idle_command(
    command: Command,
    state: &mut SessionActorState,
    queues: &mut ActorQueues,
    events: &broadcast::Sender<SessionEvent>,
    sequence: &mut u64,
    stats: &watch::Sender<SessionStats>,
) {
    match command {
        Command::Submit(turn) => queues.turns.push_back(turn),
        Command::Rollback(reply) => {
            let result = state.rollback_last_turn().await;
            if result.is_ok() {
                publish_current_stats(state, stats, events, sequence);
            }
            let _ = reply.send(result);
        }
        Command::Compact {
            cancellation,
            reply,
        } => {
            let result = state.compact(&cancellation).await;
            // A failed compaction may still have incurred and persisted model
            // usage, so always refresh from the durable projection.
            publish_current_stats(state, stats, events, sequence);
            let _ = reply.send(result);
        }
        Command::View(reply) => {
            let _ = reply.send(Ok(state.view()));
        }
        Command::ForkPoints(reply) => {
            let _ = reply.send(Ok(state.fork_points()));
        }
        Command::Fork { message_id, reply } => {
            let result = state.fork_at(message_id).await.map(|forked| {
                forked.map(|(state, prompt)| ForkedSession {
                    session: Session::spawn(state),
                    prompt,
                })
            });
            let _ = reply.send(result);
        }
    }
}

fn publish_current_stats(
    state: &SessionActorState,
    stats: &watch::Sender<SessionStats>,
    events: &broadcast::Sender<SessionEvent>,
    sequence: &mut u64,
) {
    let previous = *stats.borrow();
    let next = state.stats();
    if previous != next {
        stats.send_replace(next);
    }
    if previous.context_tokens != next.context_tokens {
        if let Some(tokens) = next.context_tokens {
            publish(
                events,
                state.id(),
                None,
                sequence,
                SessionEventKind::ContextChanged { tokens },
            );
        }
    }
}

fn reject_busy<T>(reply: oneshot::Sender<Result<T, ash_core::AshError>>) {
    let _ = reply.send(Err(busy_session()));
}

fn publish(
    events: &broadcast::Sender<SessionEvent>,
    session_id: SessionId,
    turn_id: Option<TurnId>,
    sequence: &mut u64,
    kind: SessionEventKind,
) {
    *sequence = sequence.saturating_add(1);
    let event = SessionEvent {
        session_id,
        turn_id,
        sequence: *sequence,
        timestamp: chrono::Utc::now(),
        kind,
    };
    // The session is the single publisher of everything the UI can observe.
    // Log every event here so agent output is always visible in the logs,
    // regardless of whether any UI is attached. Streaming deltas are too
    // chatty for info level; everything else is a stable boundary event.
    match &event.kind {
        SessionEventKind::Live(
            ash_core::LiveEvent::TextDelta(_) | ash_core::LiveEvent::ReasoningDelta(_),
        ) => {
            tracing::debug!(%session_id, ?turn_id, sequence, kind = ?event.kind, "session event");
        }
        _ => {
            tracing::info!(%session_id, ?turn_id, sequence, kind = ?event.kind, "session event");
        }
    }
    let _ = events.send(event);
}

const fn session_closed() -> ash_core::AshError {
    ash_core::AshError::Session(ash_core::SessionError::Closed)
}

/// Extract a human-readable message from a panic payload for logging.
fn panic_payload(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| panic.downcast_ref::<String>().map(ToString::to_string))
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

const fn busy_session() -> ash_core::AshError {
    ash_core::AshError::Session(ash_core::SessionError::Busy)
}

const fn queue_full() -> ash_core::AshError {
    ash_core::AshError::Session(ash_core::SessionError::QueueFull)
}

fn ensure_nonempty(input: &Input) -> Result<(), ash_core::AshError> {
    if input.is_empty() {
        return Err(ash_core::AshError::Config(EMPTY_INPUT_ERROR.to_string()));
    }
    Ok(())
}

fn queued_turn(
    input: Input,
    completion: Option<oneshot::Sender<Result<TurnView, ash_core::AshError>>>,
) -> QueuedTurn {
    QueuedTurn {
        id: TurnId::new(),
        input,
        cancellation: CancellationToken::new(),
        completion,
    }
}

/// A new root session forked before a selected prompt.
pub struct ForkedSession {
    pub session: Session,
    pub prompt: String,
}

pub(crate) struct SessionActorState {
    identity: SessionIdentity,
    agent: Agent,
    runtime: Runtime,
    log: SessionLog,
    store: Arc<JsonlSessionStore>,
    /// Open append-only handle to this session's file. Initialized lazily so
    /// `SessionActorState::new` stays synchronous; every write goes through it
    /// without re-scanning or re-reading the file.
    writer: Option<SessionWriter>,
}

impl SessionActorState {
    pub(crate) fn new(agent: Agent, runtime: Runtime) -> Self {
        let identity = SessionIdentity::root(SessionId::new());
        let store = runtime.session_store_handle();
        Self {
            identity,
            agent,
            runtime,
            log: SessionLog::new(),
            store,
            writer: None,
        }
    }

    /// Create a child session of `parent`, deriving its lineage from the parent.
    pub(crate) fn new_child(agent: Agent, runtime: Runtime, parent: SessionIdentity) -> Self {
        let identity = parent.child();
        let store = runtime.session_store_handle();
        Self {
            identity,
            agent,
            runtime,
            log: SessionLog::new(),
            store,
            writer: None,
        }
    }

    const fn id(&self) -> SessionId {
        self.identity.id
    }

    const fn identity(&self) -> SessionIdentity {
        self.identity
    }

    fn view(&self) -> SessionView {
        let mut view = self.log.view();
        view.stats.context_tokens = self.estimate_context_tokens(&view.context);
        view
    }

    fn stats(&self) -> SessionStats {
        SessionStats {
            settled_usage: self.log.usage(),
            active_turn: None,
            context_tokens: self.estimate_context_tokens(&self.log.model_context()),
        }
    }

    pub(crate) async fn resume(
        &mut self,
        session_id: SessionId,
    ) -> Result<bool, ash_core::AshError> {
        let Some(opened) = self.store.open(session_id).await? else {
            return Ok(false);
        };
        if !opened.session.identity.is_root() {
            return Err(ash_core::SessionError::ChildSession.into());
        }
        self.restore(opened);
        Ok(true)
    }

    async fn seed(&mut self, messages: Vec<Message>) -> Result<(), ash_core::AshError> {
        let entries = messages
            .into_iter()
            .map(LogEntry::Message)
            .collect::<Vec<_>>();
        self.append(&entries).await
    }

    fn fork_points(&self) -> Vec<ForkPoint> {
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
        &self,
        message_id: MessageId,
    ) -> Result<Option<(Self, String)>, ash_core::AshError> {
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
        let agent = self.agent.clone();
        let runtime = self.runtime.clone();

        let mut state = Self::new(agent, runtime);
        state.seed(messages).await?;

        Ok(Some((state, prompt)))
    }

    fn restore(&mut self, opened: OpenedSession) {
        self.identity = opened.session.identity;
        self.log = opened.session.log;
        self.writer = Some(opened.writer);
    }

    async fn submit_input(
        &mut self,
        turn_id: TurnId,
        input: Input,
        events: mpsc::Sender<SessionEventKind>,
        cancel: CancellationToken,
    ) -> Result<TurnView, ash_core::AshError> {
        ensure_nonempty(&input)?;
        let message = Message::user_content(input.content);
        self.append(&[LogEntry::TurnStart(turn_id), LogEntry::Message(message)])
            .await?;
        let _ = events.send(SessionEventKind::TurnStarted).await;
        let mut model_context = self.log.model_context();
        let agent = self.agent.clone();
        let outcome = run_agent_turn(
            self.runtime.model(),
            &agent,
            &mut model_context,
            self.identity,
            events.clone(),
            cancel,
        )
        .await;
        let turn_result = match &outcome.result {
            Ok(reason) => TurnResult::Completed(reason.clone()),
            Err(error) => TurnResult::Failed(error.to_string()),
        };
        // Commit point: write all messages plus the turn end in one append. On crash
        // before this point the turn has no `TurnEnd`, so the projection
        // marks it `Interrupted` and its partial output is never exposed.
        let mut entries = outcome.entries;
        entries.push(LogEntry::TurnEnd {
            id: turn_id,
            result: turn_result,
            stats: outcome.stats,
        });
        if let Err(error) = self.append(&entries).await {
            if let Err(execution) = &outcome.result {
                tracing::error!(
                    %execution,
                    persistence = %error,
                    "turn execution and persistence both failed"
                );
            }
            return Err(error);
        }
        outcome.result?;
        let committed = self.log.turn_view(turn_id).ok_or_else(|| {
            ash_core::AshError::Config(format!(
                "committed turn {turn_id} is missing from the durable projection"
            ))
        })?;
        Ok(committed)
    }

    async fn rollback_last_turn(&mut self) -> Result<Option<String>, ash_core::AshError> {
        let messages = self.log.messages();
        let Some((_, prompt)) = last_user_turn(&messages) else {
            return Ok(None);
        };
        self.append(&[LogEntry::Rollback]).await?;
        Ok(Some(prompt))
    }

    async fn compact(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<ContextUpdate, ash_core::AshError> {
        let runtime = self.runtime.clone();
        self.compact_using(runtime.model(), cancellation).await
    }

    async fn compact_using(
        &mut self,
        model: &dyn ash_core::ModelClient,
        cancel: &CancellationToken,
    ) -> Result<ContextUpdate, ash_core::AshError> {
        let tools = self.agent.tool_definitions();
        let model_context = self.log.model_context();
        let before_tokens =
            estimate_request_tokens(self.agent.system_prompt(), &model_context, &tools);
        let (result, stats) = compact_context(&self.agent, &model_context, model, cancel).await;
        let usage = stats.usage;
        let compacted = match result {
            Ok(compacted) => compacted,
            Err(error) => {
                if usage != ash_core::Usage::default() {
                    self.append(&[LogEntry::CompactionUsage(usage)]).await?;
                }
                return Err(error);
            }
        };
        let Some(compacted) = compacted else {
            if usage != ash_core::Usage::default() {
                self.append(&[LogEntry::CompactionUsage(usage)]).await?;
            }
            return Ok(ContextUpdate {
                before_tokens: u64::try_from(before_tokens).unwrap_or(u64::MAX),
                after_tokens: u64::try_from(before_tokens).unwrap_or(u64::MAX),
                dropped_messages: 0,
            });
        };
        let checkpoint = ContextCheckpoint::from_model_context(&compacted.messages)?;
        let mut entries = vec![LogEntry::Checkpoint(checkpoint)];
        if usage != ash_core::Usage::default() {
            entries.push(LogEntry::CompactionUsage(usage));
        }
        self.append(&entries).await?;
        Ok(compacted.update)
    }

    async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError> {
        if entries.is_empty() {
            return Ok(());
        }
        let writer = self.writer().await?;
        writer.append(entries).await?;
        for entry in entries {
            self.log.push(entry.clone());
        }
        Ok(())
    }

    /// Estimate the request size (system prompt + tools + history) for the
    /// given messages, tolerating overflow on the conversion.
    fn estimate_context_tokens(&self, messages: &[Message]) -> Option<u64> {
        u64::try_from(estimate_request_tokens(
            self.agent.system_prompt(),
            messages,
            &self.agent.tool_definitions(),
        ))
        .ok()
    }

    async fn writer(&mut self) -> Result<&mut SessionWriter, ash_core::AshError> {
        if self.writer.is_none() {
            self.writer = Some(self.store.open_new(self.identity).await?);
        }
        self.writer.as_mut().ok_or_else(|| {
            ash_core::AshError::Config("session writer was not initialized".to_string())
        })
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
        sync::{Arc, Mutex},
    };

    use ash_core::{
        MessageContent, ModelClient, ModelEvent, ModelId, ModelRequest, ModelStream, ModelUsage,
        StopReason,
    };
    use futures::StreamExt;
    use tempfile::TempDir;

    use super::*;

    struct MockModel {
        responses: Mutex<VecDeque<Vec<ModelEvent>>>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl MockModel {
        fn new(responses: impl IntoIterator<Item = Vec<ModelEvent>>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: Arc::new(Mutex::new(Vec::new())),
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

    struct PartialModel;

    impl ModelClient for PartialModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            Ok(Box::pin(
                futures::stream::iter([Ok(ModelEvent::Text("partial".to_string()))])
                    .chain(futures::stream::pending()),
            ))
        }
    }

    fn definition() -> Agent {
        Agent::new(ModelId::new("test-model"), Vec::new()).with_max_context_tokens(200_000)
    }

    fn runtime(model: Arc<dyn ModelClient>, directory: &TempDir) -> Runtime {
        Runtime::new(model).with_session_directory(directory.path())
    }

    #[tokio::test]
    async fn submit_persists_one_complete_turn_with_protocol_usage() {
        let directory = TempDir::new().unwrap();
        let model = Arc::new(MockModel::new([vec![
            ModelEvent::Text("answer".to_string()),
            ModelEvent::Usage(ModelUsage {
                input_tokens: 42,
                output_tokens: 7,
            }),
            ModelEvent::Stop(StopReason::EndTurn),
        ]]));
        let runtime = runtime(model, &directory);
        let session = runtime.start(&definition());

        let completed = session
            .submit("question")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        let view = session.view().await.unwrap();
        let stored = runtime
            .session_store_handle()
            .load(session.id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(completed.result, TurnResult::Completed(StopReason::EndTurn));
        assert_eq!(
            completed.stats.usage,
            ash_core::Usage {
                input_tokens: 42,
                output_tokens: 7,
                tool_calls: 0,
            }
        );
        assert_eq!(view.turns, [completed]);
        assert_eq!(view.stats.settled_usage.input_tokens, 42);
        assert!(view.stats.context_tokens.is_some());
        assert_eq!(stored.log.turns(), view.turns);
        assert!(matches!(
            view.messages.last().map(|message| &message.content),
            Some(MessageContent::Assistant(_))
        ));
    }

    #[tokio::test]
    async fn busy_session_queues_submissions_in_fifo_order() {
        let directory = TempDir::new().unwrap();
        let model = Arc::new(MockModel::new([
            vec![ModelEvent::Stop(StopReason::EndTurn)],
            vec![ModelEvent::Stop(StopReason::EndTurn)],
        ]));
        let runtime = runtime(model, &directory);
        let session = runtime.start(&definition());

        let first = session.submit("first").await.unwrap();
        let second = session.submit("second").await.unwrap();
        first.wait().await.unwrap();
        second.wait().await.unwrap();

        let prompts = session
            .view()
            .await
            .unwrap()
            .messages
            .iter()
            .filter_map(Message::user_turn_text)
            .collect::<Vec<_>>();
        assert_eq!(prompts, ["first", "second"]);
    }

    #[tokio::test]
    async fn child_starts_empty_and_uses_the_same_durable_store() {
        let directory = TempDir::new().unwrap();
        let model = Arc::new(MockModel::new([
            vec![ModelEvent::Stop(StopReason::EndTurn)],
            vec![ModelEvent::Stop(StopReason::EndTurn)],
        ]));
        let requests = Arc::clone(&model.requests);
        let runtime = runtime(model, &directory);
        let definition = definition();
        let root = runtime.start(&definition);
        root.submit("root history")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();

        let child = runtime.start_child(&definition, root.identity());
        child
            .submit("child only")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();

        let identity = child.identity();
        assert_eq!(identity.root_id, root.id());
        assert_eq!(identity.parent_id, Some(root.id()));
        assert_ne!(identity.id, root.id());
        assert_eq!(runtime.session_tree(root.id()).await.unwrap().len(), 2);

        let requests = requests.lock().unwrap();
        let child_prompts = requests[1]
            .messages
            .iter()
            .filter_map(Message::user_turn_text)
            .collect::<Vec<_>>();
        assert_eq!(child_prompts, ["child only"]);
    }

    #[tokio::test]
    async fn resume_restores_a_root_session_but_not_a_child() {
        let directory = TempDir::new().unwrap();
        let model = Arc::new(MockModel::new([
            vec![ModelEvent::Stop(StopReason::EndTurn)],
            vec![ModelEvent::Stop(StopReason::EndTurn)],
        ]));
        let runtime = runtime(model, &directory);
        let definition = definition();
        let root = runtime.start(&definition);
        root.submit("root").await.unwrap().wait().await.unwrap();
        let child = runtime.start_child(&definition, root.identity());
        child.submit("child").await.unwrap().wait().await.unwrap();
        let root_id = root.id();
        let child_id = child.id();

        drop(root);
        drop(child);
        tokio::task::yield_now().await;

        let resumed = runtime.resume(&definition, root_id).await.unwrap().unwrap();
        assert_eq!(
            resumed.view().await.unwrap().messages[0].user_turn_text(),
            Some("root".to_string())
        );
        drop(resumed);
        tokio::task::yield_now().await;
        let error = runtime
            .resume(&definition, child_id)
            .await
            .err()
            .expect("a child session must not resume through the root path");
        assert!(matches!(
            error,
            ash_core::AshError::Session(ash_core::SessionError::ChildSession)
        ));
    }

    #[tokio::test]
    async fn cancellation_discards_partial_assistant_output() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime(Arc::new(PartialModel), &directory);
        let session = runtime.start(&definition());
        let mut events = session.events();
        let turn = session.submit("question").await.unwrap();

        loop {
            let event = events.next().await.unwrap().unwrap();
            if matches!(
                event.kind,
                SessionEventKind::Live(ash_core::LiveEvent::TextDelta(_))
            ) {
                turn.cancellation_token().cancel();
                break;
            }
        }
        let completed = turn.wait().await.unwrap();

        assert_eq!(completed.result, TurnResult::Completed(StopReason::Aborted));
        assert_eq!(completed.messages.len(), 1);
        assert!(completed.messages[0].is_user_turn());
    }

    #[tokio::test]
    async fn rollback_removes_the_latest_settled_turn() {
        let directory = TempDir::new().unwrap();
        let model = Arc::new(MockModel::new([
            vec![ModelEvent::Stop(StopReason::EndTurn)],
            vec![ModelEvent::Stop(StopReason::EndTurn)],
        ]));
        let runtime = runtime(model, &directory);
        let session = runtime.start(&definition());
        session.submit("first").await.unwrap().wait().await.unwrap();
        session
            .submit("second")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();

        assert_eq!(session.rollback().await.unwrap().as_deref(), Some("second"));
        let prompts = session
            .view()
            .await
            .unwrap()
            .messages
            .iter()
            .filter_map(Message::user_turn_text)
            .collect::<Vec<_>>();
        assert_eq!(prompts, ["first"]);
    }

    #[tokio::test]
    async fn fork_copies_only_history_before_the_selected_prompt() {
        let directory = TempDir::new().unwrap();
        let model = Arc::new(MockModel::new([
            vec![ModelEvent::Stop(StopReason::EndTurn)],
            vec![ModelEvent::Stop(StopReason::EndTurn)],
        ]));
        let runtime = runtime(model, &directory);
        let session = runtime.start(&definition());
        session.submit("first").await.unwrap().wait().await.unwrap();
        session
            .submit("second")
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        let point = session
            .fork_points()
            .await
            .unwrap()
            .into_iter()
            .find(|point| point.prompt == "second")
            .unwrap();

        let forked = session.fork_at(point.message_id).await.unwrap().unwrap();
        assert_eq!(forked.prompt, "second");
        let prompts = forked
            .session
            .view()
            .await
            .unwrap()
            .messages
            .iter()
            .filter_map(Message::user_turn_text)
            .collect::<Vec<_>>();
        assert_eq!(prompts, ["first"]);
    }

    #[tokio::test]
    async fn idle_only_commands_reject_while_a_turn_is_running() {
        let directory = TempDir::new().unwrap();
        let runtime = runtime(Arc::new(PartialModel), &directory);
        let session = runtime.start(&definition());
        let turn = session.submit("question").await.unwrap();

        assert!(matches!(
            session.rollback().await.unwrap_err(),
            ash_core::AshError::Session(ash_core::SessionError::Busy)
        ));
        turn.cancellation_token().cancel();
        turn.wait().await.unwrap();
    }

    #[test]
    fn empty_input_is_rejected_at_the_single_submit_boundary() {
        assert!(Input::user(" \n\t").is_empty());
        assert!(ensure_nonempty(&Input::user(""))
            .unwrap_err()
            .to_string()
            .contains(EMPTY_INPUT_ERROR));
    }

    #[test]
    fn try_submit_reports_a_full_bounded_queue() {
        let identity = SessionIdentity::root(SessionId::new());
        let (commands, _commands_rx) = mpsc::channel(1);
        let (events, _) = broadcast::channel(1);
        let (_stats_tx, stats) = watch::channel(SessionStats::default());
        let session = Session {
            identity,
            commands,
            events,
            stats,
        };

        let _first = session.try_submit("first").unwrap();
        let error = session
            .try_submit("second")
            .err()
            .expect("the second turn must exceed queue capacity");

        assert!(matches!(
            error,
            ash_core::AshError::Session(ash_core::SessionError::QueueFull)
        ));
    }
}
