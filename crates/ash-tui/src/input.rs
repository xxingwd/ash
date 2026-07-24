use std::ops::Range;

use unicode_width::UnicodeWidthChar;

#[derive(Debug, Default)]
pub(crate) struct InputState {
    buffer: String,
    cursor: usize,
    goal_column: Option<usize>,
    history: Vec<String>,
    history_index: usize,
    history_draft: Option<String>,
}

pub(crate) struct InputView {
    pub lines: Vec<String>,
    pub cursor_row: u16,
    pub cursor_column: u16,
}

impl InputState {
    pub fn with_history(history: Vec<String>) -> Self {
        let history_index = history.len();
        Self {
            buffer: String::new(),
            cursor: 0,
            goal_column: None,
            history,
            history_index,
            history_draft: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn set_text(&mut self, value: impl Into<String>) {
        self.buffer = value.into();
        self.cursor = self.buffer.len();
        self.goal_column = None;
        self.history_index = self.history.len();
        self.history_draft = None;
    }

    pub fn restore_submission(&mut self, value: String) {
        if self.history.last() == Some(&value) {
            self.history.pop();
        }
        self.set_text(value);
    }

    pub fn record_submission(&mut self, value: &str) {
        if !value.trim().is_empty() {
            self.history.push(value.to_string());
        }
        self.history_index = self.history.len();
        self.history_draft = None;
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.goal_column = None;
        self.history_index = self.history.len();
        self.history_draft = None;
    }

    pub fn insert(&mut self, character: char) {
        self.buffer.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.goal_column = None;
    }

    pub fn insert_str(&mut self, value: &str) {
        self.buffer.insert_str(self.cursor, value);
        self.cursor += value.len();
        self.goal_column = None;
    }

    pub fn insert_paste(&mut self, value: &str) {
        self.insert_str(&value.replace("\r\n", "\n").replace('\r', "\n"));
    }

    pub fn backspace(&mut self) {
        if let Some(previous) = self.buffer[..self.cursor].char_indices().next_back() {
            self.buffer.remove(previous.0);
            self.cursor = previous.0;
            self.goal_column = None;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
            self.goal_column = None;
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.buffer[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.goal_column = None;
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor = self.buffer[self.cursor..]
                .char_indices()
                .nth(1)
                .map_or(self.buffer.len(), |(index, _)| self.cursor + index);
        }
        self.goal_column = None;
    }

    pub fn move_home(&mut self) {
        self.cursor = line_start(&self.buffer, self.cursor);
        self.goal_column = None;
    }

    pub fn move_end(&mut self) {
        self.cursor = line_end(&self.buffer, self.cursor);
        self.goal_column = None;
    }

    pub fn move_up(&mut self, width: u16) -> bool {
        self.move_vertical(width, -1)
    }

    pub fn move_down(&mut self, width: u16) -> bool {
        self.move_vertical(width, 1)
    }

    fn move_vertical(&mut self, width: u16, offset: isize) -> bool {
        let wrapped = wrap_input(&self.buffer, self.cursor, usize::from(width.max(1)));
        let Some(target_row) = wrapped.cursor_row.checked_add_signed(offset) else {
            return false;
        };
        let Some(target) = wrapped.lines.get(target_row) else {
            return false;
        };
        let column = *self.goal_column.get_or_insert(wrapped.cursor_column);
        self.cursor = byte_index_at_column(&self.buffer, target.start, target.end, column);
        true
    }

    pub fn submit(&mut self) -> String {
        let value = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.goal_column = None;
        self.history_index = self.history.len();
        self.history_draft = None;
        value
    }

    pub fn history_previous(&mut self) {
        if self.history_index == 0 || self.history.is_empty() {
            return;
        }
        if self.history_index == self.history.len() {
            self.history_draft = Some(self.buffer.clone());
        }
        self.history_index -= 1;
        self.buffer.clone_from(&self.history[self.history_index]);
        self.cursor = self.buffer.len();
        self.goal_column = None;
    }

    pub fn history_next(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index + 1 < self.history.len() {
            self.history_index += 1;
            self.buffer.clone_from(&self.history[self.history_index]);
            self.cursor = self.buffer.len();
            self.goal_column = None;
        } else {
            self.history_index = self.history.len();
            self.buffer = self.history_draft.take().unwrap_or_default();
            self.cursor = self.buffer.len();
            self.goal_column = None;
        }
    }

    pub fn view(&self, width: u16) -> InputView {
        let width = usize::from(width.max(1));
        let wrapped = wrap_input(&self.buffer, self.cursor, width);
        InputView {
            lines: wrapped
                .lines
                .iter()
                .map(|line| self.buffer[line.clone()].to_string())
                .collect(),
            cursor_row: u16::try_from(wrapped.cursor_row).unwrap_or(u16::MAX),
            cursor_column: u16::try_from(wrapped.cursor_column.min(width)).unwrap_or(u16::MAX),
        }
    }
}

struct WrappedInput {
    lines: Vec<Range<usize>>,
    cursor_row: usize,
    cursor_column: usize,
}

fn wrap_input(value: &str, cursor: usize, width: usize) -> WrappedInput {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut line_width = 0;
    let mut cursor_position = None;

    for (index, character) in value.char_indices() {
        if character != '\n' {
            let character_width = character.width().unwrap_or(0);
            if index > start && line_width + character_width > width {
                lines.push(start..index);
                start = index;
                line_width = 0;
            }
        }

        if index == cursor {
            cursor_position = Some((lines.len(), line_width));
        }

        if character == '\n' {
            lines.push(start..index);
            start = index + character.len_utf8();
            line_width = 0;
        } else {
            line_width += character.width().unwrap_or(0);
        }
    }

    if cursor == value.len() {
        cursor_position = Some((lines.len(), line_width));
    }
    lines.push(start..value.len());

    let (cursor_row, cursor_column) = cursor_position.unwrap_or_default();
    WrappedInput {
        lines,
        cursor_row,
        cursor_column,
    }
}

fn line_start(value: &str, cursor: usize) -> usize {
    value[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

fn line_end(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .find('\n')
        .map_or(value.len(), |offset| cursor + offset)
}

fn byte_index_at_column(value: &str, start: usize, end: usize, target: usize) -> usize {
    let mut column = 0;
    for (offset, character) in value[start..end].char_indices() {
        let next = column + character.width().unwrap_or(0);
        if next > target {
            return start + offset;
        }
        column = next;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_unicode_at_character_boundaries() {
        let mut input = InputState::default();
        input.insert_str("a中b");
        input.move_left();
        input.backspace();
        assert_eq!(input.submit(), "ab");
    }

    #[test]
    fn history_navigation_is_safe_when_empty() {
        let mut input = InputState::default();
        input.history_previous();
        input.history_next();
        assert!(input.is_empty());
    }

    #[test]
    fn view_keeps_cursor_visible() {
        let mut input = InputState::default();
        input.insert_str("123456789");
        let view = input.view(4);
        assert_eq!(view.lines, ["1234", "5678", "9"]);
        assert_eq!(view.cursor_row, 2);
        assert_eq!(view.cursor_column, 1);
    }

    #[test]
    fn view_preserves_newlines_and_empty_lines() {
        let mut input = InputState::default();
        input.insert_str("one\n\nthree");

        let view = input.view(20);

        assert_eq!(view.lines, ["one", "", "three"]);
        assert_eq!(view.cursor_row, 2);
        assert_eq!(view.cursor_column, 5);
    }

    #[test]
    fn vertical_movement_keeps_the_display_column() {
        let mut input = InputState::default();
        input.insert_str("abcdef\nx\n123456");

        assert!(input.move_up(80));
        assert_eq!(input.cursor(), "abcdef\nx".len());
        assert!(input.move_up(80));
        assert_eq!(input.cursor(), "abcdef".len());
        assert!(input.move_down(80));
        assert_eq!(input.cursor(), "abcdef\nx".len());
        assert!(input.move_down(80));
        assert_eq!(input.cursor(), "abcdef\nx\n123456".len());
    }

    #[test]
    fn vertical_movement_follows_soft_wrapping() {
        let mut input = InputState::default();
        input.insert_str("123456789");

        assert!(input.move_up(4));
        assert_eq!(input.cursor(), 5);
        assert!(input.move_up(4));
        assert_eq!(input.cursor(), 1);
        assert!(input.move_down(4));
        assert_eq!(input.cursor(), 5);
        assert!(input.move_down(4));
        assert_eq!(input.cursor(), 9);
    }

    #[test]
    fn paste_keeps_line_breaks_and_normalizes_carriage_returns() {
        let mut input = InputState::default();
        input.insert_paste("one\r\ntwo\rthree");

        assert_eq!(input.text(), "one\ntwo\nthree");
    }

    #[test]
    fn replaces_the_buffer_for_command_completion() {
        let mut input = InputState::default();
        input.insert_str("/cl");
        input.set_text("/clear");
        assert_eq!(input.text(), "/clear");
        assert_eq!(input.cursor(), "/clear".len());
    }

    #[test]
    fn starts_with_persisted_prompt_history() {
        let mut input = InputState::with_history(vec!["first".into(), "second".into()]);
        input.history_previous();
        assert_eq!(input.text(), "second");
        input.history_previous();
        assert_eq!(input.text(), "first");
    }

    #[test]
    fn history_navigation_restores_the_current_draft() {
        let mut input = InputState::with_history(vec!["first".into(), "second".into()]);
        input.set_text("unfinished draft");

        input.history_previous();
        assert_eq!(input.text(), "second");
        input.history_previous();
        assert_eq!(input.text(), "first");
        input.history_next();
        assert_eq!(input.text(), "second");
        input.history_next();

        assert_eq!(input.text(), "unfinished draft");
        assert_eq!(input.cursor(), "unfinished draft".len());
    }

    #[test]
    fn restoring_a_submission_returns_it_to_the_draft() {
        let mut input = InputState::default();
        input.set_text("edit me");
        assert_eq!(input.submit(), "edit me");
        input.record_submission("edit me");
        input.restore_submission("edit me".to_string());
        assert_eq!(input.text(), "edit me");
        input.clear();
        input.history_previous();
        assert!(input.is_empty());
    }

    #[test]
    fn queued_drafts_are_not_added_to_history() {
        let mut input = InputState::default();
        input.set_text("active");
        assert_eq!(input.submit(), "active");
        input.record_submission("active");
        input.set_text("queued");
        assert_eq!(input.submit(), "queued");

        input.restore_submission("active".to_string());

        assert_eq!(input.text(), "active");
        assert!(input.history.is_empty());
    }

    #[test]
    fn restoring_repeated_text_removes_only_the_dispatched_copy() {
        let mut input = InputState::with_history(vec!["same".into()]);
        input.record_submission("same");

        input.restore_submission("same".to_string());

        assert_eq!(input.text(), "same");
        assert_eq!(input.history, vec!["same"]);
    }
}
