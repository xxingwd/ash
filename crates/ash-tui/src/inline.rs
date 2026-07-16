use std::{
    collections::HashMap,
    io::{self, Stdout, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use ash_core::{Content, ContentBlock, Message, MessageContent, SessionSummary, ToolCallId};
use crossterm::{
    cursor::{MoveTo, MoveToColumn, MoveToNextLine, MoveToPreviousLine, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::{
        Attribute, Color as CrosstermColor, ResetColor, SetAttribute, SetBackgroundColor,
        SetForegroundColor,
    },
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use ratatui::{
    buffer::{Buffer, Cell},
    style::{Color as RatatuiColor, Modifier},
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::{
    block_layout::StackBoundary,
    history_block::HistoryBlock,
    input::InputState,
    live_block::LiveBlock,
    markdown::{render_markdown, RenderedLine, TextStyle},
    scrollback::{sanitize_single_line, sanitize_terminal_text},
    slash_command::CommandCompletion,
    tool_display::tool_activity_summary,
    viewport::{self, ViewportInput},
};

const REASONING_VIEW_ROWS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamKind {
    Assistant,
    Reasoning,
}

#[derive(Debug)]
struct PromptSnapshot {
    protocol: String,
    model: String,
    working_dir: PathBuf,
    text: String,
    cursor_column: u16,
}

impl Default for PromptSnapshot {
    fn default() -> Self {
        Self {
            protocol: String::new(),
            model: String::new(),
            working_dir: std::env::current_dir().unwrap_or_default(),
            text: String::new(),
            cursor_column: 0,
        }
    }
}

pub(crate) struct InlineTerminal {
    stdout: Stdout,
    composer_background: Option<crate::palette::Rgb>,
    viewport_visible: bool,
    viewport_rows: u16,
    viewport_cursor_row: u16,
    viewport_cursor_column: u16,
    viewport_line_widths: Vec<u16>,
    frame_reusable_rows: u16,
    prompt_width: u16,
    rendered_input: String,
    prompt: PromptSnapshot,
    history_boundary: StackBoundary,
    live_blocks: Vec<LiveBlock>,
    next_live_block_id: u64,
    busy: bool,
    status_header: String,
    status_started_at: Option<Instant>,
    status_frame: usize,
    queued_messages: usize,
    command_menu: Vec<CommandCompletion>,
    command_menu_selected: usize,
    session_menu: Vec<SessionSummary>,
    session_menu_selected: usize,
    command_menu_dirty: bool,
    assistant_pending: String,
    assistant_live_id: Option<u64>,
    reasoning_source: String,
    reasoning_started_at: Option<Instant>,
    active_kind: Option<StreamKind>,
    active_start: usize,
    active_lines: Vec<RenderedLine>,
    content_dirty: bool,
    current_turn_id: Option<u64>,
    next_turn_id: u64,
    _guard: TerminalGuard,
}

impl InlineTerminal {
    pub fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let composer_background = crate::palette::composer_background_color();
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste, Show)?;
        Ok(Self {
            stdout,
            composer_background,
            viewport_visible: false,
            viewport_rows: 0,
            viewport_cursor_row: 0,
            viewport_cursor_column: 0,
            viewport_line_widths: Vec::new(),
            frame_reusable_rows: 0,
            prompt_width: 0,
            rendered_input: String::new(),
            prompt: PromptSnapshot::default(),
            history_boundary: StackBoundary::default(),
            live_blocks: Vec::new(),
            next_live_block_id: 1,
            busy: false,
            status_header: String::new(),
            status_started_at: None,
            status_frame: 0,
            queued_messages: 0,
            command_menu: Vec::new(),
            command_menu_selected: 0,
            session_menu: Vec::new(),
            session_menu_selected: 0,
            command_menu_dirty: false,
            assistant_pending: String::new(),
            assistant_live_id: None,
            reasoning_source: String::new(),
            reasoning_started_at: None,
            active_kind: None,
            active_start: 0,
            active_lines: Vec::new(),
            content_dirty: false,
            current_turn_id: None,
            next_turn_id: 1,
            _guard: TerminalGuard,
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
        self.rendered_input.clear();
        self.push_history_block(HistoryBlock::info(message));
        self.redraw()
    }

    pub fn command_error(&mut self, message: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.rendered_input.clear();
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
        self.synchronized(|terminal| {
            terminal.clear_viewport()?;
            if let Some(turn_id) = turn_id {
                terminal
                    .live_blocks
                    .retain(|block| !block.belongs_to_turn(turn_id));
            }
            terminal.reset_turn_state();
            terminal.render_viewport(terminal::size()?.0.max(1))
        })
    }

    pub fn restore_session(&mut self, messages: &[Message], working_dir: &Path) -> io::Result<()> {
        self.begin_fresh_viewport()?;
        self.prompt.working_dir = working_dir.to_path_buf();
        self.enqueue_welcome();
        self.push_restored_messages(messages);
        self.redraw()
    }

    pub fn command_blocked(&mut self, command: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.update_input()?;
        self.status_header = format!("/{command} unavailable while working");
        self.refresh_status()
    }

    pub fn set_command_menu(&mut self, items: &[CommandCompletion], selected: usize) {
        let selected = if items.is_empty() {
            0
        } else {
            selected.min(items.len() - 1)
        };
        if self.command_menu != items || self.command_menu_selected != selected {
            self.command_menu.clear();
            self.command_menu.extend_from_slice(items);
            self.command_menu_selected = selected;
            self.command_menu_dirty = true;
        }
    }

    pub fn set_session_menu(&mut self, items: &[SessionSummary], selected: usize) {
        let selected = if items.is_empty() {
            0
        } else {
            selected.min(items.len() - 1)
        };
        if self.session_menu != items || self.session_menu_selected != selected {
            self.session_menu.clear();
            self.session_menu.extend_from_slice(items);
            self.session_menu_selected = selected;
            self.command_menu_dirty = true;
        }
    }

    pub fn prompt(
        &mut self,
        input: &InputState,
        protocol: &str,
        model: &str,
        working_dir: &Path,
    ) -> io::Result<()> {
        let terminal_width = terminal::size()?.0.max(1);
        let view = input.view(terminal_width.saturating_sub(3));
        self.prompt.protocol = protocol.to_string();
        self.prompt.model = model.to_string();
        self.prompt.working_dir = working_dir.to_path_buf();
        self.prompt.text = view.text;
        self.prompt.cursor_column = view.cursor_column;

        if self.viewport_visible && self.prompt_width == terminal_width && !self.command_menu_dirty
        {
            return self.update_input();
        }
        self.command_menu_dirty = false;
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
        self.rendered_input.clear();
        self.reset_streams();
        self.command_menu.clear();
        self.command_menu_selected = 0;
        self.session_menu.clear();
        self.session_menu_selected = 0;
        self.command_menu_dirty = false;
        self.busy = start_working;
        if start_working {
            self.status_header = "Working".to_string();
            self.status_started_at = Some(Instant::now());
            self.status_frame = 0;
        } else {
            self.status_header.clear();
            self.status_started_at = None;
            self.status_frame = 0;
        }
        self.push_history_block(HistoryBlock::user(input));
        self.redraw()
    }

    pub fn agent_started(&mut self) -> io::Result<()> {
        if !self.busy {
            self.busy = true;
            self.status_header = "Working".to_string();
            self.status_started_at = Some(Instant::now());
            self.status_frame = 0;
            self.redraw()?;
        }
        Ok(())
    }

    pub fn text(&mut self, text: &str) -> io::Result<()> {
        self.switch_stream(StreamKind::Assistant)?;
        self.status_header = "Working".to_string();
        self.assistant_pending
            .push_str(&sanitize_terminal_text(text));
        self.content_dirty = true;
        Ok(())
    }

    pub fn thinking(&mut self, text: &str) -> io::Result<()> {
        let width = self.markdown_width()?;
        self.switch_stream(StreamKind::Reasoning)?;
        if self.reasoning_started_at.is_none() {
            self.reasoning_started_at = Some(Instant::now());
        }
        self.reasoning_source
            .push_str(&sanitize_terminal_text(text));
        self.status_header = "Thinking".to_string();
        self.rebuild_reasoning_view(width);
        self.content_dirty = true;
        Ok(())
    }

    pub fn tool_start(&mut self, name: &str, arguments: &Value) -> io::Result<()> {
        self.finalize_current_stream()?;
        self.content_dirty = false;
        self.status_header = tool_activity_summary(name, arguments, self.markdown_width()?);
        self.redraw()
    }

    pub fn tool_end(&mut self, name: &str, arguments: &Value, is_error: bool) -> io::Result<()> {
        self.status_header = "Working".to_string();
        self.push_tool_block(name.to_string(), arguments.clone(), is_error);
        self.redraw()
    }

    pub fn error(&mut self, error: &str) -> io::Result<()> {
        self.finalize_current_stream()?;
        self.content_dirty = false;
        self.status_header = "Failed".to_string();
        self.push_history_block(HistoryBlock::error(error));
        self.redraw()
    }

    pub fn finish_response(&mut self) -> io::Result<()> {
        self.finalize_current_stream()?;
        let elapsed_seconds = self
            .status_started_at
            .map_or(0, |started| started.elapsed().as_secs());
        self.content_dirty = false;
        self.busy = false;
        self.status_header.clear();
        self.status_started_at = None;
        self.status_frame = 0;
        self.push_history_block(HistoryBlock::worked(format_elapsed(elapsed_seconds)));
        let result = self.redraw();
        self.current_turn_id = None;
        result
    }

    pub fn refresh_content(&mut self) -> io::Result<()> {
        if !self.content_dirty {
            return Ok(());
        }
        self.commit_pending_assistant();
        self.content_dirty = false;
        self.redraw()
    }

    pub fn set_queued_messages(&mut self, queued_messages: usize) {
        self.queued_messages = queued_messages;
    }

    pub fn handle_resize(&mut self, width: u16, height: u16) {
        if !self.viewport_visible {
            return;
        }

        let (viewport_rows, viewport_cursor_row) = resize_reflow_geometry(
            &self.viewport_line_widths,
            self.viewport_cursor_row,
            self.viewport_cursor_column,
            width.max(1),
            height.max(1),
        );
        self.viewport_rows = viewport_rows;
        self.viewport_cursor_row = viewport_cursor_row;
        self.prompt_width = 0;
        if self.active_kind == Some(StreamKind::Reasoning) {
            self.rebuild_reasoning_view(width.saturating_sub(2).max(1));
        }
    }

    pub fn refresh_status(&mut self) -> io::Result<()> {
        if !self.busy {
            return Ok(());
        }
        self.status_frame = self.status_frame.wrapping_add(1);
        if self.active_kind == Some(StreamKind::Reasoning) {
            let width = terminal::size()?.0.saturating_sub(2).max(1);
            self.rebuild_reasoning_view(width);
        }
        self.redraw()
    }

    pub fn leave_line(&mut self) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal.clear_viewport()?;
            terminal.finalize_current_stream()?;
            terminal.flush_all_live_blocks()?;
            queue!(
                terminal.stdout,
                MoveToColumn(0),
                SetAttribute(Attribute::Reset),
                ResetColor,
                Clear(ClearType::CurrentLine)
            )?;
            Ok(())
        })?;
        self.viewport_visible = false;
        Ok(())
    }

    fn switch_stream(&mut self, kind: StreamKind) -> io::Result<()> {
        if self.active_kind == Some(kind) {
            return Ok(());
        }
        self.finalize_current_stream()?;
        self.active_kind = Some(kind);
        Ok(())
    }

    fn finalize_current_stream(&mut self) -> io::Result<()> {
        self.commit_pending_assistant();
        if self.active_kind == Some(StreamKind::Reasoning) {
            self.commit_collapsed_reasoning();
        }
        self.active_kind = None;
        self.active_lines.clear();
        self.assistant_live_id = None;
        Ok(())
    }

    fn commit_pending_assistant(&mut self) {
        if self.assistant_pending.is_empty() {
            return;
        }
        let source = std::mem::take(&mut self.assistant_pending);
        if let Some(id) = self.assistant_live_id {
            if let Some(block) = self.live_blocks.iter_mut().find(|block| block.id() == id) {
                let _ = block.append_markdown_source(source);
                return;
            }
        }
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::markdown(id, source, false));
        self.assistant_live_id = Some(id);
    }

    fn rebuild_reasoning_view(&mut self, width: u16) {
        let elapsed = self
            .reasoning_started_at
            .map_or(0, |started| started.elapsed().as_secs());
        self.active_kind = Some(StreamKind::Reasoning);
        self.active_start = 0;
        self.active_lines = render_reasoning_view(&self.reasoning_source, elapsed, width);
    }

    fn commit_collapsed_reasoning(&mut self) {
        let elapsed = self
            .reasoning_started_at
            .map_or(0, |started| started.elapsed().as_secs());
        let had_reasoning = !self.reasoning_source.trim().is_empty();
        self.reasoning_source.clear();
        self.reasoning_started_at = None;
        self.active_lines.clear();
        if !had_reasoning {
            return;
        }
        self.push_markdown_block(format!("Thought for {}", format_elapsed(elapsed)), true);
    }

    fn reset_streams(&mut self) {
        self.assistant_pending.clear();
        self.assistant_live_id = None;
        self.reasoning_source.clear();
        self.reasoning_started_at = None;
        self.active_kind = None;
        self.active_start = 0;
        self.active_lines.clear();
        self.content_dirty = false;
    }

    fn reset_inline_state(&mut self) {
        self.viewport_visible = false;
        self.viewport_rows = 0;
        self.viewport_cursor_row = 0;
        self.viewport_cursor_column = 0;
        self.viewport_line_widths.clear();
        self.frame_reusable_rows = 0;
        self.prompt_width = 0;
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.rendered_input.clear();
        self.busy = false;
        self.status_header.clear();
        self.status_started_at = None;
        self.status_frame = 0;
        self.queued_messages = 0;
        self.command_menu.clear();
        self.command_menu_selected = 0;
        self.session_menu.clear();
        self.session_menu_selected = 0;
        self.command_menu_dirty = false;
        self.reset_turn_state();
        self.reset_streams();
    }

    fn reset_turn_state(&mut self) {
        self.busy = false;
        self.status_header.clear();
        self.status_started_at = None;
        self.status_frame = 0;
        self.queued_messages = 0;
        self.current_turn_id = None;
        self.reset_streams();
    }

    fn markdown_width(&self) -> io::Result<u16> {
        Ok(terminal::size()?.0.saturating_sub(2).max(1))
    }

    fn redraw(&mut self) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal.clear_viewport()?;
            terminal.flush_live_overflow()?;
            terminal.render_viewport(terminal::size()?.0.max(1))
        })
    }

    fn synchronized(
        &mut self,
        operation: impl FnOnce(&mut Self) -> io::Result<()>,
    ) -> io::Result<()> {
        queue!(self.stdout, BeginSynchronizedUpdate)?;
        let operation_result = operation(self);
        let finish_result =
            queue!(self.stdout, EndSynchronizedUpdate).and_then(|()| self.stdout.flush());
        operation_result.and(finish_result)
    }

    fn clear_viewport(&mut self) -> io::Result<()> {
        if !self.viewport_visible {
            self.frame_reusable_rows = 0;
            return Ok(());
        }

        queue!(self.stdout, MoveToPreviousLine(self.viewport_cursor_row))?;
        for row in 0..self.viewport_rows {
            queue!(
                self.stdout,
                MoveToColumn(0),
                SetAttribute(Attribute::Reset),
                ResetColor,
                Clear(ClearType::CurrentLine)
            )?;
            if row + 1 < self.viewport_rows {
                queue!(self.stdout, MoveToNextLine(1))?;
            }
        }
        if self.viewport_rows > 1 {
            queue!(self.stdout, MoveToPreviousLine(self.viewport_rows - 1))?;
        }
        self.frame_reusable_rows = self.viewport_rows;
        Ok(())
    }

    fn push_viewport_to_scrollback(&mut self) -> io::Result<()> {
        if !self.viewport_visible {
            return Ok(());
        }
        self.finalize_current_stream()?;
        let (width, height) = terminal::size()?;
        let height = height.max(1);
        let frame = self.viewport_frame(width.max(1), height);
        let history_rows = frame.history_rows.min(height);
        queue!(
            self.stdout,
            SetAttribute(Attribute::Reset),
            ResetColor,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        self.frame_reusable_rows = 0;
        if history_rows > 0 {
            self.write_viewport_buffer_rows(&frame.buffer, history_rows)?;
            queue!(
                self.stdout,
                SetAttribute(Attribute::Reset),
                ResetColor,
                MoveTo(0, height.saturating_sub(1))
            )?;
            for _ in 0..history_rows {
                writeln!(self.stdout)?;
            }
        }
        queue!(self.stdout, Clear(ClearType::All), MoveTo(0, 0))
    }

    fn next_frame_row(&mut self) -> io::Result<()> {
        if self.frame_reusable_rows > 1 {
            self.frame_reusable_rows -= 1;
            queue!(self.stdout, MoveToNextLine(1))?;
        } else {
            self.frame_reusable_rows = 0;
            write!(self.stdout, "\r\n")?;
        }
        Ok(())
    }

    fn render_viewport(&mut self, width: u16) -> io::Result<()> {
        let terminal_height = terminal::size()?.1;
        let frame = self.viewport_frame(width, terminal_height);
        let desired_rows = frame.total_rows;
        let line_widths = viewport_line_widths(&frame.buffer);
        self.write_viewport_buffer(&frame.buffer)?;
        self.viewport_rows = desired_rows;
        self.viewport_cursor_row = frame.cursor_row;
        self.viewport_cursor_column = frame.cursor_column;
        self.viewport_line_widths = line_widths;
        self.viewport_visible = true;
        self.prompt_width = width;
        self.rendered_input.clone_from(&self.prompt.text);
        self.frame_reusable_rows = 0;

        queue!(
            self.stdout,
            ResetColor,
            MoveToPreviousLine(
                self.viewport_rows
                    .saturating_sub(1)
                    .saturating_sub(self.viewport_cursor_row)
            ),
            MoveToColumn(frame.cursor_column)
        )
    }

    fn write_viewport_buffer(&mut self, buffer: &Buffer) -> io::Result<()> {
        self.write_viewport_buffer_rows(buffer, buffer.area.height)
    }

    fn write_viewport_buffer_rows(&mut self, buffer: &Buffer, height: u16) -> io::Result<()> {
        let height = height.min(buffer.area.height);
        let width = buffer.area.width;
        for y in 0..height {
            let background = uniform_row_background(buffer, y);
            queue!(
                self.stdout,
                MoveToColumn(0),
                SetAttribute(Attribute::Reset),
                ResetColor,
                Clear(ClearType::CurrentLine)
            )?;
            if background != RatatuiColor::Reset {
                queue!(
                    self.stdout,
                    SetBackgroundColor(crossterm_color(background)),
                    Clear(ClearType::CurrentLine),
                    ResetColor
                )?;
            }
            let mut current_style = None;
            let mut current_column = 0;
            for x in 0..width {
                let Some(cell) = buffer.cell((
                    buffer.area.x.saturating_add(x),
                    buffer.area.y.saturating_add(y),
                )) else {
                    continue;
                };
                if !cell_needs_write(cell, background) {
                    continue;
                }
                if current_column != x {
                    queue!(self.stdout, MoveToColumn(x))?;
                }
                let style = (cell.fg, cell.bg, cell.modifier);
                if current_style != Some(style) {
                    self.write_ratatui_style(cell.fg, cell.bg, cell.modifier)?;
                    current_style = Some(style);
                }
                write!(self.stdout, "{}", cell.symbol())?;
                current_column = x.saturating_add(cell_display_width(cell));
            }
            queue!(self.stdout, SetAttribute(Attribute::Reset), ResetColor)?;
            if y + 1 < height {
                self.next_frame_row()?;
            }
        }
        Ok(())
    }

    fn write_ratatui_style(
        &mut self,
        foreground: RatatuiColor,
        background: RatatuiColor,
        modifiers: Modifier,
    ) -> io::Result<()> {
        queue!(self.stdout, SetAttribute(Attribute::Reset), ResetColor)?;
        if foreground != RatatuiColor::Reset {
            queue!(self.stdout, SetForegroundColor(crossterm_color(foreground)))?;
        }
        if background != RatatuiColor::Reset {
            queue!(self.stdout, SetBackgroundColor(crossterm_color(background)))?;
        }
        for (modifier, attribute) in [
            (Modifier::BOLD, Attribute::Bold),
            (Modifier::DIM, Attribute::Dim),
            (Modifier::ITALIC, Attribute::Italic),
            (Modifier::UNDERLINED, Attribute::Underlined),
            (Modifier::REVERSED, Attribute::Reverse),
            (Modifier::HIDDEN, Attribute::Hidden),
            (Modifier::CROSSED_OUT, Attribute::CrossedOut),
            (Modifier::SLOW_BLINK, Attribute::SlowBlink),
            (Modifier::RAPID_BLINK, Attribute::RapidBlink),
        ] {
            if modifiers.contains(modifier) {
                queue!(self.stdout, SetAttribute(attribute))?;
            }
        }
        Ok(())
    }

    fn update_input(&mut self) -> io::Result<()> {
        if self.rendered_input != self.prompt.text {
            queue!(
                self.stdout,
                MoveToColumn(0),
                SetAttribute(Attribute::Reset),
                ResetColor,
                Clear(ClearType::CurrentLine)
            )?;
            let background = self
                .composer_background
                .map_or(RatatuiColor::Reset, |(r, g, b)| RatatuiColor::Rgb(r, g, b));
            if background != RatatuiColor::Reset {
                queue!(
                    self.stdout,
                    SetBackgroundColor(crossterm_color(background)),
                    Clear(ClearType::CurrentLine),
                    ResetColor
                )?;
            }
            self.write_ratatui_style(RatatuiColor::Reset, background, Modifier::BOLD)?;
            write!(self.stdout, "›")?;
            if !self.prompt.text.is_empty() {
                queue!(self.stdout, MoveToColumn(2))?;
                self.write_ratatui_style(RatatuiColor::Reset, background, Modifier::empty())?;
                write!(self.stdout, "{}", self.prompt.text)?;
            }
            queue!(self.stdout, SetAttribute(Attribute::Reset), ResetColor)?;
            self.rendered_input.clone_from(&self.prompt.text);
            if let Some(line_width) = self
                .viewport_line_widths
                .get_mut(usize::from(self.viewport_cursor_row))
            {
                let prompt_width = u16::try_from(UnicodeWidthStr::width(self.prompt.text.as_str()))
                    .unwrap_or(u16::MAX);
                *line_width = if prompt_width == 0 {
                    1
                } else {
                    2_u16.saturating_add(prompt_width)
                };
            }
        }
        queue!(self.stdout, MoveToColumn(2 + self.prompt.cursor_column))?;
        self.stdout.flush()
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

    fn push_tool_block(&mut self, name: String, arguments: Value, is_error: bool) {
        let id = self.allocate_live_id();
        self.push_live(LiveBlock::tool(id, name, arguments, is_error));
    }

    fn push_restored_messages(&mut self, messages: &[Message]) {
        let tool_results = messages
            .iter()
            .filter_map(|message| match &message.content {
                MessageContent::ToolResult { id, result } => Some((id.clone(), result.is_err())),
                MessageContent::User(_) | MessageContent::Assistant(_) => None,
            })
            .collect::<HashMap<ToolCallId, bool>>();

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
                            } => self.push_tool_block(
                                name.clone(),
                                arguments.clone(),
                                tool_results.get(id).copied().unwrap_or(false),
                            ),
                            ContentBlock::Text(_) => {}
                        }
                    }
                }
                MessageContent::ToolResult { .. } => {}
            }
        }
    }

    fn flush_live_overflow(&mut self) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        while !self.live_blocks.is_empty()
            && self.viewport_frame(width.max(1), height.max(1)).total_rows > height.max(1)
        {
            let block = self.live_blocks.remove(0);
            self.flush_live_block(block, width.max(1))?;
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
        let width = terminal_width.saturating_sub(1).max(1);
        if self.history_boundary != StackBoundary::default()
            && block.flow() == crate::block_layout::StackFlow::Block
        {
            queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            self.next_frame_row()?;
        }
        let buffer = block.render(width, self.composer_background);
        self.write_viewport_buffer(&buffer)?;
        self.next_frame_row()?;
        self.history_boundary = self.history_boundary.after_block();
        if self.assistant_live_id == Some(block.id()) {
            self.assistant_live_id = None;
        }
        Ok(())
    }

    fn viewport_frame(&self, width: u16, height: u16) -> viewport::ViewportFrame {
        let elapsed = format_elapsed(
            self.status_started_at
                .map_or(0, |started| started.elapsed().as_secs()),
        );
        let status_header = sanitize_single_line(&self.status_header);
        let queued = queued_status(self.queued_messages);
        let model = sanitize_single_line(&self.prompt.model);
        let protocol = sanitize_single_line(&self.prompt.protocol);
        viewport::render(ViewportInput {
            terminal_width: width,
            terminal_height: height,
            history_boundary: self.history_boundary,
            live_blocks: &self.live_blocks,
            busy: self.busy,
            active_start: self.active_start,
            active_lines: &self.active_lines,
            status_header: &status_header,
            status_dots: status_dots(self.status_frame),
            elapsed: &elapsed,
            queued: &queued,
            prompt: &self.prompt.text,
            prompt_cursor_column: self.prompt.cursor_column,
            composer_background: self.composer_background,
            command_menu: &self.command_menu,
            command_menu_selected: self.command_menu_selected,
            session_menu: &self.session_menu,
            session_menu_selected: self.session_menu_selected,
            model: &model,
            protocol: &protocol,
            working_dir: &self.prompt.working_dir,
        })
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn format_elapsed(elapsed_seconds: u64) -> String {
    if elapsed_seconds < 60 {
        return format!("{elapsed_seconds}s");
    }
    if elapsed_seconds < 3600 {
        return format!("{}m {:02}s", elapsed_seconds / 60, elapsed_seconds % 60);
    }
    format!(
        "{}h {:02}m {:02}s",
        elapsed_seconds / 3600,
        (elapsed_seconds % 3600) / 60,
        elapsed_seconds % 60
    )
}

fn viewport_line_widths(buffer: &Buffer) -> Vec<u16> {
    (0..buffer.area.height)
        .map(|y| {
            let background = uniform_row_background(buffer, y);
            (0..buffer.area.width).fold(0, |line_width, x| {
                let Some(cell) = buffer.cell((
                    buffer.area.x.saturating_add(x),
                    buffer.area.y.saturating_add(y),
                )) else {
                    return line_width;
                };
                if cell_needs_write(cell, background) {
                    line_width.max(x.saturating_add(cell_display_width(cell)))
                } else {
                    line_width
                }
            })
        })
        .collect()
}

fn uniform_row_background(buffer: &Buffer, row: u16) -> RatatuiColor {
    let mut background = None;
    for x in 0..buffer.area.width {
        let Some(cell) = buffer.cell((
            buffer.area.x.saturating_add(x),
            buffer.area.y.saturating_add(row),
        )) else {
            continue;
        };
        match background {
            None => background = Some(cell.bg),
            Some(current) if current == cell.bg => {}
            Some(_) => return RatatuiColor::Reset,
        }
    }
    background
        .filter(|background| *background != RatatuiColor::Reset)
        .unwrap_or(RatatuiColor::Reset)
}

fn cell_needs_write(cell: &Cell, row_background: RatatuiColor) -> bool {
    !cell.skip
        && (cell.symbol() != " "
            || cell.fg != RatatuiColor::Reset
            || !cell.modifier.is_empty()
            || cell.bg != row_background)
}

fn cell_display_width(cell: &Cell) -> u16 {
    u16::try_from(UnicodeWidthStr::width(cell.symbol()))
        .unwrap_or(u16::MAX)
        .max(1)
}

fn resize_reflow_geometry(
    line_widths: &[u16],
    cursor_row: u16,
    cursor_column: u16,
    terminal_width: u16,
    terminal_height: u16,
) -> (u16, u16) {
    let terminal_width = terminal_width.max(1);
    let terminal_height = terminal_height.max(1);
    if line_widths.is_empty() {
        return (1, 0);
    }

    let visual_rows = line_widths
        .iter()
        .map(|line_width| visual_row_count(*line_width, terminal_width))
        .collect::<Vec<_>>();
    let total_rows = visual_rows
        .iter()
        .copied()
        .fold(0u16, u16::saturating_add)
        .max(1);
    let cursor_row = usize::from(cursor_row).min(visual_rows.len() - 1);
    let rows_before_cursor = visual_rows[..cursor_row]
        .iter()
        .copied()
        .fold(0u16, u16::saturating_add);
    let cursor_line_offset = cursor_column
        .saturating_div(terminal_width)
        .min(visual_rows[cursor_row].saturating_sub(1));
    let cursor_visual_row = rows_before_cursor.saturating_add(cursor_line_offset);
    let hidden_rows = total_rows.saturating_sub(terminal_height);
    let visible_rows = total_rows.min(terminal_height);
    let visible_cursor_row = cursor_visual_row
        .saturating_sub(hidden_rows)
        .min(visible_rows.saturating_sub(1));

    (visible_rows, visible_cursor_row)
}

fn visual_row_count(line_width: u16, terminal_width: u16) -> u16 {
    if line_width == 0 {
        1
    } else {
        line_width
            .saturating_sub(1)
            .saturating_div(terminal_width.max(1))
            .saturating_add(1)
    }
}

fn crossterm_color(color: RatatuiColor) -> CrosstermColor {
    match color {
        RatatuiColor::Reset => CrosstermColor::Reset,
        RatatuiColor::Black => CrosstermColor::Black,
        RatatuiColor::Red => CrosstermColor::DarkRed,
        RatatuiColor::Green => CrosstermColor::DarkGreen,
        RatatuiColor::Yellow => CrosstermColor::DarkYellow,
        RatatuiColor::Blue => CrosstermColor::DarkBlue,
        RatatuiColor::Magenta => CrosstermColor::DarkMagenta,
        RatatuiColor::Cyan => CrosstermColor::DarkCyan,
        RatatuiColor::Gray => CrosstermColor::Grey,
        RatatuiColor::DarkGray => CrosstermColor::DarkGrey,
        RatatuiColor::LightRed => CrosstermColor::Red,
        RatatuiColor::LightGreen => CrosstermColor::Green,
        RatatuiColor::LightYellow => CrosstermColor::Yellow,
        RatatuiColor::LightBlue => CrosstermColor::Blue,
        RatatuiColor::LightMagenta => CrosstermColor::Magenta,
        RatatuiColor::LightCyan => CrosstermColor::Cyan,
        RatatuiColor::White => CrosstermColor::White,
        RatatuiColor::Rgb(r, g, b) => CrosstermColor::Rgb { r, g, b },
        RatatuiColor::Indexed(value) => CrosstermColor::AnsiValue(value),
    }
}

fn render_reasoning_view(source: &str, elapsed_seconds: u64, width: u16) -> Vec<RenderedLine> {
    let mut header = render_markdown(
        &format!("Thinking ({})", format_elapsed(elapsed_seconds)),
        width,
    );
    for line in &mut header {
        line.patch_style(TextStyle::dim_italic());
    }
    header.truncate(REASONING_VIEW_ROWS);
    let remaining_rows = REASONING_VIEW_ROWS.saturating_sub(header.len());
    if remaining_rows == 0 {
        return header;
    }

    let mut body = render_markdown(source, width);
    while body.first().is_some_and(RenderedLine::is_blank) {
        body.remove(0);
    }
    while body.last().is_some_and(RenderedLine::is_blank) {
        body.pop();
    }
    for line in &mut body {
        line.patch_style(TextStyle::dim_italic());
    }
    let keep_from = body.len().saturating_sub(remaining_rows);
    header.extend(body.into_iter().skip(keep_from));
    header
}

fn status_dots(frame: usize) -> &'static str {
    const FRAMES: [&str; 4] = [".  ", ".. ", "...", ".. "];
    FRAMES[frame % FRAMES.len()]
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
    fn formats_elapsed_status_like_codex() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(61), "1m 01s");
        assert_eq!(format_elapsed(3661), "1h 01m 01s");
    }

    #[test]
    fn viewport_width_ignores_trailing_default_cells() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 12, 1));
        buffer.set_string(0, 0, "ash", ratatui::style::Style::default());

        assert_eq!(viewport_line_widths(&buffer), vec![3]);
    }

    #[test]
    fn viewport_width_does_not_treat_uniform_background_as_text() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 12, 1));
        for x in 0..buffer.area.width {
            buffer
                .cell_mut((x, 0))
                .expect("cell")
                .set_bg(RatatuiColor::Blue);
        }
        assert_eq!(viewport_line_widths(&buffer), vec![0]);

        buffer.cell_mut((5, 0)).expect("cell").set_symbol("x");
        assert_eq!(viewport_line_widths(&buffer), vec![6]);
    }

    #[test]
    fn resize_reflow_counts_wrapped_visual_rows() {
        assert_eq!(visual_row_count(60, 48), 2);
        assert_eq!(resize_reflow_geometry(&[1, 60, 1], 2, 0, 48, 24), (4, 3));
    }

    #[test]
    fn resize_reflow_accounts_for_rows_scrolled_above_the_screen() {
        assert_eq!(resize_reflow_geometry(&[1, 60, 1, 1], 2, 0, 20, 4), (4, 2));
    }

    #[test]
    fn reasoning_view_keeps_a_timed_header_and_the_latest_three_lines() {
        let lines = render_reasoning_view("one\ntwo\nthree\nfour\nfive", 3, 80);
        let text = lines
            .iter()
            .map(RenderedLine::plain_text)
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), REASONING_VIEW_ROWS);
        assert_eq!(text[0], "Thinking (3s)");
        assert_eq!(&text[1..], ["three", "four", "five"]);
    }

    #[test]
    fn reasoning_view_uses_the_completed_thought_style() {
        let lines = render_reasoning_view("detail", 3, 80);
        let mut header = Vec::new();
        let mut body = Vec::new();
        lines[0].write_ansi(&mut header).unwrap();
        lines[1].write_ansi(&mut body).unwrap();
        let header = String::from_utf8(header).unwrap();
        let body = String::from_utf8(body).unwrap();

        for rendered in [header, body] {
            assert!(rendered.contains("\x1b[2m"));
            assert!(rendered.contains("\x1b[3m"));
            assert!(!rendered.contains("\x1b[1m"));
        }
    }

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
