use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::Instant,
};

use ash_core::{Content, ContentBlock, Message, MessageContent, SessionSummary};
use crossterm::terminal;
use serde_json::Value;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

use crate::{
    history_block::HistoryBlock,
    inline_surface::InlineSurface,
    input::InputState,
    live_block::LiveBlock,
    scrollback::sanitize_single_line,
    slash_command::CommandCompletion,
    stream_state::{format_elapsed, FinishedStream, StreamRefresh, StreamState},
    tool_display::tool_activity_summary,
    viewport::{self, drawable_width, ViewportInput, COMPOSER_TEXT_COLUMN},
};

const CONTENT_PREFIX_COLUMNS: u16 = 2;
const TERMINAL_SAFE_COLUMN: u16 = 1;

#[derive(Debug)]
struct PromptSnapshot {
    protocol: String,
    model: String,
    working_dir: PathBuf,
    text: String,
    cursor_column: u16,
}

impl PromptSnapshot {
    fn new(protocol: &str, model: &str, working_dir: &Path) -> Self {
        Self {
            protocol: protocol.to_string(),
            model: model.to_string(),
            working_dir: working_dir.to_path_buf(),
            text: String::new(),
            cursor_column: 0,
        }
    }

    fn set_context(&mut self, protocol: &str, model: &str, working_dir: &Path) {
        self.protocol = protocol.to_string();
        self.model = model.to_string();
        self.working_dir = working_dir.to_path_buf();
    }
}

#[derive(Debug, Default)]
struct StatusState {
    header: String,
    started_at: Option<Instant>,
    frame: usize,
    queued_messages: usize,
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

    fn is_busy(&self) -> bool {
        self.started_at.is_some()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
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

#[derive(Debug, Default)]
struct MenuState {
    active: ActiveMenu,
    dirty: bool,
}

impl MenuState {
    fn set_commands(&mut self, items: &[CommandCompletion], selected: usize) {
        if matches!(self.active, ActiveMenu::Sessions { .. }) {
            return;
        }
        let selected = selected.min(items.len().saturating_sub(1));
        let active = if items.is_empty() {
            ActiveMenu::None
        } else {
            ActiveMenu::Commands {
                items: items.to_vec(),
                selected,
            }
        };
        if self.active != active {
            self.active = active;
            self.dirty = true;
        }
    }

    fn set_sessions(&mut self, items: &[SessionSummary], selected: usize) {
        let selected = selected.min(items.len().saturating_sub(1));
        let active = if items.is_empty() {
            ActiveMenu::None
        } else {
            ActiveMenu::Sessions {
                items: items.to_vec(),
                selected,
            }
        };
        if self.active != active {
            self.active = active;
            self.dirty = true;
        }
    }

    fn commands(&self) -> (&[CommandCompletion], usize) {
        match &self.active {
            ActiveMenu::Commands { items, selected } => (items, *selected),
            ActiveMenu::None | ActiveMenu::Sessions { .. } => (&[], 0),
        }
    }

    fn sessions(&self) -> (&[SessionSummary], usize) {
        match &self.active {
            ActiveMenu::Sessions { items, selected } => (items, *selected),
            ActiveMenu::None | ActiveMenu::Commands { .. } => (&[], 0),
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

pub(crate) struct InlineTerminal {
    surface: InlineSurface,
    prompt: PromptSnapshot,
    pending_blocks: Vec<LiveBlock>,
    next_live_block_id: u64,
    status: StatusState,
    menus: MenuState,
    stream: StreamState,
    current_turn_id: Option<u64>,
    next_turn_id: u64,
}

impl InlineTerminal {
    pub fn enter(protocol: &str, model: &str, working_dir: &Path) -> io::Result<Self> {
        let surface = InlineSurface::enter()?;
        Ok(Self {
            surface,
            prompt: PromptSnapshot::new(protocol, model, working_dir),
            pending_blocks: Vec::new(),
            next_live_block_id: 1,
            status: StatusState::default(),
            menus: MenuState::default(),
            stream: StreamState::default(),
            current_turn_id: None,
            next_turn_id: 1,
        })
    }

    pub fn welcome(&mut self) -> io::Result<()> {
        self.redraw()
    }

    pub fn command_output(&mut self, message: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.push_history_block(HistoryBlock::info(message));
        self.commit_and_redraw()
    }

    pub fn command_error(&mut self, message: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.push_history_block(HistoryBlock::error(message));
        self.commit_and_redraw()
    }

    pub fn start_new_session(&mut self) -> io::Result<()> {
        self.replace_viewport(|_| {})
    }

    pub fn rollback_turn(&mut self) -> io::Result<()> {
        let turn_id = self.current_turn_id;
        self.synchronized(|terminal| {
            if let Some(turn_id) = turn_id {
                terminal
                    .pending_blocks
                    .retain(|block| !block.belongs_to_turn(turn_id));
            }
            terminal.reset_turn_state();
            terminal.render_viewport()
        })
    }

    pub fn restore_session(
        &mut self,
        messages: &[Message],
        protocol: &str,
        model: &str,
        working_dir: &Path,
    ) -> io::Result<()> {
        self.replace_viewport(|terminal| {
            terminal.prompt.set_context(protocol, model, working_dir);
            terminal.push_restored_messages(messages);
        })
    }

    pub fn command_blocked(&mut self, command: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.status.header = format!("/{command} unavailable while working");
        self.refresh_status()
    }

    pub fn set_command_menu(&mut self, items: &[CommandCompletion], selected: usize) {
        self.menus.set_commands(items, selected);
    }

    pub fn set_session_menu(&mut self, items: &[SessionSummary], selected: usize) {
        self.menus.set_sessions(items, selected);
    }

    pub fn prompt(&mut self, input: &InputState) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.prompt_at(input, width, height)
    }

    fn prompt_at(&mut self, input: &InputState, width: u16, height: u16) -> io::Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        let input_width = width
            .saturating_sub(COMPOSER_TEXT_COLUMN)
            .saturating_sub(TERMINAL_SAFE_COLUMN);
        let view = input.view(input_width);
        self.prompt.text = view.text;
        self.prompt.cursor_column = view.cursor_column;
        self.menus.dirty = false;
        self.redraw_at(width, height)
    }

    pub fn commit_input(&mut self, input: &str) -> io::Result<()> {
        let turn_id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        self.current_turn_id = Some(turn_id);
        self.status.start("Working");
        self.commit_user_message(input)
    }

    pub fn commit_exit(&mut self, input: &str) -> io::Result<()> {
        self.current_turn_id = None;
        self.status.stop();
        self.commit_user_message(input)
    }

    fn commit_user_message(&mut self, input: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.stream.reset();
        self.menus.clear();
        self.push_user_prompt(input);
        self.commit_and_redraw()
    }

    pub fn agent_started(&mut self) -> io::Result<()> {
        if !self.status.is_busy() {
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
        self.commit_and_redraw()
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
        self.commit_and_redraw()
    }

    pub fn error(&mut self, error: &str) -> io::Result<()> {
        self.finish_stream();
        self.status.header = "Failed".to_string();
        self.push_history_block(HistoryBlock::error(error));
        self.commit_and_redraw()
    }

    pub fn finish_response(&mut self) -> io::Result<()> {
        self.finish_stream();
        let elapsed_seconds = self.status.elapsed_seconds();
        self.status.stop();
        self.push_history_block(HistoryBlock::worked(format_elapsed(elapsed_seconds)));
        let result = self.commit_and_redraw();
        self.current_turn_id = None;
        result
    }

    pub fn refresh_content(&mut self) -> io::Result<()> {
        let Some(refresh) = self.stream.take_refresh() else {
            return Ok(());
        };
        self.synchronized(|terminal| {
            match refresh {
                StreamRefresh::Assistant { pending, block_id } => {
                    if block_id.is_none() {
                        terminal.commit_pending_blocks()?;
                    }
                    if let Some(id) = terminal.append_assistant(pending, block_id) {
                        terminal.stream.set_assistant_block_id(id);
                    }
                }
                StreamRefresh::Reasoning => {
                    terminal.commit_pending_blocks()?;
                    let width = terminal.markdown_width()?;
                    terminal.stream.refresh_reasoning(width);
                }
            }
            terminal.render_viewport()
        })
    }

    pub fn set_queued_messages(&mut self, queued_messages: usize) {
        self.status.queued_messages = queued_messages;
    }

    pub fn refresh_status(&mut self) -> io::Result<()> {
        if !self.status.is_busy() {
            return Ok(());
        }
        self.status.frame = self.status.frame.wrapping_add(1);
        self.synchronized(|terminal| {
            if terminal.stream.is_reasoning() {
                terminal.commit_pending_blocks()?;
                let width = terminal::size()?
                    .0
                    .saturating_sub(CONTENT_PREFIX_COLUMNS)
                    .max(1);
                terminal.stream.refresh_reasoning(width);
            }
            terminal.render_viewport()
        })
    }

    pub fn leave_line(&mut self) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal.finish_stream();
            terminal.commit_pending_blocks()?;
            terminal.render_viewport()?;
            terminal.surface.leave_screen()
        })
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
            Some(FinishedStream::Thought { elapsed_seconds }) => {
                self.push_thought_block(format!("Thought for {}", format_elapsed(elapsed_seconds)))
            }
            None => {}
        }
    }

    fn append_assistant(&mut self, source: String, block_id: Option<u64>) -> Option<u64> {
        if source.is_empty() {
            return block_id;
        }
        if let Some(id) = block_id {
            if let Some(block) = self
                .pending_blocks
                .iter_mut()
                .find(|block| block.id() == id)
            {
                let _ = block.append_markdown_source(&source);
                return Some(id);
            }
        }
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::assistant(id, source));
        Some(id)
    }

    fn reset_inline_state(&mut self) -> io::Result<()> {
        self.surface.reset()?;
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.menus.clear();
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
        self.synchronized(|terminal| terminal.render_viewport())
    }

    fn commit_and_redraw(&mut self) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal.commit_pending_blocks()?;
            terminal.render_viewport()
        })
    }

    fn redraw_at(&mut self, width: u16, height: u16) -> io::Result<()> {
        self.synchronized(|terminal| terminal.render_viewport_at(width, height))
    }

    fn replace_viewport(&mut self, prepare: impl FnOnce(&mut Self)) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal.commit_pending_blocks()?;
            terminal.reset_inline_state()?;
            prepare(terminal);
            terminal.commit_pending_blocks()?;
            terminal.render_viewport()
        })
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

    fn render_viewport(&mut self) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.render_viewport_at(width, height)
    }

    fn render_viewport_at(&mut self, width: u16, height: u16) -> io::Result<()> {
        let frame = self.viewport_frame(width, height);
        self.surface.render_frame(&frame)
    }

    fn allocate_live_id(&mut self) -> u64 {
        let id = self.next_live_block_id;
        self.next_live_block_id = self.next_live_block_id.saturating_add(1);
        id
    }

    fn push_live(&mut self, block: LiveBlock) {
        self.pending_blocks
            .push(block.with_turn(self.current_turn_id));
    }

    fn push_history_block(&mut self, block: HistoryBlock) {
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::history(id, block));
    }

    fn push_user_prompt(&mut self, text: &str) {
        let model = sanitize_single_line(&self.prompt.model);
        let working_dir = self.prompt.working_dir.clone();
        self.push_history_block(HistoryBlock::user_with_prompt(text, &model, &working_dir));
    }

    fn push_assistant_block(&mut self, source: String) {
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::assistant(id, source));
    }

    fn push_thought_block(&mut self, source: String) {
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::thought(id, source));
    }

    fn push_tool_block(&mut self, name: String, arguments: Value, output: String, is_error: bool) {
        if self
            .pending_blocks
            .last_mut()
            .is_some_and(|block| block.try_append_read(&name, &arguments, is_error))
        {
            return;
        }
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::tool(id, name, arguments, output, is_error));
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
                    self.push_user_prompt(&text);
                }
                MessageContent::Assistant(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text(text) if !text.is_empty() => {
                                self.push_assistant_block(text.clone());
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
                            ContentBlock::Text(_) => {}
                        }
                    }
                }
                MessageContent::ToolResult { .. } => {}
            }
        }
    }

    fn commit_pending_blocks(&mut self) -> io::Result<()> {
        let width = terminal::size()?.0.max(1);
        let blocks = std::mem::take(&mut self.pending_blocks);
        for block in blocks {
            self.commit_block(&block, width)?;
        }
        Ok(())
    }

    fn commit_block(&mut self, block: &LiveBlock, terminal_width: u16) -> io::Result<()> {
        let width = drawable_width(terminal_width);
        let buffer = block.render(width);
        self.surface.commit_output(&buffer)?;
        self.stream.clear_block_id(block.id());
        Ok(())
    }

    fn viewport_frame(&self, width: u16, height: u16) -> viewport::ViewportFrame {
        let elapsed = format_elapsed(self.status.elapsed_seconds());
        let status_header = sanitize_single_line(&self.status.header);
        let queued = queued_status(self.status.queued_messages);
        let model = sanitize_single_line(&self.prompt.model);
        let (command_menu, command_menu_selected) = self.menus.commands();
        let (session_menu, session_menu_selected) = self.menus.sessions();
        viewport::render(ViewportInput {
            terminal_width: width,
            terminal_height: height,
            pending_blocks: &self.pending_blocks,
            busy: self.status.is_busy(),
            active_lines: self.stream.active_lines(),
            status_header: &status_header,
            status_dots: status_dots(self.status.frame),
            elapsed: &elapsed,
            queued: &queued,
            prompt: &self.prompt.text,
            prompt_cursor_column: self.prompt.cursor_column,
            command_menu,
            command_menu_selected,
            session_menu,
            session_menu_selected,
            model: &model,
            working_dir: &self.prompt.working_dir,
            separate_from_output: self.surface.has_committed_output(),
        })
    }
}

fn status_dots(frame: usize) -> &'static str {
    const FRAMES: [&str; 4] = [".  ", ".. ", "...", ".. "];
    FRAMES[frame % FRAMES.len()]
}

fn terminal_size() -> io::Result<(u16, u16)> {
    let (width, height) = terminal::size()?;
    Ok((width.max(1), height.max(1)))
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
    fn reserves_one_row_per_command_completion() {
        assert_eq!(viewport::command_menu_rows(0), 0);
        assert_eq!(viewport::command_menu_rows(3), 3);
    }

    #[test]
    fn formats_queued_message_status() {
        assert_eq!(queued_status(0), "");
        assert_eq!(queued_status(1), " · 1 queued");
        assert_eq!(queued_status(3), " · 3 queued");
    }
}
