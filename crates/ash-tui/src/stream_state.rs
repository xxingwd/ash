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
        source: String,
        emitted_lines: usize,
        first_line_emitted: bool,
        tail: Vec<RenderedLine>,
    },
    Reasoning {
        source: String,
        started_at: Instant,
        lines: Vec<RenderedLine>,
    },
}

pub(crate) enum FinishedStream {
    Assistant {
        lines: Vec<RenderedLine>,
        first_line: bool,
    },
    Thought {
        elapsed_seconds: u64,
    },
}

pub(crate) enum StreamRefresh {
    Assistant {
        lines: Vec<RenderedLine>,
        first_line: bool,
    },
    Reasoning,
}

impl StreamState {
    pub(crate) fn start_assistant(&mut self, width: u16) -> Option<FinishedStream> {
        if matches!(self.mode, StreamMode::Assistant { .. }) {
            return None;
        }
        let finished = self.finish(width);
        self.mode = StreamMode::Assistant {
            source: String::new(),
            emitted_lines: 0,
            first_line_emitted: false,
            tail: Vec::new(),
        };
        finished
    }

    pub(crate) fn start_reasoning(&mut self, width: u16) -> Option<FinishedStream> {
        if matches!(self.mode, StreamMode::Reasoning { .. }) {
            return None;
        }
        let finished = self.finish(width);
        self.mode = StreamMode::Reasoning {
            source: String::new(),
            started_at: Instant::now(),
            lines: Vec::new(),
        };
        finished
    }

    pub(crate) fn push_assistant(&mut self, delta: &str) {
        let StreamMode::Assistant { source, .. } = &mut self.mode else {
            return;
        };
        source.push_str(&sanitize_terminal_text(delta));
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

    pub(crate) fn refresh_assistant(&mut self, width: u16) {
        let StreamMode::Assistant {
            source,
            emitted_lines,
            tail,
            ..
        } = &mut self.mode
        else {
            return;
        };
        let rendered = render_assistant_source(source, width);
        *emitted_lines = (*emitted_lines).min(rendered.len());
        *tail = rendered[*emitted_lines..].to_vec();
    }

    pub(crate) fn take_refresh(&mut self, width: u16) -> Option<StreamRefresh> {
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        match &mut self.mode {
            StreamMode::Assistant {
                source,
                emitted_lines,
                first_line_emitted,
                tail,
            } if !source.is_empty() => {
                let rendered = render_assistant_source(source, width);
                let stable_lines = stable_prefix_len(source, width, rendered.len());
                let stable_lines = stable_lines.max(*emitted_lines).min(rendered.len());
                let newly_stable = rendered[*emitted_lines..stable_lines].to_vec();
                *emitted_lines = stable_lines;
                *tail = rendered[stable_lines..].to_vec();
                let first_line = !*first_line_emitted;
                if !newly_stable.is_empty() {
                    *first_line_emitted = true;
                }
                Some(StreamRefresh::Assistant {
                    lines: newly_stable,
                    first_line,
                })
            }
            StreamMode::Reasoning { .. } => Some(StreamRefresh::Reasoning),
            StreamMode::Idle | StreamMode::Assistant { .. } => None,
        }
    }

    pub(crate) fn active_lines(&self) -> &[RenderedLine] {
        match &self.mode {
            StreamMode::Assistant { tail, .. } => tail,
            StreamMode::Reasoning { lines, .. } => lines,
            StreamMode::Idle => &[],
        }
    }

    pub(crate) fn active_starts_stream(&self) -> bool {
        match &self.mode {
            StreamMode::Assistant {
                first_line_emitted, ..
            } => !first_line_emitted,
            StreamMode::Reasoning { .. } => true,
            StreamMode::Idle => false,
        }
    }

    pub(crate) fn is_reasoning(&self) -> bool {
        matches!(self.mode, StreamMode::Reasoning { .. })
    }

    pub(crate) fn finish(&mut self, width: u16) -> Option<FinishedStream> {
        self.dirty = false;
        match std::mem::take(&mut self.mode) {
            StreamMode::Idle => None,
            StreamMode::Assistant {
                source,
                emitted_lines,
                first_line_emitted,
                ..
            } => {
                let rendered = render_assistant_source(&source, width);
                Some(FinishedStream::Assistant {
                    lines: rendered[emitted_lines.min(rendered.len())..].to_vec(),
                    first_line: !first_line_emitted,
                })
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

fn render_assistant_source(source: &str, width: u16) -> Vec<RenderedLine> {
    render_markdown(source, width)
}

fn stable_prefix_len(source: &str, width: u16, rendered_len: usize) -> usize {
    if source.contains("|---") || source.contains("| ---") {
        return 0;
    }
    let Some(end) = source.rfind('\n') else {
        return 0;
    };
    render_assistant_source(&source[..=end], width)
        .len()
        .min(rendered_len)
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
        assert!(stream.start_assistant(80).is_none());
        stream.push_assistant("answer");
        let finished = stream.start_reasoning(80);

        assert!(matches!(
            finished,
            Some(FinishedStream::Assistant { lines, .. })
                if lines.iter().map(RenderedLine::plain_text).collect::<String>() == "answer"
        ));
        assert!(stream.is_reasoning());
        assert!(stream.active_lines().is_empty());
    }

    #[test]
    fn assistant_commits_complete_lines_and_keeps_the_tail_live() {
        let mut stream = StreamState::default();
        stream.start_assistant(80);
        stream.push_assistant("first\nsecond");

        let Some(StreamRefresh::Assistant { lines, first_line }) = stream.take_refresh(80) else {
            panic!("assistant refresh");
        };

        assert!(first_line);
        assert_eq!(
            lines
                .iter()
                .map(RenderedLine::plain_text)
                .collect::<Vec<_>>(),
            ["first"]
        );
        assert_eq!(
            stream
                .active_lines()
                .iter()
                .map(RenderedLine::plain_text)
                .collect::<Vec<_>>(),
            ["second"]
        );
        assert!(!stream.active_starts_stream());
    }
}
