use ash_core::{ForkPoint, SessionSummary};

use crate::{
    fork_picker::ForkPickerState,
    session_picker::SessionPickerState,
    slash_command::{CommandCompletion, CommandCompletionState},
};

#[derive(Clone, Copy, Debug, Default)]
pub enum MenuView<'a> {
    #[default]
    None,
    Commands {
        items: &'a [CommandCompletion],
        selected: usize,
    },
    Sessions {
        items: &'a [SessionSummary],
        selected: usize,
    },
    ForkPoints {
        items: &'a [ForkPoint],
        selected: usize,
    },
}

impl MenuView<'_> {
    pub(crate) const fn item_count(self) -> usize {
        match self {
            Self::None => 0,
            Self::Commands { items, .. } => items.len(),
            Self::Sessions { items, .. } => items.len(),
            Self::ForkPoints { items, .. } => items.len(),
        }
    }
}

#[derive(Debug)]
pub enum ComposerMenuState {
    Commands(CommandCompletionState),
    Sessions(SessionPickerState),
    ForkPoints(ForkPickerState),
}

impl Default for ComposerMenuState {
    fn default() -> Self {
        Self::Commands(CommandCompletionState::default())
    }
}

impl ComposerMenuState {
    pub(crate) fn sync_commands(&mut self, input: &str, cursor: usize) {
        if let Self::Commands(completion) = self {
            completion.sync(input, cursor);
        }
    }

    pub(crate) fn view(&self) -> MenuView<'_> {
        match self {
            Self::Commands(completion) if completion.is_visible() => MenuView::Commands {
                items: completion.items(),
                selected: completion.selected_index(),
            },
            Self::Sessions(picker) if picker.is_visible() => MenuView::Sessions {
                items: picker.sessions(),
                selected: picker.selected_index(),
            },
            Self::ForkPoints(picker) if picker.is_visible() => MenuView::ForkPoints {
                items: picker.points(),
                selected: picker.selected_index(),
            },
            Self::Commands(_) | Self::Sessions(_) | Self::ForkPoints(_) => MenuView::None,
        }
    }

    pub(crate) const fn visible_completion_mut(&mut self) -> Option<&mut CommandCompletionState> {
        match self {
            Self::Commands(completion) if completion.is_visible() => Some(completion),
            Self::Commands(_) | Self::Sessions(_) | Self::ForkPoints(_) => None,
        }
    }

    pub(crate) const fn visible_session_picker_mut(&mut self) -> Option<&mut SessionPickerState> {
        match self {
            Self::Sessions(picker) if picker.is_visible() => Some(picker),
            Self::Commands(_) | Self::Sessions(_) | Self::ForkPoints(_) => None,
        }
    }

    pub(crate) const fn visible_fork_picker_mut(&mut self) -> Option<&mut ForkPickerState> {
        match self {
            Self::ForkPoints(picker) if picker.is_visible() => Some(picker),
            Self::Commands(_) | Self::Sessions(_) | Self::ForkPoints(_) => None,
        }
    }

    pub(crate) const fn session_picker_is_visible(&self) -> bool {
        matches!(self, Self::Sessions(picker) if picker.is_visible())
    }

    pub(crate) const fn fork_picker_is_visible(&self) -> bool {
        matches!(self, Self::ForkPoints(picker) if picker.is_visible())
    }

    pub(crate) const fn picker_is_visible(&self) -> bool {
        self.session_picker_is_visible() || self.fork_picker_is_visible()
    }

    pub(crate) fn open_sessions(&mut self, sessions: Vec<SessionSummary>) {
        *self = Self::Sessions(SessionPickerState::with_items(sessions));
    }

    pub(crate) fn open_fork_points(&mut self, points: Vec<ForkPoint>) {
        *self = Self::ForkPoints(ForkPickerState::with_items(points));
    }

    pub(crate) fn close_sessions(&mut self) {
        if matches!(self, Self::Sessions(_)) {
            *self = Self::default();
        }
    }

    pub(crate) fn close_picker(&mut self) {
        if matches!(self, Self::Sessions(_) | Self::ForkPoints(_)) {
            *self = Self::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use ash_core::{ForkPoint, MessageId, SessionId, SessionSummary};

    use super::*;

    #[test]
    fn opening_sessions_replaces_command_completion() {
        let mut menu = ComposerMenuState::default();
        menu.sync_commands("/", 1);
        assert!(matches!(menu.view(), MenuView::Commands { .. }));

        menu.open_sessions(vec![SessionSummary {
            session_id: SessionId::new(),
            title: "saved chat".to_string(),
            created_at: "2026-07-28 12:00".to_string(),
        }]);

        assert!(matches!(menu.view(), MenuView::Sessions { .. }));
        assert!(menu.visible_completion_mut().is_none());
    }

    #[test]
    fn opening_fork_points_replaces_command_completion() {
        let mut menu = ComposerMenuState::default();
        menu.sync_commands("/", 1);

        menu.open_fork_points(vec![ForkPoint {
            message_id: MessageId::new(),
            prompt: "saved prompt".to_string(),
        }]);

        assert!(matches!(menu.view(), MenuView::ForkPoints { .. }));
        assert!(menu.picker_is_visible());
    }
}
