use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Default)]
pub(crate) struct InputState {
    buffer: String,
    cursor: usize,
    history: Vec<String>,
    history_index: usize,
}

pub(crate) struct InputView {
    pub text: String,
    pub cursor_column: u16,
}

impl InputState {
    pub fn with_history(history: Vec<String>) -> Self {
        let history_index = history.len();
        Self {
            buffer: String::new(),
            cursor: 0,
            history,
            history_index,
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
        self.history_index = self.history.len();
    }

    pub fn restore_submitted(&mut self, value: String) {
        if let Some(index) = self.history.iter().rposition(|entry| entry == &value) {
            self.history.remove(index);
        }
        self.set_text(value);
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = self.history.len();
    }

    pub fn insert(&mut self, character: char) {
        self.buffer.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    pub fn insert_str(&mut self, value: &str) {
        self.buffer.insert_str(self.cursor, value);
        self.cursor += value.len();
    }

    pub fn backspace(&mut self) {
        if let Some(previous) = self.buffer[..self.cursor].char_indices().next_back() {
            self.buffer.remove(previous.0);
            self.cursor = previous.0;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.buffer[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor = self.buffer[self.cursor..]
                .char_indices()
                .nth(1)
                .map_or(self.buffer.len(), |(index, _)| self.cursor + index);
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    pub fn submit(&mut self) -> String {
        let value = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        if !value.trim().is_empty() {
            self.history.push(value.clone());
        }
        self.history_index = self.history.len();
        value
    }

    pub fn history_previous(&mut self) {
        if self.history_index == 0 || self.history.is_empty() {
            return;
        }
        self.history_index -= 1;
        self.buffer.clone_from(&self.history[self.history_index]);
        self.cursor = self.buffer.len();
    }

    pub fn history_next(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index + 1 < self.history.len() {
            self.history_index += 1;
            self.buffer.clone_from(&self.history[self.history_index]);
            self.cursor = self.buffer.len();
        } else {
            self.clear();
        }
    }

    pub fn view(&self, width: u16) -> InputView {
        let width = usize::from(width.max(1));
        let before = &self.buffer[..self.cursor];
        let mut start = self.cursor;
        let mut used = 0;
        for (index, character) in before.char_indices().rev() {
            let character_width = character.width().unwrap_or(0);
            if used + character_width > width {
                break;
            }
            used += character_width;
            start = index;
        }

        let mut end = start;
        let mut visible_width = 0;
        for (index, character) in self.buffer[start..].char_indices() {
            let character_width = character.width().unwrap_or(0);
            if visible_width + character_width > width {
                break;
            }
            visible_width += character_width;
            end = start + index + character.len_utf8();
        }

        InputView {
            text: self.buffer[start..end].to_string(),
            cursor_column: UnicodeWidthStr::width(&self.buffer[start..self.cursor]) as u16,
        }
    }
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
        assert_eq!(view.text, "6789");
        assert_eq!(view.cursor_column, 4);
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
    fn restoring_a_submission_returns_it_to_the_draft() {
        let mut input = InputState::default();
        input.set_text("edit me");
        assert_eq!(input.submit(), "edit me");
        input.restore_submitted("edit me".to_string());
        assert_eq!(input.text(), "edit me");
        input.clear();
        input.history_previous();
        assert!(input.is_empty());
    }

    #[test]
    fn restoring_removes_the_latest_matching_submission_behind_a_queue() {
        let mut input = InputState::default();
        input.set_text("active");
        assert_eq!(input.submit(), "active");
        input.set_text("queued");
        assert_eq!(input.submit(), "queued");

        input.restore_submitted("active".to_string());

        assert_eq!(input.text(), "active");
        assert_eq!(input.history, vec!["queued"]);
    }
}
