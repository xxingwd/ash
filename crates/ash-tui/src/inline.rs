use std::{
    collections::{HashMap, VecDeque},
    io::{self, Stdout, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use ash_core::{Content, ContentBlock, Message, MessageContent, ToolCallId};
use crossterm::{
    cursor::{MoveTo, MoveToColumn, MoveToNextLine, MoveToPreviousLine, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    input::InputState,
    markdown::{render_markdown, RenderedLine, TextStyle},
    markdown_stream::MarkdownStream,
    scrollback::{sanitize_single_line, sanitize_terminal_text, wrap_text},
    slash_command::CommandCompletion,
    status_line::{compact_path, fit_status_left},
    theme,
    tool_display::{tool_activity_summary, tool_call_summary},
    welcome_card::{welcome_card, WelcomeStyle},
};

const STATUS_ROWS: u16 = 1;
const TRANSCRIPT_STATUS_SPACING: u16 = 1;
const STATUS_COMPOSER_SPACING: u16 = 1;
const COMPOSER_ROWS: u16 = 3;
const FOOTER_ROWS: u16 = 1;
const STREAM_CATCH_UP_DEPTH: usize = 8;
const STREAM_CATCH_UP_AGE: Duration = Duration::from_millis(120);
const REASONING_VIEW_ROWS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamKind {
    Assistant,
    Reasoning,
}

#[derive(Debug)]
struct MarkdownBatch {
    kind: StreamKind,
    start: usize,
    lines: Vec<RenderedLine>,
}

struct ActiveViewport {
    show_spacing: bool,
    start: usize,
    kind: StreamKind,
    lines: Vec<RenderedLine>,
}

impl ActiveViewport {
    fn rows(&self) -> u16 {
        u16::try_from(self.lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(u16::from(self.show_spacing))
    }
}

impl MarkdownBatch {
    fn new(kind: StreamKind, start: usize, mut lines: Vec<RenderedLine>) -> Self {
        if kind == StreamKind::Reasoning {
            for line in &mut lines {
                line.patch_style(TextStyle::dim_italic());
            }
        }
        Self { kind, start, lines }
    }
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
    input_background: String,
    viewport_visible: bool,
    viewport_rows: u16,
    viewport_cursor_row: u16,
    frame_reusable_rows: u16,
    prompt_width: u16,
    rendered_input: String,
    prompt: PromptSnapshot,
    has_emitted_history_cells: bool,
    history_ends_with_spacing: bool,
    busy: bool,
    status_header: String,
    status_started_at: Option<Instant>,
    status_frame: usize,
    queued_messages: usize,
    command_menu: Vec<CommandCompletion>,
    command_menu_selected: usize,
    command_menu_dirty: bool,
    assistant_stream: MarkdownStream,
    reasoning_source: String,
    reasoning_started_at: Option<Instant>,
    active_kind: Option<StreamKind>,
    active_start: usize,
    active_lines: Vec<RenderedLine>,
    pending_markdown_batches: VecDeque<MarkdownBatch>,
    pending_markdown_started_at: Option<Instant>,
    content_dirty: bool,
    turn_active: bool,
    turn_history_rows: u16,
    counting_history_rows: bool,
    session_start_width: u16,
    session_history_rows: u16,
    session_overflowed: bool,
    _guard: TerminalGuard,
}

impl InlineTerminal {
    pub fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let input_background = crate::palette::composer_background_escape();
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste, Show)?;
        Ok(Self {
            stdout,
            input_background,
            viewport_visible: false,
            viewport_rows: 0,
            viewport_cursor_row: 0,
            frame_reusable_rows: 0,
            prompt_width: 0,
            rendered_input: String::new(),
            prompt: PromptSnapshot::default(),
            has_emitted_history_cells: false,
            history_ends_with_spacing: false,
            busy: false,
            status_header: String::new(),
            status_started_at: None,
            status_frame: 0,
            queued_messages: 0,
            command_menu: Vec::new(),
            command_menu_selected: 0,
            command_menu_dirty: false,
            assistant_stream: MarkdownStream::default(),
            reasoning_source: String::new(),
            reasoning_started_at: None,
            active_kind: None,
            active_start: 0,
            active_lines: Vec::new(),
            pending_markdown_batches: VecDeque::new(),
            pending_markdown_started_at: None,
            content_dirty: false,
            turn_active: false,
            turn_history_rows: 0,
            counting_history_rows: false,
            session_start_width: 0,
            session_history_rows: 0,
            session_overflowed: false,
            _guard: TerminalGuard,
        })
    }

    pub fn welcome(&mut self) -> io::Result<()> {
        let width = terminal::size()?.0.max(1);
        let lines = welcome_card(width);
        self.session_start_width = width;
        self.session_history_rows = self
            .session_history_rows
            .saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
        for line in lines {
            match line.style {
                WelcomeStyle::Logo => {
                    queue!(
                        self.stdout,
                        SetForegroundColor(Color::Cyan),
                        SetAttribute(Attribute::Bold)
                    )?;
                }
                WelcomeStyle::Title => {
                    queue!(
                        self.stdout,
                        SetForegroundColor(Color::Cyan),
                        SetAttribute(Attribute::Bold)
                    )?;
                }
                WelcomeStyle::Subtitle => {
                    queue!(self.stdout, SetAttribute(Attribute::Dim))?;
                }
                WelcomeStyle::Plain => {}
            }
            write!(self.stdout, "{}", line.text)?;
            queue!(self.stdout, SetAttribute(Attribute::Reset), ResetColor)?;
            write!(self.stdout, "\r\n")?;
        }
        self.has_emitted_history_cells = true;
        self.history_ends_with_spacing = false;
        self.stdout.flush()
    }

    pub fn command_output(&mut self, message: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.rendered_input.clear();
        let message = message.to_string();
        self.replace_viewport(move |terminal| terminal.write_info_history(&message))
    }

    pub fn command_error(&mut self, message: &str) -> io::Result<()> {
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.rendered_input.clear();
        let message = message.to_string();
        self.replace_viewport(move |terminal| terminal.write_error_history(&message))
    }

    pub fn start_new_session(&mut self) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        let clear_locally = can_clear_session_locally(
            self.session_start_width,
            width.max(1),
            self.session_history_rows,
            self.viewport_rows,
            height.max(1),
            self.session_overflowed,
        );
        self.synchronized(|terminal| {
            if clear_locally {
                terminal.clear_owned_region(height.max(1), terminal.session_history_rows)
            } else {
                queue!(
                    terminal.stdout,
                    ResetColor,
                    MoveTo(0, 0),
                    Clear(ClearType::All),
                    MoveTo(0, 0)
                )?;
                Ok(())
            }
        })?;
        self.reset_inline_state();
        self.reset_session_history();
        self.has_emitted_history_cells = false;
        self.history_ends_with_spacing = false;
        self.welcome()
    }

    pub fn rollback_turn(&mut self) -> io::Result<()> {
        self.synchronized(|terminal| terminal.clear_current_turn())?;
        self.session_history_rows = self
            .session_history_rows
            .saturating_sub(self.turn_history_rows);
        self.reset_inline_state();
        Ok(())
    }

    pub fn restore_session(&mut self, messages: &[Message], label: &str) -> io::Result<()> {
        self.reset_session_history();
        self.session_start_width = terminal::size()?.0.max(1);
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
        self.command_menu_dirty = false;
        self.reset_streams();
        let messages = messages.to_vec();
        let label = sanitize_single_line(label);
        self.replace_viewport(move |terminal| {
            terminal.write_info_history(&format!("Resumed session {label}."))?;
            terminal.write_restored_messages(&messages)
        })
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

    pub fn prompt(
        &mut self,
        input: &InputState,
        protocol: &str,
        model: &str,
        working_dir: &Path,
    ) -> io::Result<()> {
        let terminal_width = terminal::size()?.0.max(1);
        let view = input.view(terminal_width.saturating_sub(2));
        self.prompt.protocol = protocol.to_string();
        self.prompt.model = model.to_string();
        self.prompt.working_dir = working_dir.to_path_buf();
        self.prompt.text = view.text;
        self.prompt.cursor_column = view.cursor_column;

        if self.viewport_visible && self.prompt_width == terminal_width && !self.command_menu_dirty
        {
            return self.update_input();
        }
        let batches = self.take_pending_markdown();
        self.content_dirty = false;
        self.command_menu_dirty = false;
        if batches.is_empty() && self.redraw_active_region()? {
            return Ok(());
        }
        self.replace_viewport(move |terminal| terminal.write_markdown_batches(&batches))
    }

    pub fn commit_input(&mut self, input: &str) -> io::Result<()> {
        self.commit_user_message(input, /*start_working*/ true)
    }

    pub fn commit_exit(&mut self, input: &str) -> io::Result<()> {
        self.commit_user_message(input, /*start_working*/ false)
    }

    fn commit_user_message(&mut self, input: &str, start_working: bool) -> io::Result<()> {
        self.turn_active = start_working;
        self.turn_history_rows = 0;
        self.prompt.text.clear();
        self.prompt.cursor_column = 0;
        self.rendered_input.clear();
        self.reset_streams();
        self.command_menu.clear();
        self.command_menu_selected = 0;
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
        let input = input.to_string();
        self.replace_viewport(move |terminal| terminal.write_user_history(&input))
    }

    pub fn agent_started(&mut self) -> io::Result<()> {
        if !self.busy {
            self.busy = true;
            self.status_header = "Working".to_string();
            self.status_started_at = Some(Instant::now());
            self.status_frame = 0;
            self.replace_viewport(|_| Ok(()))?;
        }
        Ok(())
    }

    pub fn text(&mut self, text: &str) -> io::Result<()> {
        let width = self.markdown_width()?;
        let batches = self.switch_stream(StreamKind::Assistant, width);
        self.enqueue_markdown_batches(batches);
        self.status_header = "Working".to_string();
        if let Some(update) = self.assistant_stream.push_delta(text, width) {
            self.enqueue_markdown_batches(vec![MarkdownBatch::new(
                StreamKind::Assistant,
                update.stable_start,
                update.stable,
            )]);
            self.set_active(StreamKind::Assistant, update.tail_start, update.tail);
            self.content_dirty = true;
        }
        Ok(())
    }

    pub fn thinking(&mut self, text: &str) -> io::Result<()> {
        let width = self.markdown_width()?;
        let batches = self.switch_stream(StreamKind::Reasoning, width);
        self.enqueue_markdown_batches(batches);
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
        let width = self.markdown_width()?;
        let batches = self.take_pending_and_finalize(width);
        self.content_dirty = false;
        self.status_header = tool_activity_summary(name, arguments, width);
        self.replace_viewport(move |terminal| terminal.write_markdown_batches(&batches))
    }

    pub fn tool_end(&mut self, name: &str, arguments: &Value, is_error: bool) -> io::Result<()> {
        self.status_header = "Working".to_string();
        let name = name.to_string();
        let arguments = arguments.clone();
        self.replace_viewport(move |terminal| {
            terminal.write_tool_history(&name, &arguments, is_error)
        })
    }

    pub fn error(&mut self, error: &str) -> io::Result<()> {
        let width = self.markdown_width()?;
        let batches = self.take_pending_and_finalize(width);
        self.content_dirty = false;
        let error = error.to_string();
        self.status_header = "Failed".to_string();
        self.replace_viewport(move |terminal| {
            terminal.write_markdown_batches(&batches)?;
            terminal.write_error_history(&error)
        })
    }

    pub fn rollback_requested(&mut self) -> io::Result<()> {
        self.pending_markdown_batches.clear();
        self.pending_markdown_started_at = None;
        self.content_dirty = false;
        self.status_header = "Cancelling".to_string();
        self.refresh_status()
    }

    pub fn finish_response(&mut self) -> io::Result<()> {
        let width = self.markdown_width()?;
        let batches = self.take_pending_and_finalize(width);
        let elapsed_seconds = self
            .status_started_at
            .map_or(0, |started| started.elapsed().as_secs());
        self.content_dirty = false;
        self.busy = false;
        self.status_header.clear();
        self.status_started_at = None;
        self.status_frame = 0;
        let result = self.replace_viewport(move |terminal| {
            terminal.write_markdown_batches(&batches)?;
            terminal.write_worked_history(elapsed_seconds)
        });
        self.turn_active = false;
        self.turn_history_rows = 0;
        result
    }

    pub fn refresh_content(&mut self) -> io::Result<()> {
        if !self.content_dirty && self.pending_markdown_batches.is_empty() {
            return Ok(());
        }
        let drain_count = markdown_drain_count(
            self.pending_markdown_batches.len(),
            self.pending_markdown_started_at,
            Instant::now(),
        );
        let batches = self
            .pending_markdown_batches
            .drain(..drain_count)
            .collect::<Vec<_>>();
        if self.pending_markdown_batches.is_empty() {
            self.pending_markdown_started_at = None;
        }
        self.content_dirty = false;
        if batches.is_empty() && self.redraw_active_region()? {
            return Ok(());
        }
        self.replace_viewport(move |terminal| terminal.write_markdown_batches(&batches))
    }

    pub fn set_queued_messages(&mut self, queued_messages: usize) {
        self.queued_messages = queued_messages;
    }

    pub fn mark_session_layout_uncertain(&mut self) {
        self.session_overflowed = true;
    }

    pub fn refresh_status(&mut self) -> io::Result<()> {
        if !self.busy {
            return Ok(());
        }
        self.status_frame = self.status_frame.wrapping_add(1);
        if !self.viewport_visible {
            return self.replace_viewport(|_| Ok(()));
        }

        let width = terminal::size()?.0.max(1);
        if self.active_kind == Some(StreamKind::Reasoning) {
            self.rebuild_reasoning_view(width.saturating_sub(2).max(1));
            if !self.redraw_active_region()? {
                return self.replace_viewport(|_| Ok(()));
            }
        }
        let cursor_column = 2 + self.prompt.cursor_column;
        self.synchronized(|terminal| {
            queue!(
                terminal.stdout,
                MoveToPreviousLine(STATUS_ROWS + STATUS_COMPOSER_SPACING + 1),
                MoveToColumn(0),
                Clear(ClearType::CurrentLine)
            )?;
            terminal.render_status(width)?;
            queue!(
                terminal.stdout,
                MoveToNextLine(STATUS_ROWS + STATUS_COMPOSER_SPACING + 1),
                MoveToColumn(cursor_column)
            )?;
            Ok(())
        })
    }

    pub fn leave_line(&mut self) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal.clear_viewport()?;
            queue!(
                terminal.stdout,
                MoveToColumn(0),
                Clear(ClearType::CurrentLine)
            )?;
            Ok(())
        })?;
        self.viewport_visible = false;
        Ok(())
    }

    fn switch_stream(&mut self, kind: StreamKind, width: u16) -> Vec<MarkdownBatch> {
        if self.active_kind == Some(kind) {
            return Vec::new();
        }
        let batches = self.finalize_active(width);
        self.active_kind = Some(kind);
        batches
    }

    fn finalize_active(&mut self, width: u16) -> Vec<MarkdownBatch> {
        let Some(kind) = self.active_kind.take() else {
            self.active_lines.clear();
            return Vec::new();
        };
        let update = match kind {
            StreamKind::Assistant => self.assistant_stream.finalize(width),
            StreamKind::Reasoning => return self.collapse_reasoning(width),
        };
        self.active_lines.clear();
        if update.stable.is_empty() {
            Vec::new()
        } else {
            vec![MarkdownBatch::new(kind, update.stable_start, update.stable)]
        }
    }

    fn take_pending_and_finalize(&mut self, width: u16) -> Vec<MarkdownBatch> {
        let mut batches = self.take_pending_markdown();
        batches.extend(self.finalize_active(width));
        batches
    }

    fn enqueue_markdown_batches(&mut self, batches: Vec<MarkdownBatch>) {
        for batch in batches {
            let kind = batch.kind;
            let start = batch.start;
            for (offset, line) in batch.lines.into_iter().enumerate() {
                if self.pending_markdown_batches.is_empty() {
                    self.pending_markdown_started_at = Some(Instant::now());
                }
                self.pending_markdown_batches.push_back(MarkdownBatch {
                    kind,
                    start: start + offset,
                    lines: vec![line],
                });
            }
        }
    }

    fn take_pending_markdown(&mut self) -> Vec<MarkdownBatch> {
        self.pending_markdown_started_at = None;
        self.pending_markdown_batches.drain(..).collect()
    }

    fn set_active(&mut self, kind: StreamKind, start: usize, mut lines: Vec<RenderedLine>) {
        if kind == StreamKind::Reasoning {
            for line in &mut lines {
                line.patch_style(TextStyle::dim_italic());
            }
        }
        self.active_kind = Some(kind);
        self.active_start = start;
        self.active_lines = lines;
    }

    fn rebuild_reasoning_view(&mut self, width: u16) {
        let elapsed = self
            .reasoning_started_at
            .map_or(0, |started| started.elapsed().as_secs());
        self.active_kind = Some(StreamKind::Reasoning);
        self.active_start = 0;
        self.active_lines = render_reasoning_view(&self.reasoning_source, elapsed, width);
    }

    fn collapse_reasoning(&mut self, width: u16) -> Vec<MarkdownBatch> {
        let elapsed = self
            .reasoning_started_at
            .map_or(0, |started| started.elapsed().as_secs());
        let had_reasoning = !self.reasoning_source.trim().is_empty();
        self.reasoning_source.clear();
        self.reasoning_started_at = None;
        self.active_lines.clear();
        if !had_reasoning {
            return Vec::new();
        }
        let lines = render_markdown(&format!("Thought for {}", format_elapsed(elapsed)), width);
        vec![MarkdownBatch::new(StreamKind::Reasoning, 0, lines)]
    }

    fn reset_streams(&mut self) {
        self.assistant_stream.reset();
        self.reasoning_source.clear();
        self.reasoning_started_at = None;
        self.active_kind = None;
        self.active_start = 0;
        self.active_lines.clear();
        self.pending_markdown_batches.clear();
        self.pending_markdown_started_at = None;
        self.content_dirty = false;
    }

    fn reset_inline_state(&mut self) {
        self.viewport_visible = false;
        self.viewport_rows = 0;
        self.viewport_cursor_row = 0;
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
        self.command_menu_dirty = false;
        self.turn_active = false;
        self.turn_history_rows = 0;
        self.counting_history_rows = false;
        self.history_ends_with_spacing = false;
        self.reset_streams();
    }

    fn reset_session_history(&mut self) {
        self.session_start_width = 0;
        self.session_history_rows = 0;
        self.session_overflowed = false;
    }

    fn markdown_width(&self) -> io::Result<u16> {
        Ok(terminal::size()?.0.saturating_sub(2).max(1))
    }

    fn replace_viewport(
        &mut self,
        history: impl FnOnce(&mut Self) -> io::Result<()>,
    ) -> io::Result<()> {
        let width = terminal::size()?.0.max(1);
        self.synchronized(|terminal| {
            terminal.clear_viewport()?;
            terminal.counting_history_rows = true;
            let history_result = history(terminal);
            terminal.counting_history_rows = false;
            history_result?;
            terminal.render_viewport(width)
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
                Clear(ClearType::CurrentLine),
                ResetColor
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

    fn clear_current_turn(&mut self) -> io::Result<()> {
        if !self.viewport_visible {
            return Ok(());
        }

        let terminal_height = terminal::size()?.1.max(1);
        self.clear_owned_region(terminal_height, self.turn_history_rows)
    }

    fn clear_owned_region(&mut self, terminal_height: u16, history_rows: u16) -> io::Result<()> {
        if !self.viewport_visible {
            return Ok(());
        }
        let (move_up, clear_rows) = rollback_region(
            terminal_height,
            self.viewport_rows,
            self.viewport_cursor_row,
            history_rows,
        );
        queue!(self.stdout, MoveToPreviousLine(move_up))?;
        for row in 0..clear_rows {
            queue!(
                self.stdout,
                MoveToColumn(0),
                Clear(ClearType::CurrentLine),
                ResetColor
            )?;
            if row + 1 < clear_rows {
                queue!(self.stdout, MoveToNextLine(1))?;
            }
        }
        if clear_rows > 1 {
            queue!(self.stdout, MoveToPreviousLine(clear_rows - 1))?;
        }
        Ok(())
    }

    fn next_frame_row(&mut self) -> io::Result<()> {
        if self.counting_history_rows {
            self.session_history_rows = self.session_history_rows.saturating_add(1);
            if self.turn_active {
                self.turn_history_rows = self.turn_history_rows.saturating_add(1);
            }
        }
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
        let command_menu_rows = command_menu_rows(self.command_menu.len());
        let base_rows = base_viewport_rows(self.busy, command_menu_rows);
        let active = self.active_viewport(terminal_height, base_rows);
        let desired_rows = active.rows().saturating_add(base_rows);
        if (self.session_start_width != 0 && self.session_start_width != width)
            || self.session_history_rows.saturating_add(desired_rows) > terminal_height
        {
            self.session_overflowed = true;
        }

        if active.show_spacing {
            queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            self.next_frame_row()?;
        }
        for (index, line) in active.lines.iter().enumerate() {
            self.write_markdown_line(active.kind, active.start + index, line)?;
            self.next_frame_row()?;
        }

        queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        self.next_frame_row()?;

        if self.busy {
            queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            self.render_status(width)?;
            self.next_frame_row()?;

            queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            self.next_frame_row()?;
        }

        let background = self.input_background.clone();
        self.fill_row(&background, width)?;
        self.next_frame_row()?;

        self.fill_row(&background, width)?;
        queue!(self.stdout, MoveToColumn(0))?;
        write!(
            self.stdout,
            "{background}{}›{}{background} {}{}",
            theme::BOLD,
            theme::RESET,
            self.prompt.text,
            theme::RESET
        )?;
        self.next_frame_row()?;

        self.fill_row(&background, width)?;
        self.next_frame_row()?;

        if self.command_menu.is_empty() {
            queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            self.render_footer(width)?;
        } else {
            self.render_command_menu(width)?;
        }

        let rendered_active_rows = active.rows();
        let status_area_rows = if self.busy {
            STATUS_ROWS + STATUS_COMPOSER_SPACING
        } else {
            0
        };
        self.viewport_rows = desired_rows;
        self.viewport_cursor_row = rendered_active_rows
            .saturating_add(TRANSCRIPT_STATUS_SPACING)
            .saturating_add(status_area_rows)
            .saturating_add(1);
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
            MoveToColumn(2 + self.prompt.cursor_column)
        )
    }

    fn active_viewport(&self, terminal_height: u16, base_rows: u16) -> ActiveViewport {
        let available_active = usize::from(terminal_height.saturating_sub(base_rows));
        let wants_spacing = !self.active_lines.is_empty() && self.active_start == 0;
        let show_spacing =
            wants_spacing && self.active_lines.len().saturating_add(1) <= available_active;
        let max_active = available_active.saturating_sub(usize::from(show_spacing));
        let skip = self.active_lines.len().saturating_sub(max_active);
        ActiveViewport {
            show_spacing,
            start: self.active_start + skip,
            kind: self.active_kind.unwrap_or(StreamKind::Assistant),
            lines: self.active_lines[skip..].to_vec(),
        }
    }

    fn redraw_active_region(&mut self) -> io::Result<bool> {
        let terminal_width = terminal::size()?.0.max(1);
        if !self.viewport_visible || self.prompt_width != terminal_width {
            return Ok(false);
        }
        let terminal_height = terminal::size()?.1;
        let command_menu_rows = command_menu_rows(self.command_menu.len());
        let base_rows = base_viewport_rows(self.busy, command_menu_rows);
        let active = self.active_viewport(terminal_height, base_rows);
        let previous_active_rows = self.viewport_rows.saturating_sub(base_rows);
        if active.rows() == 0 || active.rows() != previous_active_rows {
            return Ok(false);
        }

        let cursor_row = self.viewport_cursor_row;
        let cursor_column = 2 + self.prompt.cursor_column;
        self.synchronized(|terminal| {
            queue!(terminal.stdout, MoveToPreviousLine(cursor_row))?;
            let mut rendered_rows = 0u16;
            if active.show_spacing {
                queue!(
                    terminal.stdout,
                    MoveToColumn(0),
                    Clear(ClearType::CurrentLine)
                )?;
                rendered_rows += 1;
                if rendered_rows < active.rows() {
                    queue!(terminal.stdout, MoveToNextLine(1))?;
                }
            }
            for (index, line) in active.lines.iter().enumerate() {
                terminal.write_markdown_line(active.kind, active.start + index, line)?;
                rendered_rows += 1;
                if rendered_rows < active.rows() {
                    queue!(terminal.stdout, MoveToNextLine(1))?;
                }
            }
            queue!(
                terminal.stdout,
                MoveToNextLine(cursor_row.saturating_sub(active.rows().saturating_sub(1))),
                MoveToColumn(cursor_column)
            )?;
            Ok(())
        })?;
        Ok(true)
    }

    fn render_status(&mut self, width: u16) -> io::Result<()> {
        if !self.busy {
            return Ok(());
        }
        let elapsed = self
            .status_started_at
            .map_or(0, |started| started.elapsed().as_secs());
        let elapsed = format_elapsed(elapsed);
        let header = sanitize_single_line(&self.status_header);
        let dots = status_dots(self.status_frame);
        let queued = queued_status(self.queued_messages);
        queue!(self.stdout, MoveToColumn(0))?;
        if width < 32 {
            write!(
                self.stdout,
                "{bold}{header}{dots}{reset}{dim}{queued}{reset}",
                bold = theme::BOLD,
                reset = theme::RESET,
                dim = theme::DIM,
            )
        } else {
            write!(
                self.stdout,
                "{dim}•{reset} {bold}{header}{cyan}{dots}{reset} \
                 {dim}({elapsed} • esc to interrupt){queued}{reset}",
                dim = theme::DIM,
                reset = theme::RESET,
                bold = theme::BOLD,
                cyan = theme::CYAN,
            )
        }
    }

    fn render_command_menu(&mut self, width: u16) -> io::Result<()> {
        if self.command_menu.is_empty() {
            return Ok(());
        }
        let name_width = self
            .command_menu
            .iter()
            .map(|item| UnicodeWidthStr::width(item.name))
            .max()
            .unwrap_or(1);
        let description_column = 2usize
            .saturating_add(1)
            .saturating_add(name_width)
            .saturating_add(2);

        let item_count = self.command_menu.len();
        for (index, item) in self.command_menu.clone().into_iter().enumerate() {
            queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            if index == self.command_menu_selected {
                write!(
                    self.stdout,
                    "{}{}› /{:<name_width$}{}",
                    theme::CYAN,
                    theme::BOLD,
                    item.name,
                    theme::RESET,
                )?;
            } else {
                write!(self.stdout, "  /{:<name_width$}", item.name)?;
            }
            if usize::from(width) > description_column {
                let available = usize::from(width) - description_column;
                let description = fit_menu_text(item.description, available);
                write!(self.stdout, "  {}{description}{}", theme::DIM, theme::RESET)?;
            }
            if index + 1 < item_count {
                self.next_frame_row()?;
            }
        }
        Ok(())
    }

    fn render_footer(&mut self, width: u16) -> io::Result<()> {
        let model = sanitize_single_line(&self.prompt.model);
        let protocol = sanitize_single_line(&self.prompt.protocol);
        let path = sanitize_single_line(&compact_path(&self.prompt.working_dir));
        let protocol_width = UnicodeWidthStr::width(protocol.as_str()) as u16;
        let right_x = width.saturating_sub(protocol_width.saturating_add(2));
        let show_protocol = protocol_width > 0 && right_x > 3;
        let left_width = if show_protocol {
            right_x.saturating_sub(3)
        } else {
            width.saturating_sub(4)
        };
        let (model, path) = fit_status_left(&model, &path, left_width);

        queue!(self.stdout, MoveToColumn(2))?;
        write!(self.stdout, "{}{model}{}", theme::CYAN, theme::RESET)?;
        if let Some(path) = path {
            write!(
                self.stdout,
                "{} · {}{}{path}{}",
                theme::DIM,
                theme::RESET,
                theme::GREEN,
                theme::RESET
            )?;
        }
        if show_protocol {
            queue!(self.stdout, MoveToColumn(right_x))?;
            write!(self.stdout, "{}{protocol}{}", theme::CYAN, theme::RESET)?;
        }
        Ok(())
    }

    fn update_input(&mut self) -> io::Result<()> {
        if self.rendered_input != self.prompt.text {
            let previous_width = UnicodeWidthStr::width(self.rendered_input.as_str());
            let current_width = UnicodeWidthStr::width(self.prompt.text.as_str());
            queue!(self.stdout, MoveToColumn(2))?;
            write!(self.stdout, "{}{}", self.input_background, self.prompt.text)?;
            if previous_width > current_width {
                write!(
                    self.stdout,
                    "{}",
                    " ".repeat(previous_width - current_width)
                )?;
            }
            write!(self.stdout, "{}", theme::RESET)?;
            self.rendered_input.clone_from(&self.prompt.text);
        }
        queue!(self.stdout, MoveToColumn(2 + self.prompt.cursor_column))?;
        self.stdout.flush()
    }

    fn fill_row(&mut self, background: &str, width: u16) -> io::Result<()> {
        queue!(self.stdout, MoveToColumn(0))?;
        write!(self.stdout, "{background}")?;
        queue!(self.stdout, Clear(ClearType::CurrentLine))?;
        write!(
            self.stdout,
            "{}",
            " ".repeat(usize::from(width.saturating_sub(1)))
        )?;
        write!(self.stdout, "{}", theme::RESET)?;
        queue!(self.stdout, MoveToColumn(0))?;
        Ok(())
    }

    fn write_user_history(&mut self, input: &str) -> io::Result<()> {
        let width = terminal::size()?.0.max(1);
        let background = self.input_background.clone();
        let input = sanitize_terminal_text(input);
        let lines = wrap_text(&input, width.saturating_sub(3).max(1));

        self.begin_history_cell()?;
        self.fill_row(&background, width)?;
        self.next_frame_row()?;
        for (index, line) in lines.iter().enumerate() {
            self.fill_row(&background, width)?;
            write!(self.stdout, "{background}")?;
            if index == 0 {
                write!(
                    self.stdout,
                    "{}› {}{background}",
                    theme::USER_PREFIX,
                    theme::RESET
                )?;
            } else {
                write!(self.stdout, "  ")?;
            }
            write!(self.stdout, "{line}{}", theme::RESET)?;
            self.next_frame_row()?;
        }
        self.fill_row(&background, width)?;
        self.next_frame_row()?;
        self.history_ends_with_spacing = true;
        Ok(())
    }

    fn write_markdown_batches(&mut self, batches: &[MarkdownBatch]) -> io::Result<()> {
        for batch in batches {
            if batch.start == 0 && !batch.lines.is_empty() {
                self.begin_history_cell()?;
            }
            for (index, line) in batch.lines.iter().enumerate() {
                self.write_markdown_line(batch.kind, batch.start + index, line)?;
                self.next_frame_row()?;
            }
        }
        Ok(())
    }

    fn write_markdown_line(
        &mut self,
        _kind: StreamKind,
        index: usize,
        line: &RenderedLine,
    ) -> io::Result<()> {
        queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        if index == 0 {
            write!(self.stdout, "{}•{} ", theme::DIM, theme::RESET)?;
        } else {
            write!(self.stdout, "  ")?;
        }
        line.write_ansi(&mut self.stdout)
    }

    fn write_tool_history(
        &mut self,
        name: &str,
        arguments: &Value,
        is_error: bool,
    ) -> io::Result<()> {
        self.begin_history_cell()?;
        let width = terminal::size()?.0.max(1);
        let (action, detail) =
            tool_call_summary(name, arguments, is_error, width.saturating_sub(2));
        let bullet_style = if is_error {
            theme::TOOL_ERROR_BULLET
        } else {
            theme::TOOL_SUCCESS_BULLET
        };
        write!(
            self.stdout,
            "{bullet_style}•{} {}{action}{}",
            theme::RESET,
            theme::BOLD,
            theme::RESET
        )?;
        if !detail.is_empty() {
            write!(self.stdout, " {detail}")?;
        }
        self.next_frame_row()
    }

    fn write_restored_messages(&mut self, messages: &[Message]) -> io::Result<()> {
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
                    self.write_user_history(&text)?;
                }
                MessageContent::Assistant(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text(text) if !text.is_empty() => {
                                let width = terminal::size()?.0.saturating_sub(2).max(1);
                                let lines = render_markdown(text, width);
                                self.write_markdown_batches(&[MarkdownBatch::new(
                                    StreamKind::Assistant,
                                    0,
                                    lines,
                                )])?;
                            }
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => self.write_tool_history(
                                name,
                                arguments,
                                tool_results.get(id).copied().unwrap_or(false),
                            )?,
                            ContentBlock::Text(_) => {}
                        }
                    }
                }
                MessageContent::ToolResult { .. } => {}
            }
        }
        Ok(())
    }

    fn write_error_history(&mut self, error: &str) -> io::Result<()> {
        self.begin_history_cell()?;
        let error = sanitize_terminal_text(error);
        let width = terminal::size()?.0.saturating_sub(9).max(1);
        for (index, line) in wrap_text(&error, width).iter().enumerate() {
            if index == 0 {
                write!(
                    self.stdout,
                    "{}•{} {}Error:{} {line}",
                    theme::TOOL_ERROR_BULLET,
                    theme::RESET,
                    theme::BOLD,
                    theme::RESET
                )?;
            } else {
                write!(self.stdout, "  {line}")?;
            }
            self.next_frame_row()?;
        }
        Ok(())
    }

    fn write_info_history(&mut self, message: &str) -> io::Result<()> {
        self.begin_history_cell()?;
        let message = sanitize_terminal_text(message);
        let width = terminal::size()?.0.saturating_sub(4).max(1);
        let mut first = true;
        for source_line in message.lines() {
            for line in wrap_text(source_line, width) {
                if first {
                    write!(self.stdout, "{}•{} {line}", theme::DIM, theme::RESET)?;
                    first = false;
                } else {
                    write!(self.stdout, "  {line}")?;
                }
                self.next_frame_row()?;
            }
        }
        if first {
            write!(self.stdout, "{}•{}", theme::DIM, theme::RESET)?;
            self.next_frame_row()?;
        }
        Ok(())
    }

    fn write_worked_history(&mut self, elapsed_seconds: u64) -> io::Result<()> {
        self.begin_history_cell()?;
        let width = terminal::size()?.0.max(1);
        let separator = worked_separator(elapsed_seconds, width);
        queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        write!(self.stdout, "{}{separator}{}", theme::DIM, theme::RESET)?;
        self.next_frame_row()
    }

    fn begin_history_cell(&mut self) -> io::Result<()> {
        if history_cell_needs_spacing(
            self.has_emitted_history_cells,
            self.history_ends_with_spacing,
        ) {
            queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            self.next_frame_row()?;
        }
        if !self.has_emitted_history_cells {
            self.has_emitted_history_cells = true;
        }
        self.history_ends_with_spacing = false;
        Ok(())
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

fn rollback_region(
    terminal_height: u16,
    viewport_rows: u16,
    viewport_cursor_row: u16,
    turn_history_rows: u16,
) -> (u16, u16) {
    let terminal_height = terminal_height.max(1);
    let rows_below_cursor = viewport_rows
        .saturating_sub(1)
        .saturating_sub(viewport_cursor_row);
    let move_up = viewport_cursor_row
        .saturating_add(turn_history_rows)
        .min(terminal_height - 1);
    let clear_rows = move_up
        .saturating_add(rows_below_cursor)
        .saturating_add(1)
        .min(terminal_height);
    (move_up, clear_rows)
}

fn can_clear_session_locally(
    session_start_width: u16,
    current_width: u16,
    session_history_rows: u16,
    viewport_rows: u16,
    terminal_height: u16,
    session_overflowed: bool,
) -> bool {
    !session_overflowed
        && session_start_width != 0
        && session_start_width == current_width
        && session_history_rows.saturating_add(viewport_rows) <= terminal_height
}

fn history_cell_needs_spacing(
    has_emitted_history_cells: bool,
    history_ends_with_spacing: bool,
) -> bool {
    has_emitted_history_cells && !history_ends_with_spacing
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

fn base_viewport_rows(busy: bool, command_menu_rows: u16) -> u16 {
    TRANSCRIPT_STATUS_SPACING
        + if busy {
            STATUS_ROWS + STATUS_COMPOSER_SPACING
        } else {
            0
        }
        + command_menu_rows
        + COMPOSER_ROWS
        + if command_menu_rows == 0 {
            FOOTER_ROWS
        } else {
            0
        }
}

fn command_menu_rows(item_count: usize) -> u16 {
    u16::try_from(item_count).unwrap_or(u16::MAX)
}

fn fit_menu_text(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    let available = width.saturating_sub(1);
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > available {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

fn worked_separator(elapsed_seconds: u64, width: u16) -> String {
    let label = format!("─ Worked for {} ─", format_elapsed(elapsed_seconds));
    let width = usize::from(width);
    let label_width = UnicodeWidthStr::width(label.as_str());
    if label_width >= width {
        return label.chars().take(width).collect();
    }
    format!("{label}{}", "─".repeat(width - label_width))
}

fn markdown_drain_count(queue_depth: usize, queued_at: Option<Instant>, now: Instant) -> usize {
    if queue_depth == 0 {
        return 0;
    }
    let oldest_age = queued_at.map_or(Duration::ZERO, |queued_at| {
        now.saturating_duration_since(queued_at)
    });
    if queue_depth >= STREAM_CATCH_UP_DEPTH || oldest_age >= STREAM_CATCH_UP_AGE {
        queue_depth
    } else {
        1
    }
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
    fn rollback_region_covers_only_the_current_turn_and_viewport() {
        assert_eq!(rollback_region(30, 6, 3, 8), (11, 14));
    }

    #[test]
    fn rollback_region_stays_inside_the_visible_terminal() {
        assert_eq!(rollback_region(20, 6, 3, u16::MAX), (19, 20));
    }

    #[test]
    fn clears_a_session_locally_only_when_its_layout_is_still_visible() {
        assert!(can_clear_session_locally(80, 80, 12, 6, 24, false));
        assert!(!can_clear_session_locally(80, 100, 12, 6, 24, false));
        assert!(!can_clear_session_locally(80, 80, 20, 6, 24, false));
        assert!(!can_clear_session_locally(80, 80, 12, 6, 24, true));
    }

    #[test]
    fn reuses_trailing_history_spacing_without_merging_cells() {
        assert!(history_cell_needs_spacing(true, false));
        assert!(!history_cell_needs_spacing(true, true));
        assert!(!history_cell_needs_spacing(false, false));
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
    fn matches_codex_bottom_pane_vertical_spacing() {
        assert_eq!(base_viewport_rows(false, 0), 5);
        assert_eq!(base_viewport_rows(true, 0), 7);
        assert_eq!(base_viewport_rows(false, 3), 7);
        assert_eq!(base_viewport_rows(true, 3), 9);
    }

    #[test]
    fn reserves_one_row_per_command_completion() {
        assert_eq!(command_menu_rows(0), 0);
        assert_eq!(command_menu_rows(3), 3);
    }

    #[test]
    fn formats_queued_message_status() {
        assert_eq!(queued_status(0), "");
        assert_eq!(queued_status(1), " · 1 queued");
        assert_eq!(queued_status(3), " · 3 queued");
    }

    #[test]
    fn formats_completed_work_like_codex_turn_separators() {
        let separator = worked_separator(125, 32);
        assert!(separator.starts_with("─ Worked for 2m 05s ─"));
        assert_eq!(UnicodeWidthStr::width(separator.as_str()), 32);
        assert_eq!(worked_separator(0, 10), "─ Worked f");
    }

    #[test]
    fn streams_one_line_normally_and_catches_up_under_pressure() {
        let now = Instant::now();
        assert_eq!(markdown_drain_count(0, None, now), 0);
        assert_eq!(markdown_drain_count(3, Some(now), now), 1);
        assert_eq!(
            markdown_drain_count(STREAM_CATCH_UP_DEPTH, Some(now), now),
            STREAM_CATCH_UP_DEPTH
        );
        assert_eq!(
            markdown_drain_count(
                3,
                Some(now - STREAM_CATCH_UP_AGE - Duration::from_millis(1)),
                now,
            ),
            3
        );
    }

    #[test]
    fn truncates_command_descriptions_to_the_menu_width() {
        assert_eq!(fit_menu_text("abcdef", 4), "abc…");
        assert_eq!(
            UnicodeWidthStr::width(fit_menu_text("中文说明", 5).as_str()),
            5
        );
    }
}
