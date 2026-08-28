use std::{fmt, io, time::Instant};

use ash_core::{Item, ToolCallId, Turn, TurnId, TurnResult};
use crossterm::terminal;
use serde_json::Value;

use crate::{
    app::AppState, history_block::HistoryBlock, inline_surface::InlineScreen,
    live_block::LiveBlock, status_line::format_elapsed, viewport,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum RenderPlan {
    #[default]
    None,
    Redraw,
    Commit,
    Rebuild,
    RebuildAll,
}

impl RenderPlan {
    #[must_use]
    pub(crate) const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::None, plan) | (plan, Self::None) => plan,
            (Self::RebuildAll, _) | (_, Self::RebuildAll) => Self::RebuildAll,
            (Self::Commit, Self::Rebuild) | (Self::Rebuild, Self::Commit) => Self::RebuildAll,
            (Self::Commit, _) | (_, Self::Commit) => Self::Commit,
            (Self::Rebuild, _) | (_, Self::Rebuild) => Self::Rebuild,
            (Self::Redraw, Self::Redraw) => Self::Redraw,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct StatusState {
    started_at: Option<Instant>,
}

impl StatusState {
    pub(crate) fn set_active(&mut self, active: bool) {
        match (self.started_at.is_some(), active) {
            (false, true) => {
                self.started_at = Some(Instant::now());
            }
            (true, false) => self.reset(),
            _ => {}
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn elapsed_seconds(&self) -> u64 {
        self.started_at
            .map_or(0, |started| started.elapsed().as_secs())
    }
}

#[derive(Debug, Default)]
pub(crate) struct BlockStore {
    blocks: Vec<LiveBlock>,
    committed_len: usize,
}

impl BlockStore {
    fn clear(&mut self) {
        self.blocks.clear();
        self.committed_len = 0;
    }

    const fn committed_is_empty(&self) -> bool {
        self.committed_len == 0
    }

    pub(crate) fn pending(&self) -> &[LiveBlock] {
        &self.blocks[self.committed_len..]
    }

    fn pending_mut(&mut self) -> &mut [LiveBlock] {
        &mut self.blocks[self.committed_len..]
    }

    fn pending_is_empty(&self) -> bool {
        self.committed_len == self.blocks.len()
    }

    fn push_pending(&mut self, block: LiveBlock) {
        self.blocks.push(block);
    }

    fn pending_position(&self, predicate: impl FnMut(&LiveBlock) -> bool) -> Option<usize> {
        self.pending().iter().position(predicate)
    }

    fn remove_pending(&mut self, position: usize) {
        self.blocks.remove(self.committed_len + position);
    }

    fn retain_pending(&mut self, predicate: impl FnMut(&LiveBlock) -> bool) {
        let mut pending = self.blocks.split_off(self.committed_len);
        pending.retain(predicate);
        self.blocks.extend(pending);
    }

    fn remove_streamed_turn(&mut self, current: Option<TurnId>, canonical: TurnId) {
        let Some(turn_id) = current.filter(|id| *id == canonical) else {
            return;
        };
        self.retain_pending(|block| !block.is_streamed_for_turn(turn_id));
    }

    fn remove_turn(&mut self, turn_id: TurnId) {
        let removed_committed = self.blocks[..self.committed_len]
            .iter()
            .filter(|block| block.belongs_to_turn(turn_id))
            .count();
        self.blocks.retain(|block| !block.belongs_to_turn(turn_id));
        self.committed_len -= removed_committed;
    }

    fn take_pending(&mut self) -> Vec<LiveBlock> {
        self.blocks.split_off(self.committed_len)
    }

    fn finish_commit(&mut self, pending: Vec<LiveBlock>, inserted: usize) {
        let inserted = inserted.min(pending.len());
        self.blocks.extend(pending);
        self.committed_len += inserted;
    }

    fn mark_all_committed(&mut self) {
        self.committed_len = self.blocks.len();
    }

    fn take_committed(&mut self) -> Vec<LiveBlock> {
        let pending = self.blocks.split_off(self.committed_len);
        self.committed_len = 0;
        std::mem::replace(&mut self.blocks, pending)
    }

    fn rebuild_succeeded(&mut self, mut committed: Vec<LiveBlock>) {
        let committed_len = committed.len();
        committed.append(&mut self.blocks);
        self.blocks = committed;
        self.committed_len = committed_len;
    }

    fn rebuild_failed(&mut self, committed: Vec<LiveBlock>, inserted: usize) {
        let mut remaining = committed.into_iter().skip(inserted).collect::<Vec<_>>();
        self.committed_len = remaining.len();
        remaining.append(&mut self.blocks);
        self.blocks = remaining;
    }
}

pub struct TerminalUi {
    surface: InlineScreen,
}

/// True when a streamed delta contains a newline, meaning a complete line is
/// available to display. Newline-complete deltas flush immediately; partial
/// runs are picked up by the periodic status refresh.
fn delta_completes_line(delta: &str) -> bool {
    delta.contains('\n')
}

impl TerminalUi {
    pub fn enter() -> io::Result<Self> {
        Ok(Self {
            surface: InlineScreen::enter()?,
        })
    }
}

impl AppState {
    pub fn welcome(&mut self) -> RenderPlan {
        self.enqueue_welcome();
        RenderPlan::Commit
    }

    fn enqueue_welcome(&mut self) {
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::welcome(id, self.working_dir.clone()));
    }

    pub fn command_output(&mut self, message: &str) -> RenderPlan {
        self.input.clear();
        self.scroll_top = None;
        self.push_history_block(HistoryBlock::info(message));
        RenderPlan::Commit
    }

    pub fn show_session_status(&mut self) -> RenderPlan {
        let status = format!(
            "Model: {}\nProtocol: {}\nDirectory: {}",
            self.model,
            self.protocol,
            self.working_dir.display()
        );
        self.command_output(&status)
    }

    pub fn command_error(&mut self, message: &str) -> RenderPlan {
        self.input.clear();
        self.scroll_top = None;
        self.push_history_block(HistoryBlock::error(message));
        RenderPlan::Commit
    }

    pub fn finish_compaction(&mut self, changed: bool) -> RenderPlan {
        let message = if changed {
            "Model context was compacted. Full history remains visible.".to_string()
        } else {
            "Model context is already compact; no new summary was created.".to_string()
        };
        self.command_output(&message)
    }

    pub fn start_new_session(&mut self) -> RenderPlan {
        self.begin_fresh_viewport();
        self.enqueue_welcome();
        self.blocks.mark_all_committed();
        RenderPlan::Rebuild
    }

    fn begin_fresh_viewport(&mut self) {
        self.blocks.clear();
        self.scroll_top = None;
        self.menu.close_picker();
        self.reset_turn_state();
    }

    pub fn restore_session(&mut self) -> RenderPlan {
        self.begin_fresh_viewport();
        self.enqueue_welcome();
        for turn in self.conversation.turns().to_vec() {
            self.current_turn_id = Some(turn.id);
            self.push_turn(&turn, true);
            if let Some(footer) = restored_turn_footer(&turn.result) {
                self.push_history_block(footer);
            }
        }
        self.current_turn_id = None;
        self.blocks.mark_all_committed();
        RenderPlan::Rebuild
    }
}

impl TerminalUi {
    pub(crate) fn apply_plan(&mut self, state: &mut AppState, plan: RenderPlan) -> io::Result<()> {
        if plan == RenderPlan::None {
            return Ok(());
        }
        let (width, height) = terminal_size()?;
        let width = width.max(1);
        let height = height.max(1);
        match plan {
            RenderPlan::RebuildAll => {
                state.blocks.mark_all_committed();
                self.rebuild_scrollback_at(state, width, height)
            }
            RenderPlan::Rebuild => self.rebuild_scrollback_at(state, width, height),
            RenderPlan::Commit => self.commit_transcript_to_scrollback_at(state, width, height),
            RenderPlan::Redraw => self.redraw_at(state, width, height),
            RenderPlan::None => Ok(()),
        }
    }

    pub fn composer_text_width() -> io::Result<u16> {
        Ok(viewport::composer_text_width(terminal_size()?.0))
    }
}

impl AppState {
    pub const fn current_turn_id(&self) -> Option<TurnId> {
        self.current_turn_id
    }

    pub const fn track_turn(&mut self, turn_id: TurnId) {
        self.current_turn_id = Some(turn_id);
    }

    pub fn commit_input(&mut self, turn_id: TurnId, input: &str) -> RenderPlan {
        self.current_turn_id = Some(turn_id);
        self.scroll_top = None;
        self.commit_user_message(input)
    }

    pub fn commit_exit(&mut self, input: &str) {
        self.finish_live_output();
        self.current_turn_id = None;
        let _ = self.commit_user_message(input);
    }

    fn commit_user_message(&mut self, input: &str) -> RenderPlan {
        self.input.clear();
        self.menu.close_picker();
        self.push_history_block(HistoryBlock::user(input));
        RenderPlan::Redraw
    }

    pub fn turn_started(&mut self) -> RenderPlan {
        RenderPlan::Redraw
    }

    /// Append one text delta to the turn's streaming assistant block,
    /// creating it on first use. Deltas go straight into the block's
    /// incremental markdown renderer; nothing is queued for a later frame.
    fn append_assistant(&mut self, source: &str) {
        if source.is_empty() {
            return;
        }
        let id = self.assistant_block_id.unwrap_or_else(|| {
            self.finish_reasoning();
            let id = self.allocate_block_id();
            self.push_block(LiveBlock::assistant(id, String::new()));
            self.assistant_block_id = Some(id);
            id
        });
        if let Some(block) = self
            .blocks
            .pending_mut()
            .iter_mut()
            .find(|block| block.id() == id)
        {
            block.append_markdown_source(source);
        }
    }

    /// Append one text delta and redraw immediately when it completes a line,
    /// so output reads line by line instead of character by character.
    /// Partial runs are picked up by the periodic status refresh.
    pub fn text(&mut self, text: &str) -> RenderPlan {
        self.append_assistant(text);
        if delta_completes_line(text) {
            RenderPlan::Redraw
        } else {
            RenderPlan::None
        }
    }

    /// Append one reasoning delta to the scrolling preview. Newline-complete
    /// deltas redraw immediately (like text), so the preview reads line by
    /// line; partial runs are picked up by the periodic status refresh.
    /// Reasoning always precedes the assistant text: the protocol emits
    /// thinking blocks before text output, so no late-reasoning handling is
    /// needed here.
    pub fn thinking(&mut self, text: &str) -> RenderPlan {
        let id = self.reasoning_block_id.unwrap_or_else(|| {
            let id = self.allocate_block_id();
            self.push_block(LiveBlock::reasoning(id));
            self.reasoning_block_id = Some(id);
            id
        });
        if let Some(block) = self
            .blocks
            .pending_mut()
            .iter_mut()
            .find(|block| block.id() == id)
        {
            block.append_reasoning_source(text);
        }
        if delta_completes_line(text) {
            RenderPlan::Redraw
        } else {
            RenderPlan::None
        }
    }

    pub fn tool_started(&mut self, id: ToolCallId, name: String, arguments: Value) -> RenderPlan {
        self.finish_live_output();
        let block_id = self.allocate_block_id();
        self.push_block(LiveBlock::running_tool(block_id, id, name, arguments));
        RenderPlan::Redraw
    }

    pub fn tool_finished(&mut self, id: &ToolCallId, output: &str, is_error: bool) -> RenderPlan {
        if let Some(position) = self
            .blocks
            .pending_position(|block| block.is_running_tool(id))
        {
            self.blocks.pending_mut()[position].finish_tool(id, output.to_string(), is_error);
        }
        RenderPlan::Redraw
    }

    pub fn discard_turn(&mut self, turn_id: TurnId) -> RenderPlan {
        self.finish_live_output();
        self.blocks.remove_turn(turn_id);
        self.scroll_top = None;
        self.reset_turn_state();
        RenderPlan::Rebuild
    }

    /// Discard unfinished streamed output after the user cancels.
    pub fn prepare_cancellation(&mut self) {
        self.reasoning_block_id = None;
        if let Some(turn_id) = self.current_turn_id {
            self.blocks
                .retain_pending(|block| !block.is_unfinished_response_for_turn(turn_id));
        }
        self.assistant_block_id = None;
    }

    pub fn error(&mut self, error: &str) -> RenderPlan {
        self.finish_live_output();
        self.push_history_block(HistoryBlock::error(error));
        if self.current_turn_id.is_some() {
            RenderPlan::Redraw
        } else {
            RenderPlan::Commit
        }
    }

    /// Commit a settled turn: replace the streamed preview with the canonical
    /// projection of the turn's messages, then move it into the scrollback.
    pub fn commit_turn(&mut self, turn: &Turn) -> RenderPlan {
        self.finish_live_output();
        let elapsed_seconds = self.status.elapsed_seconds();
        let footer = turn_footer(turn, elapsed_seconds);
        self.blocks
            .remove_streamed_turn(self.current_turn_id, turn.id);
        self.assistant_block_id = None;
        self.push_turn(turn, false);
        self.push_history_block(footer);
        self.current_turn_id = None;
        RenderPlan::Commit
    }
}

impl TerminalUi {
    pub fn resize_view(&mut self, state: &mut AppState, width: u16, height: u16) -> io::Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        self.surface.resize(width, height)?;
        self.rebuild_scrollback_at(state, width, height)
    }

    pub fn scroll_page_up(&mut self, state: &mut AppState) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        let rows = state.viewport_frame(width, height).page_rows;
        self.scroll_up(state, rows, width, height)
    }

    pub fn scroll_page_down(&mut self, state: &mut AppState) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        let rows = state.viewport_frame(width, height).page_rows;
        self.scroll_down(state, rows, width, height)
    }
}

impl AppState {
    /// Toggle the global tool-output display mode: full output vs short
    /// preview. Applies to every visible tool output and persists across
    /// session switches (`Ctrl+o`).
    pub fn toggle_tool_expanded(&mut self) -> RenderPlan {
        self.tools_expanded = !self.tools_expanded;
        // Committed blocks are baked into the scrollback surface; rebuilding
        // it re-renders them at their new height.
        if self.blocks.committed_is_empty() {
            RenderPlan::Redraw
        } else {
            RenderPlan::Rebuild
        }
    }

    pub fn scroll_to_top(&mut self) -> RenderPlan {
        self.scroll_top = Some(0);
        RenderPlan::Redraw
    }

    pub fn scroll_to_bottom(&mut self) -> RenderPlan {
        self.scroll_top = None;
        RenderPlan::Redraw
    }

    pub fn refresh_status(&mut self) -> RenderPlan {
        if !self.operation.activity_view().is_active() && self.subagents.is_empty() {
            return RenderPlan::None;
        }
        // The periodic working refresh is the fallback flush point: it
        // updates elapsed time and flushes partial lines that never completed
        // a newline.
        RenderPlan::Redraw
    }
}

impl TerminalUi {
    pub fn leave(&mut self, state: &mut AppState) -> io::Result<()> {
        state.finish_live_output();
        state.current_turn_id = None;
        self.commit_transcript_to_scrollback(state)?;
        self.surface.leave_screen()
    }
}

impl AppState {
    fn finish_live_output(&mut self) {
        self.finish_reasoning();
        if let Some(id) = self.assistant_block_id.take() {
            if let Some(block) = self
                .blocks
                .pending_mut()
                .iter_mut()
                .find(|block| block.id() == id)
            {
                block.finalize_markdown();
            }
        }
    }

    fn finish_reasoning(&mut self) {
        let Some(id) = self.reasoning_block_id.take() else {
            return;
        };
        let Some(position) = self.blocks.pending_position(|block| block.id() == id) else {
            return;
        };
        if !self.blocks.pending_mut()[position].finish_reasoning() {
            self.blocks.remove_pending(position);
        }
    }

    fn reset_turn_state(&mut self) {
        self.status.reset();
        self.current_turn_id = None;
        self.reasoning_block_id = None;
        self.assistant_block_id = None;
    }
}

impl TerminalUi {
    fn redraw_at(&mut self, state: &mut AppState, width: u16, height: u16) -> io::Result<()> {
        self.synchronized(|terminal| terminal.render_viewport(state, width, height))
    }

    fn commit_transcript_to_scrollback(&mut self, state: &mut AppState) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.commit_transcript_to_scrollback_at(state, width, height)
    }

    fn commit_transcript_to_scrollback_at(
        &mut self,
        state: &mut AppState,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        if state.blocks.pending_is_empty() {
            return self.redraw_at(state, width, height);
        }

        let render_width = viewport::drawable_width(width);
        let tools_expanded = state.tools_expanded;
        let pending = state.blocks.take_pending();
        let result = self.synchronized(|terminal| {
            state.scroll_top = None;

            // Shrink the live viewport before inserting history so committed rows remain visible
            // directly above the composer instead of disappearing above a full-screen viewport.
            terminal.render_viewport(state, width, height)?;
            let inserted = insert_history_blocks(
                &mut terminal.surface,
                &pending,
                render_width,
                tools_expanded,
            )?;
            terminal.render_viewport(state, width, height)?;
            Ok(inserted)
        });
        match &result {
            Ok(inserted) => state.blocks.finish_commit(pending, *inserted),
            Err(error) => {
                // Blocks written to the terminal before the failure stay
                // committed; only the untouched remainder stays pending so a
                // retry never re-renders them.
                state.blocks.finish_commit(pending, inserted_blocks(error));
            }
        }
        result.map(|_| ())
    }

    fn rebuild_scrollback_at(
        &mut self,
        state: &mut AppState,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let render_width = viewport::drawable_width(width);
        let tools_expanded = state.tools_expanded;
        let committed = state.blocks.take_committed();
        let result = self.synchronized(|terminal| {
            terminal.surface.reset()?;
            terminal.render_viewport(state, width, height)?;
            let inserted = insert_history_blocks(
                &mut terminal.surface,
                &committed,
                render_width,
                tools_expanded,
            )?;
            terminal.render_viewport(state, width, height)?;
            Ok(inserted)
        });
        match &result {
            Ok(_) => state.blocks.rebuild_succeeded(committed),
            Err(error) => {
                // Blocks already written to the terminal before the failure are
                // not restored, otherwise the next commit would render them a
                // second time.
                state
                    .blocks
                    .rebuild_failed(committed, inserted_blocks(error));
            }
        }
        result.map(|_| ())
    }

    fn scroll_up(
        &mut self,
        state: &mut AppState,
        rows: u16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let frame = state.viewport_frame(width, height);
        if frame.max_scroll_top == 0 {
            return Ok(());
        }
        state.scroll_top = Some(frame.scroll_top.saturating_sub(rows));
        self.redraw_at(state, width, height)
    }

    fn scroll_down(
        &mut self,
        state: &mut AppState,
        rows: u16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let frame = state.viewport_frame(width, height);
        let next = frame.scroll_top.saturating_add(rows);
        state.scroll_top = (next < frame.max_scroll_top).then_some(next);
        self.redraw_at(state, width, height)
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

    fn render_viewport(&mut self, state: &mut AppState, width: u16, height: u16) -> io::Result<()> {
        let frame = state.viewport_frame(width, height);
        normalize_scroll_top(&mut state.scroll_top, frame.scroll_top);
        self.surface.render_frame(&frame)
    }
}

impl AppState {
    const fn allocate_block_id(&mut self) -> u64 {
        let id = self.next_block_id;
        self.next_block_id = self.next_block_id.saturating_add(1);
        id
    }

    fn push_block(&mut self, block: LiveBlock) {
        self.blocks
            .push_pending(block.with_turn(self.current_turn_id));
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
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::tool(id, name, arguments, output, is_error));
    }

    /// Project one settled turn. Live input has already been committed by the
    /// composer, while restored input must be rendered here.
    fn push_turn(&mut self, turn: &Turn, render_input: bool) {
        if render_input {
            self.push_history_block(HistoryBlock::user(&turn.input.text()));
        }
        for item in turn.items() {
            match item {
                Item::Text(text) if !text.is_empty() => {
                    self.push_assistant_block(text.clone());
                }
                Item::Thought {
                    text,
                    elapsed_seconds,
                } if !text.is_empty() => {
                    self.push_thought_block(text.clone(), *elapsed_seconds);
                }
                Item::ToolCall(call) => {
                    let (output, is_error) = match &call.result {
                        Ok(output) => (output.text.clone(), false),
                        Err(error) => (error.clone(), true),
                    };
                    self.push_tool_block(
                        call.name.clone(),
                        call.arguments.clone(),
                        output,
                        is_error,
                    );
                }
                Item::Text(_) | Item::Thought { .. } => {}
            }
        }
    }

    fn viewport_frame(&self, width: u16, height: u16) -> viewport::ViewportFrame {
        viewport::render(self, width, height)
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
/// source blocks physically written. Consecutive silent tools of the same
/// name collapse into one inserted row; a failure still reports the
/// source-block count so a retry never re-renders already-written history.
fn insert_history_blocks(
    surface: &mut InlineScreen,
    blocks: &[LiveBlock],
    render_width: u16,
    tools_expanded: bool,
) -> io::Result<usize> {
    let mut inserted = 0;
    for group in viewport::grouped_transcript(blocks, render_width, tools_expanded) {
        if let Err(source) = surface.insert_buffer(&group.buffer, 1) {
            return Err(PartialInsert { inserted, source }.into());
        }
        inserted += group.source.len();
        for block in group.source {
            block.clear_render_cache();
        }
    }
    Ok(inserted)
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

fn turn_footer(turn: &Turn, elapsed_seconds: u64) -> HistoryBlock {
    match &turn.result {
        TurnResult::Cancelled => HistoryBlock::interrupted(),
        TurnResult::Stopped(_) | TurnResult::Truncated => HistoryBlock::worked(
            format_elapsed(elapsed_seconds),
            turn.stats,
            turn.tool_calls().count(),
        ),
        TurnResult::Failed(error) => HistoryBlock::error(error),
    }
}

fn restored_turn_footer(result: &TurnResult) -> Option<HistoryBlock> {
    match result {
        TurnResult::Cancelled => Some(HistoryBlock::interrupted()),
        TurnResult::Stopped(_) | TurnResult::Truncated => None,
        TurnResult::Failed(error) => Some(HistoryBlock::error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_turn(n: u128) -> TurnId {
        TurnId::from_u128(n)
    }

    #[test]
    fn render_plan_preserves_orthogonal_terminal_operations() {
        assert_eq!(
            RenderPlan::Commit.merge(RenderPlan::Rebuild),
            RenderPlan::RebuildAll
        );
        assert_eq!(
            RenderPlan::None.merge(RenderPlan::Redraw),
            RenderPlan::Redraw
        );
    }

    #[test]
    fn block_store_preserves_the_unwritten_suffix_after_a_partial_commit() {
        let mut blocks = BlockStore::default();
        let first = LiveBlock::history(1, HistoryBlock::info("first"));
        let second = LiveBlock::history(2, HistoryBlock::info("second"));
        blocks.push_pending(first);
        blocks.push_pending(second);

        let pending = blocks.take_pending();
        blocks.finish_commit(pending, 1);

        assert_eq!(blocks.committed_len, 1);
        assert_eq!(blocks.blocks.len(), 2);
        assert_eq!(blocks.blocks[0].id(), 1);
        assert_eq!(blocks.pending()[0].id(), 2);
    }

    #[test]
    fn block_store_rebuild_success_preserves_pending_suffix() {
        let mut blocks = BlockStore::default();
        blocks.push_pending(LiveBlock::history(1, HistoryBlock::info("committed")));
        let pending = blocks.take_pending();
        blocks.finish_commit(pending, 1);
        blocks.push_pending(LiveBlock::history(2, HistoryBlock::info("pending")));

        let committed = blocks.take_committed();
        blocks.rebuild_succeeded(committed);

        assert_eq!(blocks.committed_len, 1);
        assert_eq!(blocks.blocks.len(), 2);
        assert_eq!(blocks.pending()[0].id(), 2);
    }

    #[test]
    fn block_store_rebuild_failure_preserves_unwritten_suffix_and_pending() {
        let mut blocks = BlockStore::default();
        for id in [1, 2] {
            blocks.push_pending(LiveBlock::history(id, HistoryBlock::info("committed")));
        }
        let pending = blocks.take_pending();
        blocks.finish_commit(pending, 2);
        blocks.push_pending(LiveBlock::history(3, HistoryBlock::info("pending")));

        let committed = blocks.take_committed();
        blocks.rebuild_failed(committed, 1);

        assert_eq!(blocks.committed_len, 1);
        assert!(blocks.committed_len <= blocks.blocks.len());
        assert_eq!(blocks.blocks[0].id(), 2);
        assert_eq!(blocks.pending()[0].id(), 3);
    }

    #[test]
    fn block_store_removes_a_turn_from_committed_and_pending_blocks() {
        let mut blocks = BlockStore::default();
        blocks.push_pending(
            LiveBlock::history(1, HistoryBlock::user("question")).with_turn(Some(test_turn(7))),
        );
        let pending = blocks.take_pending();
        blocks.finish_commit(pending, 1);
        blocks.push_pending(
            LiveBlock::assistant(2, "answer".to_string()).with_turn(Some(test_turn(7))),
        );
        blocks.push_pending(LiveBlock::history(3, HistoryBlock::info("status")));

        blocks.remove_turn(test_turn(7));

        assert_eq!(blocks.committed_len, 0);
        assert_eq!(blocks.pending().len(), 1);
        assert_eq!(blocks.pending()[0].id(), 3);
    }

    #[test]
    fn delta_completes_line_flags_newline_deltas() {
        assert!(!delta_completes_line("partial"));
        assert!(!delta_completes_line(""));
        assert!(delta_completes_line("line one\n"));
        assert!(delta_completes_line("line one\nline two"));
        assert!(delta_completes_line("\n"));
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
    fn cancellation_keeps_finalized_text_until_the_turn_settles() {
        let mut text = LiveBlock::assistant(1, String::new()).with_turn(Some(test_turn(7)));
        assert!(text.append_markdown_source("Inspecting before the tool call."));
        text.finalize_markdown();

        assert!(!text.is_unfinished_response_for_turn(test_turn(7)));

        let prompt =
            LiveBlock::history(2, HistoryBlock::user("inspect")).with_turn(Some(test_turn(7)));
        assert!(!prompt.is_unfinished_response_for_turn(test_turn(7)));
    }

    #[test]
    fn cancellation_removes_a_running_tool_until_it_has_a_result() {
        let call_id = ToolCallId::from_provider("call-1");
        let mut tool = LiveBlock::running_tool(
            1,
            call_id.clone(),
            "bash".to_string(),
            serde_json::json!({"command": "sleep 10"}),
        )
        .with_turn(Some(test_turn(7)));

        assert!(tool.is_unfinished_response_for_turn(test_turn(7)));
        assert!(tool.finish_tool(&call_id, "done".to_string(), false));
        assert!(!tool.is_unfinished_response_for_turn(test_turn(7)));
    }

    #[test]
    fn completed_turns_end_with_work_summary() {
        let turn = Turn {
            id: test_turn(7),
            input: ash_core::Input::user("question"),
            steps: vec![ash_core::Step {
                items: vec![Item::ToolCall(ash_core::ToolCall {
                    id: ToolCallId::from_provider("call"),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                    result: Ok("done".into()),
                })],
            }],
            result: TurnResult::Stopped(ash_core::StopReason::EndTurn),
            stats: ash_core::TurnStats {
                input_tokens: 10,
                output_tokens: 2,
                generation_ms: 100,
            },
        };
        let footer = turn_footer(&turn, 3);

        assert_eq!(
            footer,
            HistoryBlock::worked(
                "3s".to_string(),
                ash_core::TurnStats {
                    input_tokens: 10,
                    output_tokens: 2,
                    generation_ms: 100,
                },
                1,
            )
        );
    }

    #[test]
    fn cancelled_turns_use_a_distinct_interruption_footer() {
        let turn = Turn {
            id: test_turn(7),
            input: ash_core::Input::user("question"),
            steps: Vec::new(),
            result: TurnResult::Cancelled,
            stats: ash_core::TurnStats::default(),
        };
        assert_eq!(turn_footer(&turn, 3), HistoryBlock::interrupted());
        assert_eq!(
            restored_turn_footer(&TurnResult::Cancelled),
            Some(HistoryBlock::interrupted())
        );
    }

    #[test]
    fn failed_and_interrupted_turns_end_with_their_error() {
        let turn = Turn {
            id: test_turn(7),
            input: ash_core::Input::user("question"),
            steps: Vec::new(),
            result: TurnResult::Failed("invalid response".into()),
            stats: ash_core::TurnStats::default(),
        };
        assert_eq!(
            turn_footer(&turn, 3),
            HistoryBlock::error("invalid response")
        );
    }

    #[test]
    fn restored_turns_replay_non_success_terminal_states() {
        assert_eq!(
            restored_turn_footer(&TurnResult::Stopped(ash_core::StopReason::EndTurn)),
            None
        );
        assert_eq!(
            restored_turn_footer(&TurnResult::Failed("invalid response".into())),
            Some(HistoryBlock::error("invalid response"))
        );
        assert_eq!(
            restored_turn_footer(&TurnResult::Cancelled),
            Some(HistoryBlock::interrupted())
        );
    }
}
