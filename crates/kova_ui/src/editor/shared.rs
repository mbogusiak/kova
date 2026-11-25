//! Shared document buffer handle (`SharedBuffer`) for split editor panes.
//!
//! [`Buffer`] is copy-on-write, so cloning it forks the document. Two [`EditorView`]s showing
//! the same file in different splits instead hold clones of one [`SharedBuffer`] handle: an
//! edit through either handle is immediately visible through the other, while every view
//! keeps its own scroll offset and cursor.
//!
//! The handle lives on the GPUI main thread only (`Rc<RefCell<_>>`); borrows never escape a
//! single method call, so they can never overlap.
//!
//! [`EditorView`]: crate::editor::EditorView

use std::cell::RefCell;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;

use kova_buffer::{Buffer, BufferError, BufferSnapshot, BufferVersion};

/// Reference-counted handle to a [`Buffer`] shared by every editor pane showing the document.
#[derive(Clone)]
pub struct SharedBuffer(Rc<RefCell<Buffer>>);

impl fmt::Debug for SharedBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedBuffer")
            .field("version", &self.snapshot().version())
            .field("handles", &Rc::strong_count(&self.0))
            .finish()
    }
}

impl SharedBuffer {
    /// Wraps `buffer` in a new shared handle.
    #[must_use]
    pub fn new(buffer: Buffer) -> Self {
        Self(Rc::new(RefCell::new(buffer)))
    }

    /// Returns an $O(1)$ immutable snapshot of the current document state.
    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        self.0.borrow().snapshot()
    }

    /// Replaces `range` (bytes) with `replacement` and returns the new [`BufferVersion`].
    ///
    /// # Errors
    ///
    /// Propagates [`BufferError`] when `range` is out of bounds or splits a character.
    pub fn apply_edit(
        &self,
        range: Range<usize>,
        replacement: &str,
    ) -> Result<BufferVersion, BufferError> {
        self.0.borrow_mut().apply_edit(range, replacement)
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

        let before = right.snapshot().version();
        let after = left.apply_edit(6..6, "Order").unwrap();
        assert_ne!(after, before);
        assert_eq!(right.snapshot().text(), "class OrderA {}\n");
        assert_eq!(right.snapshot().version(), after);

        let other = SharedBuffer::new(Buffer::from_text("class A {}\n"));
        assert!(!other.same_document(&left));
        assert!(left.apply_edit(100..200, "x").is_err());
        assert!(format!("{left:?}").contains("handles"));
    }
}
