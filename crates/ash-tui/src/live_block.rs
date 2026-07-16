use std::path::PathBuf;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

use crate::{
    history_block::HistoryBlock,
    markdown::{render_markdown, TextStyle},
    palette::Rgb,
    tool_display::tool_call_summary,
    welcome_card::{welcome_card, WelcomeLine, WelcomeStyle},
};

const BULLET_PREFIX_COLUMNS: u16 = 2;

/// A complete piece of output Ash still owns and can therefore re-render.
///
/// Once an entry is emitted to stdout it is dropped from the live queue. The
/// terminal's scrollback is deliberately not modeled here: it belongs to the
/// terminal and cannot be safely reflowed after a resize.
#[derive(Clone, Debug)]
pub(crate) struct LiveBlock {
    id: u64,
    turn_id: Option<u64>,
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
        Self::new(id, LiveBlockKind::Welcome(working_dir))
    }

    pub(crate) fn history(id: u64, block: HistoryBlock) -> Self {
        Self::new(id, LiveBlockKind::History(block))
    }

    pub(crate) fn markdown(id: u64, source: String, reasoning: bool) -> Self {
        Self::new(id, LiveBlockKind::Markdown { source, reasoning })
    }

    pub(crate) fn tool(id: u64, name: String, arguments: Value, is_error: bool) -> Self {
        Self::new(
            id,
            LiveBlockKind::Tool {
                name,
                arguments,
                is_error,
            },
        )
    }

    fn new(id: u64, kind: LiveBlockKind) -> Self {
        Self {
            id,
            turn_id: None,
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
    let lines = welcome_card(width, working_dir);
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    for (index, line) in lines.iter().take(usize::from(height)).enumerate() {
        let Ok(y) = u16::try_from(index) else {
            break;
        };
        buffer.set_line(0, y, &styled_welcome_line(line), width);
    }
    buffer
}

fn styled_welcome_line(line: &WelcomeLine) -> Line<'static> {
    let frame_style = Style::default().fg(Color::Cyan);
    let content_style = match line.style {
        WelcomeStyle::Frame => frame_style,
        WelcomeStyle::Subtitle => Style::default().add_modifier(Modifier::DIM),
        WelcomeStyle::Logo | WelcomeStyle::Title => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    };

    let Some(content) = line
        .text
        .strip_prefix('│')
        .and_then(|content| content.strip_suffix('│'))
    else {
        return Line::styled(line.text.clone(), content_style);
    };

    Line::from(vec![
        Span::styled("│", frame_style),
        Span::styled(content.to_string(), content_style),
        Span::styled("│", frame_style),
    ])
}

fn render_markdown_block(source: &str, reasoning: bool, width: u16) -> Buffer {
    let content_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
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
    let detail_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
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
    fn blocks_keep_turn_ownership() {
        let block = LiveBlock::history(1, HistoryBlock::info("done")).with_turn(Some(7));

        assert!(block.belongs_to_turn(7));
    }

    #[test]
    fn welcome_frame_stays_cyan_around_dim_content() {
        let buffer = render_welcome(40, std::path::Path::new("/workspace/ash"));
        let subtitle_row = (0..buffer.area.height)
            .find(|&row| {
                (0..buffer.area.width)
                    .filter_map(|column| buffer.cell((column, row)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("TERMINAL CODING AGENT")
            })
            .expect("subtitle row");

        assert_eq!(
            buffer.cell((0, subtitle_row)).expect("left frame").fg,
            Color::Cyan
        );
        assert_eq!(
            buffer
                .cell((buffer.area.width - 1, subtitle_row))
                .expect("right frame")
                .fg,
            Color::Cyan
        );
        assert!(buffer
            .cell((2, subtitle_row))
            .expect("subtitle")
            .modifier
            .contains(Modifier::DIM));
    }
}
