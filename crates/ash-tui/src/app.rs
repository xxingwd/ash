use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use ash_core::{EventKind, LiveEvent, MessageId, SubagentSnapshot, ThreadId};
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
        AgentStart, BackgroundAction, Cancellation, EventRoute, FailureCompletion, OperationState,
        RollbackCompletion, SubmissionPolicy,
    },
    session_picker::SessionPickerState,
    slash_command::{self, CommandCompletionState, ParsedInput, SlashCommand},
};

/// Pace complete assistant lines so streaming remains readable rather than
/// tracking every delta: newline deltas flush immediately and this periodic
/// status refresh redraws anything that did not complete a line yet.
const STATUS_INTERVAL: Duration = Duration::from_millis(350);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Submit(String),
    Cancel,
    CancelAndRollback,
    Rollback,
    Compact,
    NewSession,
    ListSessions,
    ListForkPoints,
    ResumeSession(ThreadId),
    ForkSession(MessageId),
    Exit,
}

struct AppState {
    input: InputState,
    operation: OperationState,
    menu: ComposerMenuState,
    pending_inputs: VecDeque<String>,
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
        SessionPickerState::move_up(self);
    }

    fn move_down(&mut self) {
        SessionPickerState::move_down(self);
    }
}

impl PickerNavigation for ForkPickerState {
    fn move_up(&mut self) {
        ForkPickerState::move_up(self);
    }

    fn move_down(&mut self) {
        ForkPickerState::move_down(self);
    }
}

impl PickerNavigation for CommandCompletionState {
    fn move_up(&mut self) {
        CommandCompletionState::move_up(self);
    }

    fn move_down(&mut self) {
        CommandCompletionState::move_down(self);
    }
}

impl AppState {
    fn new(input_history: Vec<String>) -> Self {
        Self {
            input: InputState::with_history(input_history),
            operation: OperationState::default(),
            menu: ComposerMenuState::default(),
            pending_inputs: VecDeque::new(),
            subagent_rx: None,
            subagents: Vec::new(),
        }
    }

    fn refresh_subagents(&mut self) {
        let Some(receiver) = &mut self.subagent_rx else {
            return;
        };
        if receiver.has_changed().unwrap_or(false) {
            self.subagents = receiver.borrow_and_update().clone();
        }
    }

    fn sync_menu(&mut self) {
        self.menu.sync_commands(
            self.input.text(),
            self.input.cursor(),
            self.operation.is_busy(),
        );
    }

    fn view(&self) -> TerminalView<'_> {
        TerminalView {
            input: &self.input,
            menu: self.menu.view(),
            busy: self.operation.shows_activity(),
            queued_messages: self.pending_inputs.len(),
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
    pub fn new(protocol: String, model: String, working_dir: PathBuf) -> Self {
        Self {
            protocol,
            model,
            working_dir,
            context_limit: None,
            input_history: Vec::new(),
            subagent_monitor: None,
        }
    }

    pub fn with_context_limit(mut self, context_limit: Option<u64>) -> Self {
        self.context_limit = context_limit;
        self
    }

    pub fn with_input_history(mut self, input_history: Vec<String>) -> Self {
        self.input_history = input_history;
        self
    }

    pub fn with_subagent_monitor(
        mut self,
        subagent_monitor: Option<tokio::sync::watch::Receiver<Vec<SubagentSnapshot>>>,
    ) -> Self {
        self.subagent_monitor = subagent_monitor;
        self
    }

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
                    state.refresh_subagents();
                    terminal.set_subagents(state.subagents.clone())?;
                    if state.operation.shows_activity() {
                        terminal.refresh_status()?;
                    }
                }
                event = events.next() => {
                    let Some(event) = event else { break };
                    match state.operation.route_event(&event) {
                        EventRoute::Handle => {}
                        EventRoute::Ignore => continue,
                        EventRoute::Render => {
                            state.render(&mut terminal)?;
                            continue;
                        }
                    }
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

async fn handle_agent_event(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    _commands: &tokio::sync::mpsc::Sender<UiCommand>,
    event: EventKind,
) -> anyhow::Result<LoopAction> {
    match event {
        EventKind::TurnStart => {
            let start = if state.operation.is_busy() {
                state.operation.agent_started()
            } else if let Some(input) = state.pending_inputs.pop_front() {
                state.operation.start_turn(Some(input.clone()));
                terminal.commit_input(&input)?;
                AgentStart::StartedTurn
            } else {
                state.operation.agent_started()
            };
            terminal.agent_started()?;
            state.render(terminal)?;
            Ok(match start {
                AgentStart::StartedTurn => LoopAction::ResetTimers,
                AgentStart::TurnAlreadyTracked => LoopAction::Continue,
            })
        }
        EventKind::Live(LiveEvent::TextDelta(text)) => {
            terminal.text(&text)?;
            Ok(LoopAction::Continue)
        }
        EventKind::Live(LiveEvent::ReasoningDelta(text)) => {
            terminal.thinking(&text)?;
            Ok(LoopAction::Continue)
        }
        EventKind::Live(LiveEvent::ToolStarted { .. }) => {
            terminal.tool_start()?;
            Ok(LoopAction::Continue)
        }
        EventKind::Live(LiveEvent::ToolFinished {
            name,
            arguments,
            output,
            is_error,
            ..
        }) => {
            terminal.tool_end(&name, &arguments, &output, is_error)?;
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
            state.operation.complete_turn();
            terminal.commit_turn(&view)?;
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        EventKind::TurnRolledBack {
            prompt,
            context_tokens,
        } => {
            let rollback = state.operation.finish_rollback();
            if state.input.text() != prompt {
                state.input.restore_submission(prompt);
            }
            if rollback == RollbackCompletion::ReplayViewport {
                terminal.rollback_turn()?;
            }
            if let Some(tokens) = context_tokens {
                terminal.record_rollback_context(tokens)?;
            }
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
        EventKind::Restored {
            view,
            context_tokens,
        } => {
            state.menu.close_picker();
            state.operation.finish();
            terminal.restore_session(&view.messages, context_tokens)?;
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        EventKind::ThreadsListed { threads } => {
            state
                .operation
                .finish_background(BackgroundAction::ListSessions);
            if threads.is_empty() {
                terminal.command_output("No saved chats are available to resume.")?;
            } else {
                state.menu.open_threads(threads);
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
        EventKind::ThreadForked {
            view,
            prompt,
            context_tokens,
        } => {
            state.menu.close_picker();
            state.operation.finish_background(BackgroundAction::Fork);
            terminal.restore_session(&view.messages, context_tokens)?;
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
    if handle_completion_key(state, terminal, key)? {
        return Ok(LoopAction::Continue);
    }
    if is_cancel_key(state, &key) {
        return cancel_turn(state, terminal, commands).await;
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
        KeyCode::Char('c')
            if key.modifiers.contains(KeyModifiers::CONTROL) && !state.input.is_empty() =>
        {
            state.input.clear()
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.operation.is_busy() {
                let _ = commands.send(UiCommand::Exit).await;
                return Ok(LoopAction::Exit);
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
            if !state.input.move_up(terminal.composer_text_width()?) {
                state.input.history_previous();
            }
        }
        KeyCode::Down => {
            if !state.input.move_down(terminal.composer_text_width()?) {
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
    let Some(threads) = state.menu.visible_session_picker_mut() else {
        return Ok(LoopAction::Continue);
    };
    let Some(action) = picker_key_action(threads, key, SessionPickerState::selected_thread_id)
    else {
        return Ok(LoopAction::Continue);
    };
    match action {
        PickerAction::KeepOpen => {}
        PickerAction::Close => state.menu.close_threads(),
        PickerAction::Confirm(_) => {
            state.menu.close_threads();
            state.operation.start_background(BackgroundAction::Resume);
        }
    }
    state.render(terminal)?;
    let PickerAction::Confirm(thread_id) = action else {
        return Ok(LoopAction::Continue);
    };
    if commands
        .send(UiCommand::ResumeSession(thread_id))
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
        KeyCode::Enter => Some(
            selected(picker)
                .map(PickerAction::Confirm)
                .unwrap_or(PickerAction::Close),
        ),
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
            completion.move_down()
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
        && state.input.is_empty()
        && key.code == KeyCode::Esc
        && key.kind == KeyEventKind::Press
        && key.modifiers.is_empty()
}

async fn cancel_turn(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
) -> anyhow::Result<LoopAction> {
    let Some(cancellation) = state
        .operation
        .begin_cancellation(terminal.has_response_block())
    else {
        return Ok(LoopAction::Continue);
    };
    let command = match cancellation {
        Cancellation::KeepResponse => UiCommand::Cancel,
        Cancellation::RemoveTurn { prompt } => {
            if let Some(prompt) = prompt {
                state.input.restore_submission(prompt);
            }
            terminal.rollback_turn()?;
            UiCommand::CancelAndRollback
        }
    };
    state.render(terminal)?;
    Ok(if commands.send(command).await.is_err() {
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
    if state.operation.submission_policy() == SubmissionPolicy::Block {
        terminal.command_blocked("input")?;
        state.render(terminal)?;
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

    match slash_command::parse(&input) {
        ParsedInput::Message => submit_message(state, terminal, commands, input).await,
        ParsedInput::Invalid(error) => {
            if state.operation.is_busy() {
                terminal.command_blocked("command")?;
            } else {
                terminal.command_error(&error)?;
            }
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        ParsedInput::Command(command) => {
            run_command(state, terminal, commands, command, &input).await
        }
    }
}

async fn submit_message(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    input: String,
) -> anyhow::Result<LoopAction> {
    match state.operation.submission_policy() {
        SubmissionPolicy::Enqueue => {
            if commands
                .send(UiCommand::Submit(input.clone()))
                .await
                .is_err()
            {
                return Ok(LoopAction::Exit);
            }
            state.pending_inputs.push_back(input.clone());
            state.input.record_submission(&input);
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        SubmissionPolicy::Start => start_message(state, terminal, commands, input).await,
        SubmissionPolicy::Block => {
            terminal.command_blocked("input")?;
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
    }
}

async fn start_message(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    input: String,
) -> anyhow::Result<LoopAction> {
    state.operation.start_turn(Some(input.clone()));
    terminal.commit_input(&input)?;
    if commands
        .send(UiCommand::Submit(input.clone()))
        .await
        .is_err()
    {
        return Ok(LoopAction::Exit);
    }
    state.input.record_submission(&input);
    state.render(terminal)?;
    Ok(LoopAction::ResetTimers)
}

async fn run_command(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    command: SlashCommand,
    input: &str,
) -> anyhow::Result<LoopAction> {
    if state.operation.is_busy() && !command.available_during_task() {
        terminal.command_blocked(command.name())?;
        state.render(terminal)?;
        return Ok(LoopAction::Continue);
    }

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
            state.operation.start_rollback();
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
            terminal.start_compaction()?;
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

fn inserts_newline(key: &KeyEvent) -> bool {
    (key.code == KeyCode::Enter
        && key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT))
        || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::TurnCompletion;

    #[test]
    fn derives_terminal_view_from_one_app_state() {
        let mut state = AppState::new(Vec::new());
        state.input.set_text("/");
        state.operation.start_turn(Some("question".into()));
        state.pending_inputs.push_back("next".into());

        state.sync_menu();
        let view = state.view();

        assert!(view.busy);
        assert_eq!(view.queued_messages, 1);
        assert_eq!(
            match view.menu {
                crate::menu::MenuView::Commands { items, .. } =>
                    items.iter().map(|command| command.name).collect::<Vec<_>>(),
                crate::menu::MenuView::None
                | crate::menu::MenuView::Sessions { .. }
                | crate::menu::MenuView::ForkPoints { .. } => Vec::new(),
            },
            ["exit"]
        );
    }

    #[test]
    fn cancellation_keeps_the_core_owned_pending_projection() {
        let mut state = AppState::new(Vec::new());
        state.operation.start_turn(None);
        state.operation.begin_cancellation(true);
        state.pending_inputs.push_back("next".into());
        state.pending_inputs.push_back("later".into());

        assert_eq!(
            state.operation.complete_turn(),
            Some(TurnCompletion::Cancelled)
        );
        assert!(!state.operation.is_busy());
        assert!(state.input.is_empty());
        assert_eq!(
            state.pending_inputs,
            VecDeque::from(["next".to_string(), "later".to_string()])
        );
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
