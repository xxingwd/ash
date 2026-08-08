/// Generic circular-navigation picker shared by the session and fork
/// pickers. Keeps the selection index and provides the same wrap-around
/// navigation for both, so the concrete pickers only add their typed
/// accessors.
#[derive(Debug)]
pub(crate) struct PickerState<T> {
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
    pub(crate) fn open(&mut self, items: Vec<T>) {
        self.items = items;
        self.selected = 0;
    }

    pub(crate) fn is_visible(&self) -> bool {
        !self.items.is_empty()
    }

    pub(crate) fn items(&self) -> &[T] {
        &self.items
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    pub(crate) fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub(crate) fn move_down(&mut self) {
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
