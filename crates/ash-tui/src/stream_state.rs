use std::time::Instant;

use ratatui::style::{Modifier, Style};

use crate::{
    markdown::{render_markdown, RenderedLine, StreamingMarkdownCache},
    scrollback::sanitize_terminal_text,
};

const MAX_VISIBLE_REASONING_LINES: usize = 5;

#[derive(Debug, Default)]
pub(crate) struct StreamState {
    mode: StreamMode,
}

#[derive(Debug, Default)]
enum StreamMode {
    #[default]
    Idle,
    Reasoning {
        source: String,
        started_at: Instant,
        lines: Vec<RenderedLine>,
        markdown_cache: StreamingMarkdownCache,
    },
}

pub(crate) enum FinishedStream {
    Thought { elapsed_seconds: u64 },
}

impl StreamState {
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

    pub(crate) fn push_reasoning(&mut self, delta: &str) {
        let StreamMode::Reasoning { source, .. } = &mut self.mode else {
            return;
        };
        source.push_str(&sanitize_terminal_text(delta));
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

    pub(crate) fn active_lines(&self) -> &[RenderedLine] {
        match &self.mode {
            StreamMode::Reasoning { lines, .. } => lines,
            StreamMode::Idle => &[],
        }
    }

    pub(crate) fn is_reasoning(&self) -> bool {
        matches!(self.mode, StreamMode::Reasoning { .. })
    }

    pub(crate) fn has_content(&self) -> bool {
        match &self.mode {
            StreamMode::Reasoning { source, .. } => !source.trim().is_empty(),
            StreamMode::Idle => false,
        }
    }

    pub(crate) fn finish(&mut self) -> Option<FinishedStream> {
        match std::mem::take(&mut self.mode) {
            StreamMode::Idle => None,
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
    fn finished_reasoning_produces_a_summary() {
        let mut stream = StreamState::default();
        stream.start_reasoning();
        stream.push_reasoning("first\nsecond");

        assert!(matches!(
            stream.finish(),
            Some(FinishedStream::Thought { .. })
        ));
    }

    #[test]
    fn reasoning_ends_when_text_output_starts() {
        // 正常时序: thinking 流完 -> text 开始。
        // 第一个 text delta 必须结束 reasoning（返回 Thought）并清空活动区。
        let mut stream = StreamState::default();
        stream.start_reasoning();
        stream.push_reasoning("inspect first");
        assert!(stream.is_reasoning());

        // 模拟 append_assistant 里的 finish: 固化 thinking
        let finished = stream.finish();
        assert!(matches!(finished, Some(FinishedStream::Thought { .. })));
        assert!(!stream.is_reasoning());
        assert!(stream.active_lines().is_empty());
        assert!(!stream.has_content());
    }
}
