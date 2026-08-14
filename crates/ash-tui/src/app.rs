use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(test)]
use ash_core::SessionError;
use ash_core::{
    AshError, ForkPoint, LiveEvent, MessageId, SessionEvent, SessionEventKind, SessionId,
    SessionSummary, SessionView, TurnId,
};
use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use futures::StreamExt;

use crate::{
    fork_picker::ForkPickerState,
    inline::{RenderPlan, TerminalUi, TerminalView},
    input::InputState,
    menu::ComposerMenuState,
    operation::{BackgroundAction, OperationState, SubmissionPolicy, TurnStartOutcome},
    session_picker::SessionPickerState,
    slash_command::{self, CommandCompletionState, ParsedInput, SlashCommand},
    SubagentView,
};

/// Pace complete assistant lines so streaming remains readable rather than
/// tracking every delta: newline deltas flush immediately and this periodic
/// status refresh redraws anything that did not complete a line yet.
const STATUS_INTERVAL: Duration = Duration::from_millis(350);

#[derive(Debug)]
pub enum UiError {
    Agent(AshError),
    ControllerStopped,
    InputDropped,
}

impl fmt::Display for UiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::ControllerStopped => formatter.write_str("session controller has stopped"),
            Self::InputDropped => formatter.write_str("session controller dropped the input"),
        }
    }
}

impl std::error::Error for UiError {}

impl From<AshError> for UiError {
    fn from(error: AshError) -> Self {
        Self::Agent(error)
    }
}

#[derive(Debug)]
pub enum UiCommand {
    Submit {
        input: String,
        reply: tokio::sync::oneshot::Sender<Result<TurnId, UiError>>,
    },
    Steer {
        input: String,
        reply: tokio::sync::oneshot::Sender<Result<(), UiError>>,
    },
    Cancel,
    Rollback,
    Compact,
    NewSession,
    ListSessions,
    ListForkPoints,
    ResumeSession(SessionId),
    ForkSession(MessageId),
    Exit,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Session(SessionEvent),
    SessionChanged {
        session_id: SessionId,
    },
    SessionRestored {
        session_id: SessionId,
        view: SessionView,
    },
    RollbackCompleted {
        prompt: String,
    },
    CompactionCompleted {
        before: u64,
        after: u64,
        dropped: u64,
    },
    SessionsListed {
        sessions: Vec<SessionSummary>,
    },
    ForkPointsListed {
        points: Vec<ForkPoint>,
    },
    SessionForked {
        session_id: SessionId,
        view: SessionView,
        prompt: String,
    },
    CommandFailed(String),
}

struct AppState {
    input: InputState,
    operation: OperationState,
    menu: ComposerMenuState,
    subagent_rx: Option<tokio::sync::watch::Receiver<Vec<SubagentView>>>,
    subagents: Vec<SubagentView>,
    session_id: Option<SessionId>,
    last_sequence: u64,
}

enum LoopAction {
    Continue,
    ResetTimers,
    Exit,
}

#[derive(Clone, Copy)]
enum PickerAction<T> {
    KeepOpen,
    Close,
    Confirm(T),
}

trait PickerNavigation {
    fn move_up(&mut self);
    fn move_down(&mut self);
}

impl PickerNavigation for SessionPickerState {
    fn move_up(&mut self) {
        Self::move_up(self);
    }

    fn move_down(&mut self) {
        Self::move_down(self);
    }
}

impl PickerNavigation for ForkPickerState {
    fn move_up(&mut self) {
        Self::move_up(self);
    }

    fn move_down(&mut self) {
        Self::move_down(self);
    }
}

impl PickerNavigation for CommandCompletionState {
    fn move_up(&mut self) {
        Self::move_up(self);
    }

    fn move_down(&mut self) {
        Self::move_down(self);
    }
}

impl AppState {
    fn new(input_history: Vec<String>) -> Self {
        Self {
            input: InputState::with_history(input_history),
            operation: OperationState::default(),
            menu: ComposerMenuState::default(),
            subagent_rx: None,
            subagents: Vec::new(),
            session_id: None,
            last_sequence: 0,
        }
    }

    fn refresh_subagents(&mut self) -> bool {
        let Some(receiver) = &mut self.subagent_rx else {
            return false;
        };
        match receiver.has_changed() {
            Ok(true) => {
                self.subagents.clone_from(&receiver.borrow_and_update());
                true
            }
            Ok(false) => false,
            // The monitor channel closed: no further snapshots will arrive, so
            // stop tracking subagents instead of freezing the last stale list.
            Err(_) => {
                self.subagent_rx = None;
                if self.subagents.is_empty() {
                    false
                } else {
                    self.subagents.clear();
                    true
                }
            }
        }
    }

    fn select_session(&mut self, session_id: SessionId) {
        self.session_id = Some(session_id);
        self.last_sequence = 0;
    }

    fn finish_command_failure(&mut self) {
        self.operation.finish();
        self.menu.close_picker();
    }

    fn accept_session_event(&mut self, session_id: SessionId, sequence: u64) -> bool {
        if self.session_id != Some(session_id) || sequence <= self.last_sequence {
            return false;
        }
        self.last_sequence = sequence;
        true
    }

    fn sync_menu(&mut self) {
        self.menu
            .sync_commands(self.input.text(), self.input.cursor());
    }

    fn view(&self) -> TerminalView<'_> {
        TerminalView {
            input: &self.input,
            menu: self.menu.view(),
            activity: self.operation.activity_view(),
        }
    }

    fn render(&mut self, terminal: &mut TerminalUi) -> std::io::Result<()> {
        self.apply(terminal, RenderPlan::REDRAW)
    }

    fn apply(&mut self, terminal: &mut TerminalUi, plan: RenderPlan) -> std::io::Result<()> {
        if plan.should_sync_view() {
            self.sync_menu();
        }
        terminal.apply_plan(self.view(), plan)
    }

    fn resize(
        &mut self,
        terminal: &mut TerminalUi,
        width: u16,
        height: u16,
    ) -> std::io::Result<()> {
        self.sync_menu();
        terminal.resize_view(self.view(), width, height)
    }
}

pub struct App {
    protocol: String,
    model: String,
    working_dir: PathBuf,
    context_limit: Option<u64>,
    input_history: Vec<String>,
    subagent_monitor: Option<tokio::sync::watch::Receiver<Vec<SubagentView>>>,
}

impl App {
    #[must_use]
    pub const fn new(protocol: String, model: String, working_dir: PathBuf) -> Self {
        Self {
            protocol,
            model,
            working_dir,
            context_limit: None,
            input_history: Vec::new(),
            subagent_monitor: None,
        }
    }

    #[must_use]
    pub const fn with_context_limit(mut self, context_limit: Option<u64>) -> Self {
        self.context_limit = context_limit;
        self
    }

    #[must_use]
    pub fn with_input_history(mut self, input_history: Vec<String>) -> Self {
        self.input_history = input_history;
        self
    }

    #[must_use]
    pub fn with_subagent_monitor(
        mut self,
        subagent_monitor: Option<tokio::sync::watch::Receiver<Vec<SubagentView>>>,
    ) -> Self {
        self.subagent_monitor = subagent_monitor;
        self
    }

    /// Run the TUI loop until the terminal closes or an unrecoverable I/O
    /// error occurs.
    ///
    /// # Errors
    ///
    /// Returns an error when terminal I/O (render, key input, or the
    /// alternate screen) fails and cannot be recovered.
    pub async fn run(
        mut self,
        mut events: impl futures::Stream<Item = UiEvent> + Unpin,
        commands: tokio::sync::mpsc::Sender<UiCommand>,
    ) -> anyhow::Result<()> {
        let mut terminal = TerminalUi::enter(
            &self.protocol,
            &self.model,
            &self.working_dir,
            self.context_limit,
        )?;
        let mut state = AppState::new(std::mem::take(&mut self.input_history));
        state.subagent_rx = self.subagent_monitor.take();
        let mut keys = EventStream::new();
        let mut status_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + STATUS_INTERVAL,
            STATUS_INTERVAL,
        );
        status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let effect = terminal.welcome();
        state.apply(&mut terminal, effect)?;

        loop {
            tokio::select! {
                _ = status_tick.tick() => {
                    let mut effect = RenderPlan::NONE;
                    if state.refresh_subagents() {
                        effect = effect.merge(terminal.set_subagents(state.subagents.clone()));
                    }
                    if state.operation.shows_activity() {
                        effect = effect.merge(terminal.refresh_status());
                    }
                    state.apply(&mut terminal, effect)?;
                }
                event = events.next() => {
                    let Some(event) = event else { break };
                    match handle_ui_event(&mut state, &mut terminal, event)? {
                        LoopAction::Continue => {}
                        LoopAction::ResetTimers => {
                            status_tick.reset();
                        }
                        LoopAction::Exit => break,
                    }
                }
                key = keys.next() => {
                    let Some(key) = key else { break };
                    let event = key?;
                    match handle_terminal_event(&mut state, &mut terminal, &commands, event).await? {
                        LoopAction::Continue => {}
                        LoopAction::ResetTimers => {
                            status_tick.reset();
                        }
                        LoopAction::Exit => break,
                    }
                }
            }
        }

        terminal.leave()?;
        Ok(())
    }
}

fn handle_ui_event(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    event: UiEvent,
) -> anyhow::Result<LoopAction> {
    match event {
        UiEvent::Session(event) => {
            if !state.accept_session_event(event.session_id, event.sequence) {
                return Ok(LoopAction::Continue);
            }
            handle_session_event(state, terminal, event)
        }
        UiEvent::SessionChanged { session_id } => {
            state.select_session(session_id);
            Ok(LoopAction::Continue)
        }
        UiEvent::CommandFailed(error) => {
            state.finish_command_failure();
            let plan = terminal.error(&error).merge(RenderPlan::REDRAW);
            state.apply(terminal, plan)?;
            Ok(LoopAction::Continue)
        }
        UiEvent::RollbackCompleted { prompt } => {
            state.operation.finish();
            if state.input.text() != prompt {
                state.input.restore_submission(prompt);
            }
            let effect = terminal.rollback_turn();
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        UiEvent::CompactionCompleted {
            before,
            after,
            dropped,
        } => {
            state.operation.finish();
            let effect = terminal.finish_compaction(before, after, dropped);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        UiEvent::SessionRestored { session_id, view } => {
            state.select_session(session_id);
            state.menu.close_picker();
            state.operation.finish();
            let effect = terminal.restore_session(&view);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        UiEvent::SessionsListed { sessions } => {
            state
                .operation
                .finish_background(BackgroundAction::ListSessions);
            let effect = if sessions.is_empty() {
                terminal.command_output("No saved chats are available to resume.")
            } else {
                state.menu.open_sessions(sessions);
                RenderPlan::REDRAW
            };
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        UiEvent::ForkPointsListed { points } => {
            state
                .operation
                .finish_background(BackgroundAction::ListForkPoints);
            let effect = if points.is_empty() {
                terminal.command_output("No submitted prompts are available to fork from.")
            } else {
                state.menu.open_fork_points(points);
                RenderPlan::REDRAW
            };
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        UiEvent::SessionForked {
            session_id,
            view,
            prompt,
        } => {
            state.select_session(session_id);
            state.menu.close_picker();
            state.operation.finish_background(BackgroundAction::Fork);
            let effect = terminal.restore_session(&view);
            state.input.set_text(prompt);
            state.apply(terminal, effect.merge(RenderPlan::REDRAW))?;
            Ok(LoopAction::Continue)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn handle_session_event(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    event: SessionEvent,
) -> anyhow::Result<LoopAction> {
    let turn_id = event.turn_id;
    match event.kind {
        SessionEventKind::TurnStarted => {
            let Some(turn_id) = turn_id else {
                return Ok(LoopAction::Continue);
            };
            if terminal
                .current_turn_id()
                .is_some_and(|current| current != turn_id)
            {
                return Ok(LoopAction::Continue);
            }
            terminal.track_turn(turn_id);
            let start = state.operation.turn_started();
            let effect = terminal.turn_started();
            state.apply(terminal, effect)?;
            Ok(match start {
                TurnStartOutcome::StartedTurn => LoopAction::ResetTimers,
                TurnStartOutcome::TurnAlreadyTracked => LoopAction::Continue,
            })
        }
        SessionEventKind::Live(_) if turn_id.is_none() || terminal.current_turn_id() != turn_id => {
            Ok(LoopAction::Continue)
        }
        SessionEventKind::Live(_) if !state.operation.accepts_live_output() => {
            Ok(LoopAction::Continue)
        }
        SessionEventKind::Live(LiveEvent::TextDelta(text)) => {
            let effect = terminal.text(&text);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        SessionEventKind::Live(LiveEvent::ReasoningDelta(text)) => {
            let effect = terminal.thinking(&text);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        SessionEventKind::Live(LiveEvent::ToolStarted {
            id,
            name,
            arguments,
        }) => {
            let effect = terminal.tool_started(id, name, arguments);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        SessionEventKind::Live(LiveEvent::ToolFinished {
            id,
            name,
            arguments,
            output,
            is_error,
        }) => {
            let effect = terminal.tool_finished(&id, &name, &arguments, &output, is_error);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        SessionEventKind::TurnCompleted(view) => {
            if turn_id != Some(view.id)
                || terminal
                    .current_turn_id()
                    .is_some_and(|current| current != view.id)
            {
                return Ok(LoopAction::Continue);
            }
            terminal.track_turn(view.id);
            let _ = state.operation.complete_turn();
            let effect = terminal.commit_turn(&view);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        SessionEventKind::ContextCompacted { after, .. } => {
            let effect = terminal.record_automatic_compaction(after);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
    }
}

async fn handle_terminal_event(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    event: CrosstermEvent,
) -> anyhow::Result<LoopAction> {
    match event {
        CrosstermEvent::Resize(width, height) => {
            state.resize(terminal, width, height)?;
            Ok(LoopAction::Continue)
        }
        CrosstermEvent::Paste(text) if !state.menu.picker_is_visible() => {
            state.input.insert_paste(&text);
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        CrosstermEvent::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            handle_key(state, terminal, commands, key).await
        }
        _ => Ok(LoopAction::Continue),
    }
}

async fn handle_key(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    key: KeyEvent,
) -> anyhow::Result<LoopAction> {
    if state.menu.session_picker_is_visible() {
        return handle_session_key(state, terminal, commands, key).await;
    }
    if state.menu.fork_picker_is_visible() {
        return handle_fork_key(state, terminal, commands, key).await;
    }
    if is_cancel_key(state, &key) {
        return cancel_turn(state, terminal, commands).await;
    }
    if handle_completion_key(state, terminal, key)? {
        return Ok(LoopAction::Continue);
    }

    match key.code {
        _ if inserts_newline(&key) => state.input.insert('\n'),
        KeyCode::PageUp => {
            terminal.scroll_page_up()?;
            return Ok(LoopAction::Continue);
        }
        KeyCode::PageDown => {
            terminal.scroll_page_down()?;
            return Ok(LoopAction::Continue);
        }
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
            terminal.scroll_to_top()?;
            return Ok(LoopAction::Continue);
        }
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
            terminal.scroll_to_bottom()?;
            return Ok(LoopAction::Continue);
        }
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let effect = terminal.toggle_tool_expanded();
            state.apply(terminal, effect)?;
            return Ok(LoopAction::Continue);
        }
        KeyCode::Enter => return submit_input(state, terminal, commands).await,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match ctrl_c_action(state.input.is_empty(), state.operation.can_cancel()) {
                CtrlCAction::ClearInput => state.input.clear(),
                CtrlCAction::CancelTurn => {
                    // A running turn treats Ctrl+C as a cancellation request
                    // (like Esc) instead of falling through to insert a literal
                    // 'c' into the composer.
                    return cancel_turn(state, terminal, commands).await;
                }
                CtrlCAction::Exit => {
                    let _ = commands.send(UiCommand::Exit).await;
                    return Ok(LoopAction::Exit);
                }
            }
        }
        KeyCode::Char('d')
            if key.modifiers.contains(KeyModifiers::CONTROL) && state.input.is_empty() =>
        {
            let _ = commands.send(UiCommand::Exit).await;
            return Ok(LoopAction::Exit);
        }
        KeyCode::Char(character) => state.input.insert(character),
        KeyCode::Backspace => state.input.backspace(),
        KeyCode::Delete => state.input.delete(),
        KeyCode::Left => state.input.move_left(),
        KeyCode::Right => state.input.move_right(),
        KeyCode::Home => state.input.move_home(),
        KeyCode::End => state.input.move_end(),
        KeyCode::Up => {
            if !state.input.move_up(TerminalUi::composer_text_width()?) {
                state.input.history_previous();
            }
        }
        KeyCode::Down => {
            if !state.input.move_down(TerminalUi::composer_text_width()?) {
                state.input.history_next();
            }
        }
        _ => return Ok(LoopAction::Continue),
    }
    state.render(terminal)?;
    Ok(LoopAction::Continue)
}

async fn handle_session_key(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    key: KeyEvent,
) -> anyhow::Result<LoopAction> {
    let Some(sessions) = state.menu.visible_session_picker_mut() else {
        return Ok(LoopAction::Continue);
    };
    let Some(action) = picker_key_action(sessions, key, SessionPickerState::selected_session_id)
    else {
        return Ok(LoopAction::Continue);
    };
    match action {
        PickerAction::KeepOpen => {}
        PickerAction::Close => state.menu.close_sessions(),
        PickerAction::Confirm(_) => {
            state.menu.close_sessions();
            state.operation.start_background(BackgroundAction::Resume);
        }
    }
    state.render(terminal)?;
    let PickerAction::Confirm(session_id) = action else {
        return Ok(LoopAction::Continue);
    };
    if commands
        .send(UiCommand::ResumeSession(session_id))
        .await
        .is_err()
    {
        return Ok(LoopAction::Exit);
    }
    Ok(LoopAction::Continue)
}

async fn handle_fork_key(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    key: KeyEvent,
) -> anyhow::Result<LoopAction> {
    let Some(points) = state.menu.visible_fork_picker_mut() else {
        return Ok(LoopAction::Continue);
    };
    let Some(action) = picker_key_action(points, key, ForkPickerState::selected_message_id) else {
        return Ok(LoopAction::Continue);
    };
    match action {
        PickerAction::KeepOpen => {}
        PickerAction::Close => state.menu.close_picker(),
        PickerAction::Confirm(_) => {
            state.menu.close_picker();
            state.operation.start_background(BackgroundAction::Fork);
        }
    }
    state.render(terminal)?;
    let PickerAction::Confirm(message_id) = action else {
        return Ok(LoopAction::Continue);
    };
    if commands
        .send(UiCommand::ForkSession(message_id))
        .await
        .is_err()
    {
        return Ok(LoopAction::Exit);
    }
    Ok(LoopAction::Continue)
}

fn picker_key_action<P, T>(
    picker: &mut P,
    key: KeyEvent,
    selected: impl FnOnce(&P) -> Option<T>,
) -> Option<PickerAction<T>>
where
    P: PickerNavigation,
    T: Copy,
{
    match key.code {
        KeyCode::Esc => Some(PickerAction::Close),
        KeyCode::Up => {
            picker.move_up();
            Some(PickerAction::KeepOpen)
        }
        KeyCode::Down => {
            picker.move_down();
            Some(PickerAction::KeepOpen)
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            picker.move_up();
            Some(PickerAction::KeepOpen)
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            picker.move_down();
            Some(PickerAction::KeepOpen)
        }
        KeyCode::Enter => Some(selected(picker).map_or(PickerAction::Close, PickerAction::Confirm)),
        _ => None,
    }
}

fn handle_completion_key(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    key: KeyEvent,
) -> anyhow::Result<bool> {
    let Some(completion) = state.menu.visible_completion_mut() else {
        return Ok(false);
    };
    match key.code {
        KeyCode::Esc => completion.dismiss(),
        KeyCode::Up => completion.move_up(),
        KeyCode::Down => completion.move_down(),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => completion.move_up(),
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            completion.move_down();
        }
        KeyCode::Tab => {
            if let Some(selected) = completion.selected() {
                state.input.set_text(format!("/{} ", selected.name));
            }
        }
        KeyCode::Enter if !inserts_newline(&key) => {
            if let Some(selected) = completion.selected() {
                state.input.set_text(format!("/{}", selected.name));
            }
            return Ok(false);
        }
        _ => return Ok(false),
    }
    state.render(terminal)?;
    Ok(true)
}

fn is_cancel_key(state: &AppState, key: &KeyEvent) -> bool {
    state.operation.can_cancel()
        && key.code == KeyCode::Esc
        && key.kind == KeyEventKind::Press
        && key.modifiers.is_empty()
}

/// What Ctrl+C should do in the composer. With a draft it clears the input;
/// while a turn is running it cancels the turn (matching Esc); otherwise it
/// exits the application. Kept as a pure function so the key dispatch stays
/// testable.
#[derive(Debug, Eq, PartialEq)]
enum CtrlCAction {
    ClearInput,
    CancelTurn,
    Exit,
}

const fn ctrl_c_action(input_is_empty: bool, can_cancel: bool) -> CtrlCAction {
    if !input_is_empty {
        CtrlCAction::ClearInput
    } else if can_cancel {
        CtrlCAction::CancelTurn
    } else {
        CtrlCAction::Exit
    }
}

async fn cancel_turn(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
) -> anyhow::Result<LoopAction> {
    terminal.prepare_cancellation();
    if !state.operation.begin_cancellation() {
        return Ok(LoopAction::Continue);
    }
    state.render(terminal)?;
    Ok(if commands.send(UiCommand::Cancel).await.is_err() {
        LoopAction::Exit
    } else {
        LoopAction::Continue
    })
}

async fn submit_input(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
) -> anyhow::Result<LoopAction> {
    let Some(policy) = state.operation.submission_policy() else {
        return Ok(LoopAction::Continue);
    };
    let parsed = slash_command::parse(state.input.text());
    if submission_is_blocked(policy, &parsed) {
        return Ok(LoopAction::Continue);
    }
    let input = state.input.submit();
    if input.trim().is_empty() {
        state.apply(terminal, RenderPlan::REDRAW)?;
        return Ok(LoopAction::Continue);
    }
    if slash_command::is_bare_exit(&input) {
        terminal.commit_exit(&input);
        let _ = commands.send(UiCommand::Exit).await;
        return Ok(LoopAction::Exit);
    }

    match (policy, parsed) {
        (SubmissionPolicy::Start, ParsedInput::Message) => {
            start_message(state, terminal, commands, input).await
        }
        (SubmissionPolicy::Steer, ParsedInput::Message) => {
            steer_message(state, terminal, commands, input).await
        }
        (_, ParsedInput::Invalid(error)) => {
            let effect = terminal.command_error(&error);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        (_, ParsedInput::Command(command)) => {
            run_command(state, terminal, commands, command, &input).await
        }
    }
}

async fn start_message(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    input: String,
) -> anyhow::Result<LoopAction> {
    let (reply, result) = tokio::sync::oneshot::channel();
    let command = UiCommand::Submit {
        input: input.clone(),
        reply,
    };
    match send_confirmed(commands, command, result).await {
        Ok(turn_id) => {
            state.operation.start_turn();
            let effect = terminal.commit_input(turn_id, &input);
            state.input.record_submission(&input);
            state.apply(terminal, effect)?;
            Ok(LoopAction::ResetTimers)
        }
        Err(error) => reject_input(state, terminal, input, "submit", &error),
    }
}

async fn steer_message(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    input: String,
) -> anyhow::Result<LoopAction> {
    let (reply, result) = tokio::sync::oneshot::channel();
    let command = UiCommand::Steer {
        input: input.clone(),
        reply,
    };
    match send_confirmed(commands, command, result).await {
        Ok(()) => {
            let effect = terminal.commit_steer(&input);
            state.input.record_submission(&input);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        Err(error) => reject_input(state, terminal, input, "steer", &error),
    }
}

async fn send_confirmed<T>(
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    command: UiCommand,
    result: tokio::sync::oneshot::Receiver<Result<T, UiError>>,
) -> Result<T, UiError> {
    commands
        .send(command)
        .await
        .map_err(|_| UiError::ControllerStopped)?;
    result.await.map_err(|_| UiError::InputDropped)?
}

fn reject_input(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    input: String,
    action: &str,
    error: &UiError,
) -> anyhow::Result<LoopAction> {
    state.input.set_text(input);
    let effect = terminal.error(&format!("Failed to {action} input: {error}"));
    state.apply(terminal, effect)?;
    Ok(LoopAction::Continue)
}

async fn run_command(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    command: SlashCommand,
    input: &str,
) -> anyhow::Result<LoopAction> {
    let mut effect = RenderPlan::REDRAW;
    let outgoing = match command {
        SlashCommand::New | SlashCommand::Clear => {
            state.menu.close_picker();
            effect = effect.merge(terminal.start_new_session());
            Some(UiCommand::NewSession)
        }
        SlashCommand::Resume => {
            state
                .operation
                .start_background(BackgroundAction::ListSessions);
            Some(UiCommand::ListSessions)
        }
        SlashCommand::Undo => {
            state.operation.start_background(BackgroundAction::Rollback);
            Some(UiCommand::Rollback)
        }
        SlashCommand::Fork => {
            state
                .operation
                .start_background(BackgroundAction::ListForkPoints);
            Some(UiCommand::ListForkPoints)
        }
        SlashCommand::Compact => {
            state.operation.start_background(BackgroundAction::Compact);
            Some(UiCommand::Compact)
        }
        SlashCommand::Status => {
            effect = effect.merge(terminal.show_session_status());
            None
        }
        SlashCommand::Exit => {
            terminal.commit_exit(input);
            let _ = commands.send(UiCommand::Exit).await;
            return Ok(LoopAction::Exit);
        }
    };
    state.apply(terminal, effect)?;
    if let Some(command) = outgoing {
        if commands.send(command).await.is_err() {
            return Ok(LoopAction::Exit);
        }
    }
    Ok(LoopAction::Continue)
}

const fn submission_is_blocked(policy: SubmissionPolicy, input: &ParsedInput) -> bool {
    match (policy, input) {
        (SubmissionPolicy::Start, _) => false,
        (SubmissionPolicy::Steer, ParsedInput::Message) => false,
        (SubmissionPolicy::Steer, ParsedInput::Command(command)) => !command.can_run_while_busy(),
        (SubmissionPolicy::Steer, ParsedInput::Invalid(_)) => true,
    }
}

fn inserts_newline(key: &KeyEvent) -> bool {
    (key.code == KeyCode::Enter
        && key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT))
        || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
}

#[cfg(test)]
mod tests {
    use crate::operation::TurnCompletion;

    use super::*;

    #[test]
    fn derives_terminal_view_from_one_app_state() {
        let mut state = AppState::new(Vec::new());
        state.input.set_text("/");
        state.operation.start_turn();

        state.sync_menu();
        let view = state.view();

        assert!(view.activity.is_active());
        let crate::menu::MenuView::Commands { items, .. } = view.menu else {
            panic!("expected command completions");
        };
        assert_eq!(items.len(), 8);
    }

    #[test]
    fn cancellation_finishes_when_the_turn_finishes() {
        let mut state = AppState::new(Vec::new());
        state.operation.start_turn();
        assert!(state.operation.begin_cancellation());

        assert_eq!(state.operation.complete_turn(), TurnCompletion::Cancelled);
        assert!(!state.operation.is_busy());
        assert!(state.input.is_empty());
    }

    #[test]
    fn command_failure_releases_a_cancelling_operation() {
        let mut state = AppState::new(Vec::new());
        state.operation.start_turn();
        assert!(state.operation.begin_cancellation());

        state.finish_command_failure();

        assert!(!state.operation.is_busy());
        assert_eq!(
            state.operation.submission_policy(),
            Some(SubmissionPolicy::Start)
        );
    }

    #[test]
    fn session_events_require_the_current_session_and_newer_sequence() {
        let mut state = AppState::new(Vec::new());
        let current = SessionId::new();
        let other = SessionId::new();
        state.select_session(current);

        assert!(state.accept_session_event(current, 1));
        assert!(!state.accept_session_event(current, 1));
        assert!(!state.accept_session_event(other, 2));
        assert!(state.accept_session_event(current, 2));
    }

    #[test]
    fn escape_cancels_a_running_turn_even_with_a_draft() {
        let mut state = AppState::new(Vec::new());
        state.operation.start_turn();
        state.input.set_text("keep this draft");
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        assert!(is_cancel_key(&state, &escape));
        assert_eq!(state.input.text(), "keep this draft");
    }

    #[test]
    fn ctrl_c_clears_cancels_or_exits_without_inserting_a_character() {
        // With a draft, Ctrl+C clears the input.
        assert_eq!(ctrl_c_action(false, false), CtrlCAction::ClearInput);
        assert_eq!(ctrl_c_action(false, true), CtrlCAction::ClearInput);
        // While a turn is running and the composer is empty, Ctrl+C cancels
        // the turn instead of inserting a literal 'c'.
        assert_eq!(ctrl_c_action(true, true), CtrlCAction::CancelTurn);
        // Compacting or cancelling is busy but not interruptible: empty Ctrl+C exits.
        assert_eq!(ctrl_c_action(true, false), CtrlCAction::Exit);
    }

    #[test]
    fn unavailable_input_is_blocked_without_changing_the_draft() {
        let mut state = AppState::new(Vec::new());
        state.operation.start_turn();
        state.input.set_text("/new");

        let policy = state.operation.submission_policy().unwrap();
        let parsed = slash_command::parse(state.input.text());
        assert!(submission_is_blocked(policy, &parsed));
        assert_eq!(state.input.text(), "/new");

        let parsed = slash_command::parse("/status");
        assert!(submission_is_blocked(policy, &parsed));

        let parsed = slash_command::parse("/missing");
        assert!(submission_is_blocked(policy, &parsed));

        let parsed = slash_command::parse("steer this turn");
        assert!(!submission_is_blocked(policy, &parsed));

        let parsed = slash_command::parse("/exit");
        assert!(!submission_is_blocked(policy, &parsed));

        assert!(state.operation.begin_cancellation());
        assert_eq!(state.operation.submission_policy(), None);
        assert_eq!(state.input.text(), "/new");
    }

    #[tokio::test]
    async fn confirmed_input_returns_the_controller_rejection() {
        let (commands, mut incoming) = tokio::sync::mpsc::channel(1);
        let client = tokio::spawn(async move {
            let (reply, result) = tokio::sync::oneshot::channel();
            send_confirmed(
                &commands,
                UiCommand::Steer {
                    input: "change direction".into(),
                    reply,
                },
                result,
            )
            .await
        });

        let Some(UiCommand::Steer { input, reply }) = incoming.recv().await else {
            panic!("expected steer command");
        };
        assert_eq!(input, "change direction");
        reply
            .send(Err(UiError::Agent(AshError::Session(
                SessionError::InactiveTurn,
            ))))
            .unwrap();

        let error = client.await.unwrap().unwrap_err();
        assert_eq!(error.to_string(), SessionError::InactiveTurn.to_string());
    }

    #[test]
    fn modified_enter_inserts_a_newline() {
        assert!(inserts_newline(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));
        assert!(inserts_newline(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT
        )));
        assert!(inserts_newline(&KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL
        )));
        assert!(!inserts_newline(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
    }
}
