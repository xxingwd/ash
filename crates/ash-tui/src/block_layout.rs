use ratatui::layout::{Constraint, Flex, Layout, Rect};

const BLOCK_SPACING: u16 = 1;

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

pub(crate) fn layout_stack(width: u16, items: &[StackItem]) -> StackLayout {
    if items.is_empty() {
        return StackLayout {
            height: 0,
            areas: Vec::new(),
        };
    }

    let height = measure_stack(items);
    let constraints = items
        .iter()
        .map(|item| Constraint::Length(item.height))
        .collect::<Vec<_>>();
    let area = Rect::new(0, 0, width, height);
    let sections = Layout::vertical(constraints)
        .flex(Flex::Start)
        .spacing(BLOCK_SPACING)
        .split(area);
    let areas = sections.iter().copied().collect();

    StackLayout { height, areas }
}

fn measure_stack(items: &[StackItem]) -> u16 {
    if items.is_empty() {
        return 0;
    }

    let gaps = u16::try_from(items.len().saturating_sub(1)).unwrap_or(u16::MAX);
    let content_height = items
        .iter()
        .fold(0_u16, |height, item| height.saturating_add(item.height));
    content_height.saturating_add(gaps.saturating_mul(BLOCK_SPACING))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_block_starts_at_the_top() {
        let layout = layout_stack(80, &[StackItem::block(2)]);

        assert_eq!(layout.height, 2);
        assert_eq!(layout.areas, [Rect::new(0, 0, 80, 2)]);
    }

    #[test]
    fn flex_spaces_complete_blocks_without_component_margins() {
        let items = [StackItem::block(2), StackItem::block(1)];
        let layout = layout_stack(80, &items);

        assert_eq!(layout.height, 4);
        assert_eq!(
            layout.areas,
            [Rect::new(0, 0, 80, 2), Rect::new(0, 3, 80, 1)]
        );
    }
}
