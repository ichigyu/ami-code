use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::path::Path;

use crate::workspace::WorkspaceTrustState;
use crate::workspace::sidebar::{EntryKind, GitStatus, SidebarRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarTrustChrome {
    pub state: WorkspaceTrustState,
    pub confirming: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTrustTarget {
    Review,
    Revoke,
    ConfirmTrust,
    Cancel,
    Chrome,
}

pub fn sidebar_trust_rows(chrome: SidebarTrustChrome) -> usize {
    if chrome.confirming { 4 } else { 1 }
}

pub fn sidebar_trust_hit(chrome: SidebarTrustChrome, row: usize) -> Option<SidebarTrustTarget> {
    if chrome.confirming {
        return match row {
            0 | 1 => Some(SidebarTrustTarget::Chrome),
            2 => Some(SidebarTrustTarget::ConfirmTrust),
            3 => Some(SidebarTrustTarget::Cancel),
            _ => None,
        };
    }
    if row != 0 {
        return None;
    }
    Some(match chrome.state {
        WorkspaceTrustState::Trusted => SidebarTrustTarget::Revoke,
        WorkspaceTrustState::Untrusted | WorkspaceTrustState::Stale => SidebarTrustTarget::Review,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarEditKind {
    NewFile,
    NewFolder,
    Rename,
}

#[derive(Debug, Clone, Copy)]
pub struct SidebarEditView<'a> {
    pub target: &'a Path,
    pub value: &'a str,
    pub kind: SidebarEditKind,
    pub error: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarStyle {
    pub focused_border: Color,
    pub unfocused_border: Color,
}

impl Default for SidebarStyle {
    fn default() -> Self {
        Self {
            focused_border: Color::LightYellow,
            unfocused_border: Color::DarkGray,
        }
    }
}

pub struct SidebarView<'a> {
    pub rows: &'a [SidebarRow],
    pub edit: Option<SidebarEditView<'a>>,
    pub trust: SidebarTrustChrome,
    pub error: Option<&'a str>,
    pub focused: bool,
    pub style: SidebarStyle,
}

pub fn render_sidebar(frame: &mut ratatui::Frame<'_>, area: Rect, view: SidebarView<'_>) {
    let SidebarView {
        rows,
        edit,
        trust,
        error,
        focused,
        style,
    } = view;
    let title = if error.is_some() {
        "  sidebar !"
    } else {
        "  sidebar"
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            style.focused_border
        } else {
            style.unfocused_border
        }));
    let mut lines = trust_lines(trust);
    for row in rows {
        let rename =
            edit.filter(|edit| edit.kind == SidebarEditKind::Rename && edit.target == row.path);
        lines.push(sidebar_line(row, rename.map(|edit| edit.value)));
        if let Some(edit) = edit
            && matches!(
                edit.kind,
                SidebarEditKind::NewFile | SidebarEditKind::NewFolder
            )
            && edit.target == row.path
        {
            lines.push(edit_line(row.depth + 1, edit));
        }
    }
    if rows.is_empty()
        && let Some(error) = error
    {
        lines.push(Line::styled(
            format!("! {error}"),
            Style::default().fg(Color::LightRed),
        ));
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn trust_lines(chrome: SidebarTrustChrome) -> Vec<Line<'static>> {
    if chrome.confirming {
        return vec![
            Line::styled(
                "Project resources may",
                Style::default().fg(Color::LightRed),
            ),
            Line::styled(
                "execute arbitrary code.",
                Style::default().fg(Color::LightRed),
            ),
            Line::styled(
                "[ Trust + restart Pi ]",
                Style::default().fg(Color::LightGreen),
            ),
            Line::styled("[ Cancel ]", Style::default().fg(Color::Gray)),
        ];
    }
    let (label, color) = match chrome.state {
        WorkspaceTrustState::Untrusted => ("Untrusted — click to trust", Color::Yellow),
        WorkspaceTrustState::Stale => ("Stale trust — click review", Color::LightRed),
        WorkspaceTrustState::Trusted => ("Trusted — click to revoke", Color::LightGreen),
    };
    vec![Line::styled(label, Style::default().fg(color))]
}

fn edit_line(depth: usize, edit: SidebarEditView<'_>) -> Line<'static> {
    let marker = if edit.kind == SidebarEditKind::NewFolder {
        "▸ "
    } else {
        "  "
    };
    let mut spans = vec![
        Span::raw("  ".repeat(depth)),
        Span::styled(marker, Style::default().fg(Color::LightBlue)),
        Span::styled(
            format!("{}▏", edit.value),
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(error) = edit.error {
        spans.push(Span::styled(
            format!(" ! {error}"),
            Style::default().fg(Color::LightRed),
        ));
    }
    Line::from(spans)
}

fn sidebar_line(row: &SidebarRow, name_override: Option<&str>) -> Line<'static> {
    let mut spans = Vec::with_capacity(5);
    spans.push(Span::raw("  ".repeat(row.depth)));
    spans.push(Span::styled(
        row_marker(row).to_string(),
        Style::default().fg(marker_color(row)),
    ));
    spans.push(Span::raw(
        name_override
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| row.display_name().into_owned()),
    ));

    if let Some(kind) = kind_marker(row.kind) {
        spans.push(Span::styled(kind, Style::default().fg(Color::DarkGray)));
    }
    if let Some(error) = &row.error {
        spans.push(Span::styled(
            format!(" ! {error}"),
            Style::default().fg(Color::LightRed),
        ));
    }
    if let Some(marker) = git_marker(row.git.status) {
        spans.push(Span::styled(
            format!(" {marker}"),
            Style::default().fg(git_color(row.git.status)),
        ));
    } else if row.git.dirty_descendant {
        spans.push(Span::styled(" *", Style::default().fg(Color::Yellow)));
    }

    let line = Line::from(spans);
    if row.selected {
        line.style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        line
    }
}

fn row_marker(row: &SidebarRow) -> &'static str {
    if row.loading {
        "… "
    } else if row.kind.is_directory() {
        if row.expanded { "▾ " } else { "▸ " }
    } else if row.error.is_some() {
        "! "
    } else {
        "  "
    }
}

fn kind_marker(kind: EntryKind) -> Option<&'static str> {
    match kind {
        EntryKind::SymlinkDirectory | EntryKind::Symlink => Some(" @"),
        EntryKind::Deleted => Some(" ×"),
        EntryKind::Other => Some(" ?"),
        EntryKind::Directory | EntryKind::File => None,
    }
}

fn git_marker(status: Option<GitStatus>) -> Option<char> {
    status.map(|status| match status {
        GitStatus::Modified => 'M',
        GitStatus::Added => 'A',
        GitStatus::Deleted => 'D',
        GitStatus::Renamed => 'R',
        GitStatus::Conflict => 'U',
        GitStatus::Untracked => '?',
    })
}

fn marker_color(row: &SidebarRow) -> Color {
    if row.error.is_some() {
        Color::LightRed
    } else if row.loading {
        Color::Yellow
    } else if row.kind.is_directory() {
        Color::LightBlue
    } else {
        Color::DarkGray
    }
}

fn git_color(status: Option<GitStatus>) -> Color {
    match status {
        Some(GitStatus::Modified | GitStatus::Renamed) => Color::Yellow,
        Some(GitStatus::Added | GitStatus::Untracked) => Color::Green,
        Some(GitStatus::Deleted | GitStatus::Conflict) => Color::LightRed,
        None => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_chrome_rows_and_hits_are_frontend_owned() {
        let normal = SidebarTrustChrome {
            state: WorkspaceTrustState::Untrusted,
            confirming: false,
        };
        assert_eq!(sidebar_trust_rows(normal), 1);
        assert_eq!(
            sidebar_trust_hit(normal, 0),
            Some(SidebarTrustTarget::Review)
        );
        assert_eq!(sidebar_trust_hit(normal, 1), None);

        let prompt = SidebarTrustChrome {
            confirming: true,
            ..normal
        };
        assert_eq!(sidebar_trust_rows(prompt), 4);
        assert_eq!(
            sidebar_trust_hit(prompt, 0),
            Some(SidebarTrustTarget::Chrome)
        );
        assert_eq!(
            sidebar_trust_hit(prompt, 2),
            Some(SidebarTrustTarget::ConfirmTrust)
        );
        assert_eq!(
            sidebar_trust_hit(prompt, 3),
            Some(SidebarTrustTarget::Cancel)
        );
        assert_eq!(sidebar_trust_hit(prompt, 4), None);
    }

    #[test]
    fn maps_every_git_status_to_the_expected_marker() {
        assert_eq!(git_marker(Some(GitStatus::Modified)), Some('M'));
        assert_eq!(git_marker(Some(GitStatus::Added)), Some('A'));
        assert_eq!(git_marker(Some(GitStatus::Deleted)), Some('D'));
        assert_eq!(git_marker(Some(GitStatus::Renamed)), Some('R'));
        assert_eq!(git_marker(Some(GitStatus::Conflict)), Some('U'));
        assert_eq!(git_marker(Some(GitStatus::Untracked)), Some('?'));
        assert_eq!(git_marker(None), None);
    }
}
