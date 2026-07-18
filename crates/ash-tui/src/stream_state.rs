use std::time::Instant;

use ratatui::style::{Modifier, Style};

use crate::{
    markdown::{render_markdown, RenderedLine},
    scrollback::sanitize_terminal_text,
};

const REASONING_VIEW_ROWS: usize = 4;

#[derive(Debug, Default)]
pub(crate) struct StreamState {
    mode: StreamMode,
    dirty: bool,
}

#[derive(Debug, Default)]
enum StreamMode {
    #[default]
    Idle,
    Assistant {
        pending: String,
        block_id: Option<u64>,
    },
    Reasoning {
        source: String,
        started_at: Instant,
        lines: Vec<RenderedLine>,
    },
}

pub(crate) enum FinishedStream {
    Assistant {
        pending: String,
        block_id: Option<u64>,
    },
    Thought {
        elapsed_seconds: u64,
    },
}

pub(crate) enum StreamRefresh {
    Assistant {
        pending: String,
        block_id: Option<u64>,
    },
    Reasoning,
}

impl StreamState {
    pub(crate) fn start_assistant(&mut self) -> Option<FinishedStream> {
        if matches!(self.mode, StreamMode::Assistant { .. }) {
            return None;
        }
        let finished = self.finish();
        self.mode = StreamMode::Assistant {
            pending: String::new(),
            block_id: None,
        };
        finished
    }

    pub(crate) fn start_reasoning(&mut self) -> Option<FinishedStream> {
        if matches!(self.mode, StreamMode::Reasoning { .. }) {
            return None;
        }
        let finished = self.finish();
        self.mode = StreamMode::Reasoning {
            source: String::new(),
            started_at: Instant::now(),
            lines: Vec::new(),
        };
        finished
    }

    pub(crate) fn push_assistant(&mut self, delta: &str) {
        let StreamMode::Assistant { pending, .. } = &mut self.mode else {
            return;
        };
        pending.push_str(&sanitize_terminal_text(delta));
        self.dirty = true;
    }

    pub(crate) fn push_reasoning(&mut self, delta: &str) {
        let StreamMode::Reasoning { source, .. } = &mut self.mode else {
            return;
        };
        source.push_str(&sanitize_terminal_text(delta));
        self.dirty = true;
    }

    pub(crate) fn refresh_reasoning(&mut self, width: u16) {
        let StreamMode::Reasoning {
            source,
            started_at,
            lines,
        } = &mut self.mode
        else {
            return;
        };
        *lines = render_reasoning_view(source, started_at.elapsed().as_secs(), width);
    }

    pub(crate) fn take_refresh(&mut self) -> Option<StreamRefresh> {
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        match &mut self.mode {
            StreamMode::Assistant { pending, block_id } if !pending.is_empty() => {
                Some(StreamRefresh::Assistant {
                    pending: std::mem::take(pending),
                    block_id: *block_id,
                })
            }
            StreamMode::Reasoning { .. } => Some(StreamRefresh::Reasoning),
            StreamMode::Idle | StreamMode::Assistant { .. } => None,
        }
    }

    pub(crate) fn set_assistant_block_id(&mut self, id: u64) {
        if let StreamMode::Assistant { block_id, .. } = &mut self.mode {
            *block_id = Some(id);
        }
    }

    pub(crate) fn clear_block_id(&mut self, id: u64) {
        if let StreamMode::Assistant { block_id, .. } = &mut self.mode {
            if *block_id == Some(id) {
                *block_id = None;
            }
        }
    }

    pub(crate) fn active_lines(&self) -> &[RenderedLine] {
        match &self.mode {
            StreamMode::Reasoning { lines, .. } => lines,
            StreamMode::Idle | StreamMode::Assistant { .. } => &[],
        }
    }

    pub(crate) fn is_reasoning(&self) -> bool {
        matches!(self.mode, StreamMode::Reasoning { .. })
    }

    pub(crate) fn finish(&mut self) -> Option<FinishedStream> {
        self.dirty = false;
        match std::mem::take(&mut self.mode) {
            StreamMode::Idle => None,
            StreamMode::Assistant { pending, block_id } => {
                Some(FinishedStream::Assistant { pending, block_id })
            }
            StreamMode::Reasoning {
                source, started_at, ..
            } if !source.trim().is_empty() => Some(FinishedStream::Thought {
                elapsed_seconds: started_at.elapsed().as_secs(),
            }),
            StreamMode::Reasoning { .. } => None,
        }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

pub(crate) fn format_elapsed(elapsed_seconds: u64) -> String {
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

fn render_reasoning_view(source: &str, elapsed_seconds: u64, width: u16) -> Vec<RenderedLine> {
    let mut header = render_markdown(
        &format!("Thinking ({})", format_elapsed(elapsed_seconds)),
        width,
    );
    for line in &mut header {
        line.patch_style(Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC));
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
        line.patch_style(Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC));
    }
    let keep_from = body.len().saturating_sub(remaining_rows);
    header.extend(body.into_iter().skip(keep_from));
    header
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
        for line in &lines {
            for span in line.ratatui_line().spans {
                assert!(span.style.add_modifier.contains(Modifier::DIM));
                assert!(span.style.add_modifier.contains(Modifier::ITALIC));
                assert!(!span.style.add_modifier.contains(Modifier::BOLD));
            }
        }
    }

    #[test]
    fn stream_modes_cannot_overlap() {
        let mut stream = StreamState::default();
        assert!(stream.start_assistant().is_none());
        stream.push_assistant("answer");
        let finished = stream.start_reasoning();

        assert!(matches!(
            finished,
            Some(FinishedStream::Assistant { pending, .. }) if pending == "answer"
        ));
        assert!(stream.is_reasoning());
        assert!(stream.active_lines().is_empty());
    }
}
