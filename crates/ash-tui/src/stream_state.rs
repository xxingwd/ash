use std::{collections::VecDeque, time::Instant};

use ratatui::style::{Modifier, Style};

use crate::{
    markdown::{render_markdown, RenderedLine, StreamingMarkdownCache},
    scrollback::sanitize_terminal_text,
};

const MAX_VISIBLE_REASONING_LINES: usize = 5;

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
        incomplete: String,
        queued_lines: VecDeque<String>,
        block_id: Option<u64>,
    },
    Reasoning {
        source: String,
        started_at: Instant,
        lines: Vec<RenderedLine>,
        markdown_cache: StreamingMarkdownCache,
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
            incomplete: String::new(),
            queued_lines: VecDeque::new(),
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
            markdown_cache: StreamingMarkdownCache::default(),
        };
        finished
    }

    pub(crate) fn push_assistant(&mut self, delta: &str) {
        let StreamMode::Assistant {
            incomplete,
            queued_lines,
            ..
        } = &mut self.mode
        else {
            return;
        };
        incomplete.push_str(&sanitize_terminal_text(delta));
        if let Some(last_newline) = incomplete.rfind('\n') {
            let completed = incomplete.drain(..=last_newline).collect::<String>();
            queued_lines.extend(completed.split_inclusive('\n').map(str::to_owned));
        }
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
            markdown_cache,
        } = &mut self.mode
        else {
            return;
        };
        *lines = render_reasoning_view(
            source,
            started_at.elapsed().as_secs(),
            width,
            markdown_cache,
        );
    }

    pub(crate) fn take_refresh(&mut self) -> Option<StreamRefresh> {
        match &mut self.mode {
            StreamMode::Assistant {
                queued_lines,
                block_id,
                ..
            } => queued_lines
                .pop_front()
                .map(|pending| StreamRefresh::Assistant {
                    pending,
                    block_id: *block_id,
                }),
            StreamMode::Reasoning { .. } if std::mem::take(&mut self.dirty) => {
                Some(StreamRefresh::Reasoning)
            }
            StreamMode::Idle | StreamMode::Reasoning { .. } => None,
        }
    }

    pub(crate) fn set_assistant_block_id(&mut self, id: u64) {
        if let StreamMode::Assistant { block_id, .. } = &mut self.mode {
            *block_id = Some(id);
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

    pub(crate) fn has_content(&self) -> bool {
        match &self.mode {
            StreamMode::Assistant {
                incomplete,
                queued_lines,
                ..
            } => !incomplete.is_empty() || !queued_lines.is_empty(),
            StreamMode::Reasoning { source, .. } => !source.trim().is_empty(),
            StreamMode::Idle => false,
        }
    }

    pub(crate) fn finish(&mut self) -> Option<FinishedStream> {
        self.dirty = false;
        match std::mem::take(&mut self.mode) {
            StreamMode::Idle => None,
            StreamMode::Assistant {
                incomplete,
                queued_lines,
                block_id,
            } => {
                let pending = queued_lines
                    .into_iter()
                    .chain(std::iter::once(incomplete))
                    .collect();
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

fn render_reasoning_view(
    source: &str,
    elapsed_seconds: u64,
    width: u16,
    markdown_cache: &mut StreamingMarkdownCache,
) -> Vec<RenderedLine> {
    let mut header = render_markdown(
        &format!("Thinking ({})", format_elapsed(elapsed_seconds)),
        width,
    );
    for line in &mut header {
        line.patch_style(Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC));
    }
    let tail = markdown_cache.update(source, width);
    let mut body = markdown_cache.latest_lines(&tail, MAX_VISIBLE_REASONING_LINES);
    while body.last().is_some_and(RenderedLine::is_blank) {
        body.pop();
    }
    for line in &mut body {
        line.patch_style(Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC));
    }
    header.extend(body);
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
    fn reasoning_view_keeps_a_timed_header_and_scrolls_the_latest_five_lines() {
        let lines = render_reasoning_view(
            "one\ntwo\nthree\nfour\nfive\nsix",
            3,
            80,
            &mut StreamingMarkdownCache::default(),
        );
        let text = lines
            .iter()
            .map(RenderedLine::plain_text)
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), 6);
        assert_eq!(text[0], "Thinking (3s)");
        assert_eq!(&text[1..], ["two", "three", "four", "five", "six"]);
    }

    #[test]
    fn reasoning_view_counts_wrapped_rows_toward_the_five_line_window() {
        let lines = render_reasoning_view(
            "12345\n67890\nabc\ndef",
            3,
            3,
            &mut StreamingMarkdownCache::default(),
        );
        let text = lines
            .iter()
            .map(RenderedLine::plain_text)
            .collect::<Vec<_>>();

        assert_eq!(&text[text.len() - 5..], ["45", "678", "90", "abc", "def"]);
    }

    #[test]
    fn reasoning_view_uses_the_completed_thought_style() {
        let lines = render_reasoning_view("detail", 3, 80, &mut StreamingMarkdownCache::default());
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
        assert!(!stream.has_content());
        assert!(stream.start_assistant().is_none());
        stream.push_assistant("answer");
        assert!(stream.has_content());
        let finished = stream.start_reasoning();

        assert!(matches!(
            finished,
            Some(FinishedStream::Assistant { pending, .. }) if pending == "answer"
        ));
        assert!(stream.is_reasoning());
        assert!(stream.active_lines().is_empty());
    }

    #[test]
    fn assistant_waits_for_a_newline_before_refreshing() {
        let mut stream = StreamState::default();
        stream.start_assistant();

        stream.push_assistant("first");
        assert!(stream.take_refresh().is_none());

        stream.push_assistant(" line\nsecond\nthird");
        assert!(matches!(
            stream.take_refresh(),
            Some(StreamRefresh::Assistant { pending, .. }) if pending == "first line\n"
        ));
        assert!(matches!(
            stream.take_refresh(),
            Some(StreamRefresh::Assistant { pending, .. }) if pending == "second\n"
        ));
        assert!(stream.take_refresh().is_none());
        assert!(matches!(
            stream.finish(),
            Some(FinishedStream::Assistant { pending, .. }) if pending == "third"
        ));
    }

    #[test]
    fn assistant_drains_one_complete_line_per_refresh() {
        let mut stream = StreamState::default();
        stream.start_assistant();
        stream.push_assistant("one\ntwo\nthree\n");

        let lines = (0..3)
            .filter_map(|_| match stream.take_refresh() {
                Some(StreamRefresh::Assistant { pending, .. }) => Some(pending),
                Some(StreamRefresh::Reasoning) | None => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(lines, ["one\n", "two\n", "three\n"]);
        assert!(stream.take_refresh().is_none());
    }

    #[test]
    fn finished_reasoning_produces_a_summary() {
        let mut stream = StreamState::default();
        stream.start_reasoning();
        stream.push_reasoning("first\nsecond");

        assert!(matches!(
            stream.finish(),
            Some(FinishedStream::Thought { .. })
        ));
    }
}
