use ash_core::{SessionId, SessionSummary};

#[derive(Debug, Default)]
pub(crate) struct SessionPickerState {
    sessions: Vec<SessionSummary>,
    selected: usize,
}

impl SessionPickerState {
    pub(crate) fn open(&mut self, sessions: Vec<SessionSummary>) {
        self.sessions = sessions;
        self.selected = 0;
    }

    pub(crate) fn is_visible(&self) -> bool {
        !self.sessions.is_empty()
    }

    pub(crate) fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    pub(crate) fn selected_session_id(&self) -> Option<SessionId> {
        self.sessions
            .get(self.selected)
            .map(|session| session.session_id)
    }

    pub(crate) fn move_up(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.sessions.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub(crate) fn move_down(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
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
        let mut picker = SessionPickerState::default();
        picker.open(vec![first.clone(), second.clone()]);

        assert_eq!(picker.selected_session_id(), Some(first.session_id));
        picker.move_up();
        assert_eq!(picker.selected_session_id(), Some(second.session_id));
        picker.move_down();
        assert_eq!(picker.selected_session_id(), Some(first.session_id));
    }
}
