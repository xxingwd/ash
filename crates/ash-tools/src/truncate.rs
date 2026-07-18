pub(crate) const DEFAULT_MAX_LINES: usize = 2_000;
pub(crate) const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub(crate) const GREP_MAX_LINE_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LimitKind {
    Lines,
    Bytes,
}

#[derive(Debug)]
pub(crate) struct Truncation {
    pub(crate) content: String,
    pub(crate) truncated: bool,
    pub(crate) limited_by: Option<LimitKind>,
    pub(crate) total_lines: usize,
    pub(crate) output_lines: usize,
    pub(crate) output_bytes: usize,
    pub(crate) partial_line: bool,
}

pub(crate) fn head(content: &str, max_lines: usize) -> Truncation {
    let lines = split_lines(content);
    if lines.len() <= max_lines && content.len() <= DEFAULT_MAX_BYTES {
        return complete(content, lines.len());
    }

    let mut selected = Vec::new();
    let mut bytes: usize = 0;
    let mut limited_by = LimitKind::Lines;
    for line in lines.iter().take(max_lines) {
        let next = line.len() + usize::from(!selected.is_empty());
        if bytes.saturating_add(next) > DEFAULT_MAX_BYTES {
            limited_by = LimitKind::Bytes;
            break;
        }
        selected.push(*line);
        bytes += next;
    }

    Truncation {
        content: selected.join("\n"),
        truncated: true,
        limited_by: Some(limited_by),
        total_lines: lines.len(),
        output_lines: selected.len(),
        output_bytes: bytes,
        partial_line: false,
    }
}

pub(crate) fn tail(content: &str) -> Truncation {
    let lines = split_lines(content);
    if lines.len() <= DEFAULT_MAX_LINES && content.len() <= DEFAULT_MAX_BYTES {
        return complete(content, lines.len());
    }

    let mut selected = Vec::new();
    let mut bytes: usize = 0;
    let mut limited_by = LimitKind::Lines;
    let mut partial_line = false;
    for line in lines.iter().rev().take(DEFAULT_MAX_LINES) {
        let next = line.len() + usize::from(!selected.is_empty());
        if bytes.saturating_add(next) > DEFAULT_MAX_BYTES {
            limited_by = LimitKind::Bytes;
            if selected.is_empty() {
                let start = suffix_boundary(line, DEFAULT_MAX_BYTES);
                selected.push(&line[start..]);
                bytes = line.len() - start;
                partial_line = true;
            }
            break;
        }
        selected.push(*line);
        bytes += next;
    }
    selected.reverse();

    Truncation {
        content: selected.join("\n"),
        truncated: true,
        limited_by: Some(limited_by),
        total_lines: lines.len(),
        output_lines: selected.len(),
        output_bytes: bytes,
        partial_line,
    }
}

pub(crate) fn truncate_line(line: &str) -> (String, bool) {
    if line.chars().count() <= GREP_MAX_LINE_CHARS {
        return (line.to_string(), false);
    }
    let text = line.chars().take(GREP_MAX_LINE_CHARS).collect::<String>();
    (format!("{text}... [truncated]"), true)
}

pub(crate) fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn complete(content: &str, total_lines: usize) -> Truncation {
    Truncation {
        content: content.to_string(),
        truncated: false,
        limited_by: None,
        total_lines,
        output_lines: total_lines,
        output_bytes: content.len(),
        partial_line: false,
    }
}

fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines = content.split('\n').collect::<Vec<_>>();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn suffix_boundary(text: &str, max_bytes: usize) -> usize {
    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_complete_lines_within_both_limits() {
        let content = format!("{}\nlast", "a".repeat(DEFAULT_MAX_BYTES + 1));
        let result = head(&content, DEFAULT_MAX_LINES);

        assert!(result.truncated);
        assert_eq!(result.limited_by, Some(LimitKind::Bytes));
        assert!(result.content.is_empty());
        assert_eq!(result.output_lines, 0);
    }

    #[test]
    fn tail_keeps_utf8_boundary_for_an_oversized_last_line() {
        let content = "界".repeat(DEFAULT_MAX_BYTES);
        let result = tail(&content);

        assert!(result.truncated);
        assert!(result.partial_line);
        assert!(result.content.is_char_boundary(0));
        assert!(result.content.len() <= DEFAULT_MAX_BYTES);
    }

    #[test]
    fn truncates_long_grep_lines_by_characters() {
        let (line, truncated) = truncate_line(&"界".repeat(GREP_MAX_LINE_CHARS + 1));

        assert!(truncated);
        assert!(line.ends_with("... [truncated]"));
    }
}
