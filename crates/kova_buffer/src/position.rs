//! Identifiers, versioning, coordinate types, ranges, and atomic text edits.

use std::ops::Range;

/// Opaque identifier of a buffer instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct BufferId(u64);

impl BufferId {
    /// Creates a [`BufferId`] from a raw `u64`.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the underlying `u64` identifier.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Monotonically increasing buffer revision counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct BufferVersion(u64);

impl BufferVersion {
    /// Creates a [`BufferVersion`] from a raw `u64`.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the underlying `u64` revision number.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Returns the next sequential version (`self + 1`).
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Zero-based line (`row`) and UTF-8 byte column (`column`) within that line.
///
/// Maps 1:1 to `tree_sitter::Point`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Point {
    /// Zero-based line index.
    pub row: usize,
    /// Zero-based UTF-8 byte offset from the start of `row`.
    pub column: usize,
}

impl Point {
    /// Creates a new [`Point`].
    #[must_use]
    pub const fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }

    /// Returns `Point { row: 0, column: 0 }`.
    #[must_use]
    pub const fn zero() -> Self {
        Self { row: 0, column: 0 }
    }
}

/// Zero-based line (`row`) and UTF-16 code-unit column (`column`) within that line.
///
/// Maps 1:1 to LSP `Position` (`line`, `character`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Utf16Point {
    /// Zero-based line index.
    pub row: usize,
    /// Zero-based UTF-16 code-unit offset from the start of `row`.
    pub column: usize,
}

impl Utf16Point {
    /// Creates a new [`Utf16Point`].
    #[must_use]
    pub const fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }

    /// Returns `Utf16Point { row: 0, column: 0 }`.
    #[must_use]
    pub const fn zero() -> Self {
        Self { row: 0, column: 0 }
    }
}

/// Half-open range `[start, end)` expressed in [`Point`] coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PointRange {
    /// Inclusive start point.
    pub start: Point,
    /// Exclusive end point.
    pub end: Point,
}

impl PointRange {
    /// Creates a [`PointRange`] from `start` and `end`.
    #[must_use]
    pub const fn new(start: Point, end: Point) -> Self {
        Self { start, end }
    }

    /// Returns `true` when `start == end`.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.row == self.end.row && self.start.column == self.end.column
    }

    /// Returns `true` if `point` lies within `[start, end)`.
    #[must_use]
    pub fn contains_point(self, point: Point) -> bool {
        self.start <= point && point < self.end
    }
}

/// Half-open range `[start, end)` expressed in [`Utf16Point`] coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Utf16Range {
    /// Inclusive start UTF-16 position.
    pub start: Utf16Point,
    /// Exclusive end UTF-16 position.
    pub end: Utf16Point,
}

impl Utf16Range {
    /// Creates a [`Utf16Range`] from `start` and `end`.
    #[must_use]
    pub const fn new(start: Utf16Point, end: Utf16Point) -> Self {
        Self { start, end }
    }

    /// Returns `true` when `start == end`.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.row == self.end.row && self.start.column == self.end.column
    }

    /// Returns `true` if `point` lies within `[start, end)`.
    #[must_use]
    pub fn contains_point(self, point: Utf16Point) -> bool {
        self.start <= point && point < self.end
    }
}

/// An atomic replacement of a byte range in pre-edit buffer coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    /// Byte range to replace in the pre-edit snapshot.
    pub range: Range<usize>,
    /// Replacement UTF-8 text.
    pub replacement: String,
}

impl TextEdit {
    /// Creates a [`TextEdit`] replacing `range` with `replacement`.
    #[must_use]
    pub fn new(range: Range<usize>, replacement: impl Into<String>) -> Self {
        Self {
            range,
            replacement: replacement.into(),
        }
    }

    /// Creates a pure insertion at `offset`.
    #[must_use]
    pub fn insert(offset: usize, text: impl Into<String>) -> Self {
        Self {
            range: offset..offset,
            replacement: text.into(),
        }
    }

    /// Creates a pure deletion of `range`.
    #[must_use]
    pub const fn delete(range: Range<usize>) -> Self {
        Self {
            range,
            replacement: String::new(),
        }
    }
}
