use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::Instant,
};

use ash_core::{Content, ContentBlock, ForkPoint, Message, MessageContent, ThreadSummary};
use crossterm::terminal;
use ratatui::layout::Position;
use serde_json::Value;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

use crate::{
    history_block::HistoryBlock,
    inline_surface::InlineScreen,
    input::InputState,
    live_block::LiveBlock,
    menu::MenuView,
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
    context_limit: Option<u64>,
}

impl SessionView {
    fn new(protocol: &str, model: &str, working_dir: &Path, context_limit: Option<u64>) -> Self {
        Self {
            protocol: protocol.to_string(),
            model: model.to_string(),
            working_dir: working_dir.to_path_buf(),
            context_limit,
        }
    }

    fn update(&mut self, protocol: &str, model: &str, working_dir: &Path) {
        self.protocol = protocol.to_string();
        self.model = model.to_string();
        self.working_dir = working_dir.to_path_buf();
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct TurnUsage {
    input_tokens: u64,
    output_tokens: u64,
    generation_ms: u64,
}

impl TurnUsage {
    fn add(&mut self, input_tokens: u64, output_tokens: u64, generation_ms: u64) {
        self.input_tokens = self.input_tokens.saturating_add(input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(output_tokens);
        self.generation_ms = self.generation_ms.saturating_add(generation_ms);
    }
}

#[derive(Debug, Default)]
struct UsageState {
    /// Latest known model-context size: an API-reported usage value when
    /// available, otherwise a local estimate (resume, fork, rollback, compact).
    context_tokens: Option<u64>,
    context_estimated: bool,
    turn: TurnUsage,
}

impl UsageState {
    fn begin_turn(&mut self) {
        self.turn = TurnUsage::default();
    }

    fn record(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        generation_ms: u64,
        estimated: bool,
    ) {
        self.context_tokens = Some(input_tokens.saturating_add(output_tokens));
        self.context_estimated = estimated;
        self.turn.add(input_tokens, output_tokens, generation_ms);
    }

    fn finish_turn(&mut self) -> TurnUsage {
        std::mem::take(&mut self.turn)
    }

    fn set_compacted_context(&mut self, tokens: u64) {
        self.context_tokens = Some(tokens);
        self.context_estimated = true;
    }

    /// Record a locally estimated context size (restore, fork, rollback) so
    /// the status line reflects current occupancy before any API usage is
    /// reported for the new history.
    fn set_estimated_context(&mut self, tokens: u64) {
        self.context_tokens = Some(tokens);
        self.context_estimated = true;
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
    pub(crate) menu: MenuView<'a>,
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
enum RenderedMenu {
    #[default]
    None,
    Commands {
        items: Vec<CommandCompletion>,
        selected: usize,
    },
    Sessions {
        items: Vec<ThreadSummary>,
        selected: usize,
    },
    ForkPoints {
        items: Vec<ForkPoint>,
        selected: usize,
    },
}

impl RenderedMenu {
    fn set(&mut self, menu: MenuView<'_>) {
        *self = match menu {
            MenuView::None => Self::None,
            MenuView::Commands { items, selected } => Self::Commands {
                items: items.to_vec(),
                selected: selected.min(items.len().saturating_sub(1)),
            },
            MenuView::Sessions { items, selected } => Self::Sessions {
                items: items.to_vec(),
                selected: selected.min(items.len().saturating_sub(1)),
            },
            MenuView::ForkPoints { items, selected } => Self::ForkPoints {
                items: items.to_vec(),
                selected: selected.min(items.len().saturating_sub(1)),
            },
        };
    }

    fn view(&self) -> MenuView<'_> {
        match self {
            Self::None => MenuView::None,
            Self::Commands { items, selected } => MenuView::Commands {
                items,
                selected: *selected,
            },
            Self::Sessions { items, selected } => MenuView::Sessions {
                items,
                selected: *selected,
            },
            Self::ForkPoints { items, selected } => MenuView::ForkPoints {
                items,
                selected: *selected,
            },
        }
    }

    fn clear(&mut self) {
        *self = Self::None;
    }
}

#[derive(Debug, Default)]
struct ViewState {
    composer: ComposerState,
    menu: RenderedMenu,
    busy: bool,
    queued_messages: usize,
}

pub(crate) struct TerminalUi {
    surface: InlineScreen,
    session: SessionView,
    view: ViewState,
    history: Vec<LiveBlock>,
    transcript: Vec<LiveBlock>,
    scroll_top: Option<u16>,
    next_block_id: u64,
    status: StatusState,
    usage: UsageState,
    selection: Option<TextSelection>,
    stream: StreamState,
    current_turn_id: Option<u64>,
    next_turn_id: u64,
}

impl TerminalUi {
    pub fn enter(
        protocol: &str,
        model: &str,
        working_dir: &Path,
        context_limit: Option<u64>,
    ) -> io::Result<Self> {
        let surface = InlineScreen::enter()?;
        Ok(Self {
            surface,
            session: SessionView::new(protocol, model, working_dir, context_limit),
            view: ViewState::default(),
            history: Vec::new(),
            transcript: Vec::new(),
            scroll_top: None,
            next_block_id: 1,
            status: StatusState::default(),
            usage: UsageState::default(),
            selection: None,
            stream: StreamState::default(),
            current_turn_id: None,
            next_turn_id: 1,
        })
    }

    pub fn welcome(&mut self) -> io::Result<()> {
        self.enqueue_welcome();
        self.commit_transcript_to_scrollback()
    }

    fn enqueue_welcome(&mut self) {
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::welcome(id, self.session.working_dir.clone()));
    }

    pub fn command_output(&mut self, message: &str) -> io::Result<()> {
        self.view.composer.clear();
        self.scroll_top = None;
        self.push_history_block(HistoryBlock::info(message));
        self.commit_transcript_to_scrollback()
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
        self.commit_transcript_to_scrollback()
    }

    pub fn start_compaction(&mut self) -> io::Result<()> {
        self.view.composer.clear();
        self.scroll_top = None;
        self.status.start("Compacting");
        self.redraw()
    }

    pub fn finish_compaction(
        &mut self,
        before_tokens: u64,
        after_tokens: u64,
        dropped_messages: u64,
    ) -> io::Result<()> {
        self.status.stop();
        self.usage.set_compacted_context(after_tokens);
        let message = if dropped_messages == 0 {
            "Model context is already compact; no new summary was created.".to_string()
        } else {
            format!(
                "Compacted model context from {before_tokens} to {after_tokens} tokens; summarized {dropped_messages} earlier messages. Full history remains visible."
            )
        };
        self.command_output(&message)
    }

    pub fn record_automatic_compaction(&mut self, after_tokens: u64) -> io::Result<()> {
        self.usage.set_compacted_context(after_tokens);
        self.redraw()
    }

    /// Update the status-line context occupancy after a rollback, without
    /// clearing the transcript (unlike `restore_session`).
    pub fn record_rollback_context(&mut self, tokens: u64) -> io::Result<()> {
        self.usage.set_estimated_context(tokens);
        self.redraw()
    }

    pub fn start_new_session(&mut self) -> io::Result<()> {
        self.begin_fresh_viewport()?;
        self.enqueue_welcome();
        self.commit_transcript_to_scrollback()
    }

    fn begin_fresh_viewport(&mut self) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal.history.clear();
            terminal.transcript.clear();
            terminal.scroll_top = None;
            terminal.reset_ui_state()?;
            Ok(())
        })
    }

    pub fn rollback_turn(&mut self) -> io::Result<()> {
        let turn_id = latest_turn_id(self.current_turn_id, &self.transcript, &self.history);
        let (width, height) = terminal_size()?;
        if let Some(turn_id) = turn_id {
            self.history.retain(|block| !block.belongs_to_turn(turn_id));
            self.transcript
                .retain(|block| !block.belongs_to_turn(turn_id));
        }
        self.scroll_top = None;
        self.reset_turn_state();
        self.rebuild_scrollback_at(width, height)
    }

    pub fn restore_session(
        &mut self,
        messages: &[Message],
        protocol: &str,
        model: &str,
        working_dir: &Path,
        context_tokens: Option<u64>,
    ) -> io::Result<()> {
        self.begin_fresh_viewport()?;
        self.session.update(protocol, model, working_dir);
        self.enqueue_welcome();
        self.push_restored_messages(messages);
        // `begin_fresh_viewport` resets usage; restore the estimated context
        // size so the status line reflects current occupancy before any API
        // usage is reported for the new history.
        if let Some(tokens) = context_tokens {
            self.usage.set_estimated_context(tokens);
        }
        self.commit_transcript_to_scrollback()
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
        self.apply_view(view, width);
        self.redraw_at(width, height)
    }

    fn apply_view(&mut self, view: TerminalView<'_>, width: u16) {
        let input = view.input.view(composer_text_width(width));
        self.view.composer.lines = input.lines;
        self.view.composer.cursor_row = input.cursor_row;
        self.view.composer.cursor_column = input.cursor_column;
        self.view.menu.set(view.menu);
        self.view.busy = view.busy;
        self.view.queued_messages = view.queued_messages;
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
        self.usage.begin_turn();
        self.status.start("Working");
        self.commit_user_message(input)
    }

    pub fn commit_exit(&mut self, input: &str) -> io::Result<()> {
        self.finish_stream();
        self.current_turn_id = None;
        self.status.stop();
        self.view.busy = false;
        self.commit_user_message(input)?;
        self.commit_transcript_to_scrollback()
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

    pub fn record_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        generation_ms: u64,
        estimated: bool,
    ) -> io::Result<()> {
        self.usage
            .record(input_tokens, output_tokens, generation_ms, estimated);
        self.redraw()
    }

    pub fn error(&mut self, error: &str) -> io::Result<()> {
        self.finish_stream();
        self.status.header = "Failed".to_string();
        self.push_history_block(HistoryBlock::error(error));
        if self.current_turn_id.is_some() {
            self.redraw()
        } else {
            self.commit_transcript_to_scrollback()
        }
    }

    pub fn finish_response(&mut self) -> io::Result<()> {
        self.finish_stream();
        let elapsed_seconds = self.status.elapsed_seconds();
        let usage = self.usage.finish_turn();
        self.status.stop();
        self.view.busy = false;
        self.push_history_block(HistoryBlock::worked(
            format_elapsed(elapsed_seconds),
            usage.input_tokens,
            usage.output_tokens,
            usage.generation_ms,
        ));
        self.current_turn_id = None;
        self.commit_transcript_to_scrollback()
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
        self.apply_view(view, width);
        self.rebuild_scrollback_at(width, height)
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

    pub fn start_selection(&mut self, column: u16, row: u16) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.selection = self
            .viewport_frame(width, height)
            .selection_start(column, row)
            .map(TextSelection::new);
        Ok(())
    }

    pub fn drag_selection(&mut self, column: u16, row: u16) -> io::Result<()> {
        let Some(mut selection) = self.selection else {
            return Ok(());
        };
        let (width, height) = terminal_size()?;
        let frame = self.viewport_frame(width, height);
        let next_scroll_top = frame.scroll_top_for_drag(row, selection.anchor, selection.focus);
        if next_scroll_top != frame.scroll_top {
            self.scroll_top = (next_scroll_top < frame.max_scroll_top).then_some(next_scroll_top);
        }
        let frame = self.viewport_frame(width, height);
        if let Some(focus) = frame.selection_focus(selection.anchor, column, row) {
            selection.focus = focus;
        }
        self.selection = Some(selection);
        self.redraw_at(width, height)
    }

    pub fn finish_selection(&mut self, column: u16, row: u16) -> io::Result<()> {
        let Some(mut selection) = self.selection else {
            return Ok(());
        };
        let (width, height) = terminal_size()?;
        let frame = self.viewport_frame(width, height);
        if let Some(focus) = frame.selection_focus(selection.anchor, column, row) {
            selection.focus = focus;
        }
        if selection.anchor == selection.focus {
            self.selection = None;
            return Ok(());
        }

        self.selection = Some(selection);
        let text = frame.selection_text(selection.anchor, selection.focus);
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
        self.status.stop();
        self.view.busy = false;
        self.current_turn_id = None;
        self.commit_transcript_to_scrollback()?;
        self.surface.leave_screen()
    }

    fn finish_stream(&mut self) {
        let finished = self.stream.finish();
        self.commit_finished_stream(finished);
    }

    fn commit_finished_stream(&mut self, finished: Option<FinishedStream>) {
        match finished {
            Some(FinishedStream::Assistant { pending, block_id }) => {
                if let Some(id) = self.append_assistant(pending, block_id) {
                    if let Some(block) = self.transcript.iter_mut().find(|block| block.id() == id) {
                        block.finalize_markdown();
                    }
                }
            }
            Some(FinishedStream::Thought { elapsed_seconds }) => {
                self.push_thought_block(elapsed_seconds)
            }
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
        self.usage = UsageState::default();
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

    fn commit_transcript_to_scrollback(&mut self) -> io::Result<()> {
        if self.transcript.is_empty() {
            return self.redraw();
        }

        let (width, height) = terminal_size()?;
        let render_width = viewport::drawable_width(width);
        let committed = std::mem::take(&mut self.transcript);
        let result = self.synchronized(|terminal| {
            terminal.scroll_top = None;
            terminal.selection = None;

            // Shrink the live viewport before inserting history so committed rows remain visible
            // directly above the composer instead of disappearing above a full-screen viewport.
            terminal.render_viewport(width, height)?;
            insert_history_blocks(&mut terminal.surface, &committed, render_width)?;
            terminal.render_viewport(width, height)
        });
        if result.is_ok() {
            self.history.extend(committed);
        } else {
            self.transcript = committed;
        }
        result
    }

    fn rebuild_scrollback_at(&mut self, width: u16, height: u16) -> io::Result<()> {
        let render_width = viewport::drawable_width(width);
        let history = std::mem::take(&mut self.history);
        let result = self.synchronized(|terminal| {
            terminal.surface.reset()?;
            terminal.selection = None;
            terminal.render_viewport(width, height)?;
            insert_history_blocks(&mut terminal.surface, &history, render_width)?;
            terminal.render_viewport(width, height)
        });
        for block in &history {
            block.clear_render_cache();
        }
        self.history = history;
        result
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

    fn push_thought_block(&mut self, elapsed_seconds: u64) {
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::thought(id, elapsed_seconds));
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
                                self.push_thought_block(*elapsed_seconds);
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
            menu: self.view.menu.view(),
            model: &model,
            protocol: &protocol,
            working_dir: &self.session.working_dir,
            context_tokens: self.usage.context_tokens,
            context_estimated: self.usage.context_estimated,
            context_limit: self.session.context_limit,
        })
    }
}

fn insert_history_blocks(
    surface: &mut InlineScreen,
    blocks: &[LiveBlock],
    render_width: u16,
) -> io::Result<()> {
    for block in blocks {
        let buffer = block.render(render_width);
        surface.insert_buffer(&buffer, 1)?;
        block.clear_render_cache();
    }
    Ok(())
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

fn latest_turn_id(
    current_turn_id: Option<u64>,
    transcript: &[LiveBlock],
    history: &[LiveBlock],
) -> Option<u64> {
    current_turn_id
        .or_else(|| transcript.iter().rev().find_map(LiveBlock::turn_id))
        .or_else(|| history.iter().rev().find_map(LiveBlock::turn_id))
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
    fn usage_sums_model_calls_and_keeps_the_latest_context_size() {
        let mut usage = UsageState::default();
        usage.begin_turn();
        usage.record(100, 20, 400, false);
        usage.record(150, 30, 600, true);

        assert_eq!(usage.context_tokens, Some(180));
        assert!(usage.context_estimated);
        let turn = usage.finish_turn();
        assert_eq!(turn.input_tokens, 250);
        assert_eq!(turn.output_tokens, 50);
        assert_eq!(turn.generation_ms, 1_000);
    }

    #[test]
    fn estimated_context_sets_tokens() {
        let mut usage = UsageState::default();
        usage.set_estimated_context(42_000);
        assert_eq!(usage.context_tokens, Some(42_000));
        assert!(usage.context_estimated);
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

    #[test]
    fn rollback_finds_the_latest_committed_turn_behind_non_turn_history() {
        let history = [
            LiveBlock::history(1, HistoryBlock::user("question")).with_turn(Some(7)),
            LiveBlock::assistant(2, "answer".to_string()).with_turn(Some(7)),
            LiveBlock::history(3, HistoryBlock::info("status")),
        ];

        assert_eq!(latest_turn_id(None, &[], &history), Some(7));
    }
}
