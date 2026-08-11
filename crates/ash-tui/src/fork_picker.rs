use ash_core::{ForkPoint, MessageId};

use crate::picker::PickerState;

#[derive(Debug, Default)]
pub struct ForkPickerState {
    inner: PickerState<ForkPoint>,
}

impl ForkPickerState {
    pub(crate) const fn with_items(points: Vec<ForkPoint>) -> Self {
        Self {
            inner: PickerState::with_items(points),
        }
    }

    pub(crate) const fn is_visible(&self) -> bool {
        self.inner.is_visible()
    }

    pub(crate) fn points(&self) -> &[ForkPoint] {
        self.inner.items()
    }

    pub(crate) const fn selected_index(&self) -> usize {
        self.inner.selected_index()
    }

    pub(crate) fn selected_message_id(&self) -> Option<MessageId> {
        self.inner
            .items()
            .get(self.inner.selected_index())
            .map(|point| point.message_id)
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

    fn point(prompt: &str) -> ForkPoint {
        ForkPoint {
            message_id: MessageId::new(),
            prompt: prompt.to_string(),
        }
    }

    #[test]
    fn picker_wraps_navigation_and_returns_the_selected_prompt() {
        let first = point("first");
        let second = point("second");
        let mut picker = ForkPickerState::with_items(vec![first.clone(), second.clone()]);

        assert_eq!(picker.selected_message_id(), Some(first.message_id));
        picker.move_up();
        assert_eq!(picker.selected_message_id(), Some(second.message_id));
        picker.move_down();
        assert_eq!(picker.selected_message_id(), Some(first.message_id));
    }
}
