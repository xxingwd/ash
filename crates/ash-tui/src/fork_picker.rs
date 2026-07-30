use ash_core::{ForkPoint, MessageId};

#[derive(Debug, Default)]
pub(crate) struct ForkPickerState {
    points: Vec<ForkPoint>,
    selected: usize,
}

impl ForkPickerState {
    pub(crate) fn open(&mut self, points: Vec<ForkPoint>) {
        self.points = points;
        self.selected = 0;
    }

    pub(crate) fn is_visible(&self) -> bool {
        !self.points.is_empty()
    }

    pub(crate) fn points(&self) -> &[ForkPoint] {
        &self.points
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    pub(crate) fn selected_message_id(&self) -> Option<MessageId> {
        self.points.get(self.selected).map(|point| point.message_id)
    }

    pub(crate) fn move_up(&mut self) {
        if self.points.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.points.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub(crate) fn move_down(&mut self) {
        if !self.points.is_empty() {
            self.selected = (self.selected + 1) % self.points.len();
        }
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
        let mut picker = ForkPickerState::default();
        picker.open(vec![first.clone(), second.clone()]);

        assert_eq!(picker.selected_message_id(), Some(first.message_id));
        picker.move_up();
        assert_eq!(picker.selected_message_id(), Some(second.message_id));
        picker.move_down();
        assert_eq!(picker.selected_message_id(), Some(first.message_id));
    }
}
