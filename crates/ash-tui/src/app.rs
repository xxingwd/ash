use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use ash_core::{Event, SessionId, ToolCallId};
use crossterm::event::{Event as CrosstermEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;

use crate::{
    inline::InlineTerminal,
    input::InputState,
    session_picker::SessionPickerState,
    slash_command::{self, CommandCompletionState, ParsedInput, SlashCommand},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Submit(String),
    CancelAndUndo,
    NewSession,
    ListSessions,
    ResumeSession(SessionId),
    Exit,
}

pub struct App {
    protocol: String,
    model: String,
    working_dir: PathBuf,
    input_history: Vec<String>,
}

impl App {
    pub fn new(protocol: String, model: String, working_dir: PathBuf) -> Self {
        Self {
            protocol,
            model,
            working_dir,
            input_history: Vec::new(),
        }
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
        let mut terminal = InlineTerminal::enter()?;
        let mut input = InputState::with_history(std::mem::take(&mut self.input_history));
        let mut keys = EventStream::new();
        let render_interval = Duration::from_nanos(8_333_334);
        let mut render_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + render_interval,
            render_interval,
        );
        render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let status_interval = Duration::from_millis(350);
        let mut status_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + status_interval,
            status_interval,
        );
        status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut busy = false;
        let mut command_completion = CommandCompletionState::default();
        let mut session_picker = SessionPickerState::default();
        let mut calls: HashMap<ToolCallId, (String, serde_json::Value)> = HashMap::new();
        let mut queued_inputs: VecDeque<String> = VecDeque::new();
        let mut active_prompt: Option<String> = None;
        let mut rollback_in_progress = false;

        terminal.welcome()?;
        render_prompt(
            &mut terminal,
            &input,
            &mut command_completion,
            busy,
            &self.protocol,
            &self.model,
            &self.working_dir,
        )?;

        loop {
            tokio::select! {
                _ = render_tick.tick(), if busy => terminal.refresh_content()?,
                _ = status_tick.tick(), if busy => terminal.refresh_status()?,
                event = events.next() => {
                    let Some(event) = event else { break };
                    match event {
                        event if rollback_in_progress && is_stale_turn_event(&event) => {}
                        Event::AgentStarted { .. } => {
                            if !busy {
                                render_tick.reset();
                                status_tick.reset();
                            }
                            busy = true;
                            terminal.agent_started()?;
                            render_prompt(
                                &mut terminal,
                                &input,
                                &mut command_completion,
                                busy,
                                &self.protocol,
                                &self.model,
                                &self.working_dir,
                            )?;
                        }
                        Event::TextDelta(text) => terminal.text(&text)?,
                        Event::Thinking(text) => terminal.thinking(&text)?,
                        Event::ToolCallStart { id, name, arguments } => {
                            terminal.tool_start(&name, &arguments)?;
                            calls.insert(id.clone(), (name.clone(), arguments));
                        }
                        Event::ToolCallEnd { id, output: _, is_error } => {
                            let (name, arguments) = calls
                                .remove(&id)
                                .unwrap_or_else(|| ("tool".into(), serde_json::Value::Null));
                            terminal.tool_end(&name, &arguments, is_error)?;
                        }
                        Event::Error(error) => {
                            terminal.error(&error)?;
                        }
                        Event::AgentFinished { .. } => {
                            if rollback_in_progress {
                                queued_inputs.clear();
                                terminal.set_queued_messages(0);
                                busy = false;
                            } else {
                                terminal.finish_response()?;
                                if let Some(next_input) = queued_inputs.pop_front() {
                                    active_prompt = Some(next_input.clone());
                                    terminal.set_queued_messages(queued_inputs.len());
                                    terminal.commit_input(&next_input)?;
                                    render_tick.reset();
                                    status_tick.reset();
                                    busy = true;
                                } else {
                                    active_prompt = None;
                                    terminal.set_queued_messages(0);
                                    busy = false;
                                }
                                render_prompt(
                                    &mut terminal,
                                    &input,
                                    &mut command_completion,
                                    busy,
                                    &self.protocol,
                                    &self.model,
                                    &self.working_dir,
                                )?;
                            }
                        }
                        Event::TurnRolledBack { messages: _, prompt } => {
                            let ui_already_rolled_back = rollback_in_progress;
                            rollback_in_progress = false;
                            busy = false;
                            calls.clear();
                            queued_inputs.clear();
                            terminal.set_queued_messages(0);
                            active_prompt = None;
                            if input.text() != prompt {
                                input.restore_submitted(prompt);
                            }
                            if !ui_already_rolled_back {
                                terminal.rollback_turn()?;
                            }
                            render_prompt(
                                &mut terminal,
                                &input,
                                &mut command_completion,
                                busy,
                                &self.protocol,
                                &self.model,
                                &self.working_dir,
                            )?;
                        }
                        Event::SessionRestored {
                            session_id: _,
                            path: _,
                            title: _,
                            model,
                            protocol,
                            working_dir,
                            messages,
                        } => {
                            session_picker.close();
                            terminal.set_session_menu(&[], 0);
                            rollback_in_progress = false;
                            busy = false;
                            calls.clear();
                            queued_inputs.clear();
                            active_prompt = None;
                            terminal.set_queued_messages(0);
                            self.model = model;
                            self.protocol = protocol;
                            self.working_dir = working_dir;
                            terminal.restore_session(&messages, &self.working_dir)?;
                            render_prompt(
                                &mut terminal,
                                &input,
                                &mut command_completion,
                                busy,
                                &self.protocol,
                                &self.model,
                                &self.working_dir,
                            )?;
                        }
                        Event::SessionsListed { sessions } => {
                            if sessions.is_empty() {
                                terminal.command_output("No saved chats are available to resume.")?;
                            } else {
                                session_picker.open(sessions);
                                terminal.set_session_menu(
                                    session_picker.sessions(),
                                    session_picker.selected_index(),
                                );
                            }
                            render_prompt(
                                &mut terminal,
                                &input,
                                &mut command_completion,
                                busy,
                                &self.protocol,
                                &self.model,
                                &self.working_dir,
                            )?;
                        }
                        Event::Usage { .. }
                        | Event::ChildSpawned { .. }
                        | Event::ChildCompleted { .. } => {}
                    }
                }
                key = keys.next() => {
                    let Some(key) = key else { break };
                    let event = key?;
                    match event {
                        CrosstermEvent::Resize(width, height) => {
                            terminal.handle_resize(width, height);
                            render_prompt(
                                &mut terminal,
                                &input,
                                &mut command_completion,
                                busy,
                                &self.protocol,
                                &self.model,
                                &self.working_dir,
                            )?
                        },
                        CrosstermEvent::Paste(text) => {
                            if session_picker.is_visible() {
                                continue;
                            }
                            input.insert_str(&text.replace(['\r', '\n'], " "));
                            render_prompt(
                                &mut terminal,
                                &input,
                                &mut command_completion,
                                busy,
                                &self.protocol,
                                &self.model,
                                &self.working_dir,
                            )?;
                        }
                        CrosstermEvent::Key(key)
                            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                        {
                            if session_picker.is_visible() {
                                match key.code {
                                    KeyCode::Esc => {
                                        session_picker.close();
                                        terminal.set_session_menu(&[], 0);
                                    }
                                    KeyCode::Up => session_picker.move_up(),
                                    KeyCode::Down => session_picker.move_down(),
                                    KeyCode::Char('p')
                                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                    {
                                        session_picker.move_up();
                                    }
                                    KeyCode::Char('n')
                                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                    {
                                        session_picker.move_down();
                                    }
                                    KeyCode::Enter => {
                                        let selected = session_picker.selected_session_id();
                                        session_picker.close();
                                        terminal.set_session_menu(&[], 0);
                                        render_prompt(
                                            &mut terminal,
                                            &input,
                                            &mut command_completion,
                                            busy,
                                            &self.protocol,
                                            &self.model,
                                            &self.working_dir,
                                        )?;
                                        if let Some(session_id) = selected {
                                            if commands
                                                .send(UiCommand::ResumeSession(session_id))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        continue;
                                    }
                                    _ => continue,
                                }
                                terminal.set_session_menu(
                                    session_picker.sessions(),
                                    session_picker.selected_index(),
                                );
                                render_prompt(
                                    &mut terminal,
                                    &input,
                                    &mut command_completion,
                                    busy,
                                    &self.protocol,
                                    &self.model,
                                    &self.working_dir,
                                )?;
                                continue;
                            }

                            if command_completion.is_visible() {
                                match key.code {
                                    KeyCode::Esc => {
                                        command_completion.dismiss();
                                        terminal.set_command_menu(
                                            command_completion.items(),
                                            command_completion.selected_index(),
                                        );
                                        terminal.prompt(
                                            &input,
                                            &self.protocol,
                                            &self.model,
                                            &self.working_dir,
                                        )?;
                                        continue;
                                    }
                                    KeyCode::Up => {
                                        command_completion.move_up();
                                        render_prompt(
                                            &mut terminal,
                                            &input,
                                            &mut command_completion,
                                            busy,
                                            &self.protocol,
                                            &self.model,
                                            &self.working_dir,
                                        )?;
                                        continue;
                                    }
                                    KeyCode::Down => {
                                        command_completion.move_down();
                                        render_prompt(
                                            &mut terminal,
                                            &input,
                                            &mut command_completion,
                                            busy,
                                            &self.protocol,
                                            &self.model,
                                            &self.working_dir,
                                        )?;
                                        continue;
                                    }
                                    KeyCode::Char('p')
                                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                    {
                                        command_completion.move_up();
                                        render_prompt(
                                            &mut terminal,
                                            &input,
                                            &mut command_completion,
                                            busy,
                                            &self.protocol,
                                            &self.model,
                                            &self.working_dir,
                                        )?;
                                        continue;
                                    }
                                    KeyCode::Char('n')
                                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                    {
                                        command_completion.move_down();
                                        render_prompt(
                                            &mut terminal,
                                            &input,
                                            &mut command_completion,
                                            busy,
                                            &self.protocol,
                                            &self.model,
                                            &self.working_dir,
                                        )?;
                                        continue;
                                    }
                                    KeyCode::Tab => {
                                        if let Some(selected) = command_completion.selected() {
                                            input.set_text(format!("/{} ", selected.name));
                                        }
                                        render_prompt(
                                            &mut terminal,
                                            &input,
                                            &mut command_completion,
                                            busy,
                                            &self.protocol,
                                            &self.model,
                                            &self.working_dir,
                                        )?;
                                        continue;
                                    }
                                    KeyCode::Enter => {
                                        if let Some(selected) = command_completion.selected() {
                                            input.set_text(format!("/{}", selected.name));
                                        }
                                    }
                                    _ => {}
                                }
                            }

                            if busy
                                && input.is_empty()
                                && key.code == KeyCode::Esc
                                && key.kind == KeyEventKind::Press
                                && key.modifiers.is_empty()
                            {
                                if rollback_in_progress {
                                    continue;
                                }
                                rollback_in_progress = true;
                                queued_inputs.clear();
                                terminal.set_queued_messages(0);
                                if let Some(prompt) = active_prompt.take() {
                                    input.restore_submitted(prompt);
                                }
                                terminal.rollback_turn()?;
                                render_prompt(
                                    &mut terminal,
                                    &input,
                                    &mut command_completion,
                                    busy,
                                    &self.protocol,
                                    &self.model,
                                    &self.working_dir,
                                )?;
                                if commands.send(UiCommand::CancelAndUndo).await.is_err() {
                                    break;
                                }
                                continue;
                            }

                            match key.code {
                                KeyCode::Enter => {
                                    let value = input.submit();
                                    sync_command_menu(
                                        &mut terminal,
                                        &input,
                                        &mut command_completion,
                                        busy,
                                    );
                                    if value.trim().is_empty() {
                                        render_prompt(
                                            &mut terminal,
                                            &input,
                                            &mut command_completion,
                                            busy,
                                            &self.protocol,
                                            &self.model,
                                            &self.working_dir,
                                        )?;
                                        continue;
                                    }
                                    if matches!(value.trim(), "exit" | "quit") {
                                        terminal.commit_exit(&value)?;
                                        let _ = commands.send(UiCommand::Exit).await;
                                        break;
                                    }
                                    match slash_command::parse(&value) {
                                        ParsedInput::Message(_) => {}
                                        ParsedInput::Invalid(error) => {
                                            if busy {
                                                terminal.command_blocked("command")?;
                                            } else {
                                                terminal.command_error(&error)?;
                                            }
                                            continue;
                                        }
                                        ParsedInput::Command(command) => {
                                            if busy && !command.available_during_task() {
                                                terminal.command_blocked(command.name())?;
                                                continue;
                                            }
                                            match command {
                                                SlashCommand::New | SlashCommand::Clear => {
                                                    session_picker.close();
                                                    terminal.set_session_menu(&[], 0);
                                                    active_prompt = None;
                                                    terminal.start_new_session()?;
                                                    render_prompt(
                                                        &mut terminal,
                                                        &input,
                                                        &mut command_completion,
                                                        busy,
                                                        &self.protocol,
                                                        &self.model,
                                                        &self.working_dir,
                                                    )?;
                                                    if commands
                                                        .send(UiCommand::NewSession)
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                SlashCommand::Resume => {
                                                    render_prompt(
                                                        &mut terminal,
                                                        &input,
                                                        &mut command_completion,
                                                        busy,
                                                        &self.protocol,
                                                        &self.model,
                                                        &self.working_dir,
                                                    )?;
                                                    if commands
                                                        .send(UiCommand::ListSessions)
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                SlashCommand::Status => {
                                                    terminal.command_output(&format!(
                                                        "Model: {}\nProtocol: {}\nDirectory: {}",
                                                        self.model,
                                                        self.protocol,
                                                        self.working_dir.display()
                                                    ))?;
                                                }
                                                SlashCommand::Help => {
                                                    terminal.command_output(
                                                        &slash_command::help_text(),
                                                    )?;
                                                }
                                                SlashCommand::Exit => {
                                                    terminal.commit_exit(&value)?;
                                                    let _ = commands.send(UiCommand::Exit).await;
                                                    break;
                                                }
                                            }
                                            continue;
                                        }
                                    }
                                    if busy {
                                        queued_inputs.push_back(value.clone());
                                        terminal.set_queued_messages(queued_inputs.len());
                                        render_prompt(
                                            &mut terminal,
                                            &input,
                                            &mut command_completion,
                                            busy,
                                            &self.protocol,
                                            &self.model,
                                            &self.working_dir,
                                        )?;
                                        if commands.send(UiCommand::Submit(value)).await.is_err() {
                                            break;
                                        }
                                        continue;
                                    }
                                    active_prompt = Some(value.clone());
                                    terminal.commit_input(&value)?;
                                    render_tick.reset();
                                    status_tick.reset();
                                    busy = true;
                                    if commands.send(UiCommand::Submit(value)).await.is_err() {
                                        break;
                                    }
                                }
                                KeyCode::Char('c')
                                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    if input.is_empty() {
                                        if busy {
                                            continue;
                                        }
                                        let _ = commands.send(UiCommand::Exit).await;
                                        break;
                                    }
                                    input.clear();
                                    render_prompt(
                                        &mut terminal,
                                        &input,
                                        &mut command_completion,
                                        busy,
                                        &self.protocol,
                                        &self.model,
                                        &self.working_dir,
                                    )?;
                                }
                                KeyCode::Char('d')
                                    if key.modifiers.contains(KeyModifiers::CONTROL)
                                        && input.is_empty() =>
                                {
                                    let _ = commands.send(UiCommand::Exit).await;
                                    break;
                                }
                                KeyCode::Char(character) => input.insert(character),
                                KeyCode::Backspace => input.backspace(),
                                KeyCode::Delete => input.delete(),
                                KeyCode::Left => input.move_left(),
                                KeyCode::Right => input.move_right(),
                                KeyCode::Home => input.move_home(),
                                KeyCode::End => input.move_end(),
                                KeyCode::Up => input.history_previous(),
                                KeyCode::Down => input.history_next(),
                                _ => continue,
                            }
                            render_prompt(
                                &mut terminal,
                                &input,
                                &mut command_completion,
                                busy,
                                &self.protocol,
                                &self.model,
                                &self.working_dir,
                            )?;
                        }
                        _ => {}
                    }
                }
            }
        }

        terminal.leave_line()?;
        Ok(())
    }
}

fn is_stale_turn_event(event: &Event) -> bool {
    matches!(
        event,
        Event::AgentStarted { .. }
            | Event::TextDelta(_)
            | Event::Thinking(_)
            | Event::ToolCallStart { .. }
            | Event::ToolCallEnd { .. }
            | Event::Usage { .. }
            | Event::Error(_)
    )
}

fn sync_command_menu(
    terminal: &mut InlineTerminal,
    input: &InputState,
    completion: &mut CommandCompletionState,
    busy: bool,
) {
    completion.sync(input.text(), input.cursor(), busy);
    terminal.set_command_menu(completion.items(), completion.selected_index());
}

fn render_prompt(
    terminal: &mut InlineTerminal,
    input: &InputState,
    completion: &mut CommandCompletionState,
    busy: bool,
    protocol: &str,
    model: &str,
    working_dir: &std::path::Path,
) -> std::io::Result<()> {
    sync_command_menu(terminal, input, completion, busy);
    terminal.prompt(input, protocol, model, working_dir)
}

#[cfg(test)]
mod tests {
    use ash_core::StopReason;

    use super::*;

    #[test]
    fn ignores_late_render_events_while_a_turn_is_being_rolled_back() {
        assert!(is_stale_turn_event(&Event::TextDelta("late".into())));
        assert!(is_stale_turn_event(&Event::Thinking("late".into())));
        assert!(is_stale_turn_event(&Event::Error("cancelled".into())));
        assert!(!is_stale_turn_event(&Event::AgentFinished {
            reason: StopReason::Aborted,
        }));
        assert!(!is_stale_turn_event(&Event::TurnRolledBack {
            messages: Vec::new(),
            prompt: "draft".into(),
        }));
    }
}
