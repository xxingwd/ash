use crate::{
    markdown::{render_markdown, RenderedLine},
    scrollback::sanitize_terminal_text,
};

#[derive(Debug, Default)]
pub(crate) struct StreamUpdate {
    pub(crate) stable_start: usize,
    pub(crate) stable: Vec<RenderedLine>,
    pub(crate) tail_start: usize,
    pub(crate) tail: Vec<RenderedLine>,
}

/// Keeps completed Markdown lines stable while retaining only the mutable tail
/// in the inline viewport. Redraws are frame-coalesced by `App`.
#[derive(Debug, Default)]
pub(crate) struct MarkdownStream {
    source: String,
    pending: String,
    committed_lines: usize,
}

impl MarkdownStream {
    pub(crate) fn push_delta(&mut self, delta: &str, width: u16) -> Option<StreamUpdate> {
        self.pending.push_str(&sanitize_terminal_text(delta));
        let commit_end = self.pending.rfind('\n').map(|index| index + 1)?;
        let remainder = self.pending.split_off(commit_end);
        let completed = std::mem::replace(&mut self.pending, remainder);
        self.source.push_str(&completed);
        Some(self.render_update(width))
    }

    pub(crate) fn finalize(&mut self, width: u16) -> StreamUpdate {
        self.source.push_str(&self.pending);
        let rendered = render_markdown(&self.source, width);
        let stable_start = self.committed_lines.min(rendered.len());
        let stable = rendered[stable_start..].to_vec();
        let tail_start = rendered.len();
        self.reset();
        StreamUpdate {
            stable_start,
            stable,
            tail_start,
            tail: Vec::new(),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.source.clear();
        self.pending.clear();
        self.committed_lines = 0;
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    fn render_update(&mut self, width: u16) -> StreamUpdate {
        let rendered = render_markdown(&self.source, width);
        let mut stable_target = rendered.len();
        if active_pipe_table(&self.source) {
            stable_target = self.committed_lines;
        } else if pending_pipe_table_header(&self.source) {
            stable_target = stable_target.saturating_sub(1);
        }
        stable_target = stable_target.max(self.committed_lines).min(rendered.len());

        let stable_start = self.committed_lines;
        let stable = rendered[stable_start..stable_target].to_vec();
        self.committed_lines = stable_target;
        StreamUpdate {
            stable_start,
            stable,
            tail_start: stable_target,
            tail: rendered[stable_target..].to_vec(),
        }
    }
}

fn pending_pipe_table_header(source: &str) -> bool {
    let block = source
        .rsplit_once("\n\n")
        .map_or(source, |(_, block)| block);
    let mut lines = block.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(header) = lines.next() else {
        return false;
    };
    lines.next().is_none() && header.contains('|')
}

fn active_pipe_table(source: &str) -> bool {
    if source.ends_with("\n\n") {
        return false;
    }
    let block = source
        .rsplit_once("\n\n")
        .map_or(source, |(_, block)| block);
    let mut lines = block.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(header) = lines.next() else {
        return false;
    };
    let Some(delimiter) = lines.next() else {
        return false;
    };
    header.contains('|') && is_table_delimiter(delimiter)
}

fn is_table_delimiter(line: &str) -> bool {
    let line = line.trim_matches('|');
    let mut count = 0;
    for cell in line.split('|').map(str::trim) {
        let cell = cell.trim_matches(':');
        if cell.len() < 3 || !cell.chars().all(|character| character == '-') {
            return false;
        }
        count += 1;
    }
    count > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_partial_lines_until_a_newline_arrives() {
        let mut stream = MarkdownStream::default();
        assert!(stream.push_delta("hello", 80).is_none());
        let update = stream.push_delta(" world\n", 80).unwrap();
        assert_eq!(update.stable.len(), 1);
        assert!(update.tail.is_empty());
    }

    #[test]
    fn commits_completed_lines_to_the_stable_region() {
        let mut stream = MarkdownStream::default();
        let update = stream.push_delta("one\n\ntwo\n\nthree\n\n", 80).unwrap();
        assert_eq!(update.stable.len(), 5);
        assert!(update.tail.is_empty());
        assert_eq!(update.stable_start, 0);
        assert_eq!(update.tail_start, 5);
    }

    #[test]
    fn holds_active_tables_until_the_block_finishes() {
        let mut stream = MarkdownStream::default();
        let update = stream.push_delta("| A | B |\n", 80).unwrap();
        assert!(update.stable.is_empty());
        assert!(!update.tail.is_empty());
        let update = stream.push_delta("|---|---|\n| 1 | 2 |\n", 80).unwrap();
        assert!(update.stable.is_empty());
        assert!(!update.tail.is_empty());
        let update = stream.push_delta("\n", 80).unwrap();
        assert!(!update.stable.is_empty());
        assert!(update.tail.is_empty());
    }

    #[test]
    fn finalize_commits_the_remaining_tail() {
        let mut stream = MarkdownStream::default();
        stream.push_delta("**done**", 80);
        let update = stream.finalize(80);
        assert_eq!(update.stable.len(), 1);
        assert!(update.tail.is_empty());
        assert!(stream.is_empty());
    }
}
