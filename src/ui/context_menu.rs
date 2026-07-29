use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

const MENU_WIDTH: u16 = 18;
const MENU_BACKGROUND: Color = Color::Rgb(38, 38, 40);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuAction {
    Copy,
    Paste,
    NewFile,
    NewFolder,
    Rename,
    Delete,
}

#[derive(Debug, Clone)]
struct MenuItem {
    action: ContextMenuAction,
    label: &'static str,
    enabled: bool,
}

#[derive(Debug, Clone)]
pub struct ContextMenu {
    area: Rect,
    items: Vec<MenuItem>,
    hovered: Option<ContextMenuAction>,
}

impl ContextMenu {
    pub fn new(viewport: Rect, column: u16, row: u16, copy_enabled: bool) -> Self {
        Self::from_items(
            viewport,
            column,
            row,
            vec![
                MenuItem {
                    action: ContextMenuAction::Copy,
                    label: "Copy",
                    enabled: copy_enabled,
                },
                MenuItem {
                    action: ContextMenuAction::Paste,
                    label: "Paste",
                    enabled: true,
                },
            ],
        )
    }

    pub fn sidebar(
        viewport: Rect,
        column: u16,
        row: u16,
        create_enabled: bool,
        item_actions_enabled: bool,
    ) -> Self {
        Self::from_items(
            viewport,
            column,
            row,
            vec![
                MenuItem {
                    action: ContextMenuAction::NewFile,
                    label: "New File",
                    enabled: create_enabled,
                },
                MenuItem {
                    action: ContextMenuAction::NewFolder,
                    label: "New Folder",
                    enabled: create_enabled,
                },
                MenuItem {
                    action: ContextMenuAction::Rename,
                    label: "Rename",
                    enabled: item_actions_enabled,
                },
                MenuItem {
                    action: ContextMenuAction::Delete,
                    label: "Delete",
                    enabled: item_actions_enabled,
                },
            ],
        )
    }

    fn from_items(viewport: Rect, column: u16, row: u16, items: Vec<MenuItem>) -> Self {
        let width = MENU_WIDTH.min(viewport.width);
        let height = u16::try_from(items.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(viewport.height);
        let max_x = viewport.right().saturating_sub(width);
        let max_y = viewport.bottom().saturating_sub(height);
        Self {
            area: Rect::new(
                column.min(max_x).max(viewport.x),
                row.min(max_y).max(viewport.y),
                width,
                height,
            ),
            items,
            hovered: None,
        }
    }

    pub fn update_hover(&mut self, column: u16, row: u16) {
        self.hovered = self.enabled_action_at(column, row);
    }

    pub fn action_at(&self, column: u16, row: u16) -> Option<ContextMenuAction> {
        self.enabled_action_at(column, row)
    }

    fn enabled_action_at(&self, column: u16, row: u16) -> Option<ContextMenuAction> {
        let index = self.item_index(column, row)?;
        self.items[index]
            .enabled
            .then_some(self.items[index].action)
    }

    fn item_index(&self, column: u16, row: u16) -> Option<usize> {
        if column <= self.area.x
            || column >= self.area.right().saturating_sub(1)
            || row <= self.area.y
            || row >= self.area.bottom().saturating_sub(1)
        {
            return None;
        }
        let index = usize::from(row - self.area.y - 1);
        (index < self.items.len()).then_some(index)
    }

    #[cfg(test)]
    fn area(&self) -> Rect {
        self.area
    }

    #[cfg(test)]
    fn hovered(&self) -> Option<ContextMenuAction> {
        self.hovered
    }
}

pub fn render_context_menu(frame: &mut ratatui::Frame<'_>, menu: &ContextMenu) {
    let inner_width = usize::from(menu.area.width.saturating_sub(2));
    let lines = menu
        .items
        .iter()
        .map(|item| {
            let label = format!(
                " {:<width$}",
                item.label,
                width = inner_width.saturating_sub(1)
            );
            Line::styled(
                label,
                menu_item_style(menu.hovered == Some(item.action), item.enabled),
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, menu.area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(MENU_BACKGROUND))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
        menu.area,
    );
}

fn menu_item_style(hovered: bool, enabled: bool) -> Style {
    if hovered && enabled {
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else if enabled {
        Style::default().fg(Color::White).bg(MENU_BACKGROUND)
    } else {
        Style::default().fg(Color::DarkGray).bg(MENU_BACKGROUND)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_menu_clamps_and_disables_copy() {
        let mut menu = ContextMenu::new(Rect::new(5, 3, 20, 10), 24, 12, false);
        assert_eq!(menu.area(), Rect::new(7, 9, 18, 4));
        assert_eq!(menu.action_at(8, 10), None);
        assert_eq!(menu.action_at(8, 11), Some(ContextMenuAction::Paste));
        menu.update_hover(8, 11);
        assert_eq!(menu.hovered(), Some(ContextMenuAction::Paste));
    }

    #[test]
    fn sidebar_menu_applies_capability_matrix_without_geometry_shift() {
        let menu = ContextMenu::sidebar(Rect::new(0, 0, 30, 12), 2, 2, true, false);
        assert_eq!(menu.area().height, 6);
        assert_eq!(menu.action_at(3, 3), Some(ContextMenuAction::NewFile));
        assert_eq!(menu.action_at(3, 4), Some(ContextMenuAction::NewFolder));
        assert_eq!(menu.action_at(3, 5), None);
        assert_eq!(menu.action_at(3, 6), None);
    }
}
