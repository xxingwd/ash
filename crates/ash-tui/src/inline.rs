use std::{
    collections::{HashMap, HashSet},
    fmt, io,
    path::{Path, PathBuf},
    time::Instant,
};

use ash_core::{
    Content, ContentBlock, ForkPoint, Message, MessageContent, MessageId, SessionSummary,
    SessionView, StopReason, ToolCallId, TurnId, TurnResult, TurnView, Usage,
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
    operation::ActivityView,
    scrollback::sanitize_single_line,
    slash_command::CommandCompletion,
    status_line::format_elapsed,
    viewport::{self, ViewportInput, COMPOSER_TEXT_COLUMN},
    SubagentView,
};

const TERMINAL_SAFE_COLUMN: u16 = 1;

#[derive(Debug)]
struct SessionUiInfo {
    protocol: String,
    model: String,
    working_dir: PathBuf,
    context_limit: Option<u64>,
}

impl SessionUiInfo {
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RenderPlan {
    sync_view: bool,
    redraw: bool,
    commit: bool,
    rebuild: bool,
}

impl RenderPlan {
    pub(crate) const NONE: Self = Self {
        sync_view: false,
        redraw: false,
        commit: false,
        rebuild: false,
    };
    pub(crate) const REDRAW: Self = Self {
        sync_view: true,
        redraw: true,
        commit: false,
        rebuild: false,
    };
    pub(crate) const COMMIT: Self = Self {
        sync_view: true,
        redraw: false,
        commit: true,
        rebuild: false,
    };
    pub(crate) const REBUILD: Self = Self {
        sync_view: true,
        redraw: false,
        commit: false,
        rebuild: true,
    };

    #[must_use]
    pub(crate) const fn merge(self, other: Self) -> Self {
        Self {
            sync_view: self.sync_view || other.sync_view,
            redraw: self.redraw || other.redraw,
            commit: self.commit || other.commit,
            rebuild: self.rebuild || other.rebuild,
        }
    }

    pub(crate) const fn should_sync_view(self) -> bool {
        self.sync_view
    }

    const fn needs_io(self) -> bool {
        self.redraw || self.commit || self.rebuild
    }
}

#[derive(Clone, Copy)]
pub struct TerminalView<'a> {
    pub(crate) input: &'a InputState,
    pub(crate) menu: MenuView<'a>,
    pub(crate) activity: ActivityView,
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
    started_at: Option<Instant>,
    frame: usize,
}

impl StatusState {
    fn set_active(&mut self, active: bool) {
        match (self.started_at.is_some(), active) {
            (false, true) => {
                self.started_at = Some(Instant::now());
                self.frame = 0;
            }
            (true, false) => self.reset(),
            _ => {}
        }
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
        items: Vec<SessionSummary>,
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
    activity: ActivityView,
}

#[derive(Debug, Default)]
struct BlockStore {
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

    fn pending(&self) -> &[LiveBlock] {
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

    fn pending_last_mut(&mut self) -> Option<&mut LiveBlock> {
        (self.blocks.len() > self.committed_len)
            .then(|| self.blocks.last_mut())
            .flatten()
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

    fn latest_turn_id(&self, current: Option<TurnId>) -> Option<TurnId> {
        current.or_else(|| self.blocks.iter().rev().find_map(LiveBlock::turn_id))
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
    session: SessionUiInfo,
    view: ViewState,
    blocks: BlockStore,
    /// Global display mode for tool output: true renders full output, false
    /// renders a short preview. Persists across session switches (`Ctrl+o`).
    tools_expanded: bool,
    /// Display-only snapshots of the session's sub-agents, refreshed by the
    /// app from the collaboration control's watch channel.
    subagents: Vec<SubagentView>,
    scroll_top: Option<u16>,
    next_block_id: u64,
    status: StatusState,
    usage: UsageState,
    current_turn_id: Option<TurnId>,
    reasoning_block_id: Option<u64>,
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
            session: SessionUiInfo::new(protocol, model, working_dir, context_limit),
            view: ViewState::default(),
            blocks: BlockStore::default(),
            tools_expanded: false,
            subagents: Vec::new(),
            scroll_top: None,
            next_block_id: 1,
            status: StatusState::default(),
            usage: UsageState::default(),
            current_turn_id: None,
            reasoning_block_id: None,
            assistant_block_id: None,
        })
    }

    pub fn welcome(&mut self) -> RenderPlan {
        self.enqueue_welcome();
        RenderPlan::COMMIT
    }

    fn enqueue_welcome(&mut self) {
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::welcome(id, self.session.working_dir.clone()));
    }

    pub fn command_output(&mut self, message: &str) -> RenderPlan {
        self.view.composer.clear();
        self.scroll_top = None;
        self.push_history_block(HistoryBlock::info(message));
        RenderPlan::COMMIT
    }

    pub fn show_session_status(&mut self) -> RenderPlan {
        let status = format!(
            "Model: {}\nProtocol: {}\nDirectory: {}",
            self.session.model,
            self.session.protocol,
            self.session.working_dir.display()
        );
        self.command_output(&status)
    }

    pub fn command_error(&mut self, message: &str) -> RenderPlan {
        self.view.composer.clear();
        self.scroll_top = None;
        self.push_history_block(HistoryBlock::error(message));
        RenderPlan::COMMIT
    }

    pub fn finish_compaction(
        &mut self,
        before_tokens: u64,
        after_tokens: u64,
        dropped_messages: u64,
    ) -> RenderPlan {
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

    pub fn record_automatic_compaction(&mut self, after_tokens: u64) -> RenderPlan {
        self.usage.set_context_tokens(after_tokens);
        RenderPlan::REDRAW
    }

    pub fn start_new_session(&mut self) -> RenderPlan {
        self.begin_fresh_viewport();
        self.enqueue_welcome();
        self.blocks.mark_all_committed();
        RenderPlan::REBUILD
    }

    fn begin_fresh_viewport(&mut self) {
        self.blocks.clear();
        self.scroll_top = None;
        self.view = ViewState::default();
        self.usage = UsageState::default();
        self.reset_turn_state();
    }

    pub fn rollback_turn(&mut self) -> RenderPlan {
        let turn_id = self.blocks.latest_turn_id(self.current_turn_id);
        if let Some(turn_id) = turn_id {
            self.blocks.remove_turn(turn_id);
        }
        self.scroll_top = None;
        self.reset_turn_state();
        RenderPlan::REBUILD
    }

    pub fn restore_session(&mut self, session: &SessionView) -> RenderPlan {
        self.begin_fresh_viewport();
        self.enqueue_welcome();
        if session.turns.is_empty() {
            self.push_restored_messages(&session.messages);
        } else {
            let turn_message_ids = session
                .turns
                .iter()
                .flat_map(|turn| turn.messages.iter().map(|message| message.id))
                .collect::<HashSet<_>>();
            self.push_restored_messages_excluding(&session.messages, &turn_message_ids);
            self.push_restored_turns(&session.turns);
        }
        // `begin_fresh_viewport` resets usage; restore the estimated context
        // size so the status line reflects current occupancy before any API
        // usage is reported for the new history.
        if let Some(tokens) = session.context_tokens {
            self.usage.set_context_tokens(tokens);
        }
        self.blocks.mark_all_committed();
        RenderPlan::REBUILD
    }

    pub(crate) fn apply_plan(
        &mut self,
        view: TerminalView<'_>,
        plan: RenderPlan,
    ) -> io::Result<()> {
        if !plan.sync_view && !plan.needs_io() {
            return Ok(());
        }
        let (width, height) = terminal_size()?;
        let width = width.max(1);
        let height = height.max(1);
        if plan.sync_view {
            self.apply_view(view, width);
        }
        match (plan.rebuild, plan.commit, plan.redraw) {
            (true, true, _) => {
                self.blocks.mark_all_committed();
                self.rebuild_scrollback_at(width, height)
            }
            (true, false, _) => self.rebuild_scrollback_at(width, height),
            (false, true, _) => self.commit_transcript_to_scrollback_at(width, height),
            (false, false, true) => self.redraw_at(width, height),
            (false, false, false) => Ok(()),
        }
    }

    fn apply_view(&mut self, view: TerminalView<'_>, width: u16) {
        let input = view.input.view(composer_text_width(width));
        self.view.composer.lines = input.lines;
        self.view.composer.cursor_row = input.cursor_row;
        self.view.composer.cursor_column = input.cursor_column;
        self.view.menu.set(view.menu);
        self.view.activity = view.activity;
        self.status.set_active(view.activity.is_active());
    }

    pub fn composer_text_width() -> io::Result<u16> {
        Ok(composer_text_width(terminal_size()?.0))
    }

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

    pub fn commit_steer(&mut self, input: &str) -> RenderPlan {
        self.finish_live_output();
        self.scroll_top = None;
        self.commit_user_message(input)
    }

    pub fn commit_exit(&mut self, input: &str) {
        self.finish_live_output();
        self.current_turn_id = None;
        let _ = self.commit_user_message(input);
    }

    fn commit_user_message(&mut self, input: &str) -> RenderPlan {
        self.view.composer.clear();
        self.view.menu.clear();
        self.push_history_block(HistoryBlock::user(input));
        RenderPlan::REDRAW
    }

    pub const fn agent_started(&self) -> RenderPlan {
        RenderPlan::REDRAW
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
            RenderPlan::REDRAW
        } else {
            RenderPlan::NONE
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
            RenderPlan::REDRAW
        } else {
            RenderPlan::NONE
        }
    }

    pub fn tool_started(&mut self, id: ToolCallId, name: String, arguments: Value) -> RenderPlan {
        self.finish_live_output();
        if absorbs_running_read(self.blocks.pending(), &name, &arguments) {
            return RenderPlan::NONE;
        }
        let block_id = self.allocate_block_id();
        self.push_block(LiveBlock::running_tool(block_id, id, name, arguments));
        RenderPlan::REDRAW
    }

    pub fn tool_finished(
        &mut self,
        id: &ToolCallId,
        name: &str,
        arguments: &Value,
        output: &str,
        is_error: bool,
    ) -> RenderPlan {
        if let Some(position) = self
            .blocks
            .pending_position(|block| block.is_running_tool(id))
        {
            self.blocks.pending_mut()[position].finish_tool(id, output.to_string(), is_error);
            if position > 0
                && self.blocks.pending_mut()[position - 1]
                    .try_append_tool(name, arguments, is_error)
            {
                self.blocks.remove_pending(position);
            }
        } else {
            self.push_tool_block(
                name.to_string(),
                arguments.clone(),
                output.to_string(),
                is_error,
            );
        }
        RenderPlan::REDRAW
    }

    /// Discard unfinished streamed output after the user cancels. Completed
    /// tool results stay until the settled `TurnView` arrives; that view is
    /// what decides whether the turn is kept or rolled back.
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
            RenderPlan::REDRAW
        } else {
            RenderPlan::COMMIT
        }
    }

    /// Commit a settled turn: replace the streamed preview with the canonical
    /// projection of the turn's messages, then move it into the scrollback.
    pub fn commit_turn(&mut self, view: &TurnView) -> RenderPlan {
        self.finish_live_output();
        let elapsed_seconds = self.status.elapsed_seconds();
        if let Some(tokens) = view.context_tokens {
            self.usage.set_context_tokens(tokens);
        }
        let usage = UsageState::commit_turn_usage(view.usage.as_ref());
        let footer = turn_footer(&view.result, elapsed_seconds, usage);
        self.blocks
            .remove_streamed_turn(self.current_turn_id, view.id);
        self.assistant_block_id = None;
        self.push_turn_messages(&view.messages);
        self.push_history_block(footer);
        self.current_turn_id = None;
        RenderPlan::COMMIT
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
    pub fn toggle_tool_expanded(&mut self) -> RenderPlan {
        self.tools_expanded = !self.tools_expanded;
        // Committed blocks are baked into the scrollback surface; rebuilding
        // it re-renders them at their new height.
        if self.blocks.committed_is_empty() {
            RenderPlan::REDRAW
        } else {
            RenderPlan::REBUILD
        }
    }

    pub fn scroll_to_top(&mut self) -> io::Result<()> {
        self.scroll_top = Some(0);
        self.redraw()
    }

    pub fn scroll_to_bottom(&mut self) -> io::Result<()> {
        self.scroll_top = None;
        self.redraw()
    }

    pub fn refresh_status(&mut self) -> RenderPlan {
        if !self.view.activity.is_active() && self.subagents.is_empty() {
            return RenderPlan::NONE;
        }
        self.status.frame = self.status.frame.wrapping_add(1);
        // The periodic working refresh is the fallback flush point: it
        // redraws current state, so partial lines that never completed a
        // newline still appear here.
        RenderPlan::REDRAW
    }

    /// Replace the displayed sub-agent snapshots.
    pub fn set_subagents(&mut self, subagents: Vec<SubagentView>) -> RenderPlan {
        if self.subagents == subagents {
            return RenderPlan::NONE;
        }
        self.subagents = subagents;
        RenderPlan::REDRAW
    }

    pub fn leave(&mut self) -> io::Result<()> {
        self.finish_live_output();
        self.current_turn_id = None;
        self.commit_transcript_to_scrollback()?;
        self.surface.leave_screen()
    }

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

    fn redraw(&mut self) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.redraw_at(width, height)
    }

    fn redraw_at(&mut self, width: u16, height: u16) -> io::Result<()> {
        self.synchronized(|terminal| terminal.render_viewport(width, height))
    }

    fn commit_transcript_to_scrollback(&mut self) -> io::Result<()> {
        let (width, height) = terminal_size()?;
        self.commit_transcript_to_scrollback_at(width, height)
    }

    fn commit_transcript_to_scrollback_at(&mut self, width: u16, height: u16) -> io::Result<()> {
        if self.blocks.pending_is_empty() {
            return self.redraw_at(width, height);
        }

        let render_width = viewport::drawable_width(width);
        let tools_expanded = self.tools_expanded;
        let pending = self.blocks.take_pending();
        let result = self.synchronized(|terminal| {
            terminal.scroll_top = None;

            // Shrink the live viewport before inserting history so committed rows remain visible
            // directly above the composer instead of disappearing above a full-screen viewport.
            terminal.render_viewport(width, height)?;
            let inserted = insert_history_blocks(
                &mut terminal.surface,
                &pending,
                render_width,
                tools_expanded,
            )?;
            terminal.render_viewport(width, height)?;
            Ok(inserted)
        });
        match &result {
            Ok(inserted) => self.blocks.finish_commit(pending, *inserted),
            Err(error) => {
                // Blocks written to the terminal before the failure stay
                // committed; only the untouched remainder stays pending so a
                // retry never re-renders them.
                self.blocks.finish_commit(pending, inserted_blocks(error));
            }
        }
        result.map(|_| ())
    }

    fn rebuild_scrollback_at(&mut self, width: u16, height: u16) -> io::Result<()> {
        let render_width = viewport::drawable_width(width);
        let tools_expanded = self.tools_expanded;
        let committed = self.blocks.take_committed();
        let result = self.synchronized(|terminal| {
            terminal.surface.reset()?;
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
            Ok(_) => self.blocks.rebuild_succeeded(committed),
            Err(error) => {
                // Blocks already written to the terminal before the failure are
                // not restored, otherwise the next commit would render them a
                // second time.
                self.blocks
                    .rebuild_failed(committed, inserted_blocks(error));
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
        if self
            .blocks
            .pending_last_mut()
            .is_some_and(|block| block.try_append_tool(&name, &arguments, is_error))
        {
            return;
        }
        let id = self.allocate_block_id();
        self.push_block(LiveBlock::tool(id, name, arguments, output, is_error));
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
                self.current_turn_id = Some(TurnId::new());
            }
            self.push_turn_message(message, &tool_results, true);
        }
        self.current_turn_id = None;
    }

    fn push_restored_turns(&mut self, turns: &[TurnView]) {
        for turn in turns {
            self.current_turn_id = Some(turn.id);
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
            MessageContent::User(_)
            | MessageContent::System(_)
            | MessageContent::ToolResult { .. } => {}
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
                    });
                    self.push_tool_block(
                        name.clone(),
                        arguments.clone(),
                        result.output.to_string(),
                        result.is_error,
                    );
                }
                ContentBlock::Text(_) | ContentBlock::Thought { .. } => {}
            }
        }
    }

    fn viewport_frame(&self, width: u16, height: u16) -> viewport::ViewportFrame {
        let elapsed = format_elapsed(self.status.elapsed_seconds());
        let (busy, interruptible, status_header) = match self.view.activity {
            ActivityView::Idle => (false, false, ""),
            ActivityView::Active {
                header,
                interruptible,
            } => (true, interruptible, header),
        };
        let status_header = sanitize_single_line(status_header);
        let model = sanitize_single_line(&self.session.model);
        let protocol = sanitize_single_line(&self.session.protocol);
        viewport::render(ViewportInput {
            terminal_width: width,
            terminal_height: height,
            transcript: self.blocks.pending(),
            scroll_top: self.scroll_top,
            busy,
            interruptible,
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
}

fn tool_results_map(messages: &[Message]) -> HashMap<ToolCallId, ToolResultView<'_>> {
    let mut results = HashMap::new();
    for message in messages {
        if let MessageContent::ToolResult { id, result, .. } = &message.content {
            let (is_error, output) = match result {
                Ok(output) => (false, output.as_str()),
                Err(error) => (true, error.as_str()),
            };
            results.insert(id.clone(), ToolResultView { is_error, output });
        }
    }
    results
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

fn absorbs_running_read(transcript: &[LiveBlock], name: &str, arguments: &Value) -> bool {
    transcript
        .last()
        .is_some_and(|block| block.can_group_read(name, arguments))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_turn(n: u128) -> TurnId {
        TurnId::from_u128(n)
    }

    #[test]
    fn render_plan_preserves_orthogonal_terminal_operations() {
        let plan = RenderPlan::COMMIT.merge(RenderPlan::REBUILD);
        assert!(plan.commit);
        assert!(plan.rebuild);
        assert!(plan.should_sync_view());

        let no_op = RenderPlan::NONE;
        assert!(!no_op.should_sync_view());
        assert!(!no_op.needs_io());
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
        let mut blocks = BlockStore::default();
        for block in [
            LiveBlock::history(1, HistoryBlock::user("question")).with_turn(Some(test_turn(7))),
            LiveBlock::assistant(2, "answer".to_string()).with_turn(Some(test_turn(7))),
            LiveBlock::history(3, HistoryBlock::info("status")),
        ] {
            blocks.push_pending(block);
        }
        let pending = blocks.take_pending();
        blocks.finish_commit(pending, 3);

        assert_eq!(blocks.latest_turn_id(None), Some(test_turn(7)));
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
    fn durable_tool_results_retain_output_for_replay() {
        let id = ToolCallId::from_provider("call-1");
        let messages = [Message::tool_result(
            id.clone(),
            Ok("updated".to_string()),
            Vec::new(),
        )];

        let results = tool_results_map(&messages);
        let result = results.get(&id).expect("tool result");

        assert!(!result.is_error);
        assert_eq!(result.output, "updated");
    }

    #[test]
    fn consecutive_reads_do_not_open_a_running_row() {
        let first = LiveBlock::tool(
            1,
            "read".to_string(),
            serde_json::json!({"path": "/workspace/src/inline.rs"}),
            String::new(),
            false,
        );
        let second = LiveBlock::tool(
            2,
            "bash".to_string(),
            serde_json::json!({"command": "pwd"}),
            String::new(),
            false,
        );

        assert!(absorbs_running_read(
            std::slice::from_ref(&first),
            "read",
            &serde_json::json!({"path": "/workspace/src/viewport.rs"}),
        ));
        assert!(!absorbs_running_read(
            &[first, second],
            "read",
            &serde_json::json!({"path": "/workspace/src/viewport.rs"}),
        ));
    }
}
