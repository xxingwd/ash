use std::{
    collections::{HashMap, HashSet},
    fmt, io,
    path::{Path, PathBuf},
    time::Instant,
};

use ash_core::{
    Content, ContentBlock, FileChange, ForkPoint, Message, MessageContent, MessageId, StopReason,
    SubagentSnapshot, ThreadSummary, ThreadView, ToolCallId, TurnResult, TurnView, Usage,
};
use crossterm::terminal;
use serde_json::Value;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

use crate::{
    history_block::HistoryBlock,
    inline_surface::InlineScreen,
    input::InputState,
    live_block::LiveBlock,
    menu::MenuView,
    operation::CancellationMode,
    scrollback::sanitize_single_line,
    slash_command::CommandCompletion,
    stream_state::{format_elapsed, FinishedThought, StreamState},
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TurnUsage {
    input_tokens: u64,
    output_tokens: u64,
    generation_ms: u64,
}

#[derive(Debug, Default)]
struct UsageState {
    /// Current model-context size, always a local estimate from the agent
    /// (same estimator the runtime uses for compaction). The API usage is
    /// not used here: it is billed per request/turn and would drift from the
    /// context that actually drives compaction.
    context_tokens: Option<u64>,
}

impl UsageState {
    /// Record the settled turn's usage for the worked summary block. The
    /// context size is updated separately from `view.context_tokens`.
    fn commit_turn_usage(usage: Option<&Usage>) -> TurnUsage {
        usage.map_or_else(TurnUsage::default, |usage| TurnUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            generation_ms: usage.generation_ms,
        })
    }

    /// Record the current model-context size (turn end, compaction, restore,
    /// fork, rollback). Always an estimate; never mixed with API usage.
    const fn set_context_tokens(&mut self, tokens: u64) {
        self.context_tokens = Some(tokens);
    }
}

#[derive(Debug, Default)]
struct ComposerState {
    lines: Vec<String>,
    cursor_row: u16,
    cursor_column: u16,
}

#[derive(Clone, Copy)]
pub struct TerminalView<'a> {
    pub(crate) input: &'a InputState,
    pub(crate) menu: MenuView<'a>,
    pub(crate) busy: bool,
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
}

pub struct TerminalUi {
    surface: InlineScreen,
    session: SessionView,
    view: ViewState,
    history: Vec<LiveBlock>,
    transcript: Vec<LiveBlock>,
    /// Global display mode for tool output: true renders full output, false
    /// renders a short preview. Persists across session switches (`Ctrl+o`).
    tools_expanded: bool,
    /// Display-only snapshots of the session's sub-agents, refreshed by the
    /// app from the collaboration control's watch channel.
    subagents: Vec<SubagentSnapshot>,
    scroll_top: Option<u16>,
    next_block_id: u64,
    status: StatusState,
    usage: UsageState,
    stream: StreamState,
    current_turn_id: Option<u64>,
    next_turn_id: u64,
    /// Id of the turn's active streaming assistant block (if any). Deltas are
    /// appended straight into this block; cleared when the turn commits.
    assistant_block_id: Option<u64>,
}

/// True when a streamed delta contains a newline, meaning a complete line is
/// available to display. Newline-complete deltas flush immediately; partial
/// runs are picked up by the periodic status refresh.
fn delta_completes_line(delta: &str) -> bool {
    delta.contains('\n')
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
            tools_expanded: false,
            subagents: Vec::new(),
            scroll_top: None,
            next_block_id: 1,
            status: StatusState::default(),
            usage: UsageState::default(),
            stream: StreamState::default(),
            current_turn_id: None,
            next_turn_id: 1,
            assistant_block_id: None,
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
        self.usage.set_context_tokens(after_tokens);
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
        self.usage.set_context_tokens(after_tokens);
        self.redraw()
    }

    /// Update the status-line context occupancy after a rollback, without
    /// clearing the transcript (unlike `restore_session`).
    pub fn record_rollback_context(&mut self, tokens: u64) -> io::Result<()> {
        self.usage.set_context_tokens(tokens);
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
        thread: &ThreadView,
        context_tokens: Option<u64>,
    ) -> io::Result<()> {
        self.begin_fresh_viewport()?;
        self.enqueue_welcome();
        if thread.turns.is_empty() {
            self.push_restored_messages(&thread.messages);
        } else {
            let turn_message_ids = thread
                .turns
                .iter()
                .flat_map(|turn| turn.messages.iter().map(|message| message.id))
                .collect::<HashSet<_>>();
            self.push_restored_messages_excluding(&thread.messages, &turn_message_ids);
            self.push_restored_turns(&thread.turns);
        }
        // `begin_fresh_viewport` resets usage; restore the estimated context
        // size so the status line reflects current occupancy before any API
        // usage is reported for the new history.
        if let Some(tokens) = context_tokens {
            self.usage.set_context_tokens(tokens);
        }
        self.commit_transcript_to_scrollback()
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
    }

    pub fn composer_text_width() -> io::Result<u16> {
        Ok(composer_text_width(terminal_size()?.0))
    }

    pub fn commit_input(&mut self, input: &str) -> io::Result<()> {
        let turn_id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        self.current_turn_id = Some(turn_id);
        self.scroll_top = None;
        self.status.start("Working");
        self.commit_user_message(input)
    }

    pub fn commit_steer(&mut self, input: &str) -> io::Result<()> {
        self.finish_stream();
        self.assistant_block_id = None;
        self.scroll_top = None;
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

    /// Append one text delta to the turn's streaming assistant block,
    /// creating it on first use. Deltas go straight into the block's
    /// incremental markdown renderer; nothing is queued for a later frame.
    fn append_assistant(&mut self, source: &str) {
        if source.is_empty() {
            return;
        }
        let id = self.assistant_block_id.unwrap_or_else(|| {
            // Force-finish any in-progress reasoning before the first text
            // delta: `start_reasoning` is a no-op when already reasoning, so
            // use `finish` to always solidify the thought into a block.
            let finished = self.stream.finish();
            self.commit_finished_stream(finished);
            let id = self.allocate_block_id();
            self.push_block(LiveBlock::assistant(id, String::new()));
            self.assistant_block_id = Some(id);
            id
        });
        if let Some(block) = self.transcript.iter_mut().find(|block| block.id() == id) {
            block.append_markdown_source(source);
        }
    }

    /// Append one text delta and redraw immediately when it completes a line,
    /// so output reads line by line instead of character by character.
    /// Partial runs are picked up by the periodic status refresh.
    pub fn text(&mut self, text: &str) -> io::Result<()> {
        self.append_assistant(text);
        if delta_completes_line(text) {
            self.refresh_stream_view()?;
        }
        Ok(())
    }

    /// Append one reasoning delta to the scrolling preview. Newline-complete
    /// deltas redraw immediately (like text), so the preview reads line by
    /// line; partial runs are picked up by the periodic status refresh.
    /// Reasoning always precedes the assistant text: the protocol emits
    /// thinking blocks before text output, so no late-reasoning handling is
    /// needed here.
    pub fn thinking(&mut self, text: &str) -> io::Result<()> {
        self.stream.start_reasoning();
        self.stream.push_reasoning(text);
        if delta_completes_line(text) {
            self.refresh_stream_view()?;
        }
        Ok(())
    }

    pub fn tool_start(&mut self) -> io::Result<()> {
        self.finish_stream();
        self.redraw()
    }

    pub fn tool_end(
        &mut self,
        name: &str,
        arguments: &Value,
        output: &str,
        is_error: bool,
        file_change: Option<FileChange>,
    ) -> io::Result<()> {
        self.push_tool_block(
            name.to_string(),
            arguments.clone(),
            output.to_string(),
            is_error,
            file_change,
        );
        self.redraw()
    }

    /// Prepare the live projection for cancellation. A completed tool result
    /// keeps the turn; otherwise all streamed output is removed immediately
    /// because text and reasoning have no reliable completion boundary.
    pub fn prepare_cancellation(&mut self) -> CancellationMode {
        self.stream.reset();
        let Some(turn_id) = self.current_turn_id else {
            self.assistant_block_id = None;
            return CancellationMode::Rollback;
        };
        let mode = if self
            .transcript
            .iter()
            .any(|block| block.is_completed_tool_for_turn(turn_id))
        {
            CancellationMode::Interrupt
        } else {
            CancellationMode::Rollback
        };
        self.transcript
            .retain(|block| keep_during_cancellation(block, turn_id, mode));
        self.assistant_block_id = None;
        mode
    }

    pub fn error(&mut self, error: &str) -> io::Result<()> {
        self.finish_stream();
        self.push_history_block(HistoryBlock::error(error));
        if self.current_turn_id.is_some() {
            self.redraw()
        } else {
            self.commit_transcript_to_scrollback()
        }
    }

    /// Commit a settled turn: replace the streamed preview with the canonical
    /// projection of the turn's messages, then move it into the scrollback.
    pub fn commit_turn(&mut self, view: &TurnView) -> io::Result<()> {
        self.finish_stream();
        let elapsed_seconds = self.status.elapsed_seconds();
        if let Some(tokens) = view.context_tokens {
            self.usage.set_context_tokens(tokens);
        }
        let usage = UsageState::commit_turn_usage(view.usage.as_ref());
        let footer = turn_footer(&view.result, elapsed_seconds, usage);
        if let Some(turn_id) = self.current_turn_id {
            self.transcript.retain(|block| {
                !block.is_streamed_for_turn(turn_id)
                    && !block.matches_history_for_turn(turn_id, &footer)
            });
        }
        self.assistant_block_id = None;
        self.push_turn_messages(&view.messages);
        self.status.stop();
        self.view.busy = false;
        self.push_history_block(footer);
        self.current_turn_id = None;
        self.commit_transcript_to_scrollback()
    }

    pub fn resize_view(
        &mut self,
        view: TerminalView<'_>,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        self.surface.resize(width, height)?;
        if self.stream.is_reasoning() {
            let width = width.saturating_sub(CONTENT_PREFIX_COLUMNS).max(1);
            self.stream.refresh_reasoning(width, self.tools_expanded);
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

    /// Toggle the global tool-output display mode: full output vs short
    /// preview. Applies to every tool block and persists across session
    /// switches (`Ctrl+o`).
    pub fn toggle_tool_expanded(&mut self) -> io::Result<()> {
        self.tools_expanded = !self.tools_expanded;
        // Committed blocks are baked into the scrollback surface; rebuilding
        // it re-renders them at their new height.
        if !self.history.is_empty() {
            let (width, height) = terminal_size()?;
            self.rebuild_scrollback_at(width, height)?;
        }
        if self.stream.is_reasoning() {
            let width = Self::markdown_width()?;
            self.stream.refresh_reasoning(width, self.tools_expanded);
        }
        self.redraw()
    }

    pub fn scroll_to_top(&mut self) -> io::Result<()> {
        self.scroll_top = Some(0);
        self.redraw()
    }

    pub fn scroll_to_bottom(&mut self) -> io::Result<()> {
        self.scroll_top = None;
        self.redraw()
    }

    pub fn refresh_status(&mut self) -> io::Result<()> {
        if !self.view.busy && self.subagents.is_empty() {
            return Ok(());
        }
        self.status.frame = self.status.frame.wrapping_add(1);
        // The periodic working refresh is the fallback flush point: it
        // redraws current state, so partial lines that never completed a
        // newline still appear here.
        self.refresh_stream_view()
    }

    /// Single render path for live streamed content: refresh the reasoning
    /// view (elapsed header + latest lines) and redraw the viewport.
    fn refresh_stream_view(&mut self) -> io::Result<()> {
        if self.stream.is_reasoning() {
            let width = Self::markdown_width()?;
            self.stream.refresh_reasoning(width, self.tools_expanded);
        }
        self.redraw()
    }

    /// Replace the displayed sub-agent snapshots and redraw when they changed.
    pub fn set_subagents(&mut self, subagents: Vec<SubagentSnapshot>) -> io::Result<()> {
        if self.subagents == subagents {
            return Ok(());
        }
        self.subagents = subagents;
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
        if let Some(id) = self.assistant_block_id.take() {
            if let Some(block) = self.transcript.iter_mut().find(|block| block.id() == id) {
                block.finalize_markdown();
            }
        }
    }

    fn commit_finished_stream(&mut self, finished: Option<FinishedThought>) {
        if let Some(FinishedThought {
            source,
            elapsed_seconds,
        }) = finished
        {
            self.push_thought_block(source, elapsed_seconds);
        }
    }

    fn reset_ui_state(&mut self) -> io::Result<()> {
        self.surface.reset()?;
        self.view = ViewState::default();
        self.usage = UsageState::default();
        self.reset_turn_state();
        Ok(())
    }

    fn reset_turn_state(&mut self) {
        self.status.reset();
        self.current_turn_id = None;
        self.stream.reset();
        self.assistant_block_id = None;
    }

    fn markdown_width() -> io::Result<u16> {
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
        let tools_expanded = self.tools_expanded;
        let committed = std::mem::take(&mut self.transcript);
        let result = self.synchronized(|terminal| {
            terminal.scroll_top = None;

            // Shrink the live viewport before inserting history so committed rows remain visible
            // directly above the composer instead of disappearing above a full-screen viewport.
            terminal.render_viewport(width, height)?;
            let inserted = insert_history_blocks(
                &mut terminal.surface,
                &committed,
                render_width,
                tools_expanded,
            )?;
            terminal.render_viewport(width, height)?;
            Ok(inserted)
        });
        match &result {
            Ok(inserted) => self.history.extend(committed.into_iter().take(*inserted)),
            Err(error) => {
                // Blocks written to the terminal before the failure stay
                // committed; only the untouched remainder returns to the
                // transcript so a retry never re-renders them.
                let inserted = inserted_blocks(error);
                self.history
                    .extend(committed.iter().take(inserted).cloned());
                self.transcript = committed.into_iter().skip(inserted).collect();
            }
        }
        result.map(|_| ())
    }

    fn rebuild_scrollback_at(&mut self, width: u16, height: u16) -> io::Result<()> {
        let render_width = viewport::drawable_width(width);
        let tools_expanded = self.tools_expanded;
        let history = std::mem::take(&mut self.history);
        let result = self.synchronized(|terminal| {
            terminal.surface.reset()?;
            terminal.render_viewport(width, height)?;
            let inserted = insert_history_blocks(
                &mut terminal.surface,
                &history,
                render_width,
                tools_expanded,
            )?;
            terminal.render_viewport(width, height)?;
            Ok(inserted)
        });
        match &result {
            Ok(_) => self.history = history,
            Err(error) => {
                // Blocks already written to the terminal before the failure are
                // not restored, otherwise the next commit would render them a
                // second time.
                let inserted = inserted_blocks(error);
                self.history = history.into_iter().skip(inserted).collect();
            }
        }
        result.map(|_| ())
    }

    fn scroll_up(&mut self, rows: u16, width: u16, height: u16) -> io::Result<()> {
        let frame = self.viewport_frame(width, height);
        if frame.max_scroll_top == 0 {
            return Ok(());
        }
        self.scroll_top = Some(frame.scroll_top.saturating_sub(rows));
        self.redraw_at(width, height)
    }

    fn scroll_down(&mut self, rows: u16, width: u16, height: u16) -> io::Result<()> {
        let frame = self.viewport_frame(width, height);
        let next = frame.scroll_top.saturating_add(rows);
        self.scroll_top = (next < frame.max_scroll_top).then_some(next);
        self.redraw_at(width, height)
    }

    fn synchronized<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> io::Result<T>,
    ) -> io::Result<T> {
        self.surface.begin_synchronized()?;
        let operation_result = operation(self);
        let finish_result = self.surface.end_synchronized();
        match (operation_result, finish_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    fn render_viewport(&mut self, width: u16, height: u16) -> io::Result<()> {
        let frame = self.viewport_frame(width, height);
        normalize_scroll_top(&mut self.scroll_top, frame.scroll_top);
        self.surface.render_frame(&frame)
    }

    const fn allocate_block_id(&mut self) -> u64 {
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

    fn push_tool_block(
        &mut self,
        name: String,
        arguments: Value,
        output: String,
        is_error: bool,
        file_change: Option<FileChange>,
    ) {
        if self
            .transcript
            .last_mut()
            .is_some_and(|block| block.try_append_tool(&name, &arguments, is_error))
        {
            return;
        }
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::tool(
            id,
            name,
            arguments,
            output,
            is_error,
            file_change,
        ));
    }

    fn push_restored_messages(&mut self, messages: &[Message]) {
        self.push_restored_messages_excluding(messages, &HashSet::new());
    }

    fn push_restored_messages_excluding(
        &mut self,
        messages: &[Message],
        excluded: &HashSet<MessageId>,
    ) {
        let tool_results = tool_results_map(messages);

        for message in messages
            .iter()
            .filter(|message| !excluded.contains(&message.id))
        {
            if matches!(&message.content, MessageContent::User(_)) {
                let turn_id = self.next_turn_id;
                self.next_turn_id = self.next_turn_id.saturating_add(1);
                self.current_turn_id = Some(turn_id);
            }
            self.push_turn_message(message, &tool_results, true);
        }
        self.current_turn_id = None;
    }

    fn push_restored_turns(&mut self, turns: &[TurnView]) {
        for turn in turns {
            let turn_id = self.next_turn_id;
            self.next_turn_id = self.next_turn_id.saturating_add(1);
            self.current_turn_id = Some(turn_id);
            self.push_restored_turn_messages(&turn.messages);
            if let Some(footer) = restored_turn_footer(&turn.result) {
                self.push_history_block(footer);
            }
        }
        self.current_turn_id = None;
    }

    fn push_restored_turn_messages(&mut self, messages: &[Message]) {
        let tool_results = tool_results_map(messages);
        for message in messages {
            self.push_turn_message(message, &tool_results, true);
        }
    }

    /// Project one settled turn's canonical messages into transcript blocks.
    /// When `render_user` is set (restored sessions) the user input is
    /// committed as a block; during a live turn it is skipped because the
    /// composer already committed it when the turn started (or when steering
    /// was accepted).
    fn push_turn_messages(&mut self, messages: &[Message]) {
        let tool_results = tool_results_map(messages);
        for message in messages {
            self.push_turn_message(message, &tool_results, false);
        }
    }

    fn push_turn_message(
        &mut self,
        message: &Message,
        tool_results: &HashMap<ToolCallId, ToolResultView<'_>>,
        render_user: bool,
    ) {
        match &message.content {
            MessageContent::User(contents) if render_user => {
                let text = contents
                    .iter()
                    .map(|content| match content {
                        Content::Text(text) => text.clone(),
                        Content::Image { media_type, .. } => format!("[image: {media_type}]"),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.push_history_block(HistoryBlock::user(&text));
            }
            MessageContent::User(_) | MessageContent::ToolResult { .. } => {}
            MessageContent::Assistant(blocks) => {
                self.push_assistant_blocks(blocks, tool_results);
            }
        }
    }

    fn push_assistant_blocks(
        &mut self,
        blocks: &[ContentBlock],
        tool_results: &HashMap<ToolCallId, ToolResultView<'_>>,
    ) {
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
                    let result = tool_results.get(id).copied().unwrap_or(ToolResultView {
                        is_error: true,
                        output: "tool result unavailable",
                        file_change: None,
                    });
                    self.push_tool_block(
                        name.clone(),
                        arguments.clone(),
                        result.output.to_string(),
                        result.is_error,
                        result.file_change.cloned(),
                    );
                }
                ContentBlock::Text(_) | ContentBlock::Thought { .. } => {}
            }
        }
    }

    fn viewport_frame(&self, width: u16, height: u16) -> viewport::ViewportFrame {
        let elapsed = format_elapsed(self.status.elapsed_seconds());
        let status_header = sanitize_single_line(&self.status.header);
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
            prompt_lines: &self.view.composer.lines,
            prompt_cursor_row: self.view.composer.cursor_row,
            prompt_cursor_column: self.view.composer.cursor_column,
            menu: self.view.menu.view(),
            model: &model,
            protocol: &protocol,
            working_dir: &self.session.working_dir,
            context_tokens: self.usage.context_tokens,
            context_limit: self.session.context_limit,
            tools_expanded: self.tools_expanded,
            subagents: &self.subagents,
        })
    }
}

/// Number of blocks that were rendered into the terminal before the first
/// failure. Carried inside the error so a partial insert can roll back only
/// the blocks that were never written.
#[derive(Debug)]
struct PartialInsert {
    inserted: usize,
    source: io::Error,
}

impl fmt::Display for PartialInsert {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed after inserting {} of {} history blocks",
            self.inserted, self.source
        )
    }
}

impl std::error::Error for PartialInsert {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<PartialInsert> for io::Error {
    fn from(partial: PartialInsert) -> Self {
        Self::new(partial.source.kind(), partial)
    }
}

fn inserted_blocks(error: &io::Error) -> usize {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<PartialInsert>())
        .map_or(0, |partial| partial.inserted)
}

/// Inserts the blocks into the terminal surface, returning the number of
/// blocks physically written. On failure, the error carries how many blocks
/// were already inserted so callers never re-commit already-rendered rows.
fn insert_history_blocks(
    surface: &mut InlineScreen,
    blocks: &[LiveBlock],
    render_width: u16,
    tools_expanded: bool,
) -> io::Result<usize> {
    for (index, block) in blocks.iter().enumerate() {
        let buffer = block.render(render_width, tools_expanded);
        if let Err(source) = surface.insert_buffer(&buffer, 1) {
            return Err(PartialInsert {
                inserted: index,
                source,
            }
            .into());
        }
        block.clear_render_cache();
    }
    Ok(blocks.len())
}

const fn status_dots(frame: usize) -> &'static str {
    const FRAMES: [&str; 4] = [".  ", ".. ", "...", ".. "];
    FRAMES[frame % FRAMES.len()]
}

const fn normalize_scroll_top(scroll_top: &mut Option<u16>, rendered_top: u16) {
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

#[derive(Clone, Copy)]
struct ToolResultView<'a> {
    is_error: bool,
    output: &'a str,
    file_change: Option<&'a FileChange>,
}

fn tool_results_map(messages: &[Message]) -> HashMap<ToolCallId, ToolResultView<'_>> {
    let mut results = HashMap::new();
    for message in messages {
        if let MessageContent::ToolResult {
            id,
            result,
            file_change,
            ..
        } = &message.content
        {
            let (is_error, output) = match result {
                Ok(output) => (false, output.as_str()),
                Err(error) => (true, error.as_str()),
            };
            results.insert(
                id.clone(),
                ToolResultView {
                    is_error,
                    output,
                    file_change: file_change.as_ref(),
                },
            );
        }
    }
    results
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

fn keep_during_cancellation(block: &LiveBlock, turn_id: u64, mode: CancellationMode) -> bool {
    match mode {
        CancellationMode::Interrupt => !block.is_unfinished_response_for_turn(turn_id),
        CancellationMode::Rollback => !block.is_streamed_for_turn(turn_id),
    }
}

fn turn_footer(result: &TurnResult, elapsed_seconds: u64, usage: TurnUsage) -> HistoryBlock {
    match result {
        TurnResult::Completed(StopReason::Aborted) => HistoryBlock::interrupted(),
        TurnResult::Completed(_) => HistoryBlock::worked(
            format_elapsed(elapsed_seconds),
            usage.input_tokens,
            usage.output_tokens,
            usage.generation_ms,
        ),
        TurnResult::Failed(error) | TurnResult::Interrupted(error) => HistoryBlock::error(error),
    }
}

fn restored_turn_footer(result: &TurnResult) -> Option<HistoryBlock> {
    match result {
        TurnResult::Completed(StopReason::Aborted) => Some(HistoryBlock::interrupted()),
        TurnResult::Completed(_) => None,
        TurnResult::Failed(error) | TurnResult::Interrupted(error) => {
            Some(HistoryBlock::error(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_completes_line_flags_newline_deltas() {
        assert!(!delta_completes_line("partial"));
        assert!(!delta_completes_line(""));
        assert!(delta_completes_line("line one\n"));
        assert!(delta_completes_line("line one\nline two"));
        assert!(delta_completes_line("\n"));
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
    fn usage_commits_turn_usage_for_the_worked_summary_without_touching_context() {
        let mut usage = UsageState::default();
        usage.set_context_tokens(500);
        let turn = UsageState::commit_turn_usage(Some(&Usage {
            input_tokens: 100,
            output_tokens: 20,
            generation_ms: 400,
            estimated: false,
        }));
        // The context size is updated separately from `set_context_tokens`;
        // committing turn usage only feeds the worked summary block.
        assert_eq!(usage.context_tokens, Some(500));
        assert_eq!(turn.input_tokens, 100);
        assert_eq!(turn.output_tokens, 20);
        assert_eq!(turn.generation_ms, 400);

        let turn = UsageState::commit_turn_usage(Some(&Usage {
            input_tokens: 150,
            output_tokens: 30,
            generation_ms: 600,
            estimated: true,
        }));
        assert_eq!(usage.context_tokens, Some(500));
        assert_eq!(turn.input_tokens, 150);
        assert_eq!(turn.output_tokens, 30);
        assert_eq!(turn.generation_ms, 600);

        let turn = UsageState::commit_turn_usage(None);
        assert_eq!(turn, TurnUsage::default());
        assert_eq!(usage.context_tokens, Some(500));
    }

    #[test]
    fn context_tokens_are_always_estimates_recorded_explicitly() {
        let mut usage = UsageState::default();
        usage.set_context_tokens(42_000);
        assert_eq!(usage.context_tokens, Some(42_000));
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

    #[test]
    fn rollback_cancellation_removes_finalized_text_from_the_live_projection() {
        let mut text = LiveBlock::assistant(1, String::new()).with_turn(Some(7));
        assert!(text.append_markdown_source("Inspecting before the tool call."));
        text.finalize_markdown();

        assert!(keep_during_cancellation(
            &text,
            7,
            CancellationMode::Interrupt
        ));
        assert!(!keep_during_cancellation(
            &text,
            7,
            CancellationMode::Rollback
        ));

        let prompt = LiveBlock::history(2, HistoryBlock::user("inspect")).with_turn(Some(7));
        assert!(keep_during_cancellation(
            &prompt,
            7,
            CancellationMode::Rollback
        ));
    }

    #[test]
    fn completed_turns_end_with_work_summary() {
        let footer = turn_footer(
            &TurnResult::Completed(ash_core::StopReason::EndTurn),
            3,
            TurnUsage {
                input_tokens: 10,
                output_tokens: 2,
                generation_ms: 100,
            },
        );

        assert_eq!(footer, HistoryBlock::worked("3s".to_string(), 10, 2, 100));
    }

    #[test]
    fn aborted_turns_use_a_distinct_interruption_footer() {
        assert_eq!(
            turn_footer(
                &TurnResult::Completed(StopReason::Aborted),
                3,
                TurnUsage::default()
            ),
            HistoryBlock::interrupted()
        );
        assert_eq!(
            restored_turn_footer(&TurnResult::Completed(StopReason::Aborted)),
            Some(HistoryBlock::interrupted())
        );
    }

    #[test]
    fn failed_and_interrupted_turns_end_with_their_error() {
        assert_eq!(
            turn_footer(
                &TurnResult::Failed("invalid response".into()),
                3,
                TurnUsage::default()
            ),
            HistoryBlock::error("invalid response")
        );
        assert_eq!(
            turn_footer(
                &TurnResult::Interrupted("session closed".into()),
                3,
                TurnUsage::default(),
            ),
            HistoryBlock::error("session closed")
        );
    }

    #[test]
    fn restored_turns_replay_non_success_terminal_states() {
        assert_eq!(
            restored_turn_footer(&TurnResult::Completed(ash_core::StopReason::EndTurn)),
            None
        );
        assert_eq!(
            restored_turn_footer(&TurnResult::Failed("invalid response".into())),
            Some(HistoryBlock::error("invalid response"))
        );
        assert_eq!(
            restored_turn_footer(&TurnResult::Interrupted("session closed".into())),
            Some(HistoryBlock::error("session closed"))
        );
    }

    #[test]
    fn durable_tool_results_retain_file_changes_for_replay() {
        let id = ToolCallId::from_provider("call-1");
        let messages = [Message {
            id: MessageId::new(),
            role: ash_core::Role::User,
            content: MessageContent::ToolResult {
                id: id.clone(),
                result: Ok("updated".to_string()),
                attachments: Vec::new(),
                file_change: Some(FileChange::Update {
                    path: PathBuf::from("src/main.rs"),
                    unified_diff: "-old\n+new\n".to_string(),
                }),
            },
        }];

        let results = tool_results_map(&messages);
        let result = results.get(&id).expect("tool result");

        assert!(!result.is_error);
        assert_eq!(result.output, "updated");
        assert_eq!(
            result.file_change.map(FileChange::path),
            Some(Path::new("src/main.rs"))
        );
    }
}
