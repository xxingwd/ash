use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use ash_core::{Event, SessionId};
use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use futures::StreamExt;

use crate::{
    inline::{TerminalUi, TerminalView},
    input::InputState,
    session_picker::SessionPickerState,
    slash_command::{self, CommandCompletionState, ParsedInput, SlashCommand},
};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const STATUS_INTERVAL: Duration = Duration::from_millis(350);
const MOUSE_SCROLL_ROWS: u16 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Submit(String),
    Cancel,
    CancelAndRollback,
    Rollback,
    Compact,
    NewSession,
    ListSessions,
    ResumeSession(SessionId),
    Exit,
}

enum TurnPhase {
    Idle,
    Running { prompt: Option<String> },
    Pending(PendingAction),
}

enum PendingAction {
    Cancelling,
    Rollback {
        viewport_removed: bool,
        turn_finished: bool,
    },
    ListSessions,
    Resume,
    Compact,
}

impl TurnPhase {
    fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    fn shows_activity(&self) -> bool {
        !matches!(
            self,
            Self::Idle | Self::Pending(PendingAction::ListSessions | PendingAction::Resume)
        )
    }

    fn is_rolling_back(&self) -> bool {
        matches!(self, Self::Pending(PendingAction::Rollback { .. }))
    }

    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }

    fn is_cancelling(&self) -> bool {
        matches!(self, Self::Pending(PendingAction::Cancelling))
    }

    fn mark_turn_finished(&mut self) {
        if let Self::Pending(PendingAction::Rollback { turn_finished, .. }) = self {
            *turn_finished = true;
        }
    }

    fn ignores_turn_events(&self) -> bool {
        matches!(
            self,
            Self::Pending(PendingAction::Rollback {
                turn_finished: false,
                ..
            }) | Self::Pending(
                PendingAction::ListSessions | PendingAction::Resume | PendingAction::Compact
            )
        )
    }

    fn ignores_turn_finished(&self) -> bool {
        matches!(
            self,
            Self::Pending(
                PendingAction::ListSessions | PendingAction::Resume | PendingAction::Compact
            )
        )
    }

    fn accepts_action_result(&self) -> bool {
        matches!(
            self,
            Self::Pending(PendingAction::Rollback {
                turn_finished: true,
                ..
            }) | Self::Pending(
                PendingAction::ListSessions | PendingAction::Resume | PendingAction::Compact
            )
        )
    }

    fn ignores_action_error(&self) -> bool {
        matches!(
            self,
            Self::Pending(PendingAction::Rollback {
                turn_finished: false,
                ..
            })
        )
    }

    fn viewport_was_removed(&self) -> bool {
        matches!(
            self,
            Self::Pending(PendingAction::Rollback {
                viewport_removed: true,
                ..
            })
        )
    }
}

struct AppState {
    input: InputState,
    phase: TurnPhase,
    completion: CommandCompletionState,
    sessions: SessionPickerState,
    queue: VecDeque<String>,
}

enum LoopAction {
    Continue,
    ResetTimers,
    Exit,
}

impl AppState {
    fn new(input_history: Vec<String>) -> Self {
        Self {
            input: InputState::with_history(input_history),
            phase: TurnPhase::Idle,
            completion: CommandCompletionState::default(),
            sessions: SessionPickerState::default(),
            queue: VecDeque::new(),
        }
    }

    fn view(&mut self) -> TerminalView<'_> {
        self.completion
            .sync(self.input.text(), self.input.cursor(), self.phase.is_busy());
        TerminalView {
            input: &self.input,
            commands: self.completion.items(),
            selected_command: self.completion.selected_index(),
            sessions: self.sessions.sessions(),
            selected_session: self.sessions.selected_index(),
            busy: self.phase.shows_activity(),
            queued_messages: self.queue.len(),
        }
    }

    fn finish_cancellation(&mut self) {
        self.phase = TurnPhase::Idle;
        if self.input.is_empty() {
            if let Some(input) = self.queue.pop_front() {
                self.input.set_text(input);
            }
        }
    }

    fn render(&mut self, terminal: &mut TerminalUi) -> std::io::Result<()> {
        terminal.sync_view(self.view())
    }

    fn resize(
        &mut self,
        terminal: &mut TerminalUi,
        width: u16,
        height: u16,
    ) -> std::io::Result<()> {
        terminal.resize_view(self.view(), width, height)
    }
}

pub struct App {
    protocol: String,
    model: String,
    working_dir: PathBuf,
    context_limit: Option<u64>,
    input_history: Vec<String>,
}

impl App {
    pub fn new(protocol: String, model: String, working_dir: PathBuf) -> Self {
        Self {
            protocol,
            model,
            working_dir,
            context_limit: None,
            input_history: Vec::new(),
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

    pub async fn run(
        mut self,
        mut events: impl futures::Stream<Item = Event> + Unpin,
        commands: tokio::sync::mpsc::Sender<UiCommand>,
    ) -> anyhow::Result<()> {
        let mut terminal = TerminalUi::enter(
            &self.protocol,
            &self.model,
            &self.working_dir,
            self.context_limit,
        )?;
        let mut state = AppState::new(std::mem::take(&mut self.input_history));
        let mut keys = EventStream::new();
        let mut render_tick =
            tokio::time::interval_at(tokio::time::Instant::now() + FRAME_INTERVAL, FRAME_INTERVAL);
        render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut status_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + STATUS_INTERVAL,
            STATUS_INTERVAL,
        );
        status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        terminal.welcome()?;
        state.render(&mut terminal)?;

        loop {
            tokio::select! {
                _ = render_tick.tick(), if state.phase.shows_activity() => terminal.refresh_content()?,
                _ = status_tick.tick(), if state.phase.shows_activity() => terminal.refresh_status()?,
                event = events.next() => {
                    let Some(event) = event else { break };
                    if state.phase.ignores_turn_events() && is_turn_output_event(&event) {
                        continue;
                    }
                    match handle_agent_event(&mut state, &mut terminal, &commands, event).await? {
                        LoopAction::Continue => {}
                        LoopAction::ResetTimers => {
                            render_tick.reset();
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
                            render_tick.reset();
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
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    event: Event,
) -> anyhow::Result<LoopAction> {
    match event {
        Event::AgentStarted { .. } => {
            let reset_timers = !state.phase.is_busy();
            if reset_timers {
                state.phase = TurnPhase::Running { prompt: None };
            }
            terminal.agent_started()?;
            state.render(terminal)?;
            Ok(if reset_timers {
                LoopAction::ResetTimers
            } else {
                LoopAction::Continue
            })
        }
        Event::TextDelta(text) => {
            terminal.text(&text);
            Ok(LoopAction::Continue)
        }
        Event::Thinking(text) => {
            terminal.thinking(&text);
            Ok(LoopAction::Continue)
        }
        Event::ToolCallStart {
            name, arguments, ..
        } => {
            terminal.tool_start(&name, &arguments)?;
            Ok(LoopAction::Continue)
        }
        Event::ToolCallEnd {
            name,
            arguments,
            output,
            is_error,
            ..
        } => {
            terminal.tool_end(&name, &arguments, &output, is_error)?;
            Ok(LoopAction::Continue)
        }
        Event::Error(_) if state.phase.ignores_action_error() => Ok(LoopAction::Continue),
        Event::Error(error) => {
            terminal.error(&error)?;
            if state.phase.accepts_action_result() {
                state.phase = TurnPhase::Idle;
                state.sessions.close();
                state.render(terminal)?;
            }
            Ok(LoopAction::Continue)
        }
        Event::AgentFinished { .. } if state.phase.is_rolling_back() => {
            state.phase.mark_turn_finished();
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        Event::AgentFinished { .. } if state.phase.is_cancelling() => {
            terminal.finish_response()?;
            state.finish_cancellation();
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        Event::AgentFinished { .. } if state.phase.ignores_turn_finished() => {
            Ok(LoopAction::Continue)
        }
        Event::AgentFinished { .. } => {
            terminal.finish_response()?;
            let action = if let Some(input) = state.queue.pop_front() {
                state.phase = TurnPhase::Running {
                    prompt: Some(input.clone()),
                };
                terminal.commit_input(&input)?;
                if commands
                    .send(UiCommand::Submit(input.clone()))
                    .await
                    .is_err()
                {
                    return Ok(LoopAction::Exit);
                }
                state.input.record_submission(&input);
                LoopAction::ResetTimers
            } else {
                state.phase = TurnPhase::Idle;
                LoopAction::Continue
            };
            state.render(terminal)?;
            Ok(action)
        }
        Event::TurnRolledBack { prompt } => {
            let already_rolled_back = state.phase.viewport_was_removed();
            state.phase = TurnPhase::Idle;
            if state.input.text() != prompt {
                state.input.restore_submission(prompt);
            }
            if !already_rolled_back {
                terminal.rollback_turn()?;
            }
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        Event::ContextCompacted {
            before_tokens,
            after_tokens,
            dropped_messages,
            automatic,
        } => {
            if automatic {
                terminal.record_automatic_compaction(after_tokens)?;
            } else {
                state.phase = TurnPhase::Idle;
                terminal.finish_compaction(before_tokens, after_tokens, dropped_messages)?;
            }
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        Event::SessionRestored {
            model,
            protocol,
            working_dir,
            messages,
        } => {
            state.sessions.close();
            state.phase = TurnPhase::Idle;
            terminal.restore_session(&messages, &protocol, &model, &working_dir)?;
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        Event::SessionsListed { sessions } => {
            if matches!(state.phase, TurnPhase::Pending(PendingAction::ListSessions)) {
                state.phase = TurnPhase::Idle;
            }
            if sessions.is_empty() {
                terminal.command_output("No saved chats are available to resume.")?;
            } else {
                state.sessions.open(sessions);
            }
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        Event::Usage {
            input_tokens,
            output_tokens,
            generation_ms,
            estimated,
        } => {
            terminal.record_usage(input_tokens, output_tokens, generation_ms, estimated)?;
            Ok(LoopAction::Continue)
        }
        Event::ChildSpawned { .. } | Event::ChildCompleted { .. } => Ok(LoopAction::Continue),
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
        CrosstermEvent::Paste(text) if !state.sessions.is_visible() => {
            state.input.insert_paste(&text);
            state.render(terminal)?;
            Ok(LoopAction::Continue)
        }
        CrosstermEvent::Mouse(mouse) => handle_mouse(state, terminal, mouse),
        CrosstermEvent::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            handle_key(state, terminal, commands, key).await
        }
        _ => Ok(LoopAction::Continue),
    }
}

fn handle_mouse(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    mouse: crossterm::event::MouseEvent,
) -> anyhow::Result<LoopAction> {
    if state.sessions.is_visible() {
        match mouse.kind {
            MouseEventKind::ScrollUp => state.sessions.move_up(),
            MouseEventKind::ScrollDown => state.sessions.move_down(),
            _ => return Ok(LoopAction::Continue),
        }
        state.render(terminal)?;
        return Ok(LoopAction::Continue);
    }
    if state.completion.is_visible() {
        match mouse.kind {
            MouseEventKind::ScrollUp => state.completion.move_up(),
            MouseEventKind::ScrollDown => state.completion.move_down(),
            _ => return Ok(LoopAction::Continue),
        }
        state.render(terminal)?;
        return Ok(LoopAction::Continue);
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => terminal.scroll_lines_up(MOUSE_SCROLL_ROWS)?,
        MouseEventKind::ScrollDown => terminal.scroll_lines_down(MOUSE_SCROLL_ROWS)?,
        MouseEventKind::Down(MouseButton::Left) => {
            terminal.start_selection(mouse.column, mouse.row)?
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            terminal.drag_selection(mouse.column, mouse.row)?
        }
        MouseEventKind::Up(MouseButton::Left) => {
            terminal.finish_selection(mouse.column, mouse.row)?
        }
        _ => {}
    }
    Ok(LoopAction::Continue)
}

async fn handle_key(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    commands: &tokio::sync::mpsc::Sender<UiCommand>,
    key: KeyEvent,
) -> anyhow::Result<LoopAction> {
    if state.sessions.is_visible() {
        return handle_session_key(state, terminal, commands, key).await;
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
        KeyCode::Enter => return submit_input(state, terminal, commands).await,
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.input.is_empty() {
                terminal.toggle_latest_thought()?;
            }
            return Ok(LoopAction::Continue);
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.input.is_empty() {
                return Ok(if state.phase.is_busy() {
                    LoopAction::Continue
                } else {
                    let _ = commands.send(UiCommand::Exit).await;
                    LoopAction::Exit
                });
            }
            state.input.clear();
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
    let selected = match key.code {
        KeyCode::Esc => {
            state.sessions.close();
            None
        }
        KeyCode::Up => {
            state.sessions.move_up();
            None
        }
        KeyCode::Down => {
            state.sessions.move_down();
            None
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.sessions.move_up();
            None
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.sessions.move_down();
            None
        }
        KeyCode::Enter => {
            let selected = state.sessions.selected_session_id();
            state.sessions.close();
            if selected.is_some() {
                state.phase = TurnPhase::Pending(PendingAction::Resume);
            }
            selected
        }
        _ => return Ok(LoopAction::Continue),
    };
    state.render(terminal)?;
    if let Some(session_id) = selected {
        if commands
            .send(UiCommand::ResumeSession(session_id))
            .await
            .is_err()
        {
            return Ok(LoopAction::Exit);
        }
    }
    Ok(LoopAction::Continue)
}

fn handle_completion_key(
    state: &mut AppState,
    terminal: &mut TerminalUi,
    key: KeyEvent,
) -> anyhow::Result<bool> {
    if !state.completion.is_visible() {
        return Ok(false);
    }
    match key.code {
        KeyCode::Esc => state.completion.dismiss(),
        KeyCode::Up => state.completion.move_up(),
        KeyCode::Down => state.completion.move_down(),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.completion.move_up()
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.completion.move_down()
        }
        KeyCode::Tab => {
            if let Some(selected) = state.completion.selected() {
                state.input.set_text(format!("/{} ", selected.name));
            }
        }
        KeyCode::Enter if !inserts_newline(&key) => {
            if let Some(selected) = state.completion.selected() {
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
    state.phase.is_running()
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
    if !state.phase.is_running() {
        return Ok(LoopAction::Continue);
    }
    let command = if terminal.has_response_block() {
        state.phase = TurnPhase::Pending(PendingAction::Cancelling);
        UiCommand::Cancel
    } else {
        let prompt = match std::mem::replace(
            &mut state.phase,
            TurnPhase::Pending(PendingAction::Rollback {
                viewport_removed: true,
                turn_finished: false,
            }),
        ) {
            TurnPhase::Running { prompt } => prompt,
            TurnPhase::Idle | TurnPhase::Pending(_) => None,
        };
        if let Some(prompt) = prompt {
            state.input.restore_submission(prompt);
        }
        terminal.rollback_turn()?;
        UiCommand::CancelAndRollback
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
    if state.phase.is_pending() {
        terminal.command_blocked("input")?;
        state.render(terminal)?;
        return Ok(LoopAction::Continue);
    }
    let input = state.input.submit();
    state.render(terminal)?;
    if input.trim().is_empty() {
        return Ok(LoopAction::Continue);
    }
    if matches!(input.trim(), "exit" | "quit") {
        terminal.commit_exit(&input)?;
        let _ = commands.send(UiCommand::Exit).await;
        return Ok(LoopAction::Exit);
    }

    match slash_command::parse(&input) {
        ParsedInput::Message => submit_message(state, terminal, commands, input).await,
        ParsedInput::Invalid(error) => {
            if state.phase.is_busy() {
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
    if state.phase.is_busy() {
        state.queue.push_back(input);
        state.render(terminal)?;
        return Ok(LoopAction::Continue);
    }
    state.phase = TurnPhase::Running {
        prompt: Some(input.clone()),
    };
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
    if state.phase.is_busy() && !command.available_during_task() {
        terminal.command_blocked(command.name())?;
        state.render(terminal)?;
        return Ok(LoopAction::Continue);
    }

    let outgoing = match command {
        SlashCommand::New | SlashCommand::Clear => {
            state.sessions.close();
            terminal.start_new_session()?;
            Some(UiCommand::NewSession)
        }
        SlashCommand::Resume => {
            state.phase = TurnPhase::Pending(PendingAction::ListSessions);
            Some(UiCommand::ListSessions)
        }
        SlashCommand::Undo => {
            state.phase = TurnPhase::Pending(PendingAction::Rollback {
                viewport_removed: false,
                turn_finished: true,
            });
            Some(UiCommand::Rollback)
        }
        SlashCommand::Compact => {
            state.phase = TurnPhase::Pending(PendingAction::Compact);
            terminal.start_compaction()?;
            Some(UiCommand::Compact)
        }
        SlashCommand::Status => {
            terminal.show_session_status()?;
            None
        }
        SlashCommand::Help => {
            terminal.command_output(&slash_command::help_text())?;
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

fn is_turn_output_event(event: &Event) -> bool {
    matches!(
        event,
        Event::AgentStarted { .. }
            | Event::TextDelta(_)
            | Event::Thinking(_)
            | Event::ToolCallStart { .. }
            | Event::ToolCallEnd { .. }
            | Event::Usage { .. }
    )
}

#[cfg(test)]
mod tests {
    use ash_core::StopReason;

    use super::*;

    #[test]
    fn derives_terminal_view_from_one_app_state() {
        let mut state = AppState::new(Vec::new());
        state.input.set_text("/");
        state.phase = TurnPhase::Running {
            prompt: Some("question".into()),
        };
        state.queue.push_back("next".into());

        let view = state.view();

        assert!(view.busy);
        assert_eq!(view.queued_messages, 1);
        assert_eq!(
            view.commands
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            ["exit"]
        );
        assert!(view.sessions.is_empty());
    }

    #[test]
    fn identifies_turn_output_without_hiding_action_results() {
        assert!(is_turn_output_event(&Event::TextDelta("late".into())));
        assert!(is_turn_output_event(&Event::Thinking("late".into())));
        assert!(!is_turn_output_event(&Event::Error("cancelled".into())));
        assert!(!is_turn_output_event(&Event::AgentFinished {
            reason: StopReason::Aborted,
        }));
        assert!(!is_turn_output_event(&Event::TurnRolledBack {
            prompt: "draft".into(),
        }));
    }

    #[test]
    fn pending_actions_ignore_late_turn_events_until_their_result() {
        let list = TurnPhase::Pending(PendingAction::ListSessions);
        assert!(list.ignores_turn_events());
        assert!(list.ignores_turn_finished());
        assert!(list.accepts_action_result());
        assert!(!list.shows_activity());

        let resume = TurnPhase::Pending(PendingAction::Resume);
        assert!(!resume.shows_activity());

        let mut rollback = TurnPhase::Pending(PendingAction::Rollback {
            viewport_removed: true,
            turn_finished: false,
        });
        assert!(rollback.ignores_turn_events());
        assert!(!rollback.accepts_action_result());
        assert!(rollback.ignores_action_error());

        rollback.mark_turn_finished();
        assert!(!rollback.ignores_turn_events());
        assert!(rollback.accepts_action_result());
        assert!(!rollback.ignores_action_error());
    }

    #[test]
    fn cancellation_restores_the_next_queued_prompt_without_losing_the_rest() {
        let mut state = AppState::new(Vec::new());
        state.phase = TurnPhase::Pending(PendingAction::Cancelling);
        state.queue.push_back("next".into());
        state.queue.push_back("later".into());

        state.finish_cancellation();

        assert!(matches!(state.phase, TurnPhase::Idle));
        assert_eq!(state.input.text(), "next");
        assert_eq!(state.queue, VecDeque::from(["later".to_string()]));
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
