mod context_menu;
mod sidebar;
mod terminal;

use crate::workbench::{LayoutHandle, WorkbenchLayout, layout_handle_position};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;

pub use context_menu::{ContextMenu, ContextMenuAction, render_context_menu};
pub use sidebar::{
    SidebarEditKind, SidebarEditView, SidebarStyle, SidebarTrustChrome, SidebarTrustTarget,
    SidebarView, render_sidebar, sidebar_trust_hit, sidebar_trust_rows,
};
pub use terminal::{
    ShellTerminalPaneView, TerminalPaneStyle, render_compact_workbench, render_shell_terminal_pane,
    render_terminal_pane, render_unavailable_terminal_pane, shell_terminal_content_size,
    terminal_content_size,
};

/// Paint compact, familiar restore/collapse affordances over pane borders.
pub fn render_layout_controls(frame: &mut ratatui::Frame<'_>, layout: WorkbenchLayout) {
    if layout.compact {
        return;
    }
    let style = Style::default().fg(Color::DarkGray);
    let sidebar_symbol = if layout.sidebar.width > 0 {
        "◀"
    } else {
        "▶"
    };
    let (sidebar_x, sidebar_y) = layout_handle_position(layout, LayoutHandle::Sidebar);
    frame.render_widget(
        Paragraph::new(sidebar_symbol).style(style),
        Rect::new(sidebar_x, sidebar_y, 1, 1),
    );

    let bottom_symbol = if layout.bottom.height > 0 {
        "▼"
    } else {
        "▲"
    };
    let (bottom_x, bottom_y) = layout_handle_position(layout, LayoutHandle::Bottom);
    frame.render_widget(
        Paragraph::new(bottom_symbol).style(style),
        Rect::new(bottom_x, bottom_y, 1, 1),
    );
}
