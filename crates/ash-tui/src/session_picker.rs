use ash_core::{ThreadId, ThreadSummary};

#[derive(Debug, Default)]
pub(crate) struct SessionPickerState {
    threads: Vec<ThreadSummary>,
    selected: usize,
}

impl SessionPickerState {
    pub(crate) fn open(&mut self, threads: Vec<ThreadSummary>) {
        self.threads = threads;
        self.selected = 0;
    }

    pub(crate) fn is_visible(&self) -> bool {
        !self.threads.is_empty()
    }

    pub(crate) fn threads(&self) -> &[ThreadSummary] {
        &self.threads
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    pub(crate) fn selected_thread_id(&self) -> Option<ThreadId> {
        self.threads
            .get(self.selected)
            .map(|session| session.thread_id)
    }

    pub(crate) fn move_up(&mut self) {
        if self.threads.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.threads.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub(crate) fn move_down(&mut self) {
        if !self.threads.is_empty() {
            self.selected = (self.selected + 1) % self.threads.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(title: &str) -> ThreadSummary {
        ThreadSummary {
            thread_id: ThreadId::new(),
            title: title.to_string(),
            created_at: "2026-07-15 12:00".to_string(),
        }
    }

    #[test]
    fn picker_wraps_navigation_and_returns_the_selected_session() {
        let first = summary("first");
        let second = summary("second");
        let mut picker = SessionPickerState::default();
        picker.open(vec![first.clone(), second.clone()]);

        assert_eq!(picker.selected_thread_id(), Some(first.thread_id));
        picker.move_up();
        assert_eq!(picker.selected_thread_id(), Some(second.thread_id));
        picker.move_down();
        assert_eq!(picker.selected_thread_id(), Some(first.thread_id));
    }
}
