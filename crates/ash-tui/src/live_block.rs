use std::path::PathBuf;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

use crate::{
    block_layout::StackFlow,
    history_block::HistoryBlock,
    markdown::{render_markdown, TextStyle},
    palette::Rgb,
    tool_display::tool_call_summary,
    welcome_card::{welcome_card, WelcomeStyle},
};

/// A complete piece of output Ash still owns and can therefore re-render.
///
/// Once an entry is emitted to stdout it is dropped from the live queue. The
/// terminal's scrollback is deliberately not modeled here: it belongs to the
/// terminal and cannot be safely reflowed after a resize.
#[derive(Clone, Debug)]
pub(crate) struct LiveBlock {
    id: u64,
    turn_id: Option<u64>,
    flow: StackFlow,
    kind: LiveBlockKind,
}

#[derive(Clone, Debug)]
enum LiveBlockKind {
    Welcome(PathBuf),
    History(HistoryBlock),
    Markdown {
        source: String,
        reasoning: bool,
    },
    Tool {
        name: String,
        arguments: Value,
        is_error: bool,
    },
}

impl LiveBlock {
    pub(crate) fn welcome(id: u64, working_dir: PathBuf) -> Self {
        Self::new(id, StackFlow::Block, LiveBlockKind::Welcome(working_dir))
    }

    pub(crate) fn history(id: u64, block: HistoryBlock) -> Self {
        Self::new(id, StackFlow::Block, LiveBlockKind::History(block))
    }

    pub(crate) fn markdown(id: u64, source: String, reasoning: bool) -> Self {
        Self::new(
            id,
            StackFlow::Block,
            LiveBlockKind::Markdown { source, reasoning },
        )
    }

    pub(crate) fn tool(id: u64, name: String, arguments: Value, is_error: bool) -> Self {
        Self::new(
            id,
            StackFlow::Block,
            LiveBlockKind::Tool {
                name,
                arguments,
                is_error,
            },
        )
    }

    fn new(id: u64, flow: StackFlow, kind: LiveBlockKind) -> Self {
        Self {
            id,
            turn_id: None,
            flow,
            kind,
        }
    }

    pub(crate) fn with_turn(mut self, turn_id: Option<u64>) -> Self {
        self.turn_id = turn_id;
        self
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn belongs_to_turn(&self, turn_id: u64) -> bool {
        self.turn_id == Some(turn_id)
    }

    pub(crate) const fn flow(&self) -> StackFlow {
        self.flow
    }

    pub(crate) fn append_markdown_source(&mut self, source: String) -> bool {
        let LiveBlockKind::Markdown {
            source: current, ..
        } = &mut self.kind
        else {
            return false;
        };
        current.push_str(&source);
        true
    }

    pub(crate) fn render(&self, width: u16, composer_background: Option<Rgb>) -> Buffer {
        match &self.kind {
            LiveBlockKind::Welcome(working_dir) => render_welcome(width, working_dir),
            LiveBlockKind::History(block) => block.render(width, composer_background),
            LiveBlockKind::Markdown { source, reasoning } => {
                render_markdown_block(source, *reasoning, width)
            }
            LiveBlockKind::Tool {
                name,
                arguments,
                is_error,
            } => render_tool(name, arguments, *is_error, width),
        }
    }
}

fn render_welcome(width: u16, working_dir: &std::path::Path) -> Buffer {
    let lines = welcome_card(width.saturating_add(1), working_dir);
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    for (index, line) in lines.iter().take(usize::from(height)).enumerate() {
        let Ok(y) = u16::try_from(index) else {
            break;
        };
        let style = match line.style {
            WelcomeStyle::Frame | WelcomeStyle::Subtitle => {
                Style::default().add_modifier(Modifier::DIM)
            }
            WelcomeStyle::Logo | WelcomeStyle::Title => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        };
        buffer.set_line(0, y, &Line::styled(line.text.clone(), style), width);
    }
    buffer
}

fn render_markdown_block(source: &str, reasoning: bool, width: u16) -> Buffer {
    let content_width = width.saturating_sub(2).max(1);
    let mut lines = render_markdown(source, content_width);
    if reasoning {
        for line in &mut lines {
            line.patch_style(TextStyle::dim_italic());
        }
    }
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    for (index, rendered) in lines.iter().take(usize::from(height)).enumerate() {
        let Ok(y) = u16::try_from(index) else {
            break;
        };
        let mut spans = vec![if index == 0 {
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM))
        } else {
            Span::raw("  ")
        }];
        spans.extend(rendered.ratatui_line().spans);
        buffer.set_line(0, y, &Line::from(spans), width);
    }
    buffer
}

fn render_tool(name: &str, arguments: &Value, is_error: bool, width: u16) -> Buffer {
    let detail_width = width.saturating_sub(2).max(1);
    let (action, detail) = tool_call_summary(name, arguments, is_error, detail_width);
    let bullet_style = if is_error {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![Span::styled("•", bullet_style), Span::raw(" ")];
    spans.push(Span::styled(
        action,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if !detail.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::raw(detail));
    }
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), 1));
    buffer.set_line(0, 0, &Line::from(spans), width);
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_blocks_reflow_at_the_current_width() {
        let block = LiveBlock::markdown(1, "a long line that must wrap".to_string(), false);

        let narrow = block.render(10, None);
        let wide = block.render(40, None);

        assert!(narrow.area.height > wide.area.height);
    }

    #[test]
    fn blocks_keep_turn_ownership_separate_from_layout_flow() {
        let block = LiveBlock::history(1, HistoryBlock::info("done")).with_turn(Some(7));

        assert!(block.belongs_to_turn(7));
        assert_eq!(block.flow(), StackFlow::Block);
    }
}
