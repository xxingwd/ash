use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use ash_collab::SubagentSnapshot;
#[cfg(test)]
use ash_core::SessionError;
use ash_core::{AshError, EventKind, LiveEvent, Message, MessageId, SessionId, TurnId, TurnView};
use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use futures::StreamExt;

use crate::{
    fork_picker::ForkPickerState,
    inline::{TerminalUi, TerminalView},
    input::InputState,
    menu::ComposerMenuState,
    operation::{
        AgentStart, BackgroundAction, FailureCompletion, OperationState, SubmissionPolicy,
        TurnCompletion,
    },
    session_picker::SessionPickerState,
    slash_command::{self, CommandCompletionState, ParsedInput, SlashCommand},
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

struct AppState {
    input: InputState,
    operation: OperationState,
    menu: ComposerMenuState,
    subagent_rx: Option<tokio::sync::watch::Receiver<Vec<SubagentSnapshot>>>,
    subagents: Vec<SubagentSnapshot>,
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

    fn sync_menu(&mut self) {
        self.menu
            .sync_commands(self.input.text(), self.input.cursor());
    }

    fn view(&self) -> TerminalView<'_> {
        TerminalView {
            input: &self.input,
            menu: self.menu.view(),
            busy: self.operation.shows_activity(),
            interruptible: self.operation.can_cancel(),
            status_header: self.operation.status_header(),
        }
    }

    fn render(&mut self, terminal: &mut TerminalUi) -> std::io::Result<()> {
        self.sync_menu();
        terminal.sync_view(self.view())
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
    subagent_monitor: Option<tokio::sync::watch::Receiver<Vec<SubagentSnapshot>>>,
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
        subagent_monitor: Option<tokio::sync::watch::Receiver<Vec<SubagentSnapshot>>>,
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
        mut events: impl futures::Stream<Item = EventKind> + Unpin,
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
        terminal.welcome()?;
        state.render(&mut terminal)?;

        loop {
            tokio::select! {
                _ = status_tick.tick() => {
                    if state.refresh_subagents() {
                        terminal.set_subagents(state.subagents.clone())?;
                    }
                    if state.operation.shows_activity() {
                        terminal.refresh_status()?;
                    }
                }
                event = events.next() => {
                    let Some(event) = event else { break };
                    match handle_agent_event(&mut state, &mut terminal, &commands, event).await? {
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

// One branch per `EventKind`: an exhaustive match over the whole event
// vocabulary, where each branch only touches the few structures it needs.
// Splitting this into per-event handlers would session `state`, `terminal`,
// and `commands` through every call for no readability gain.
#[allow(clippy::too_many_lines)]
async fn handle_agent_event(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    event: EventKind,
) -> anyhow::Result<LoopAction> {
    match event {
        EventKind::TurnStart => {
            let start = state.operation.agent_started();
            terminal.agent_started()?;
            state.render(terminal)?;
            Ok(match start {
                AgentStart::StartedTurn => LoopAction::ResetTimers,
                AgentStart::TurnAlreadyTracked => LoopAction::Continue,
            })
        }
        EventKind::Live(_) if !state.operation.accepts_live_output() => Ok(LoopAction::Continue),
        EventKind::Live(LiveEvent::TextDelta(text)) => {
            terminal.text(&text)?;
            Ok(LoopAction::Continue)
        }
        EventKind::Live(LiveEvent::ReasoningDelta(text)) => {
            terminal.thinking(&text)?;
            Ok(LoopAction::Continue)
        }
        EventKind::Live(LiveEvent::ToolStarted {
            id,
            name,
            arguments,
        }) => {
            terminal.tool_started(id, name, arguments)?;
            Ok(LoopAction::Continue)
        }
        EventKind::Live(LiveEvent::ToolFinished {
            id,
            name,
            arguments,
            output,
            is_error,
        }) => {
            terminal.tool_finished(&id, &name, &arguments, &output, is_error)?;
            Ok(LoopAction::Continue)
        }
        EventKind::Error(error) => {
            terminal.error(&error)?;
            match state.operation.complete_failed_action() {
                FailureCompletion::FinishedOperation => {
                    state.menu.close_picker();
                    state.render(terminal)?;
                }
                FailureCompletion::OperationUnchanged => {}
            }
            Ok(LoopAction::Continue)
        }
        EventKind::Turn(view) => {
            match state.operation.complete_turn() {
                TurnCompletion::Commit => {
                    terminal.commit_turn(&view)?;
                    state.render(terminal)?;
                }
                TurnCompletion::Cancelled => {
                    if turn_has_completed_tool(&view) {
                        terminal.commit_turn(&view)?;
                        state.render(terminal)?;
                    } else {
                        state.operation.start_background(BackgroundAction::Rollback);
                        if commands.send(UiCommand::Rollback).await.is_err() {
                            return Ok(LoopAction::Exit);
                        }
                    }
                }
            }
            Ok(LoopAction::Continue)
        }
        EventKind::TurnRolledBack { prompt } => {
            state
                .operation
                .finish_background(BackgroundAction::Rollback);
            if state.input.text() != prompt {
                state.input.restore_submission(prompt);
            }
            terminal.rollback_turn()?;
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        EventKind::Compacted {
            before,
            after,
            dropped,
            automatic,
        } => {
            if automatic {
                terminal.record_automatic_compaction(after)?;
            } else {
                state.operation.finish();
                terminal.finish_compaction(before, after, dropped)?;
            }
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        EventKind::Restored { view } => {
            state.menu.close_picker();
            state.operation.finish();
            terminal.restore_session(&view)?;
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        EventKind::SessionsListed { sessions } => {
            state
                .operation
                .finish_background(BackgroundAction::ListSessions);
            if sessions.is_empty() {
                terminal.command_output("No saved chats are available to resume.")?;
            } else {
                state.menu.open_sessions(sessions);
            }
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        EventKind::ForkPointsListed { points } => {
            state
                .operation
                .finish_background(BackgroundAction::ListForkPoints);
            if points.is_empty() {
                terminal.command_output("No submitted prompts are available to fork from.")?;
            } else {
                state.menu.open_fork_points(points);
            }
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        EventKind::SessionForked { view, prompt } => {
            state.menu.close_picker();
            state.operation.finish_background(BackgroundAction::Fork);
            terminal.restore_session(&view)?;
            state.input.set_text(prompt);
            state.render(terminal)?;
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
            terminal.toggle_tool_expanded()?;
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
    let _ = terminal.prepare_cancellation();
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
    state.render(terminal)?;
    if input.trim().is_empty() {
        return Ok(LoopAction::Continue);
    }
    if slash_command::is_bare_exit(&input) {
        terminal.commit_exit(&input)?;
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
            terminal.command_error(&error)?;
            state.render(terminal)?;
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
            terminal.commit_input(turn_id, &input)?;
            state.input.record_submission(&input);
            state.render(terminal)?;
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
            terminal.commit_steer(&input)?;
            state.input.record_submission(&input);
            state.render(terminal)?;
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
    terminal.error(&format!("Failed to {action} input: {error}"))?;
    state.render(terminal)?;
    Ok(LoopAction::Continue)
}

async fn run_command(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    command: SlashCommand,
    input: &str,
) -> anyhow::Result<LoopAction> {
    let outgoing = match command {
        SlashCommand::New | SlashCommand::Clear => {
            state.menu.close_picker();
            terminal.start_new_session()?;
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
            terminal.show_session_status()?;
            None
        }
        SlashCommand::Exit => {
            terminal.commit_exit(input)?;
            let _ = commands.send(UiCommand::Exit).await;
            return Ok(LoopAction::Exit);
        }
    };
    state.render(terminal)?;
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

fn turn_has_completed_tool(view: &TurnView) -> bool {
    view.messages.iter().any(message_has_completed_tool)
}

fn message_has_completed_tool(message: &Message) -> bool {
    match &message.content {
        ash_core::MessageContent::ToolResult { .. } => true,
        ash_core::MessageContent::User(_)
        | ash_core::MessageContent::Assistant(_)
        | ash_core::MessageContent::System(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_terminal_view_from_one_app_state() {
        let mut state = AppState::new(Vec::new());
        state.input.set_text("/");
        state.operation.start_turn();

        state.sync_menu();
        let view = state.view();

        assert!(view.busy);
        let crate::menu::MenuView::Commands { items, .. } = view.menu else {
            panic!("expected command completions");
        };
        assert_eq!(items.len(), 8);
    }

    #[test]
    fn cancelled_turns_keep_completed_tool_results() {
        let call = ash_core::ToolCallId::from_provider("call-1");
        let kept = TurnView {
            id: ash_core::TurnId::new(),
            result: ash_core::TurnResult::Completed(ash_core::StopReason::Aborted),
            messages: vec![ash_core::Message::tool_result(
                call,
                Ok("done".into()),
                Vec::new(),
            )],
            usage: None,
            context_tokens: None,
        };
        let rolled_back = TurnView {
            id: ash_core::TurnId::new(),
            result: ash_core::TurnResult::Completed(ash_core::StopReason::Aborted),
            messages: vec![ash_core::Message::assistant_text("partial")],
            usage: None,
            context_tokens: None,
        };

        assert!(turn_has_completed_tool(&kept));
        assert!(!turn_has_completed_tool(&rolled_back));
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
