//! Semantic token groups for Aether UI surfaces, typography, borders, editor, gutter, and terminal.

use serde::{Deserialize, Serialize};

use crate::{color::RgbaColor, plugin::PluginVariants, syntax::SyntaxTokens};

/// Complete set of 8 semantic token sections defining an Aether theme.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticTokens {
    /// Islands UI elevated surfaces and interaction states (`[surfaces]`).
    pub surfaces: SurfaceTokens,
    /// UI text hierarchy (`[text]`).
    pub text: UiTextTokens,
    /// Structural borders, dividers, and focus rings (`[borders]`).
    pub borders: BorderTokens,
    /// Text editor canvas, selections, inlay hints, and diagnostics (`[editor]`).
    pub editor: EditorTokens,
    /// Editor gutter line numbers, run icons, breakpoints, and Git diff bars (`[gutter]`).
    pub gutter: GutterTokens,
    /// Tree-sitter syntax highlight tokens (`[syntax]`).
    pub syntax: SyntaxTokens,
    /// Integrated PTY terminal ANSI 16 palette (`[terminal]`).
    pub terminal: TerminalPalette,
    /// WASM plugin declarative UI component variants (`[plugins]`).
    pub plugins: PluginVariants,
}

/// Islands UI surface hierarchy and interaction backdrop tokens (`[surfaces]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceTokens {
    /// Outer window backdrop and gap between floating Islands.
    pub canvas: RgbaColor,
    /// Docked tool window islands (`Project`, `Structure`, `JUnit`, `Terminal`).
    pub island: RgbaColor,
    /// Active code editor island.
    pub editor: RgbaColor,
    /// Floating popups, `Search Everywhere`, completions, and tooltips.
    pub elevated: RgbaColor,
    /// Translucent scrim behind modal overlays.
    pub overlay: RgbaColor,
    /// Hover surface for list rows, tree items, and buttons.
    pub hover: RgbaColor,
    /// Selected row or active item surface.
    pub active: RgbaColor,
    /// Inactive document tab background.
    pub tab_inactive: RgbaColor,
    /// Active document tab background.
    pub tab_active: RgbaColor,
    /// Bottom status bar surface.
    pub statusbar: RgbaColor,
    /// Text input field background.
    pub input: RgbaColor,
    /// Resting overlay scrollbar thumb color.
    pub scrollbar_thumb: RgbaColor,
    /// Hovered or dragged scrollbar thumb color.
    pub scrollbar_thumb_hover: RgbaColor,
}

/// UI typography color tokens (`[text]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTextTokens {
    /// Primary labels, file names, active tabs, and headings.
    pub primary: RgbaColor,
    /// Secondary labels, package paths, and status metadata.
    pub secondary: RgbaColor,
    /// Placeholders, disabled labels, and shortcut hints.
    pub muted: RgbaColor,
    /// Foreground text placed on primary/focus accent fills.
    pub inverse: RgbaColor,
}

/// Structural border, divider, and keyboard focus ring tokens (`[borders]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BorderTokens {
    /// Outer `1px` border around each Islands UI container.
    pub island: RgbaColor,
    /// Subtle internal dividers between tab bars, headers, and sections.
    pub subtle: RgbaColor,
    /// Stronger borders for elevated popups and secondary controls.
    pub strong: RgbaColor,
    /// Keyboard focus ring outline (`Omarchy` keyboard-first navigation).
    pub focus: RgbaColor,
    /// Active tab indicator bar.
    pub tab_indicator: RgbaColor,
    /// Resting border for text inputs and select boxes.
    pub input: RgbaColor,
}

/// Code editor viewport, selection, inlay hint, and diagnostic tokens (`[editor]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorTokens {
    /// Main text area background.
    pub background: RgbaColor,
    /// Default code foreground color.
    pub foreground: RgbaColor,
    /// Current cursor line background highlight.
    pub line_highlight: RgbaColor,
    /// Active selection background.
    pub selection: RgbaColor,
    /// Inactive split selection background.
    pub selection_inactive: RgbaColor,
    /// Text cursor / Vim block color.
    pub cursor: RgbaColor,
    /// Vertical indentation guides.
    pub indent_guide: RgbaColor,
    /// Active scope indentation guide.
    pub indent_guide_active: RgbaColor,
    /// Visible whitespace glyphs.
    pub whitespace: RgbaColor,
    /// Matching bracket background highlight.
    pub bracket_match_bg: RgbaColor,
    /// Matching bracket border outline.
    pub bracket_match_border: RgbaColor,
    /// Non-active search match highlight.
    pub find_match: RgbaColor,
    /// Current search match highlight.
    pub find_match_current: RgbaColor,
    /// Inline inlay hint pill background.
    pub inlay_hint_bg: RgbaColor,
    /// Inline inlay hint text color.
    pub inlay_hint_fg: RgbaColor,
    /// Compiler / LSP error underline color.
    pub diagnostic_error: RgbaColor,
    /// Compiler / LSP warning underline color.
    pub diagnostic_warning: RgbaColor,
    /// Compiler / LSP info underline color.
    pub diagnostic_info: RgbaColor,
    /// Compiler / LSP hint underline color.
    pub diagnostic_hint: RgbaColor,
}

/// Editor gutter tokens (`[gutter]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GutterTokens {
    /// Gutter background color.
    pub background: RgbaColor,
    /// Inactive line numbers.
    pub line_number: RgbaColor,
    /// Line numbers on rows containing active cursors.
    pub line_number_active: RgbaColor,
    /// Code folding chevron icons.
    pub fold_marker: RgbaColor,
    /// Runnable `main` / `@Test` play icon (`▶`).
    pub run_icon: RgbaColor,
    /// Debug launch icon.
    pub debug_icon: RgbaColor,
    /// Active JVM debugger breakpoint dot.
    pub breakpoint: RgbaColor,
    /// Git added lines bar.
    pub git_added: RgbaColor,
    /// Git modified lines bar.
    pub git_modified: RgbaColor,
    /// Git deleted lines marker.
    pub git_deleted: RgbaColor,
}

/// Terminal background, cursor, selection, and ANSI 16 palette (`[terminal]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalPalette {
    /// Terminal viewport background.
    pub background: RgbaColor,
    /// Default terminal text foreground.
    pub foreground: RgbaColor,
    /// Terminal cursor color.
    pub cursor: RgbaColor,
    /// Text color inside a solid terminal cursor block.
    pub cursor_text: RgbaColor,
    /// Terminal text selection background.
    pub selection_bg: RgbaColor,
    /// Terminal text selection foreground.
    pub selection_fg: RgbaColor,
    /// ANSI color 0 (`black`).
    pub black: RgbaColor,
    /// ANSI color 1 (`red`).
    pub red: RgbaColor,
    /// ANSI color 2 (`green`).
    pub green: RgbaColor,
    /// ANSI color 3 (`yellow`).
    pub yellow: RgbaColor,
    /// ANSI color 4 (`blue`).
    pub blue: RgbaColor,
    /// ANSI color 5 (`magenta`).
    pub magenta: RgbaColor,
    /// ANSI color 6 (`cyan`).
    pub cyan: RgbaColor,
    /// ANSI color 7 (`white`).
    pub white: RgbaColor,
    /// ANSI color 8 (`bright_black`).
    pub bright_black: RgbaColor,
    /// ANSI color 9 (`bright_red`).
    pub bright_red: RgbaColor,
    /// ANSI color 10 (`bright_green`).
    pub bright_green: RgbaColor,
    /// ANSI color 11 (`bright_yellow`).
    pub bright_yellow: RgbaColor,
    /// ANSI color 12 (`bright_blue`).
    pub bright_blue: RgbaColor,
    /// ANSI color 13 (`bright_magenta`).
    pub bright_magenta: RgbaColor,
    /// ANSI color 14 (`bright_cyan`).
    pub bright_cyan: RgbaColor,
    /// ANSI color 15 (`bright_white`).
    pub bright_white: RgbaColor,
}

impl TerminalPalette {
    /// Looks up one of the 16 standard ANSI colors by index (`0..=15`).
    #[must_use]
    pub const fn ansi(&self, index: u8) -> Option<RgbaColor> {
        match index {
            0 => Some(self.black),
            1 => Some(self.red),
            2 => Some(self.green),
            3 => Some(self.yellow),
            4 => Some(self.blue),
            5 => Some(self.magenta),
            6 => Some(self.cyan),
            7 => Some(self.white),
            8 => Some(self.bright_black),
            9 => Some(self.bright_red),
            10 => Some(self.bright_green),
            11 => Some(self.bright_yellow),
            12 => Some(self.bright_blue),
            13 => Some(self.bright_magenta),
            14 => Some(self.bright_cyan),
            15 => Some(self.bright_white),
            _ => None,
        }
    }
}
