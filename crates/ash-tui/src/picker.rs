/// Generic circular-navigation picker shared by the session and fork
/// pickers. Keeps the selection index and provides the same wrap-around
/// navigation for both, so the concrete pickers only add their typed
/// accessors.
#[derive(Debug)]
pub struct PickerState<T> {
    items: Vec<T>,
    selected: usize,
}

impl<T> Default for PickerState<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
        }
    }
}

impl<T> PickerState<T> {
    pub(crate) const fn with_items(items: Vec<T>) -> Self {
        Self { items, selected: 0 }
    }

    pub(crate) const fn is_visible(&self) -> bool {
        !self.items.is_empty()
    }

    pub(crate) fn items(&self) -> &[T] {
        &self.items
    }

    pub(crate) const fn selected_index(&self) -> usize {
        self.selected
    }

    pub(crate) const fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub(crate) const fn move_down(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    /// Clamp the selection into the current item range, or reset it to the
    /// first item when the list is empty. Used when the item list changes
    /// while a selection already exists.
    pub(crate) fn set_selected(&mut self, index: usize) {
        if self.items.is_empty() {
            self.selected = 0;
        } else {
            self.selected = index.min(self.items.len() - 1);
        }
    }

    /// Replace the wrapped items while keeping the selection clamped into
    /// the new range (or reset when empty).
    pub(crate) fn replace_items(&mut self, items: Vec<T>) {
        self.items = items;
        self.set_selected(self.selected);
    }

    /// Drop all items and reset the selection. Keeps the wrapped list owned
    /// so the type stays a single navigation state.
    pub(crate) fn clear(&mut self) {
        self.items.clear();
        self.selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_items_clamps_the_existing_selection() {
        let mut picker = PickerState::with_items(vec!["first", "second", "third"]);
        picker.set_selected(2);

        picker.replace_items(vec!["first", "second"]);

        assert_eq!(picker.selected_index(), 1);
        assert_eq!(picker.items()[picker.selected_index()], "second");
    }

    #[test]
    fn replacing_items_with_an_empty_list_resets_the_selection() {
        let mut picker = PickerState::with_items(vec!["first", "second"]);
        picker.set_selected(1);

        picker.replace_items(Vec::new());

        assert_eq!(picker.selected_index(), 0);
        assert!(!picker.is_visible());
    }
}
