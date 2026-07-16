use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::Instant,
};

use ash_core::{Content, ContentBlock, Message, MessageContent, SessionSummary, ToolCallId};
use crossterm::terminal;
use serde_json::Value;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

use crate::{
    block_layout::StackBoundary,
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
    busy: bool,
    header: String,
    started_at: Option<Instant>,
    frame: usize,
    queued_messages: usize,
}

impl StatusState {
    fn start(&mut self, header: &str) {
        self.busy = true;
        self.header.clear();
        self.header.push_str(header);
        self.started_at = Some(Instant::now());
        self.frame = 0;
    }

    fn stop(&mut self) {
        self.busy = false;
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
struct MenuState {
    commands: Vec<CommandCompletion>,
    command_selected: usize,
    sessions: Vec<SessionSummary>,
    session_selected: usize,
    dirty: bool,
}

impl MenuState {
    fn set_commands(&mut self, items: &[CommandCompletion], selected: usize) {
        let selected = selected.min(items.len().saturating_sub(1));
        if self.commands != items || self.command_selected != selected {
            self.commands.clear();
            self.commands.extend_from_slice(items);
            self.command_selected = selected;
            self.dirty = true;
        }
    }

    fn set_sessions(&mut self, items: &[SessionSummary], selected: usize) {
        let selected = selected.min(items.len().saturating_sub(1));
        if self.sessions != items || self.session_selected != selected {
            self.sessions.clear();
            self.sessions.extend_from_slice(items);
            self.session_selected = selected;
            self.dirty = true;
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

pub(crate) struct InlineTerminal {
    surface: InlineSurface,
    composer_background: Option<crate::palette::Rgb>,
    prompt: PromptSnapshot,
    history_boundary: StackBoundary,
    live_blocks: Vec<LiveBlock>,
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
        let composer_background = crate::palette::composer_background_color();
        Ok(Self {
            surface,
            composer_background,
            prompt: PromptSnapshot::new(protocol, model, working_dir),
            history_boundary: StackBoundary::default(),
            live_blocks: Vec::new(),
            next_live_block_id: 1,
            status: StatusState::default(),
            menus: MenuState::default(),
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
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::welcome(id, self.prompt.working_dir.clone()));
    }

    pub fn command_output(&mut self, message: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.push_history_block(HistoryBlock::info(message));
        self.redraw()
    }

    pub fn command_error(&mut self, message: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
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
            terminal.push_viewport_to_scrollback()?;
            terminal.live_blocks.clear();
            terminal.reset_inline_state();
            terminal.history_boundary = StackBoundary::default();
            Ok(())
        })
    }

    pub fn rollback_turn(&mut self) -> io::Result<()> {
        let turn_id = self.current_turn_id;
        let (width, height) = terminal_size()?;
        self.synchronized(|terminal| {
            terminal.surface.clear_viewport()?;
            if let Some(turn_id) = turn_id {
                terminal
                    .live_blocks
                    .retain(|block| !block.belongs_to_turn(turn_id));
            }
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
        self.prompt.set_context(protocol, model, working_dir);
        self.enqueue_welcome();
        self.push_restored_messages(messages);
        self.redraw()
    }

    pub fn command_blocked(&mut self, command: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.update_input()?;
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
        let terminal_width = terminal::size()?.0.max(1);
        let input_width = terminal_width
            .saturating_sub(COMPOSER_TEXT_COLUMN)
            .saturating_sub(TERMINAL_SAFE_COLUMN);
        let view = input.view(input_width);
        self.prompt.text = view.text;
        self.prompt.cursor_column = view.cursor_column;

        if self.surface.is_visible()
            && self.surface.prompt_width() == terminal_width
            && !self.menus.dirty
        {
            return self.update_input();
        }
        self.menus.dirty = false;
        self.redraw()
    }

    pub fn commit_input(&mut self, input: &str) -> io::Result<()> {
        self.commit_user_message(input, /*start_working*/ true)
    }

    pub fn commit_exit(&mut self, input: &str) -> io::Result<()> {
        self.commit_user_message(input, /*start_working*/ false)
    }

    fn commit_user_message(&mut self, input: &str, start_working: bool) -> io::Result<()> {
        self.current_turn_id = if start_working {
            let turn_id = self.next_turn_id;
            self.next_turn_id = self.next_turn_id.saturating_add(1);
            Some(turn_id)
        } else {
            None
        };
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.stream.reset();
        self.menus.clear();
        if start_working {
            self.status.start("Working");
        } else {
            self.status.stop();
        }
        self.push_history_block(HistoryBlock::user(input));
        self.redraw()
    }

    pub fn agent_started(&mut self) -> io::Result<()> {
        if !self.status.busy {
            self.status.start("Working");
            self.redraw()?;
        }
        Ok(())
    }

    pub fn text(&mut self, text: &str) -> io::Result<()> {
        let finished = self.stream.start_assistant();
        self.commit_finished_stream(finished);
        self.status.header = "Working".to_string();
        self.stream.push_assistant(text);
        Ok(())
    }

    pub fn thinking(&mut self, text: &str) -> io::Result<()> {
        let width = self.markdown_width()?;
        let finished = self.stream.start_reasoning();
        self.commit_finished_stream(finished);
        self.stream.push_reasoning(text, width);
        self.status.header = "Thinking".to_string();
        Ok(())
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
            Some(StreamRefresh::Reasoning) => {}
            None => return Ok(()),
        }
        self.redraw()
    }

    pub fn set_queued_messages(&mut self, queued_messages: usize) {
        self.status.queued_messages = queued_messages;
    }

    pub fn handle_resize(&mut self, width: u16, height: u16) {
        if !self.surface.is_visible() {
            return;
        }
        self.surface.handle_resize(width, height);
        if self.stream.is_reasoning() {
            self.stream
                .refresh_reasoning(width.saturating_sub(CONTENT_PREFIX_COLUMNS).max(1));
        }
    }

    pub fn refresh_status(&mut self) -> io::Result<()> {
        if !self.status.busy {
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

    pub fn leave_line(&mut self) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal.surface.clear_viewport()?;
            terminal.finish_stream();
            terminal.flush_all_live_blocks()?;
            terminal.surface.clear_current_line()
        })?;
        self.surface.reset();
        Ok(())
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
            Some(FinishedStream::Thought { elapsed_seconds }) => self.push_markdown_block(
                format!("Thought for {}", format_elapsed(elapsed_seconds)),
                true,
            ),
            None => {}
        }
    }

    fn append_assistant(&mut self, source: String, block_id: Option<u64>) -> Option<u64> {
        if source.is_empty() {
            return block_id;
        }
        if let Some(id) = block_id {
            if let Some(block) = self.live_blocks.iter_mut().find(|block| block.id() == id) {
                let _ = block.append_markdown_source(source);
                return Some(id);
            }
        }
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::markdown(id, source, false));
        Some(id)
    }

    fn reset_inline_state(&mut self) {
        self.surface.reset();
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.menus.clear();
        self.reset_turn_state();
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
        self.synchronized(|terminal| {
            terminal.surface.clear_viewport()?;
            terminal.flush_live_overflow(width, height)?;
            terminal.render_viewport(width, height)
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

    fn push_viewport_to_scrollback(&mut self) -> io::Result<()> {
        self.surface.push_history_to_scrollback()
    }

    fn render_viewport(&mut self, width: u16, height: u16) -> io::Result<()> {
        let frame = self.viewport_frame(width, height);
        self.surface.render_frame(&frame, width, &self.prompt.text)
    }

    fn update_input(&mut self) -> io::Result<()> {
        self.surface.update_input(
            &self.prompt.text,
            self.prompt.cursor_column,
            self.composer_background,
        )
    }

    fn allocate_live_id(&mut self) -> u64 {
        let id = self.next_live_block_id;
        self.next_live_block_id = self.next_live_block_id.saturating_add(1);
        id
    }

    fn push_live(&mut self, block: LiveBlock) {
        self.live_blocks.push(block.with_turn(self.current_turn_id));
    }

    fn push_history_block(&mut self, block: HistoryBlock) {
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::history(id, block));
    }

    fn push_markdown_block(&mut self, source: String, reasoning: bool) {
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::markdown(id, source, reasoning));
    }

    fn push_tool_block(&mut self, name: String, arguments: Value, output: String, is_error: bool) {
        if self
            .live_blocks
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
                MessageContent::ToolResult { id, result } => {
                    let output = match result {
                        Ok(output) | Err(output) => output.clone(),
                    };
                    Some((id.clone(), (result.is_err(), output)))
                }
                MessageContent::User(_) | MessageContent::Assistant(_) => None,
            })
            .collect::<HashMap<ToolCallId, (bool, String)>>();

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
                    self.push_history_block(HistoryBlock::user(&text));
                }
                MessageContent::Assistant(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text(text) if !text.is_empty() => {
                                self.push_markdown_block(text.clone(), false);
                            }
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => {
                                let (is_error, output) = tool_results
                                    .get(id)
                                    .cloned()
                                    .unwrap_or_else(|| (false, String::new()));
                                self.push_tool_block(
                                    name.clone(),
                                    arguments.clone(),
                                    output,
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

    fn flush_live_overflow(&mut self, width: u16, height: u16) -> io::Result<()> {
        while !self.live_blocks.is_empty() && self.viewport_frame(width, height).total_rows > height
        {
            let block = self.live_blocks.remove(0);
            self.flush_live_block(block, width)?;
        }
        Ok(())
    }

    fn flush_all_live_blocks(&mut self) -> io::Result<()> {
        let width = terminal::size()?.0.max(1);
        while !self.live_blocks.is_empty() {
            let block = self.live_blocks.remove(0);
            self.flush_live_block(block, width)?;
        }
        Ok(())
    }

    fn flush_live_block(&mut self, block: LiveBlock, terminal_width: u16) -> io::Result<()> {
        let width = drawable_width(terminal_width);
        if self.history_boundary.has_block() {
            self.surface.clear_current_line()?;
            self.surface.next_row()?;
        }
        let buffer = block.render(width, self.composer_background);
        self.surface.write_buffer(&buffer)?;
        self.surface.next_row()?;
        self.history_boundary = self.history_boundary.after_block();
        self.stream.clear_block_id(block.id());
        Ok(())
    }

    fn viewport_frame(&self, width: u16, height: u16) -> viewport::ViewportFrame {
        let elapsed = format_elapsed(self.status.elapsed_seconds());
        let status_header = sanitize_single_line(&self.status.header);
        let queued = queued_status(self.status.queued_messages);
        let model = sanitize_single_line(&self.prompt.model);
        let protocol = sanitize_single_line(&self.prompt.protocol);
        viewport::render(ViewportInput {
            terminal_width: width,
            terminal_height: height,
            history_boundary: self.history_boundary,
            live_blocks: &self.live_blocks,
            busy: self.status.busy,
            active_lines: self.stream.active_lines(),
            status_header: &status_header,
            status_dots: status_dots(self.status.frame),
            elapsed: &elapsed,
            queued: &queued,
            prompt: &self.prompt.text,
            prompt_cursor_column: self.prompt.cursor_column,
            composer_background: self.composer_background,
            command_menu: &self.menus.commands,
            command_menu_selected: self.menus.command_selected,
            session_menu: &self.menus.sessions,
            session_menu_selected: self.menus.session_selected,
            model: &model,
            protocol: &protocol,
            working_dir: &self.prompt.working_dir,
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

    #[test]
    fn truncates_command_descriptions_to_the_menu_width() {
        assert_eq!(viewport::fit_menu_text("abcdef", 4), "abc…");
        assert_eq!(
            UnicodeWidthStr::width(viewport::fit_menu_text("中文说明", 5).as_str()),
            5
        );
    }
}
