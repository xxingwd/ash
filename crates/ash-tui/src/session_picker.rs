use ash_core::{SessionId, SessionSummary};

use crate::picker::PickerState;

#[derive(Debug, Default)]
pub struct SessionPickerState {
    inner: PickerState<SessionSummary>,
}

impl SessionPickerState {
    pub(crate) const fn with_items(sessions: Vec<SessionSummary>) -> Self {
        Self {
            inner: PickerState::with_items(sessions),
        }
    }

    pub(crate) const fn is_visible(&self) -> bool {
        self.inner.is_visible()
    }

    pub(crate) fn sessions(&self) -> &[SessionSummary] {
        self.inner.items()
    }

    pub(crate) const fn selected_index(&self) -> usize {
        self.inner.selected_index()
    }

    pub(crate) fn selected_session_id(&self) -> Option<SessionId> {
        self.inner
            .items()
            .get(self.inner.selected_index())
            .map(|session| session.session_id)
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

    fn summary(title: &str) -> SessionSummary {
        SessionSummary {
            session_id: SessionId::new(),
            title: title.to_string(),
            created_at: "2026-07-15 12:00".to_string(),
        }
    }

    #[test]
    fn picker_wraps_navigation_and_returns_the_selected_session() {
        let first = summary("first");
        let second = summary("second");
        let mut picker = SessionPickerState::with_items(vec![first.clone(), second.clone()]);

        assert_eq!(picker.selected_session_id(), Some(first.session_id));
        picker.move_up();
        assert_eq!(picker.selected_session_id(), Some(second.session_id));
        picker.move_down();
        assert_eq!(picker.selected_session_id(), Some(first.session_id));
    }
}
