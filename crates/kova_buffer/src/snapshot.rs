//! Immutable `BufferSnapshot` backed by `Arc<Rope>` with $O(1)$ cloning.

use std::ops::Range;
use std::sync::Arc;

use ropey::{Rope, RopeSlice};

use crate::error::BufferError;
use crate::line_ending::{LineEnding, line_content_len_bytes, line_content_len_utf16};
use crate::position::{BufferId, BufferVersion, Point, Utf16Point};

/// An immutable, lock-free snapshot of a [`Buffer`](crate::Buffer) at a specific [`BufferVersion`].
///
/// Cloning a `BufferSnapshot` increments the internal `Arc<Rope>` reference count
/// in $O(1)$ time without copying text, making it safe and cheap to hand off across
/// `rayon` CPU workers, `tokio` async tasks, and `wasmtime` plugins.
#[derive(Debug, Clone)]
pub struct BufferSnapshot {
    id: BufferId,
    version: BufferVersion,
    rope: Arc<Rope>,
    line_ending: LineEnding,
}

impl BufferSnapshot {
    /// Creates a snapshot from parts.
    #[must_use]
    pub(crate) fn new(
        id: BufferId,
        version: BufferVersion,
        rope: Arc<Rope>,
        line_ending: LineEnding,
    ) -> Self {
        Self {
            id,
            version,
            rope,
            line_ending,
        }
    }

    /// Returns the identifier of the source buffer.
    #[must_use]
    pub const fn id(&self) -> BufferId {
        self.id
    }

    /// Returns the revision number captured by this snapshot.
    #[must_use]
    pub const fn version(&self) -> BufferVersion {
        self.version
    }

    /// Returns the buffer's active line ending style.
    #[must_use]
    pub const fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Grants read-only access to the underlying [`Rope`].
    #[must_use]
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// Returns the total byte length of the buffer.
    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Returns the total number of Unicode scalar values (`char`s) in the buffer.
    #[must_use]
    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Returns the total number of UTF-16 code units in the buffer.
    #[must_use]
    pub fn len_utf16(&self) -> usize {
        self.rope.len_utf16_cu()
    }

    /// Returns the total number of lines in the buffer (at least `1`, even when empty).
    #[must_use]
    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Returns `true` if the buffer contains zero bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rope.len_bytes() == 0
    }

    /// Materializes the entire snapshot as an owned `String`.
    #[must_use]
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Validates that `byte` is within bounds, lies on a UTF-8 char boundary,
    /// and does not split an atomic `\r\n` sequence.
    pub(crate) fn validate_byte_boundary(&self, byte: usize) -> Result<(), BufferError> {
        let len = self.len_bytes();
        if byte > len {
            return Err(BufferError::ByteOffsetOutOfBounds { byte, len });
        }
        let char_idx = self.rope.byte_to_char(byte);
        if self.rope.char_to_byte(char_idx) != byte {
            return Err(BufferError::NotCharBoundary { byte });
        }
        if byte > 0
            && byte < len
            && self.rope.get_byte(byte - 1) == Some(b'\r')
            && self.rope.get_byte(byte) == Some(b'\n')
        {
            return Err(BufferError::SplitsCrLf { byte });
        }
        Ok(())
    }

    /// Returns a [`RopeSlice`] covering `range` (in UTF-8 bytes).
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::InvalidByteRange`] if `range.start > range.end` or
    /// `range.end > len_bytes()`, [`BufferError::NotCharBoundary`] if either bound
    /// splits a multi-byte UTF-8 character, or [`BufferError::SplitsCrLf`] if either
    /// bound lies strictly between `\r` and `\n`.
    pub fn slice_bytes(&self, range: Range<usize>) -> Result<RopeSlice<'_>, BufferError> {
        let len = self.len_bytes();
        if range.start > range.end || range.end > len {
            return Err(BufferError::InvalidByteRange {
                start: range.start,
                end: range.end,
                len,
            });
        }
        self.validate_byte_boundary(range.start)?;
        self.validate_byte_boundary(range.end)?;
        let start_char = self.rope.byte_to_char(range.start);
        let end_char = self.rope.byte_to_char(range.end);
        Ok(self.rope.slice(start_char..end_char))
    }

    /// Returns the content slice of 0-based line `row` excluding its trailing `\n` or `\r\n`.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::RowOutOfBounds`] when `row >= len_lines()`.
    pub fn line_slice(&self, row: usize) -> Result<RopeSlice<'_>, BufferError> {
        let line = self.line_slice_with_ending(row)?;
        let content_bytes = line_content_len_bytes(line);
        let content_chars = line.byte_to_char(content_bytes);
        Ok(line.slice(..content_chars))
    }

    /// Returns the slice of 0-based line `row` including any trailing `\n` or `\r\n`.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::RowOutOfBounds`] when `row >= len_lines()`.
    pub fn line_slice_with_ending(&self, row: usize) -> Result<RopeSlice<'_>, BufferError> {
        let line_count = self.len_lines();
        self.rope
            .get_line(row)
            .ok_or(BufferError::RowOutOfBounds { row, line_count })
    }

    /// Returns the byte length of line `row` excluding any trailing `\n` or `\r\n`.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::RowOutOfBounds`] when `row >= len_lines()`.
    pub fn line_len_bytes(&self, row: usize) -> Result<usize, BufferError> {
        Ok(line_content_len_bytes(self.line_slice_with_ending(row)?))
    }

    /// Returns the UTF-16 code-unit length of line `row` excluding any trailing `\n` or `\r\n`.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::RowOutOfBounds`] when `row >= len_lines()`.
    pub fn line_len_utf16(&self, row: usize) -> Result<usize, BufferError> {
        Ok(line_content_len_utf16(self.line_slice_with_ending(row)?))
    }

    /// Returns the raw byte at `byte`.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::ByteOffsetOutOfBounds`] when `byte >= len_bytes()`.
    pub fn byte_at(&self, byte: usize) -> Result<u8, BufferError> {
        let len = self.len_bytes();
        self.rope
            .get_byte(byte)
            .ok_or(BufferError::ByteOffsetOutOfBounds { byte, len })
    }

    /// Returns the `char` starting at byte offset `byte`.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::ByteOffsetOutOfBounds`] when `byte >= len_bytes()`,
    /// [`BufferError::NotCharBoundary`] if `byte` is inside a multi-byte UTF-8 sequence,
    /// or [`BufferError::SplitsCrLf`] if `byte` points at `\n` inside `\r\n`.
    pub fn char_at(&self, byte: usize) -> Result<char, BufferError> {
        let len = self.len_bytes();
        if byte >= len {
            return Err(BufferError::ByteOffsetOutOfBounds { byte, len });
        }
        self.validate_byte_boundary(byte)?;
        let char_idx = self.rope.byte_to_char(byte);
        Ok(self.rope.char(char_idx))
    }

    /// Converts a byte offset to a `char` offset.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::ByteOffsetOutOfBounds`], [`BufferError::NotCharBoundary`],
    /// or [`BufferError::SplitsCrLf`] for invalid offsets.
    pub fn byte_to_char(&self, byte: usize) -> Result<usize, BufferError> {
        self.validate_byte_boundary(byte)?;
        Ok(self.rope.byte_to_char(byte))
    }

    /// Converts a `char` offset to a byte offset.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::CharOffsetOutOfBounds`] if `char_idx > len_chars()`,
    /// or [`BufferError::SplitsCrLf`] if `char_idx` points strictly between `\r` and `\n`.
    pub fn char_to_byte(&self, char_idx: usize) -> Result<usize, BufferError> {
        let len = self.len_chars();
        if char_idx > len {
            return Err(BufferError::CharOffsetOutOfBounds { char_idx, len });
        }
        let byte = self.rope.char_to_byte(char_idx);
        self.validate_byte_boundary(byte)?;
        Ok(byte)
    }

    /// Converts a byte offset to a global UTF-16 code-unit offset.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::ByteOffsetOutOfBounds`], [`BufferError::NotCharBoundary`],
    /// or [`BufferError::SplitsCrLf`] for invalid offsets.
    pub fn byte_to_utf16(&self, byte: usize) -> Result<usize, BufferError> {
        let char_idx = self.byte_to_char(byte)?;
        Ok(self.rope.char_to_utf16_cu(char_idx))
    }

    /// Converts a global UTF-16 code-unit offset to a byte offset.
    ///
    /// If `utf16_idx` falls on the trailing surrogate of a 4-byte UTF-8 character
    /// (such as an emoji), it snaps down to the starting byte of that character,
    /// matching `ropey::Rope::utf16_cu_to_char` semantics.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::Utf16OffsetOutOfBounds`] if `utf16_idx > len_utf16()`,
    /// or [`BufferError::SplitsCrLf`] if `utf16_idx` points strictly between `\r` and `\n`.
    pub fn utf16_to_byte(&self, utf16_idx: usize) -> Result<usize, BufferError> {
        let len = self.len_utf16();
        if utf16_idx > len {
            return Err(BufferError::Utf16OffsetOutOfBounds { utf16_idx, len });
        }
        let char_idx = self.rope.utf16_cu_to_char(utf16_idx);
        let byte = self.rope.char_to_byte(char_idx);
        self.validate_byte_boundary(byte)?;
        Ok(byte)
    }

    /// Converts a byte offset to a [`Point`] (0-based row and UTF-8 byte column).
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::ByteOffsetOutOfBounds`], [`BufferError::NotCharBoundary`],
    /// or [`BufferError::SplitsCrLf`] for invalid offsets.
    pub fn byte_to_point(&self, byte: usize) -> Result<Point, BufferError> {
        self.validate_byte_boundary(byte)?;
        let row = self.rope.byte_to_line(byte);
        let line_start = self.rope.line_to_byte(row);
        Ok(Point::new(row, byte - line_start))
    }

    /// Converts a [`Point`] (0-based row and UTF-8 byte column) to a byte offset.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::RowOutOfBounds`], [`BufferError::ColumnOutOfBounds`]
    /// (when `point.column` exceeds the line's content length excluding `\n`/`\r\n`),
    /// or [`BufferError::NotCharBoundary`].
    pub fn point_to_byte(&self, point: Point) -> Result<usize, BufferError> {
        let max_column = self.line_len_bytes(point.row)?;
        if point.column > max_column {
            return Err(BufferError::ColumnOutOfBounds {
                row: point.row,
                column: point.column,
                max_column,
            });
        }
        let byte = self.rope.line_to_byte(point.row) + point.column;
        self.validate_byte_boundary(byte)?;
        Ok(byte)
    }

    /// Converts a byte offset to a [`Utf16Point`] (0-based row and UTF-16 code-unit column).
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::ByteOffsetOutOfBounds`], [`BufferError::NotCharBoundary`],
    /// or [`BufferError::SplitsCrLf`] for invalid offsets.
    pub fn byte_to_utf16_point(&self, byte: usize) -> Result<Utf16Point, BufferError> {
        self.validate_byte_boundary(byte)?;
        let row = self.rope.byte_to_line(byte);
        let line_start_char = self.rope.line_to_char(row);
        let char_idx = self.rope.byte_to_char(byte);
        let line_start_utf16 = self.rope.char_to_utf16_cu(line_start_char);
        let col_utf16 = self.rope.char_to_utf16_cu(char_idx) - line_start_utf16;
        Ok(Utf16Point::new(row, col_utf16))
    }

    /// Converts a [`Utf16Point`] (0-based row and UTF-16 code-unit column) to a byte offset.
    ///
    /// If `point.column` points at the trailing surrogate of a surrogate pair, it snaps
    /// down to the starting byte of that character, matching [`Self::utf16_to_byte`].
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::RowOutOfBounds`] or [`BufferError::ColumnOutOfBounds`]
    /// when `point.column` exceeds the line's UTF-16 content length (excluding `\n`/`\r\n`).
    pub fn utf16_point_to_byte(&self, point: Utf16Point) -> Result<usize, BufferError> {
        let max_column = self.line_len_utf16(point.row)?;
        if point.column > max_column {
            return Err(BufferError::ColumnOutOfBounds {
                row: point.row,
                column: point.column,
                max_column,
            });
        }
        let line_start_char = self.rope.line_to_char(point.row);
        let line_start_utf16 = self.rope.char_to_utf16_cu(line_start_char);
        let char_idx = self.rope.utf16_cu_to_char(line_start_utf16 + point.column);
        Ok(self.rope.char_to_byte(char_idx))
    }

    /// Converts a byte [`Point`] to a [`Utf16Point`].
    ///
    /// # Errors
    ///
    /// Propagates validation errors from [`Self::point_to_byte`] and [`Self::byte_to_utf16_point`].
    pub fn point_to_utf16_point(&self, point: Point) -> Result<Utf16Point, BufferError> {
        let byte = self.point_to_byte(point)?;
        self.byte_to_utf16_point(byte)
    }

    /// Converts a [`Utf16Point`] to a byte [`Point`].
    ///
    /// # Errors
    ///
    /// Propagates validation errors from [`Self::utf16_point_to_byte`] and [`Self::byte_to_point`].
    pub fn utf16_point_to_point(&self, point: Utf16Point) -> Result<Point, BufferError> {
        let byte = self.utf16_point_to_byte(point)?;
        self.byte_to_point(byte)
    }

    /// Converts a `char` offset to a byte [`Point`].
    ///
    /// # Errors
    ///
    /// Propagates errors from [`Self::char_to_byte`] and [`Self::byte_to_point`].
    pub fn char_to_point(&self, char_idx: usize) -> Result<Point, BufferError> {
        let byte = self.char_to_byte(char_idx)?;
        self.byte_to_point(byte)
    }

    /// Converts a byte [`Point`] to a `char` offset.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`Self::point_to_byte`] and [`Self::byte_to_char`].
    pub fn point_to_char(&self, point: Point) -> Result<usize, BufferError> {
        let byte = self.point_to_byte(point)?;
        self.byte_to_char(byte)
    }

    /// Clamps `byte` into `0..=len_bytes()`, snapping down to a UTF-8 char boundary
    /// and backing out of the middle of an atomic `\r\n` sequence to the `\r` offset.
    #[must_use]
    pub fn clamp_byte_to_char_boundary(&self, byte: usize) -> usize {
        let len = self.len_bytes();
        let bounded = byte.min(len);
        let char_idx = self.rope.byte_to_char(bounded);
        let mut aligned = self.rope.char_to_byte(char_idx);
        if aligned > 0
            && aligned < len
            && self.rope.get_byte(aligned - 1) == Some(b'\r')
            && self.rope.get_byte(aligned) == Some(b'\n')
        {
            aligned -= 1;
        }
        aligned
    }

    /// Clamps `point` to the closest valid [`Point`] not exceeding `point`,
    /// aligned to a UTF-8 character boundary and line content bounds.
    #[must_use]
    pub fn clamp_point(&self, point: Point) -> Point {
        let last_row = self.len_lines().saturating_sub(1);
        let row = point.row.min(last_row);
        let line = self.rope.line(row);
        let max_col = line_content_len_bytes(line);
        let line_start = self.rope.line_to_byte(row);
        let target_byte = line_start + point.column.min(max_col);
        let clamped_byte = self.clamp_byte_to_char_boundary(target_byte);
        Point::new(row, clamped_byte - line_start)
    }
}
