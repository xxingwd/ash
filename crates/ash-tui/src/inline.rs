use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::Instant,
};

use ash_core::{Content, ContentBlock, Message, MessageContent, SessionSummary};
use crossterm::terminal;
use ratatui::layout::Position;
use serde_json::Value;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

use crate::{
    history_block::HistoryBlock,
    inline_surface::AlternateScreen,
    input::InputState,
    live_block::LiveBlock,
    scrollback::sanitize_single_line,
    slash_command::CommandCompletion,
    stream_state::{format_elapsed, FinishedStream, StreamRefresh, StreamState},
    tool_display::tool_activity_summary,
    viewport::{self, ViewportInput, COMPOSER_TEXT_COLUMN},
};

const CONTENT_PREFIX_COLUMNS: u16 = 2;
const TERMINAL_SAFE_COLUMN: u16 = 1;

#[derive(Debug)]
struct SessionView {
    protocol: String,
    model: String,
    working_dir: PathBuf,
}

impl SessionView {
    fn new(protocol: &str, model: &str, working_dir: &Path) -> Self {
        Self {
            protocol: protocol.to_string(),
            model: model.to_string(),
            working_dir: working_dir.to_path_buf(),
        }
    }

    fn update(&mut self, protocol: &str, model: &str, working_dir: &Path) {
        self.protocol = protocol.to_string();
        self.model = model.to_string();
        self.working_dir = working_dir.to_path_buf();
    }
}

#[derive(Debug, Default)]
struct ComposerState {
    lines: Vec<String>,
    cursor_row: u16,
    cursor_column: u16,
}

pub(crate) struct TerminalView<'a> {
    pub(crate) input: &'a InputState,
    pub(crate) commands: &'a [CommandCompletion],
    pub(crate) selected_command: usize,
    pub(crate) sessions: &'a [SessionSummary],
    pub(crate) selected_session: usize,
    pub(crate) busy: bool,
    pub(crate) queued_messages: usize,
}

impl ComposerState {
    fn clear(&mut self) {
        self.lines.clear();
        self.cursor_row = 0;
        self.cursor_column = 0;
    }
}

#[derive(Debug, Default)]
struct StatusState {
    header: String,
    started_at: Option<Instant>,
    frame: usize,
}

#[derive(Clone, Copy, Debug)]
struct TextSelection {
    anchor: Position,
    focus: Position,
}

impl TextSelection {
    fn new(position: Position) -> Self {
        Self {
            anchor: position,
            focus: position,
        }
    }
}

impl StatusState {
    fn start(&mut self, header: &str) {
        self.header.clear();
        self.header.push_str(header);
        self.started_at = Some(Instant::now());
        self.frame = 0;
    }

    fn stop(&mut self) {
        self.header.clear();
        self.started_at = None;
        self.frame = 0;
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn elapsed_seconds(&self) -> u64 {
        self.started_at
            .map_or(0, |started| started.elapsed().as_secs())
    }
}

#[derive(Debug, Default)]
enum ActiveMenu {
    #[default]
    None,
    Commands {
        items: Vec<CommandCompletion>,
        selected: usize,
    },
    Sessions {
        items: Vec<SessionSummary>,
        selected: usize,
    },
}

impl ActiveMenu {
    fn set_commands(&mut self, items: &[CommandCompletion], selected: usize) {
        let selected = selected.min(items.len().saturating_sub(1));
        *self = if items.is_empty() {
            Self::None
        } else {
            Self::Commands {
                items: items.to_vec(),
                selected,
            }
        };
    }

    fn set_sessions(&mut self, items: &[SessionSummary], selected: usize) {
        let selected = selected.min(items.len().saturating_sub(1));
        *self = if items.is_empty() {
            Self::None
        } else {
            Self::Sessions {
                items: items.to_vec(),
                selected,
            }
        };
    }

    fn commands(&self) -> (&[CommandCompletion], usize) {
        match self {
            Self::Commands { items, selected } => (items, *selected),
            Self::None | Self::Sessions { .. } => (&[], 0),
        }
    }

    fn sessions(&self) -> (&[SessionSummary], usize) {
        match self {
            Self::Sessions { items, selected } => (items, *selected),
            Self::None | Self::Commands { .. } => (&[], 0),
        }
    }

    fn clear(&mut self) {
        *self = Self::None;
    }
}

#[derive(Debug, Default)]
struct ViewState {
    composer: ComposerState,
    menu: ActiveMenu,
    busy: bool,
    queued_messages: usize,
}

pub(crate) struct TerminalUi {
    surface: AlternateScreen,
    session: SessionView,
    view: ViewState,
    transcript: Vec<LiveBlock>,
    scroll_top: Option<u16>,
    next_block_id: u64,
    status: StatusState,
    selection: Option<TextSelection>,
    stream: StreamState,
    current_turn_id: Option<u64>,
    next_turn_id: u64,
}

impl TerminalUi {
    pub fn enter(protocol: &str, model: &str, working_dir: &Path) -> io::Result<Self> {
        let surface = AlternateScreen::enter()?;
        Ok(Self {
            surface,
            session: SessionView::new(protocol, model, working_dir),
            view: ViewState::default(),
            transcript: Vec::new(),
            scroll_top: None,
            next_block_id: 1,
            status: StatusState::default(),
            selection: None,
            stream: StreamState::default(),
            current_turn_id: None,
            next_turn_id: 1,
        })
    }

    pub fn welcome(&mut self) -> io::Result<()> {
        self.enqueue_welcome();
        self.redraw()
    }

    fn enqueue_welcome(&mut self) {
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::welcome(id, self.session.working_dir.clone()));
    }

    pub fn command_output(&mut self, message: &str) -> io::Result<()> {
        self.view.composer.clear();
        self.scroll_top = None;
        self.push_history_block(HistoryBlock::info(message));
        self.redraw()
    }

    pub fn show_session_status(&mut self) -> io::Result<()> {
        let status = format!(
            "Model: {}\nProtocol: {}\nDirectory: {}",
            self.session.model,
            self.session.protocol,
            self.session.working_dir.display()
        );
        self.command_output(&status)
    }

    pub fn command_error(&mut self, message: &str) -> io::Result<()> {
        self.view.composer.clear();
        self.scroll_top = None;
        self.push_history_block(HistoryBlock::error(message));
        self.redraw()
    }

    pub fn start_new_session(&mut self) -> io::Result<()> {
        self.begin_fresh_viewport()?;
        self.enqueue_welcome();
        self.redraw()
    }

    fn begin_fresh_viewport(&mut self) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal.transcript.clear();
            terminal.scroll_top = None;
            terminal.reset_ui_state()?;
            Ok(())
        })
    }

    pub fn rollback_turn(&mut self) -> io::Result<()> {
        let turn_id = self
            .current_turn_id
            .or_else(|| self.transcript.iter().rev().find_map(LiveBlock::turn_id));
        let (width, height) = terminal_size()?;
        self.synchronized(|terminal| {
            if let Some(turn_id) = turn_id {
                terminal
                    .transcript
                    .retain(|block| !block.belongs_to_turn(turn_id));
            }
            terminal.scroll_top = None;
            terminal.reset_turn_state();
            terminal.render_viewport(width, height)
        })
    }

    pub fn restore_session(
        &mut self,
        messages: &[Message],
        protocol: &str,
        model: &str,
        working_dir: &Path,
    ) -> io::Result<()> {
        self.begin_fresh_viewport()?;
        self.session.update(protocol, model, working_dir);
        self.enqueue_welcome();
        self.push_restored_messages(messages);
        self.redraw()
    }

    pub fn command_blocked(&mut self, command: &str) -> io::Result<()> {
        self.view.composer.clear();
        self.status.header = format!("/{command} unavailable while working");
        self.refresh_status()
    }

    pub fn sync_view(&mut self, view: TerminalView<'_>) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.sync_view_at(view, width, height)
    }

    fn sync_view_at(&mut self, view: TerminalView<'_>, width: u16, height: u16) -> io::Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        let input = view.input.view(composer_text_width(width));
        self.view.composer.lines = input.lines;
        self.view.composer.cursor_row = input.cursor_row;
        self.view.composer.cursor_column = input.cursor_column;
        if view.sessions.is_empty() {
            self.view
                .menu
                .set_commands(view.commands, view.selected_command);
        } else {
            self.view
                .menu
                .set_sessions(view.sessions, view.selected_session);
        }
        self.view.busy = view.busy;
        self.view.queued_messages = view.queued_messages;
        self.redraw_at(width, height)
    }

    pub fn composer_text_width(&self) -> io::Result<u16> {
        Ok(composer_text_width(terminal_size()?.0))
    }

    pub fn has_response_block(&self) -> bool {
        self.stream.has_content()
            || self.current_turn_id.is_some_and(|turn_id| {
                self.transcript
                    .iter()
                    .any(|block| block.is_response_for_turn(turn_id))
            })
    }

    pub fn commit_input(&mut self, input: &str) -> io::Result<()> {
        let turn_id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        self.current_turn_id = Some(turn_id);
        self.scroll_top = None;
        self.status.start("Working");
        self.commit_user_message(input)
    }

    pub fn commit_exit(&mut self, input: &str) -> io::Result<()> {
        self.current_turn_id = None;
        self.status.stop();
        self.commit_user_message(input)
    }

    fn commit_user_message(&mut self, input: &str) -> io::Result<()> {
        self.view.composer.clear();
        self.stream.reset();
        self.view.menu.clear();
        self.push_history_block(HistoryBlock::user(input));
        self.redraw()
    }

    pub fn agent_started(&mut self) -> io::Result<()> {
        if self.status.started_at.is_none() {
            self.status.start("Working");
            self.redraw()?;
        }
        Ok(())
    }

    pub fn text(&mut self, text: &str) {
        let finished = self.stream.start_assistant();
        self.commit_finished_stream(finished);
        self.status.header = "Working".to_string();
        self.stream.push_assistant(text);
    }

    pub fn thinking(&mut self, text: &str) {
        let finished = self.stream.start_reasoning();
        self.commit_finished_stream(finished);
        self.stream.push_reasoning(text);
        self.status.header = "Thinking".to_string();
    }

    pub fn tool_start(&mut self, name: &str, arguments: &Value) -> io::Result<()> {
        self.finish_stream();
        self.status.header = tool_activity_summary(name, arguments, self.markdown_width()?);
        self.redraw()
    }

    pub fn tool_end(
        &mut self,
        name: &str,
        arguments: &Value,
        output: &str,
        is_error: bool,
    ) -> io::Result<()> {
        self.status.header = "Working".to_string();
        self.push_tool_block(
            name.to_string(),
            arguments.clone(),
            output.to_string(),
            is_error,
        );
        self.redraw()
    }

    pub fn error(&mut self, error: &str) -> io::Result<()> {
        self.finish_stream();
        self.status.header = "Failed".to_string();
        self.push_history_block(HistoryBlock::error(error));
        self.redraw()
    }

    pub fn finish_response(&mut self) -> io::Result<()> {
        self.finish_stream();
        let elapsed_seconds = self.status.elapsed_seconds();
        self.status.stop();
        self.push_history_block(HistoryBlock::worked(format_elapsed(elapsed_seconds)));
        let result = self.redraw();
        self.current_turn_id = None;
        result
    }

    pub fn refresh_content(&mut self) -> io::Result<()> {
        match self.stream.take_refresh() {
            Some(StreamRefresh::Assistant { pending, block_id }) => {
                if let Some(id) = self.append_assistant(pending, block_id) {
                    self.stream.set_assistant_block_id(id);
                }
            }
            Some(StreamRefresh::Reasoning) => {
                let width = self.markdown_width()?;
                self.stream.refresh_reasoning(width);
            }
            None => return Ok(()),
        }
        self.redraw()
    }

    pub fn resize_view(
        &mut self,
        view: TerminalView<'_>,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        self.selection = None;
        self.surface.resize(width, height)?;
        if self.stream.is_reasoning() {
            self.stream
                .refresh_reasoning(width.saturating_sub(CONTENT_PREFIX_COLUMNS).max(1));
        }
        self.sync_view_at(view, width, height)
    }

    pub fn scroll_page_up(&mut self) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        let rows = self.viewport_frame(width, height).page_rows;
        self.scroll_up(rows, width, height)
    }

    pub fn scroll_page_down(&mut self) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        let rows = self.viewport_frame(width, height).page_rows;
        self.scroll_down(rows, width, height)
    }

    pub fn scroll_lines_up(&mut self, rows: u16) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.scroll_up(rows, width, height)
    }

    pub fn scroll_lines_down(&mut self, rows: u16) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.scroll_down(rows, width, height)
    }

    pub fn scroll_to_top(&mut self) -> io::Result<()> {
        self.scroll_top = Some(0);
        self.redraw()
    }

    pub fn scroll_to_bottom(&mut self) -> io::Result<()> {
        self.scroll_top = None;
        self.redraw()
    }

    pub fn toggle_latest_thought(&mut self) -> io::Result<()> {
        self.selection = None;
        let Some(block) = self
            .transcript
            .iter_mut()
            .rev()
            .find(|block| block.is_thought())
        else {
            return Ok(());
        };
        if block.toggle_thought() {
            self.redraw()?;
        }
        Ok(())
    }

    pub fn toggle_thought_at(&mut self, row: u16) -> io::Result<()> {
        self.selection = None;
        let (width, height) = terminal_size()?;
        let Some(block_id) = self.viewport_frame(width, height).thought_at(row) else {
            return Ok(());
        };
        let Some(block) = self
            .transcript
            .iter_mut()
            .find(|block| block.id() == block_id)
        else {
            return Ok(());
        };
        if block.toggle_thought() {
            self.redraw_at(width, height)?;
        }
        Ok(())
    }

    pub fn start_selection(&mut self, column: u16, row: u16) -> io::Result<()> {
        self.selection = Some(TextSelection::new(Position::new(column, row)));
        self.redraw()
    }

    pub fn drag_selection(&mut self, column: u16, row: u16) -> io::Result<()> {
        let Some(selection) = &mut self.selection else {
            return Ok(());
        };
        selection.focus = Position::new(column, row);
        self.redraw()
    }

    pub fn finish_selection(&mut self, column: u16, row: u16) -> io::Result<()> {
        let Some(mut selection) = self.selection else {
            return Ok(());
        };
        selection.focus = Position::new(column, row);
        if selection.anchor == selection.focus {
            self.selection = None;
            return self.toggle_thought_at(row);
        }

        self.selection = Some(selection);
        let (width, height) = terminal_size()?;
        let text = self
            .viewport_frame(width, height)
            .selection_text(selection.anchor, selection.focus);
        let copy_result = if text.is_empty() {
            Ok(())
        } else {
            self.surface.copy_to_clipboard(&text)
        };
        self.selection = None;
        let redraw_result = self.redraw_at(width, height);
        copy_result.and(redraw_result)
    }

    pub fn refresh_status(&mut self) -> io::Result<()> {
        if !self.view.busy {
            return Ok(());
        }
        self.status.frame = self.status.frame.wrapping_add(1);
        if self.stream.is_reasoning() {
            let width = terminal::size()?
                .0
                .saturating_sub(CONTENT_PREFIX_COLUMNS)
                .max(1);
            self.stream.refresh_reasoning(width);
        }
        self.redraw()
    }

    pub fn leave(&mut self) -> io::Result<()> {
        self.finish_stream();
        self.surface.leave_screen()
    }

    fn finish_stream(&mut self) {
        let finished = self.stream.finish();
        self.commit_finished_stream(finished);
    }

    fn commit_finished_stream(&mut self, finished: Option<FinishedStream>) {
        match finished {
            Some(FinishedStream::Assistant { pending, block_id }) => {
                let _ = self.append_assistant(pending, block_id);
            }
            Some(FinishedStream::Thought {
                source,
                elapsed_seconds,
            }) => self.push_thought_block(source, elapsed_seconds),
            None => {}
        }
    }

    fn append_assistant(&mut self, source: String, block_id: Option<u64>) -> Option<u64> {
        if source.is_empty() {
            return block_id;
        }
        if let Some(id) = block_id {
            if let Some(block) = self.transcript.iter_mut().find(|block| block.id() == id) {
                let _ = block.append_markdown_source(&source);
                return Some(id);
            }
        }
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::assistant(id, source));
        Some(id)
    }

    fn reset_ui_state(&mut self) -> io::Result<()> {
        self.surface.reset()?;
        self.view = ViewState::default();
        self.selection = None;
        self.reset_turn_state();
        Ok(())
    }

    fn reset_turn_state(&mut self) {
        self.status.reset();
        self.current_turn_id = None;
        self.stream.reset();
    }

    fn markdown_width(&self) -> io::Result<u16> {
        Ok(terminal::size()?
            .0
            .saturating_sub(CONTENT_PREFIX_COLUMNS)
            .max(1))
    }

    fn redraw(&mut self) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.redraw_at(width, height)
    }

    fn redraw_at(&mut self, width: u16, height: u16) -> io::Result<()> {
        self.synchronized(|terminal| terminal.render_viewport(width, height))
    }

    fn scroll_up(&mut self, rows: u16, width: u16, height: u16) -> io::Result<()> {
        self.selection = None;
        let frame = self.viewport_frame(width, height);
        if frame.max_scroll_top == 0 {
            return Ok(());
        }
        self.scroll_top = Some(frame.scroll_top.saturating_sub(rows));
        self.redraw_at(width, height)
    }

    fn scroll_down(&mut self, rows: u16, width: u16, height: u16) -> io::Result<()> {
        self.selection = None;
        let frame = self.viewport_frame(width, height);
        let next = frame.scroll_top.saturating_add(rows);
        self.scroll_top = (next < frame.max_scroll_top).then_some(next);
        self.redraw_at(width, height)
    }

    fn synchronized(
        &mut self,
        operation: impl FnOnce(&mut Self) -> io::Result<()>,
    ) -> io::Result<()> {
        self.surface.begin_synchronized()?;
        let operation_result = operation(self);
        let finish_result = self.surface.end_synchronized();
        operation_result.and(finish_result)
    }

    fn render_viewport(&mut self, width: u16, height: u16) -> io::Result<()> {
        let mut frame = self.viewport_frame(width, height);
        normalize_scroll_top(&mut self.scroll_top, frame.scroll_top);
        if let Some(selection) = self.selection {
            frame.highlight_selection(selection.anchor, selection.focus);
        }
        self.surface.render_frame(&frame)
    }

    fn allocate_block_id(&mut self) -> u64 {
        let id = self.next_block_id;
        self.next_block_id = self.next_block_id.saturating_add(1);
        id
    }

    fn push_block(&mut self, block: LiveBlock) {
        self.transcript.push(block.with_turn(self.current_turn_id));
    }

    fn push_history_block(&mut self, block: HistoryBlock) {
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::history(id, block));
    }

    fn push_assistant_block(&mut self, source: String) {
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::assistant(id, source));
    }

    fn push_thought_block(&mut self, source: String, elapsed_seconds: u64) {
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::thought(id, source, elapsed_seconds));
    }

    fn push_tool_block(&mut self, name: String, arguments: Value, output: String, is_error: bool) {
        if self
            .transcript
            .last_mut()
            .is_some_and(|block| block.try_append_tool(&name, &arguments, is_error))
        {
            return;
        }
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::tool(id, name, arguments, output, is_error));
    }

    fn push_restored_messages(&mut self, messages: &[Message]) {
        let tool_results = messages
            .iter()
            .filter_map(|message| match &message.content {
                MessageContent::ToolResult { id, result, .. } => {
                    let output = match result {
                        Ok(output) | Err(output) => output.as_str(),
                    };
                    Some((id, (result.is_err(), output)))
                }
                MessageContent::User(_) | MessageContent::Assistant(_) => None,
            })
            .collect::<HashMap<_, _>>();

        for message in messages {
            match &message.content {
                MessageContent::User(contents) => {
                    let turn_id = self.next_turn_id;
                    self.next_turn_id = self.next_turn_id.saturating_add(1);
                    self.current_turn_id = Some(turn_id);
                    let text = contents
                        .iter()
                        .map(|content| match content {
                            Content::Text(text) => text.clone(),
                            Content::Image { media_type, .. } => {
                                format!("[image: {media_type}]")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.push_history_block(HistoryBlock::user(&text));
                }
                MessageContent::Assistant(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text(text) if !text.is_empty() => {
                                self.push_assistant_block(text.clone());
                            }
                            ContentBlock::Thought {
                                text,
                                elapsed_seconds,
                            } if !text.is_empty() => {
                                self.push_thought_block(text.clone(), *elapsed_seconds);
                            }
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => {
                                let (is_error, output) = tool_results
                                    .get(id)
                                    .copied()
                                    .unwrap_or((true, "tool result unavailable"));
                                self.push_tool_block(
                                    name.clone(),
                                    arguments.clone(),
                                    output.to_string(),
                                    is_error,
                                );
                            }
                            ContentBlock::Text(_) | ContentBlock::Thought { .. } => {}
                        }
                    }
                }
                MessageContent::ToolResult { .. } => {}
            }
        }
        self.current_turn_id = None;
    }

    fn viewport_frame(&self, width: u16, height: u16) -> viewport::ViewportFrame {
        let elapsed = format_elapsed(self.status.elapsed_seconds());
        let status_header = sanitize_single_line(&self.status.header);
        let queued = queued_status(self.view.queued_messages);
        let model = sanitize_single_line(&self.session.model);
        let protocol = sanitize_single_line(&self.session.protocol);
        let (command_menu, command_menu_selected) = self.view.menu.commands();
        let (session_menu, session_menu_selected) = self.view.menu.sessions();
        viewport::render(ViewportInput {
            terminal_width: width,
            terminal_height: height,
            transcript: &self.transcript,
            scroll_top: self.scroll_top,
            busy: self.view.busy,
            active_lines: self.stream.active_lines(),
            status_header: &status_header,
            status_dots: status_dots(self.status.frame),
            elapsed: &elapsed,
            queued: &queued,
            prompt_lines: &self.view.composer.lines,
            prompt_cursor_row: self.view.composer.cursor_row,
            prompt_cursor_column: self.view.composer.cursor_column,
            command_menu,
            command_menu_selected,
            session_menu,
            session_menu_selected,
            model: &model,
            protocol: &protocol,
            working_dir: &self.session.working_dir,
        })
    }
}

fn status_dots(frame: usize) -> &'static str {
    const FRAMES: [&str; 4] = [".  ", ".. ", "...", ".. "];
    FRAMES[frame % FRAMES.len()]
}

fn normalize_scroll_top(scroll_top: &mut Option<u16>, rendered_top: u16) {
    if scroll_top.is_some() {
        *scroll_top = Some(rendered_top);
    }
}

fn terminal_size() -> io::Result<(u16, u16)> {
    let (width, height) = terminal::size()?;
    Ok((width.max(1), height.max(1)))
}

fn composer_text_width(terminal_width: u16) -> u16 {
    terminal_width
        .saturating_sub(COMPOSER_TEXT_COLUMN)
        .saturating_sub(TERMINAL_SAFE_COLUMN)
        .max(1)
}

fn queued_status(queued_messages: usize) -> String {
    match queued_messages {
        0 => String::new(),
        1 => " · 1 queued".to_string(),
        count => format!(" · {count} queued"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animates_status_with_a_fixed_width_dot_pulse() {
        assert_eq!(status_dots(0), ".  ");
        assert_eq!(status_dots(1), ".. ");
        assert_eq!(status_dots(2), "...");
        assert_eq!(status_dots(3), ".. ");
        assert_eq!(status_dots(4), ".  ");
        assert!((0..8).all(|frame| UnicodeWidthStr::width(status_dots(frame)) == 3));
    }

    #[test]
    fn formats_queued_message_status() {
        assert_eq!(queued_status(0), "");
        assert_eq!(queued_status(1), " · 1 queued");
        assert_eq!(queued_status(3), " · 3 queued");
    }

    #[test]
    fn explicit_scroll_position_tracks_the_rendered_position() {
        let mut scroll_top = Some(20);
        normalize_scroll_top(&mut scroll_top, 8);
        assert_eq!(scroll_top, Some(8));

        let mut follow_bottom = None;
        normalize_scroll_top(&mut follow_bottom, 8);
        assert_eq!(follow_bottom, None);
    }
}
