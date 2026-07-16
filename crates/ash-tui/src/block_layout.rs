use ratatui::layout::{Constraint, Flex, Layout, Rect};

const BLOCK_SPACING: u16 = 1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StackBoundary {
    has_block: bool,
}

impl StackBoundary {
    pub(crate) const fn after_block(self) -> Self {
        Self { has_block: true }
    }

    pub(crate) const fn has_block(self) -> bool {
        self.has_block
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StackItem {
    pub(crate) height: u16,
}

impl StackItem {
    pub(crate) const fn block(height: u16) -> Self {
        Self { height }
    }
}

#[derive(Debug)]
pub(crate) struct StackLayout {
    pub(crate) height: u16,
    pub(crate) areas: Vec<Rect>,
}

pub(crate) fn stack_height(boundary: StackBoundary, items: &[StackItem]) -> u16 {
    measure_stack(boundary, items).0
}

pub(crate) fn layout_stack(
    width: u16,
    boundary: StackBoundary,
    items: &[StackItem],
) -> StackLayout {
    if items.is_empty() {
        return StackLayout {
            height: 0,
            areas: Vec::new(),
        };
    }

    let (height, history_anchor) = measure_stack(boundary, items);
    let mut constraints = Vec::with_capacity(items.len() + usize::from(history_anchor));
    if history_anchor {
        constraints.push(Constraint::Length(0));
    }
    constraints.extend(items.iter().map(|item| Constraint::Length(item.height)));
    let area = Rect::new(0, 0, width, height);
    let sections = Layout::vertical(constraints)
        .flex(Flex::Start)
        .spacing(BLOCK_SPACING)
        .split(area);
    let skip = usize::from(history_anchor);
    let areas = sections.iter().skip(skip).copied().collect();

    StackLayout { height, areas }
}

fn measure_stack(boundary: StackBoundary, items: &[StackItem]) -> (u16, bool) {
    if items.is_empty() {
        return (0, false);
    }

    let history_anchor = boundary.has_block;
    let section_count = items.len() + usize::from(history_anchor);
    let gaps = u16::try_from(section_count.saturating_sub(1)).unwrap_or(u16::MAX);
    let content_height = items
        .iter()
        .fold(0_u16, |height, item| height.saturating_add(item.height));
    (
        content_height.saturating_add(gaps.saturating_mul(BLOCK_SPACING)),
        history_anchor,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flex_adds_spacing_before_a_new_block_after_history() {
        let boundary = StackBoundary::default().after_block();
        let layout = layout_stack(80, boundary, &[StackItem::block(2)]);

        assert_eq!(layout.height, 3);
        assert_eq!(layout.areas, [Rect::new(0, 1, 80, 2)]);
    }

    #[test]
    fn zero_height_history_anchor_still_produces_flex_spacing() {
        let boundary = StackBoundary::default().after_block();
        let layout = layout_stack(1, boundary, &[StackItem::block(0)]);

        assert_eq!(layout.height, 1);
        assert_eq!(layout.areas, [Rect::new(0, 1, 1, 0)]);
    }

    #[test]
    fn flex_spaces_complete_blocks_without_component_margins() {
        let boundary = StackBoundary::default().after_block();
        let items = [StackItem::block(2), StackItem::block(1)];
        let layout = layout_stack(80, boundary, &items);

        assert_eq!(layout.height, 5);
        assert_eq!(
            layout.areas,
            [Rect::new(0, 1, 80, 2), Rect::new(0, 4, 80, 1)]
        );
    }
}
