use std::{collections::HashMap, fmt, path::PathBuf, sync::Arc, time::Duration};

#[cfg(test)]
use ash_core::SessionError;
use ash_core::{AshError, Conversation, Input, SessionEvent, SessionId, SessionSummary, TurnId};
use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use futures::StreamExt;

use crate::{
    inline::{BlockStore, RenderPlan, StatusState, TerminalUi},
    input::InputState,
    menu::ComposerMenuState,
    operation::{BackgroundAction, OperationState, SubmissionPolicy, TurnStartOutcome},
    picker::PickerState,
    slash_command::{self, ParsedInput, SlashCommand},
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
    Cancel,
    Undo,
    Compact,
    NewSession,
    ListSessions,
    ResumeSession(SessionId),
    ForkSession(TurnId),
    Exit,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Session(SessionEvent),
    ConversationChanged {
        session_id: SessionId,
        conversation: Conversation,
        input: Option<Input>,
    },
    CompactionCompleted(bool),
    SessionsListed {
        sessions: Vec<SessionSummary>,
    },
    CommandFailed(String),
}

pub(crate) struct AppState {
    pub(crate) protocol: String,
    pub(crate) model: String,
    pub(crate) working_dir: PathBuf,
    pub(crate) input: InputState,
    pub(crate) operation: OperationState,
    pub(crate) menu: ComposerMenuState,
    pub(crate) subagent_rx: Option<tokio::sync::watch::Receiver<Vec<SubagentView>>>,
    pub(crate) subagents: Vec<SubagentView>,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) conversation: Conversation,
    pub(crate) inputs: HashMap<TurnId, String>,
    pub(crate) blocks: BlockStore,
    pub(crate) tools_expanded: bool,
    pub(crate) scroll_top: Option<u16>,
    pub(crate) next_block_id: u64,
    pub(crate) status: StatusState,
    pub(crate) current_turn_id: Option<TurnId>,
    pub(crate) reasoning_block_id: Option<u64>,
    pub(crate) assistant_block_id: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

impl AppState {
    fn new(
        input_history: Vec<String>,
        protocol: String,
        model: String,
        working_dir: PathBuf,
    ) -> Self {
        Self {
            protocol,
            model,
            working_dir,
            input: InputState::with_history(input_history),
            operation: OperationState::default(),
            menu: ComposerMenuState::default(),
            subagent_rx: None,
            subagents: Vec::new(),
            session_id: None,
            conversation: Conversation::new(),
            inputs: HashMap::new(),
            blocks: BlockStore::default(),
            tools_expanded: false,
            scroll_top: None,
            next_block_id: 1,
            status: StatusState::default(),
            current_turn_id: None,
            reasoning_block_id: None,
            assistant_block_id: None,
        }
    }

    fn refresh_subagents(&mut self) -> bool {
        let Some(receiver) = &mut self.subagent_rx else {
            return false;
        };
        match receiver.has_changed() {
            Ok(true) => {
                let subagents = visible_subagents(&receiver.borrow_and_update(), self.session_id);
                self.replace_subagents(subagents)
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

    fn select_session(&mut self, session_id: SessionId, conversation: Conversation) -> bool {
        self.session_id = Some(session_id);
        self.conversation = conversation;
        self.inputs.clear();
        let subagents = self.subagent_rx.as_ref().map_or_else(Vec::new, |receiver| {
            visible_subagents(&receiver.borrow(), self.session_id)
        });
        self.replace_subagents(subagents)
    }

    fn replace_subagents(&mut self, subagents: Vec<SubagentView>) -> bool {
        if self.subagents == subagents {
            false
        } else {
            self.subagents = subagents;
            true
        }
    }

    fn queue_input(&mut self, turn_id: TurnId, input: String) {
        self.inputs.insert(turn_id, input);
    }

    fn input(&self, turn_id: TurnId) -> Option<&str> {
        self.inputs.get(&turn_id).map(String::as_str)
    }

    fn finish_turn(&mut self, turn_id: TurnId) -> Option<String> {
        self.inputs.remove(&turn_id)
    }

    fn finish_command_failure(&mut self) {
        self.operation.finish();
        self.menu.close_picker();
    }

    fn sync_menu(&mut self) {
        self.menu
            .sync_commands(self.input.text(), self.input.cursor());
    }

    fn render(&mut self, terminal: &mut TerminalUi) -> std::io::Result<()> {
        self.apply(terminal, RenderPlan::Redraw)
    }

    fn apply(&mut self, terminal: &mut TerminalUi, plan: RenderPlan) -> std::io::Result<()> {
        self.sync_menu();
        self.status
            .set_active(self.operation.activity_view().is_active());
        terminal.apply_plan(self, plan)
    }

    fn resize(
        &mut self,
        terminal: &mut TerminalUi,
        width: u16,
        height: u16,
    ) -> std::io::Result<()> {
        self.sync_menu();
        terminal.resize_view(self, width, height)
    }
}

fn visible_subagents(subagents: &[SubagentView], root_id: Option<SessionId>) -> Vec<SubagentView> {
    subagents
        .iter()
        .filter(|subagent| Some(subagent.root_id) == root_id)
        .cloned()
        .collect()
}

pub struct App {
    protocol: String,
    model: String,
    working_dir: PathBuf,
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
            input_history: Vec::new(),
            subagent_monitor: None,
        }
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
        let mut terminal = TerminalUi::enter()?;
        let mut state = AppState::new(
            std::mem::take(&mut self.input_history),
            self.protocol,
            self.model,
            self.working_dir,
        );
        state.subagent_rx = self.subagent_monitor.take();
        let mut keys = EventStream::new();
        let mut status_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + STATUS_INTERVAL,
            STATUS_INTERVAL,
        );
        status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let effect = state.welcome();
        state.apply(&mut terminal, effect)?;

        loop {
            tokio::select! {
                _ = status_tick.tick() => {
                    let mut effect = RenderPlan::None;
                    if state.refresh_subagents() {
                        effect = effect.merge(RenderPlan::Redraw);
                    }
                    if state.operation.shows_activity() {
                        effect = effect.merge(state.refresh_status());
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

        terminal.leave(&mut state)?;
        Ok(())
    }
}

fn handle_ui_event(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    event: UiEvent,
) -> anyhow::Result<LoopAction> {
    match event {
        UiEvent::Session(event) => handle_session_event(state, terminal, event),
        UiEvent::CommandFailed(error) => {
            state.finish_command_failure();
            let plan = state.error(&error).merge(RenderPlan::Redraw);
            state.apply(terminal, plan)?;
            Ok(LoopAction::Continue)
        }
        UiEvent::CompactionCompleted(changed) => {
            state.operation.finish();
            let effect = state.finish_compaction(changed);
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        UiEvent::ConversationChanged {
            session_id,
            conversation,
            input,
        } => {
            let changed = state.select_session(session_id, conversation);
            state.menu.close_picker();
            state.operation.finish();
            if let Some(input) = input {
                state.input.set_text(input.text());
            }
            let mut effect = state.restore_session();
            if changed {
                effect = effect.merge(RenderPlan::Redraw);
            }
            state.apply(terminal, effect)?;
            Ok(LoopAction::Continue)
        }
        UiEvent::SessionsListed { sessions } => {
            state
                .operation
                .finish_background(BackgroundAction::ListSessions);
            let effect = if sessions.is_empty() {
                state.command_output("No saved chats are available to resume.")
            } else {
                state.menu.open_sessions(sessions);
                RenderPlan::Redraw
            };
            state.apply(terminal, effect)?;
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
    let (plan, action) = update_session(state, event);
    state.apply(terminal, plan)?;
    Ok(action)
}

#[allow(clippy::too_many_lines)]
fn update_session(state: &mut AppState, event: SessionEvent) -> (RenderPlan, LoopAction) {
    match event {
        SessionEvent::Started(turn_id) => {
            if state
                .current_turn_id()
                .is_some_and(|current| current != turn_id)
            {
                return (RenderPlan::None, LoopAction::Continue);
            }
            let input_effect = if state.current_turn_id().is_none() {
                state
                    .input(turn_id)
                    .map(str::to_string)
                    .map_or(RenderPlan::None, |input| {
                        state.commit_input(turn_id, &input)
                    })
            } else {
                RenderPlan::None
            };
            state.track_turn(turn_id);
            let start = state.operation.turn_started();
            let effect = state.turn_started().merge(input_effect);
            let action = match start {
                TurnStartOutcome::StartedTurn => LoopAction::ResetTimers,
                TurnStartOutcome::TurnAlreadyTracked => LoopAction::Continue,
            };
            (effect, action)
        }
        SessionEvent::Text { turn_id, text }
            if state.current_turn_id() == Some(turn_id)
                && state.operation.accepts_live_output() =>
        {
            let effect = state.text(&text);
            (effect, LoopAction::Continue)
        }
        SessionEvent::Thought { turn_id, text }
            if state.current_turn_id() == Some(turn_id)
                && state.operation.accepts_live_output() =>
        {
            let effect = state.thinking(&text);
            (effect, LoopAction::Continue)
        }
        SessionEvent::ToolStarted {
            turn_id,
            id,
            name,
            arguments,
        } if state.current_turn_id() == Some(turn_id) && state.operation.accepts_live_output() => {
            let effect = state.tool_started(id, name, arguments);
            (effect, LoopAction::Continue)
        }
        SessionEvent::ToolFinished {
            turn_id,
            id,
            result,
        } if state.current_turn_id() == Some(turn_id) && state.operation.accepts_live_output() => {
            let (output, is_error) = match &result {
                Ok(output) => (output.as_str(), false),
                Err(error) => (error.as_str(), true),
            };
            let effect = state.tool_finished(&id, output, is_error);
            (effect, LoopAction::Continue)
        }
        SessionEvent::Finished(turn) => {
            let owns_turn = state.current_turn_id() == Some(turn.id)
                || (state.current_turn_id().is_none() && state.inputs.contains_key(&turn.id));
            if !owns_turn {
                return (RenderPlan::None, LoopAction::Continue);
            }
            state.track_turn(turn.id);
            state.operation.complete_turn();
            state.finish_turn(turn.id);
            state.conversation.push(Arc::clone(&turn), None);
            let effect = state.commit_turn(&turn);
            (effect, LoopAction::Continue)
        }
        SessionEvent::Discarded { turn_id, error }
            if state.current_turn_id() == Some(turn_id)
                || (state.current_turn_id().is_none() && state.inputs.contains_key(&turn_id)) =>
        {
            let input = state.finish_turn(turn_id);
            state.operation.complete_turn();
            if let Some(input) = input {
                state.input.restore_submission(input);
            }
            let mut effect = state.discard_turn(turn_id);
            if let Some(error) = error {
                effect = effect.merge(state.error(&error));
            }
            (effect, LoopAction::Continue)
        }
        SessionEvent::Text { .. }
        | SessionEvent::Thought { .. }
        | SessionEvent::ToolStarted { .. }
        | SessionEvent::ToolFinished { .. }
        | SessionEvent::Discarded { .. } => (RenderPlan::None, LoopAction::Continue),
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
            terminal.scroll_page_up(state)?;
            return Ok(LoopAction::Continue);
        }
        KeyCode::PageDown => {
            terminal.scroll_page_down(state)?;
            return Ok(LoopAction::Continue);
        }
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let effect = state.scroll_to_top();
            state.apply(terminal, effect)?;
            return Ok(LoopAction::Continue);
        }
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let effect = state.scroll_to_bottom();
            state.apply(terminal, effect)?;
            return Ok(LoopAction::Continue);
        }
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let effect = state.toggle_tool_expanded();
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
    let Some(action) = picker_key_action(sessions, key) else {
        return Ok(LoopAction::Continue);
    };
    let action = action.map(|index| sessions.items()[index].session_id);
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
    let Some(action) = picker_key_action(points, key) else {
        return Ok(LoopAction::Continue);
    };
    let action = action.map(|index| points.items()[index].turn_id);
    match action {
        PickerAction::KeepOpen => {}
        PickerAction::Close => state.menu.close_picker(),
        PickerAction::Confirm(_) => {
            state.menu.close_picker();
            state.operation.start_background(BackgroundAction::Fork);
        }
    }
    state.render(terminal)?;
    let PickerAction::Confirm(turn_id) = action else {
        return Ok(LoopAction::Continue);
    };
    if commands
        .send(UiCommand::ForkSession(turn_id))
        .await
        .is_err()
    {
        return Ok(LoopAction::Exit);
    }
    Ok(LoopAction::Continue)
}

fn picker_key_action<T>(picker: &mut PickerState<T>, key: KeyEvent) -> Option<PickerAction<usize>> {
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
        KeyCode::Enter => Some(if picker.is_visible() {
            PickerAction::Confirm(picker.selected_index())
        } else {
            PickerAction::Close
        }),
        _ => None,
    }
}

impl<T> PickerAction<T> {
    fn map<U>(self, map: impl FnOnce(T) -> U) -> PickerAction<U> {
        match self {
            Self::KeepOpen => PickerAction::KeepOpen,
            Self::Close => PickerAction::Close,
            Self::Confirm(value) => PickerAction::Confirm(map(value)),
        }
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
    state.prepare_cancellation();
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
        state.apply(terminal, RenderPlan::Redraw)?;
        return Ok(LoopAction::Continue);
    }
    if slash_command::is_bare_exit(&input) {
        state.commit_exit(&input);
        let _ = commands.send(UiCommand::Exit).await;
        return Ok(LoopAction::Exit);
    }

    match (policy, parsed) {
        (SubmissionPolicy::Start, ParsedInput::Message) => {
            start_message(state, terminal, commands, input).await
        }
        (SubmissionPolicy::Queue, ParsedInput::Message) => {
            queue_message(state, terminal, commands, input).await
        }
        (_, ParsedInput::Invalid(error)) => {
            let effect = state.command_error(&error);
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
            state.queue_input(turn_id, input.clone());
            let effect = state.commit_input(turn_id, &input);
            state.input.record_submission(&input);
            state.apply(terminal, effect)?;
            Ok(LoopAction::ResetTimers)
        }
        Err(error) => reject_input(state, terminal, input, "submit", &error),
    }
}

async fn queue_message(
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
            state.queue_input(turn_id, input.clone());
            state.input.record_submission(&input);
            state.apply(terminal, RenderPlan::Redraw)?;
            Ok(LoopAction::Continue)
        }
        Err(error) => reject_input(state, terminal, input, "queue", &error),
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
    let effect = state.error(&format!("Failed to {action} input: {error}"));
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
    let mut effect = RenderPlan::Redraw;
    let outgoing = match command {
        SlashCommand::New | SlashCommand::Clear => {
            state.menu.close_picker();
            effect = effect.merge(state.start_new_session());
            Some(UiCommand::NewSession)
        }
        SlashCommand::Resume => {
            state
                .operation
                .start_background(BackgroundAction::ListSessions);
            Some(UiCommand::ListSessions)
        }
        SlashCommand::Undo => {
            state.operation.start_background(BackgroundAction::Undo);
            Some(UiCommand::Undo)
        }
        SlashCommand::Fork => {
            let points = state
                .conversation
                .turns()
                .iter()
                .rev()
                .map(|turn| crate::fork_picker::ForkOption {
                    turn_id: turn.id,
                    prompt: turn.input.text(),
                })
                .collect::<Vec<_>>();
            if points.is_empty() {
                effect = effect.merge(
                    state.command_output("No submitted prompts are available to fork from."),
                );
            } else {
                state.menu.open_fork_points(points);
            }
            None
        }
        SlashCommand::Compact => {
            state.operation.start_background(BackgroundAction::Compact);
            Some(UiCommand::Compact)
        }
        SlashCommand::Status => {
            effect = effect.merge(state.show_session_status());
            None
        }
        SlashCommand::Exit => {
            state.commit_exit(input);
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
        (SubmissionPolicy::Start, _) | (SubmissionPolicy::Queue, ParsedInput::Message) => false,
        (SubmissionPolicy::Queue, ParsedInput::Command(command)) => !command.can_run_while_busy(),
        (SubmissionPolicy::Queue, ParsedInput::Invalid(_)) => true,
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
    use crate::SubagentViewState;

    use super::*;

    fn test_state() -> AppState {
        AppState::new(
            Vec::new(),
            "openai".to_string(),
            "mock".to_string(),
            PathBuf::from("/tmp/ash"),
        )
    }

    #[test]
    fn derives_terminal_view_from_one_app_state() {
        let mut state = test_state();
        state.input.set_text("/");
        state.operation.start_turn();

        state.sync_menu();
        assert!(state.operation.activity_view().is_active());
        let crate::menu::MenuView::Commands { items, .. } = state.menu.view() else {
            panic!("expected command completions");
        };
        assert_eq!(items.len(), 8);
    }

    #[test]
    fn cancellation_finishes_when_the_turn_finishes() {
        let mut state = test_state();
        state.operation.start_turn();
        assert!(state.operation.begin_cancellation());

        state.operation.complete_turn();
        assert!(!state.operation.is_busy());
        assert!(state.input.is_empty());
    }

    #[test]
    fn command_failure_releases_a_cancelling_operation() {
        let mut state = test_state();
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
    fn session_selection_filters_subagents_by_root() {
        let first = SessionId::new();
        let second = SessionId::new();
        let (_, receiver) = tokio::sync::watch::channel(vec![
            SubagentView {
                root_id: first,
                name: "first".to_string(),
                state: SubagentViewState::Running,
            },
            SubagentView {
                root_id: second,
                name: "second".to_string(),
                state: SubagentViewState::Running,
            },
        ]);
        let mut state = test_state();
        state.subagent_rx = Some(receiver);

        assert!(state.select_session(first, Conversation::new()));
        assert_eq!(state.subagents[0].name, "first");
        assert!(state.select_session(second, Conversation::new()));
        assert_eq!(state.subagents[0].name, "second");
    }

    #[test]
    fn queued_inputs_are_displayed_by_their_turn_id() {
        let mut state = test_state();
        let first = TurnId::new();
        let second = TurnId::new();
        state.queue_input(first, "first".to_string());
        state.queue_input(second, "second".to_string());

        assert_eq!(state.input(first), Some("first"));
        assert_eq!(state.finish_turn(first).as_deref(), Some("first"));
        assert_eq!(state.finish_turn(second).as_deref(), Some("second"));
        assert!(state.inputs.is_empty());
    }

    fn settled_turn(id: TurnId, input: &str) -> Arc<ash_core::Turn> {
        Arc::new(ash_core::Turn {
            id,
            input: Input::user(input),
            steps: Vec::new(),
            result: ash_core::TurnResult::Stopped(ash_core::StopReason::EndTurn),
            stats: ash_core::TurnStats::default(),
        })
    }

    #[test]
    fn ignores_events_for_an_untracked_turn() {
        let mut state = test_state();
        let tracked = TurnId::new();
        let stale = TurnId::new();
        state.track_turn(tracked);
        state.operation.start_turn();
        let next_block_id = state.next_block_id;

        let (plan, action) = update_session(
            &mut state,
            SessionEvent::Text {
                turn_id: stale,
                text: "stale".to_string(),
            },
        );

        assert_eq!(plan, RenderPlan::None);
        assert_eq!(action, LoopAction::Continue);
        assert_eq!(state.next_block_id, next_block_id);
        assert!(state.conversation.turns().is_empty());
    }

    #[test]
    fn accepts_a_finished_turn_when_started_was_missed() {
        let mut state = test_state();
        let id = TurnId::new();
        state.queue_input(id, "hello".to_string());

        let (_, action) = update_session(
            &mut state,
            SessionEvent::Finished(settled_turn(id, "hello")),
        );

        assert_eq!(action, LoopAction::Continue);
        assert_eq!(state.conversation.turns().len(), 1);
        assert!(!state.inputs.contains_key(&id));
    }

    #[test]
    fn ignores_a_finished_turn_after_session_state_was_replaced() {
        let mut state = test_state();

        let (plan, action) = update_session(
            &mut state,
            SessionEvent::Finished(settled_turn(TurnId::new(), "old")),
        );

        assert_eq!(plan, RenderPlan::None);
        assert_eq!(action, LoopAction::Continue);
        assert!(state.conversation.turns().is_empty());
    }

    #[test]
    fn discarded_storage_failure_restores_input_and_surfaces_the_error() {
        let mut state = test_state();
        let id = TurnId::new();
        state.queue_input(id, "retry this".to_string());
        state.track_turn(id);
        state.operation.start_turn();

        let (plan, action) = update_session(
            &mut state,
            SessionEvent::Discarded {
                turn_id: id,
                error: Some("disk full".to_string()),
            },
        );

        assert_eq!(action, LoopAction::Continue);
        assert_eq!(plan, RenderPlan::Rebuild.merge(RenderPlan::Commit));
        assert_eq!(state.input.text(), "retry this");
        assert!(!state.operation.is_busy());
    }

    #[test]
    fn escape_cancels_a_running_turn_even_with_a_draft() {
        let mut state = test_state();
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
        let mut state = test_state();
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

        let parsed = slash_command::parse("queue another turn");
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
                UiCommand::Submit {
                    input: "change direction".into(),
                    reply,
                },
                result,
            )
            .await
        });

        let Some(UiCommand::Submit { input, reply }) = incoming.recv().await else {
            panic!("expected submit command");
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
