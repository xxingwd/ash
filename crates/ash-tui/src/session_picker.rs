use ash_core::{ThreadId, ThreadSummary};

use crate::picker::PickerState;

#[derive(Debug, Default)]
pub struct SessionPickerState {
    inner: PickerState<ThreadSummary>,
}

impl SessionPickerState {
    pub(crate) const fn with_items(threads: Vec<ThreadSummary>) -> Self {
        Self {
            inner: PickerState::with_items(threads),
        }
    }

    pub(crate) const fn is_visible(&self) -> bool {
        self.inner.is_visible()
    }

    pub(crate) fn threads(&self) -> &[ThreadSummary] {
        self.inner.items()
    }

    pub(crate) const fn selected_index(&self) -> usize {
        self.inner.selected_index()
    }

    pub(crate) fn selected_thread_id(&self) -> Option<ThreadId> {
        self.inner
            .items()
            .get(self.inner.selected_index())
            .map(|session| session.thread_id)
    }

    pub(crate) const fn move_up(&mut self) {
        self.inner.move_up();
    }

    pub(crate) const fn move_down(&mut self) {
        self.inner.move_down();
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
        let mut picker = SessionPickerState::with_items(vec![first.clone(), second.clone()]);

        assert_eq!(picker.selected_thread_id(), Some(first.thread_id));
        picker.move_up();
        assert_eq!(picker.selected_thread_id(), Some(second.thread_id));
        picker.move_down();
        assert_eq!(picker.selected_thread_id(), Some(first.thread_id));
    }
}
