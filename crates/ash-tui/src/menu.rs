use ash_core::SessionSummary;

use crate::{
    session_picker::SessionPickerState,
    slash_command::{CommandCompletion, CommandCompletionState},
};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum MenuView<'a> {
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
}

impl MenuView<'_> {
    pub(crate) fn item_count(self) -> usize {
        match self {
            Self::None => 0,
            Self::Commands { items, .. } => items.len(),
            Self::Sessions { items, .. } => items.len(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ComposerMenuState {
    Commands(CommandCompletionState),
    Sessions(SessionPickerState),
}

impl Default for ComposerMenuState {
    fn default() -> Self {
        Self::Commands(CommandCompletionState::default())
    }
}

impl ComposerMenuState {
    pub(crate) fn sync_commands(&mut self, input: &str, cursor: usize, busy: bool) {
        if let Self::Commands(completion) = self {
            completion.sync(input, cursor, busy);
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
            Self::Commands(_) | Self::Sessions(_) => MenuView::None,
        }
    }

    pub(crate) fn visible_completion_mut(&mut self) -> Option<&mut CommandCompletionState> {
        match self {
            Self::Commands(completion) if completion.is_visible() => Some(completion),
            Self::Commands(_) | Self::Sessions(_) => None,
        }
    }

    pub(crate) fn visible_session_picker_mut(&mut self) -> Option<&mut SessionPickerState> {
        match self {
            Self::Sessions(picker) if picker.is_visible() => Some(picker),
            Self::Commands(_) | Self::Sessions(_) => None,
        }
    }

    pub(crate) fn session_picker_is_visible(&self) -> bool {
        matches!(self, Self::Sessions(picker) if picker.is_visible())
    }

    pub(crate) fn open_sessions(&mut self, sessions: Vec<SessionSummary>) {
        let mut picker = SessionPickerState::default();
        picker.open(sessions);
        *self = Self::Sessions(picker);
    }

    pub(crate) fn close_sessions(&mut self) {
        if matches!(self, Self::Sessions(_)) {
            *self = Self::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use ash_core::{SessionId, SessionSummary};

    use super::*;

    #[test]
    fn opening_sessions_replaces_command_completion() {
        let mut menu = ComposerMenuState::default();
        menu.sync_commands("/", 1, false);
        assert!(matches!(menu.view(), MenuView::Commands { .. }));

        menu.open_sessions(vec![SessionSummary {
            session_id: SessionId::new(),
            title: "saved chat".to_string(),
            created_at: "2026-07-28 12:00".to_string(),
        }]);

        assert!(matches!(menu.view(), MenuView::Sessions { .. }));
        assert!(menu.visible_completion_mut().is_none());
    }
}
