//! Headless Markdown AST, view modes, and deterministic parser (`KOVA-057`).
//!
//! This module lives in [`crate`] (`kova_core`) with zero windowing or GPU
//! dependencies so it can be tested and reused headlessly.

pub mod parser;

use std::path::Path;

pub use parser::parse_markdown;

/// Returns `true` when `path` has a `.md` or `.markdown` extension (ASCII case-insensitive).
#[must_use]
pub fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

/// Display mode for a Markdown document tab inside an editor pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum MarkdownViewMode {
    /// Show only the source text editor.
    Editor,
    /// Show the source text editor on the left and live preview on the right.
    #[default]
    Split,
    /// Show only the native rendered Markdown preview.
    Preview,
}

impl MarkdownViewMode {
    /// All view modes in segmented-control display order (`Editor | Split | Preview`).
    pub const ALL: [Self; 3] = [Self::Editor, Self::Split, Self::Preview];

    /// Short human-readable label shown in the tab-bar mode switcher.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Editor => "Editor",
            Self::Split => "Split",
            Self::Preview => "Preview",
        }
    }
}

/// Parsed Markdown document AST produced by [`parse_markdown`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarkdownDocument {
    /// Top-level block elements in document order.
    pub blocks: Vec<MarkdownBlock>,
}

/// Block-level element in a [`MarkdownDocument`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    /// ATX heading (`#` through `######`).
    Heading {
        /// Heading level in `1..=6`.
        level: u8,
        /// Inline spans inside the heading.
        spans: Vec<InlineSpan>,
    },
    /// Paragraph of inline spans.
    Paragraph(Vec<InlineSpan>),
    /// Fenced code block (```` ```lang ```` or `~~~lang`).
    CodeBlock {
        /// Optional normalized language identifier from the opening fence info string.
        language: Option<String>,
        /// Raw code body lines joined with `\n`.
        code: String,
    },
    /// Block quote (`> ...`), supporting nested blocks.
    BlockQuote(Vec<MarkdownBlock>),
    /// Contiguous list of bullet, ordered, or task checklist items.
    List(Vec<ListItem>),
    /// GitHub-Flavored Markdown table.
    Table(TableBlock),
    /// Horizontal thematic break (`---`, `***`, `___`).
    ThematicBreak,
}

/// Single item inside a [`MarkdownBlock::List`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// Marker kind (bullet, ordered number, or GFM task checkbox).
    pub kind: ListMarkerKind,
    /// Nesting level (`0` for top-level items, increasing with indentation).
    pub indent: u8,
    /// Inline spans forming the item text.
    pub spans: Vec<InlineSpan>,
}

/// Marker kind for a [`ListItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMarkerKind {
    /// Unordered bullet (`-`, `*`, `+`).
    Bullet,
    /// Ordered item (`1.`, `2)`, etc.) carrying its parsed starting/display number.
    Ordered(usize),
    /// GFM task list item (`- [ ]` or `- [x]`).
    Task {
        /// Whether the checkbox is checked (`[x]` / `[X]`).
        checked: bool,
    },
}

/// Horizontal text alignment for a GFM [`TableBlock`] column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TableAlignment {
    /// Left-aligned column (`:---` or `---`).
    #[default]
    Left,
    /// Center-aligned column (`:---:`).
    Center,
    /// Right-aligned column (`---:`).
    Right,
}

/// GitHub-Flavored Markdown table block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableBlock {
    /// Column alignments parsed from the delimiter row.
    pub alignments: Vec<TableAlignment>,
    /// Header cells, one `Vec<InlineSpan>` per column.
    pub headers: Vec<Vec<InlineSpan>>,
    /// Data rows, each padded/truncated to `alignments.len()` columns.
    pub rows: Vec<Vec<Vec<InlineSpan>>>,
}

/// Inline formatting span inside headings, paragraphs, list items, and table cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineSpan {
    /// Unstyled plain text.
    Text(String),
    /// Bold text (`**...**` or `__...__`).
    Bold(Vec<InlineSpan>),
    /// Italic text (`*...*` or `_..._`).
    Italic(Vec<InlineSpan>),
    /// Strikethrough text (`~~...~~`).
    Strikethrough(Vec<InlineSpan>),
    /// Inline monospace code span (`` `...` ``).
    Code(String),
    /// Hyperlink (`[label](url)`).
    Link {
        /// Visible link label spans.
        label: Vec<InlineSpan>,
        /// Target URL or relative file path.
        url: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_markdown_extensions_case_insensitively() {
        assert!(is_markdown_path(Path::new("README.md")));
        assert!(is_markdown_path(Path::new("docs/SPEC.MD")));
        assert!(is_markdown_path(Path::new("CHANGELOG.markdown")));
        assert!(is_markdown_path(Path::new("notes/Guide.MarkDown")));

        assert!(!is_markdown_path(Path::new("README")));
        assert!(!is_markdown_path(Path::new("notes.txt")));
        assert!(!is_markdown_path(Path::new("App.java")));
        assert!(!is_markdown_path(Path::new("component.mdx")));
        assert!(!is_markdown_path(Path::new(".md")));
    }

    #[test]
    fn view_mode_defaults_to_split_and_exposes_labels() {
        assert_eq!(MarkdownViewMode::default(), MarkdownViewMode::Split);
        assert_eq!(
            MarkdownViewMode::ALL,
            [
                MarkdownViewMode::Editor,
                MarkdownViewMode::Split,
                MarkdownViewMode::Preview,
            ]
        );
        assert_eq!(MarkdownViewMode::Editor.label(), "Editor");
        assert_eq!(MarkdownViewMode::Split.label(), "Split");
        assert_eq!(MarkdownViewMode::Preview.label(), "Preview");
    }
}
