//! Phase 1 status bar state and Vim mode badge formatting.

use kova_core::vim::{VimMode, VimStatus};

use crate::dock::layout::FocusedRegion;
use crate::dock::split::PaneId;

/// Modal Vim mode badge displayed on the left of the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VimModeBadge {
    /// Vim mode disabled (`JetBrains` / `EDIT` pill).
    Off,
    /// `NORMAL` mode.
    Normal,
    /// `INSERT` mode.
    Insert,
    /// `VISUAL` character-wise mode.
    Visual,
    /// `V-LINE` line-wise mode.
    VisualLine,
    /// `V-BLOCK` block-wise mode.
    VisualBlock,
    /// Operator-pending mode.
    OperatorPending,
    /// Command-line (`:`) mode.
    CommandLine,
}

impl VimModeBadge {
    /// Returns the pill text for the status bar.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "EDIT",
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Visual => "VISUAL",
            Self::VisualLine => "V-LINE",
            Self::VisualBlock => "V-BLOCK",
            Self::OperatorPending => "OP",
            Self::CommandLine => "COMMAND",
        }
    }
}

impl From<VimMode> for VimModeBadge {
    fn from(mode: VimMode) -> Self {
        match mode {
            VimMode::Normal => Self::Normal,
            VimMode::Insert => Self::Insert,
            VimMode::Visual => Self::Visual,
            VimMode::VisualLine => Self::VisualLine,
        }
    }
}

impl VimModeBadge {
    /// Builds the badge from an optional `kova_core` [`VimStatus`] (`None` = Vim disabled).
    ///
    /// A non-empty pending sequence in `Normal` mode is shown as operator-pending.
    #[must_use]
    pub fn from_status(status: Option<&VimStatus>) -> Self {
        match status {
            None => Self::Off,
            Some(status) if status.mode == VimMode::Normal && !status.pending.is_empty() => {
                if status.pending.starts_with(':') {
                    Self::CommandLine
                } else {
                    Self::OperatorPending
                }
            }
            Some(status) => status.mode.into(),
        }
    }

    /// Returns the pill colour variant name from the design tokens.
    #[must_use]
    pub const fn variant(self) -> BadgeVariant {
        match self {
            Self::Off => BadgeVariant::Muted,
            Self::Normal | Self::OperatorPending => BadgeVariant::Accent,
            Self::Insert => BadgeVariant::Success,
            Self::Visual | Self::VisualLine | Self::VisualBlock => BadgeVariant::Warning,
            Self::CommandLine => BadgeVariant::Primary,
        }
    }
}

/// Colour variant of a status bar pill (`KOVA-007` design tokens).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BadgeVariant {
    /// Muted (Vim disabled).
    Muted,
    /// Accent (`NORMAL`): `bg #1A365D`, `border #38BDF8`, `fg #BAE6FD`.
    Accent,
    /// Green (`INSERT`): `bg #1B2E22`, `border #4ADE80`, `fg #98E08D`.
    Success,
    /// Warning (`VISUAL`): `bg #422F10`, `border #CA8A04`, `fg #FDE68A`.
    Warning,
    /// Primary (`COMMAND`).
    Primary,
}

impl BadgeVariant {
    /// Returns `(background, border, foreground)` as `0xRRGGBB` colours.
    #[must_use]
    pub const fn colors_rgb(self) -> (u32, u32, u32) {
        match self {
            Self::Muted => (0x0015_1A23, 0x0026_2E3E, 0x0082_91A8),
            Self::Accent => (0x001A_365D, 0x0038_BDF8, 0x00BA_E6FD),
            Self::Success => (0x001B_2E22, 0x004A_DE80, 0x0098_E08D),
            Self::Warning => (0x0042_2F10, 0x00CA_8A04, 0x00FD_E68A),
            Self::Primary => (0x0016_2B4A, 0x0058_A6FF, 0x00E6_EDF6),
        }
    }
}

/// Snapshot of all metrics and badges rendered in the Phase 1 status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBarState {
    /// Active Vim mode badge.
    pub vim_mode: VimModeBadge,
    /// Pending Vim key sequence (e.g. `"d2"`).
    pub vim_sequence: String,
    /// Active Git branch label (e.g. `"main*"`).
    pub git_branch: String,
    /// Active file name (e.g. `"OrderApp.java"`).
    pub active_file: String,
    /// Currently focused island.
    pub focused_region: FocusedRegion,
    /// 1-based cursor line.
    pub cursor_line: usize,
    /// 1-based cursor column.
    pub cursor_col: usize,
    /// Number of active cursors (`1` for single cursor).
    pub cursor_count: usize,
    /// Indentation and encoding label.
    pub indent_label: String,
    /// Active language label from `kova_syntax`.
    pub language_label: String,
    /// Refresh rate label (`"● 120 FPS"`).
    pub fps_label: String,
}

impl Default for StatusBarState {
    fn default() -> Self {
        Self {
            vim_mode: VimModeBadge::Normal,
            vim_sequence: String::new(),
            git_branch: "main".to_owned(),
            active_file: "OrderApp.java".to_owned(),
            focused_region: FocusedRegion::EditorPane(PaneId(1)),
            cursor_line: 1,
            cursor_col: 1,
            cursor_count: 1,
            indent_label: "Spaces: 4 · UTF-8".to_owned(),
            language_label: "Java".to_owned(),
            fps_label: "● 120 FPS".to_owned(),
        }
    }
}

impl StatusBarState {
    /// Formats the cursor position label (e.g. `"Ln 8, Col 14"` or `"Ln 8, Col 14 · 3 cursors"`).
    #[must_use]
    pub fn format_cursor_position(&self) -> String {
        if self.cursor_count > 1 {
            format!(
                "Ln {}, Col {} · {} cursors",
                self.cursor_line, self.cursor_col, self.cursor_count
            )
        } else {
            format!("Ln {}, Col {}", self.cursor_line, self.cursor_col)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_vim_badges_and_cursor_positions() {
        let mut status = StatusBarState {
            cursor_line: 8,
            cursor_col: 14,
            ..StatusBarState::default()
        };
        assert_eq!(status.vim_mode.label(), "NORMAL");
        assert_eq!(status.format_cursor_position(), "Ln 8, Col 14");

        status.cursor_count = 3;
        status.vim_mode = VimModeBadge::Visual;
        assert_eq!(status.vim_mode.label(), "VISUAL");
        assert_eq!(status.format_cursor_position(), "Ln 8, Col 14 · 3 cursors");
        assert_eq!(status.vim_mode.variant(), BadgeVariant::Warning);
    }

    #[test]
    fn maps_kova_core_vim_status_to_badges() {
        assert_eq!(VimModeBadge::from_status(None), VimModeBadge::Off);
        let engine = kova_core::vim::VimEngine::new(kova_buffer::Buffer::from_text("x"));
        let status = engine.status_snapshot();
        assert_eq!(
            VimModeBadge::from_status(Some(&status)),
            VimModeBadge::Normal
        );
        assert_eq!(VimModeBadge::from(VimMode::Insert), VimModeBadge::Insert);
        assert_eq!(VimModeBadge::from(VimMode::VisualLine).label(), "V-LINE");
        assert_eq!(VimModeBadge::Insert.variant(), BadgeVariant::Success);
        assert_eq!(BadgeVariant::Accent.colors_rgb().1, 0x0038_BDF8);
    }
}
