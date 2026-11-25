//! Edit descriptors and immutable syntax tree snapshots (`SyntaxEdit`, `SyntaxTree`).

use std::sync::Arc;
use std::time::Duration;

use kova_buffer::{
    BufferError, BufferId, BufferSnapshot, BufferVersion, Point, TextEdit, TreeSitterEdit,
    VersionChanges,
};
use kova_theme::SyntaxLanguage;

use crate::input::to_ts_point;

/// Byte- and point-based edit descriptor mapping 1:1 to `tree_sitter::InputEdit`
/// and [`kova_buffer::TreeSitterEdit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxEdit {
    /// Byte offset where the edit begins.
    pub start_byte: usize,
    /// Byte offset where the replaced range ended before the edit.
    pub old_end_byte: usize,
    /// Byte offset where the replacement text ends after the edit.
    pub new_end_byte: usize,
    /// Zero-based `(row, byte_column)` where the edit begins.
    pub start_point: Point,
    /// Zero-based `(row, byte_column)` where the replaced range ended before the edit.
    pub old_end_point: Point,
    /// Zero-based `(row, byte_column)` where the replacement text ends after the edit.
    pub new_end_point: Point,
}

impl SyntaxEdit {
    /// Computes a [`SyntaxEdit`] from a pre-edit [`BufferSnapshot`] and a [`TextEdit`].
    ///
    /// # Errors
    ///
    /// Returns [`BufferError`] if `edit.range` is out of bounds, inverted (`start > end`),
    /// splits a multi-byte UTF-8 character, or splits an atomic `\r\n` sequence.
    pub fn from_text_edit(before: &BufferSnapshot, edit: &TextEdit) -> Result<Self, BufferError> {
        let len = before.len_bytes();
        if edit.range.start > edit.range.end || edit.range.end > len {
            return Err(BufferError::InvalidByteRange {
                start: edit.range.start,
                end: edit.range.end,
                len,
            });
        }

        let start_byte = edit.range.start;
        let old_end_byte = edit.range.end;
        let start_point = before.byte_to_point(start_byte)?;
        let old_end_point = before.byte_to_point(old_end_byte)?;
        let new_end_byte = start_byte + edit.replacement.len();

        let new_end_point = if let Some(last_nl) = edit.replacement.rfind('\n') {
            let newlines = edit.replacement.matches('\n').count();
            let trailing_col = edit.replacement.len() - last_nl - 1;
            Point::new(start_point.row + newlines, trailing_col)
        } else {
            Point::new(start_point.row, start_point.column + edit.replacement.len())
        };

        Ok(Self {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_point,
            old_end_point,
            new_end_point,
        })
    }

    /// Converts incremental [`VersionChanges`] from [`kova_buffer::EditLog`] into sequential
    /// [`SyntaxEdit`]s, returning `None` when [`VersionChanges::FullResyncRequired`] is signalled.
    #[must_use]
    pub fn from_version_changes(changes: &VersionChanges<'_>) -> Option<Vec<Self>> {
        changes
            .collect_tree_sitter_edits()
            .map(|edits| edits.into_iter().map(Self::from).collect())
    }
}

impl From<SyntaxEdit> for tree_sitter::InputEdit {
    fn from(edit: SyntaxEdit) -> Self {
        Self::from(&edit)
    }
}

impl From<&SyntaxEdit> for tree_sitter::InputEdit {
    fn from(edit: &SyntaxEdit) -> Self {
        Self {
            start_byte: edit.start_byte,
            old_end_byte: edit.old_end_byte,
            new_end_byte: edit.new_end_byte,
            start_position: to_ts_point(edit.start_point),
            old_end_position: to_ts_point(edit.old_end_point),
            new_end_position: to_ts_point(edit.new_end_point),
        }
    }
}

impl From<TreeSitterEdit> for SyntaxEdit {
    fn from(edit: TreeSitterEdit) -> Self {
        Self {
            start_byte: edit.start_byte,
            old_end_byte: edit.old_end_byte,
            new_end_byte: edit.new_end_byte,
            start_point: edit.start_point,
            old_end_point: edit.old_end_point,
            new_end_point: edit.new_end_point,
        }
    }
}

impl From<&TreeSitterEdit> for SyntaxEdit {
    fn from(edit: &TreeSitterEdit) -> Self {
        Self::from(*edit)
    }
}

impl From<SyntaxEdit> for TreeSitterEdit {
    fn from(edit: SyntaxEdit) -> Self {
        Self {
            start_byte: edit.start_byte,
            old_end_byte: edit.old_end_byte,
            new_end_byte: edit.new_end_byte,
            start_point: edit.start_point,
            old_end_point: edit.old_end_point,
            new_end_point: edit.new_end_point,
        }
    }
}

/// Immutable, cheaply cloneable Tree-sitter syntax tree snapshot bound to a [`BufferId`] and [`BufferVersion`].
///
/// Cloning shares the internal [`Arc<tree_sitter::Tree>`] in $O(1)$ time without locking, allowing
/// GPUI renderers to read an earlier version while `rayon` parses newer edits concurrently.
#[derive(Debug, Clone)]
pub struct SyntaxTree {
    buffer_id: BufferId,
    version: BufferVersion,
    language: SyntaxLanguage,
    tree: Arc<tree_sitter::Tree>,
    parse_duration: Duration,
    incremental: bool,
}

impl SyntaxTree {
    /// Wraps a parsed [`tree_sitter::Tree`] in an immutable [`SyntaxTree`] snapshot.
    #[must_use]
    pub fn new(
        buffer_id: BufferId,
        version: BufferVersion,
        language: SyntaxLanguage,
        tree: tree_sitter::Tree,
        parse_duration: Duration,
        incremental: bool,
    ) -> Self {
        Self {
            buffer_id,
            version,
            language,
            tree: Arc::new(tree),
            parse_duration,
            incremental,
        }
    }

    /// Returns the identifier of the source buffer.
    #[must_use]
    pub const fn buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    /// Returns the buffer revision represented by this syntax tree.
    #[must_use]
    pub const fn version(&self) -> BufferVersion {
        self.version
    }

    /// Returns the language grammar used to build this syntax tree.
    #[must_use]
    pub const fn language(&self) -> SyntaxLanguage {
        self.language
    }

    /// Grants read-only access to the underlying [`tree_sitter::Tree`].
    #[must_use]
    pub fn tree(&self) -> &tree_sitter::Tree {
        &self.tree
    }

    /// Returns the root [`tree_sitter::Node`] of the syntax tree.
    #[must_use]
    pub fn root_node(&self) -> tree_sitter::Node<'_> {
        self.tree.root_node()
    }

    /// Returns `true` if the syntax tree contains any syntax or missing-token errors.
    #[must_use]
    pub fn has_error(&self) -> bool {
        self.tree.root_node().has_error()
    }

    /// Returns the wall-clock duration spent parsing this snapshot.
    #[must_use]
    pub const fn parse_duration(&self) -> Duration {
        self.parse_duration
    }

    /// Returns `true` if this tree was produced via incremental re-parsing against an earlier tree.
    #[must_use]
    pub const fn was_incremental(&self) -> bool {
        self.incremental
    }

    /// Clones the underlying [`tree_sitter::Tree`] (shallow structural copy) and applies `edits`
    /// sequentially without mutating `self`.
    #[must_use]
    pub fn with_edits(&self, edits: &[SyntaxEdit]) -> tree_sitter::Tree {
        let mut cloned = self.tree.as_ref().clone();
        for edit in edits {
            let ts_edit = tree_sitter::InputEdit::from(edit);
            cloned.edit(&ts_edit);
        }
        cloned
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "unit tests assert deterministic edit descriptors and immutable tree clones"
)]
mod tests {
    use kova_buffer::Buffer;

    use super::*;

    #[test]
    fn syntax_edit_from_text_edit_handles_multiline_crlf_and_multibyte() {
        let mut buffer = Buffer::from_text("{\"a\": \"hello\",\r\n \"b\": 1}\r\n");
        let before = buffer.snapshot();

        let edit = TextEdit::new(7..12, "zażółć\r\n🚀");
        let syntax_edit = SyntaxEdit::from_text_edit(&before, &edit).unwrap();

        buffer.apply_edits(std::slice::from_ref(&edit)).unwrap();
        let log_edits = buffer.tree_sitter_edits_since(before.version()).unwrap();
        assert_eq!(log_edits.len(), 1);
        assert_eq!(SyntaxEdit::from(log_edits[0]), syntax_edit);
    }

    #[test]
    fn with_edits_leaves_original_syntax_tree_unchanged() {
        let source = "{\"a\": 1}";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_json::LANGUAGE.into())
            .unwrap();
        let ts_tree = parser.parse(source, None).unwrap();

        let tree = SyntaxTree::new(
            BufferId::new(1),
            BufferVersion::new(0),
            SyntaxLanguage::Json,
            ts_tree,
            Duration::from_micros(50),
            false,
        );
        let original_end = tree.root_node().end_byte();
        assert_eq!(original_end, source.len());

        let edited = tree.with_edits(&[SyntaxEdit {
            start_byte: 6,
            old_end_byte: 7,
            new_end_byte: 10,
            start_point: Point::new(0, 6),
            old_end_point: Point::new(0, 7),
            new_end_point: Point::new(0, 10),
        }]);

        assert_eq!(edited.root_node().end_byte(), 11);
        // Original `SyntaxTree` remains completely immutable.
        assert_eq!(tree.root_node().end_byte(), source.len());
    }
}
