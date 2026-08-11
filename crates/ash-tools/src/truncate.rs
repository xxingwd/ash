pub const DEFAULT_MAX_LINES: usize = 2_000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    Lines,
    Bytes,
}

#[derive(Debug)]
pub struct Truncation {
    pub(crate) content: String,
    pub(crate) truncated: bool,
    pub(crate) limited_by: Option<LimitKind>,
    pub(crate) partial_line: bool,
}

pub fn tail(content: &str) -> Truncation {
    let lines = split_lines(content);
    if lines.len() <= DEFAULT_MAX_LINES && content.len() <= DEFAULT_MAX_BYTES {
        return complete(content);
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
        partial_line,
    }
}

pub fn format_size(bytes: usize) -> String {
    // Display-only rounding; the u64 -> f64 precision loss is irrelevant here.
    #[allow(clippy::cast_precision_loss)]
    const fn to_f64(bytes: usize) -> f64 {
        bytes as f64
    }

    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", to_f64(bytes) / 1024.0)
    } else {
        format!("{:.1}MB", to_f64(bytes) / (1024.0 * 1024.0))
    }
}

fn complete(content: &str) -> Truncation {
    Truncation {
        content: content.to_string(),
        truncated: false,
        limited_by: None,
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

const fn suffix_boundary(text: &str, max_bytes: usize) -> usize {
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
    fn tail_keeps_utf8_boundary_for_an_oversized_last_line() {
        let content = "界".repeat(DEFAULT_MAX_BYTES);
        let result = tail(&content);

        assert!(result.truncated);
        assert!(result.partial_line);
        assert!(result.content.is_char_boundary(0));
        assert!(result.content.len() <= DEFAULT_MAX_BYTES);
    }
}
