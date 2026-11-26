//! Shared document handle (`SharedBuffer`) for split editor panes.
//!
//! [`Buffer`] is copy-on-write, so cloning it forks the document. Two [`EditorView`]s showing
//! the same file in different splits instead hold clones of one [`SharedBuffer`] handle: an
//! edit through either handle is immediately visible through the other, while every view
//! keeps its own scroll offset and cursor. The Undo/Redo [`History`] lives next to the
//! buffer, so undo is per document (as in `IntelliJ`) and never replays stale offsets after a
//! sibling split edited the same file.
//!
//! The handle lives on the GPUI main thread only (`Rc<RefCell<_>>`); borrows never escape a
//! single method call, so they can never overlap.
//!
//! [`EditorView`]: crate::editor::EditorView

use std::cell::RefCell;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;

use kova_buffer::{Buffer, BufferError, BufferSnapshot, BufferVersion, History, HistoryConfig};

#[derive(Debug)]
struct SharedDocument {
    buffer: Buffer,
    history: History,
}

/// Reference-counted handle to a [`Buffer`] (and its undo [`History`]) shared by every editor
/// pane showing the document.
#[derive(Clone)]
pub struct SharedBuffer(Rc<RefCell<SharedDocument>>);

impl fmt::Debug for SharedBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedBuffer")
            .field("version", &self.snapshot().version())
            .field("handles", &Rc::strong_count(&self.0))
            .finish()
    }
}

impl SharedBuffer {
    /// Wraps `buffer` in a new shared handle with an empty undo history.
    #[must_use]
    pub fn new(buffer: Buffer) -> Self {
        Self(Rc::new(RefCell::new(SharedDocument {
            buffer,
            history: History::new(HistoryConfig::default()),
        })))
    }

    /// Returns an $O(1)$ immutable snapshot of the current document state.
    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        self.0.borrow().buffer.snapshot()
    }

    /// Returns the current [`BufferVersion`] of the document.
    #[must_use]
    pub fn version(&self) -> BufferVersion {
        self.0.borrow().buffer.version()
    }

    /// Replaces `range` (bytes) with `replacement` outside the undo history and returns the new
    /// [`BufferVersion`]. Used for programmatic edits (e.g. reload from disk).
    ///
    /// # Errors
    ///
    /// Propagates [`BufferError`] when `range` is out of bounds or splits a character.
    pub fn apply_edit(
        &self,
        range: Range<usize>,
        replacement: &str,
    ) -> Result<BufferVersion, BufferError> {
        self.0.borrow_mut().buffer.apply_edit(range, replacement)
    }

    /// Runs `f` with mutable access to the buffer and its undo history.
    pub fn with_document<R>(&self, f: impl FnOnce(&mut Buffer, &mut History) -> R) -> R {
        let mut doc = self.0.borrow_mut();
        let SharedDocument { buffer, history } = &mut *doc;
        f(buffer, history)
    }

    /// Replaces the entire buffer with `text` and resets the undo [`History`] to a clean
    /// save point (used when reloading an unmodified file from disk after a branch switch).
    ///
    /// # Errors
    /// Propagates [`BufferError`] if the edit fails.
    pub fn reset_to_text(&self, text: &str) -> Result<BufferVersion, BufferError> {
        let mut doc = self.0.borrow_mut();
        let len = doc.buffer.snapshot().len_bytes();
        let version = doc.buffer.apply_edit(0..len, text)?;
        doc.history = History::new(HistoryConfig::default());
        Ok(version)
    }

    /// Returns `true` when the shared history has a transaction to undo.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.0.borrow().history.can_undo()
    }

    /// Returns `true` when the document differs from its last save point.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.0.borrow().history.is_dirty()
    }

    /// Returns `true` when both handles point at the same underlying document.
    #[must_use]
    pub fn same_document(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_through_one_handle_are_visible_through_the_other() {
        let left = SharedBuffer::new(Buffer::from_text("class A {}\n"));
        let right = left.clone();
        assert!(left.same_document(&right));

        let before = right.version();
        let after = left.apply_edit(6..6, "Order").unwrap();
        assert_ne!(after, before);
        assert_eq!(right.snapshot().text(), "class OrderA {}\n");
        assert_eq!(right.snapshot().version(), after);
        assert!(!right.can_undo());

        let other = SharedBuffer::new(Buffer::from_text("class A {}\n"));
        assert!(!other.same_document(&left));
        assert!(left.apply_edit(100..200, "x").is_err());
        assert!(format!("{left:?}").contains("handles"));
    }
}
