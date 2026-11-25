//! `kova_buffer` — Rope text buffer, multi-cursors, selections and Undo/Redo tree.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod buffer;
pub mod error;
pub mod line_ending;
pub mod position;
pub mod snapshot;

pub use buffer::Buffer;
pub use error::BufferError;
pub use line_ending::LineEnding;
pub use position::{BufferId, BufferVersion, Point, PointRange, TextEdit, Utf16Point, Utf16Range};
pub use snapshot::BufferSnapshot;

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_buffer");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }

    #[test]
    fn snapshot_is_send_and_sync() {
        assert_send_sync::<BufferSnapshot>();
        assert_send_sync::<Buffer>();
    }

    #[test]
    fn snapshot_is_immutable_across_buffer_mutations() {
        let mut buf = Buffer::from_text("hello world\n");
        let snap_v0 = buf.snapshot();
        assert_eq!(snap_v0.version(), BufferVersion::new(0));
        assert_eq!(snap_v0.text(), "hello world\n");

        let v1 = buf
            .apply_edit(6..11, "kova")
            .expect("valid ASCII replacement");
        assert_eq!(v1, BufferVersion::new(1));

        let snap_v1 = buf.snapshot();
        assert_eq!(snap_v0.text(), "hello world\n");
        assert_eq!(snap_v0.version(), BufferVersion::new(0));
        assert_eq!(snap_v1.text(), "hello kova\n");
        assert_eq!(snap_v1.version(), BufferVersion::new(1));
    }

    #[test]
    fn multibyte_polish_cjk_and_utf16_surrogate_conversions() {
        // "zażółć\n日本🚀!\n"
        // Line 0: "zażółć" -> z(1) a(1) ż(2) ó(2) ł(2) ć(2) = 10 bytes, 6 chars, 6 UTF-16 CU, + '\n' (1) = 11 bytes
        // Line 1: "日本🚀!" -> 日(3) 本(3) 🚀(4, 2 UTF-16 CU) !(1) = 11 bytes, 4 chars, 5 UTF-16 CU, + '\n' (1) = 12 bytes
        let text = "zażółć\n日本🚀!\n";
        let buf = Buffer::from_text(text);
        let snap = buf.snapshot();

        assert_eq!(snap.len_bytes(), 23);
        assert_eq!(snap.len_chars(), 12);
        assert_eq!(snap.len_utf16(), 13);
        assert_eq!(snap.len_lines(), 3);

        assert_eq!(snap.line_len_bytes(0).unwrap(), 10);
        assert_eq!(snap.line_len_utf16(0).unwrap(), 6);
        assert_eq!(snap.line_len_bytes(1).unwrap(), 11);
        assert_eq!(snap.line_len_utf16(1).unwrap(), 5);
        assert_eq!(snap.line_len_bytes(2).unwrap(), 0);

        // Start of 🚀: byte 11 + 6 = 17, char 7 + 2 = 9, utf16 7 + 2 = 9
        let rocket_byte = 17;
        assert_eq!(snap.char_at(rocket_byte).unwrap(), '🚀');
        assert_eq!(snap.byte_to_char(rocket_byte).unwrap(), 9);
        assert_eq!(snap.char_to_byte(9).unwrap(), rocket_byte);
        assert_eq!(snap.byte_to_utf16(rocket_byte).unwrap(), 9);
        assert_eq!(snap.utf16_to_byte(9).unwrap(), rocket_byte);
        assert_eq!(snap.byte_to_point(rocket_byte).unwrap(), Point::new(1, 6));
        assert_eq!(snap.point_to_byte(Point::new(1, 6)).unwrap(), rocket_byte);
        assert_eq!(
            snap.byte_to_utf16_point(rocket_byte).unwrap(),
            Utf16Point::new(1, 2)
        );
        assert_eq!(
            snap.utf16_point_to_byte(Utf16Point::new(1, 2)).unwrap(),
            rocket_byte
        );
        assert_eq!(
            snap.point_to_utf16_point(Point::new(1, 6)).unwrap(),
            Utf16Point::new(1, 2)
        );
        assert_eq!(
            snap.utf16_point_to_point(Utf16Point::new(1, 2)).unwrap(),
            Point::new(1, 6)
        );

        // Trailing surrogate index inside 🚀 snaps down to start of 🚀
        assert_eq!(snap.utf16_to_byte(10).unwrap(), rocket_byte);
        assert_eq!(
            snap.utf16_point_to_byte(Utf16Point::new(1, 3)).unwrap(),
            rocket_byte
        );

        // '!' right after 🚀: byte 21, utf16 11, point (1, 10), utf16_point (1, 4)
        assert_eq!(snap.utf16_to_byte(11).unwrap(), 21);
        assert_eq!(snap.char_at(21).unwrap(), '!');
        assert_eq!(snap.byte_to_utf16_point(21).unwrap(), Utf16Point::new(1, 4));

        // Inside multi-byte UTF-8 character returns NotCharBoundary
        assert_eq!(
            snap.byte_to_point(18),
            Err(BufferError::NotCharBoundary { byte: 18 })
        );
        assert_eq!(
            snap.point_to_byte(Point::new(1, 7)),
            Err(BufferError::NotCharBoundary { byte: 18 })
        );
        assert_eq!(snap.clamp_byte_to_char_boundary(20), 17);
    }

    #[test]
    fn zwj_emoji_sequence_utf16_mapping() {
        // 👨‍👩‍👧‍👦 = 4 code points (4 * 2 UTF-16 CU = 8) + 3 ZWJ (3 * 1 UTF-16 CU = 3) = 11 UTF-16 CU, 25 UTF-8 bytes
        let family = "👨‍👩‍👧‍👦";
        let text = format!("a{family}b");
        let buf = Buffer::from_text(&text);
        let snap = buf.snapshot();

        assert_eq!(snap.len_bytes(), 1 + 25 + 1);
        assert_eq!(snap.len_utf16(), 1 + 11 + 1);
        assert_eq!(
            snap.byte_to_utf16_point(26).unwrap(),
            Utf16Point::new(0, 12)
        );
        assert_eq!(
            snap.utf16_point_to_byte(Utf16Point::new(0, 12)).unwrap(),
            26
        );
        assert_eq!(snap.char_at(26).unwrap(), 'b');
    }

    #[test]
    fn crlf_atomicity_and_lf_line_handling() {
        let buf = Buffer::from_text("ab\r\ncd\r\n");
        let snap = buf.snapshot();

        assert_eq!(snap.line_ending(), LineEnding::CrLf);
        assert_eq!(snap.len_lines(), 3);
        assert_eq!(snap.line_len_bytes(0).unwrap(), 2);
        assert_eq!(snap.line_len_utf16(0).unwrap(), 2);
        assert_eq!(snap.line_slice(0).unwrap().to_string(), "ab");
        assert_eq!(
            snap.line_slice_with_ending(0).unwrap().to_string(),
            "ab\r\n"
        );

        // Offset 2 is right before \r\n (valid)
        assert_eq!(snap.byte_to_point(2).unwrap(), Point::new(0, 2));
        // Offset 3 is strictly between \r and \n (invalid in strict methods)
        assert_eq!(
            snap.byte_to_point(3),
            Err(BufferError::SplitsCrLf { byte: 3 })
        );
        assert_eq!(
            snap.byte_to_utf16_point(3),
            Err(BufferError::SplitsCrLf { byte: 3 })
        );
        assert_eq!(
            snap.byte_to_char(3),
            Err(BufferError::SplitsCrLf { byte: 3 })
        );
        assert_eq!(
            snap.byte_to_utf16(3),
            Err(BufferError::SplitsCrLf { byte: 3 })
        );
        assert_eq!(
            snap.char_to_byte(3),
            Err(BufferError::SplitsCrLf { byte: 3 })
        );
        assert_eq!(
            snap.utf16_to_byte(3),
            Err(BufferError::SplitsCrLf { byte: 3 })
        );
        assert_eq!(
            snap.slice_bytes(0..3),
            Err(BufferError::SplitsCrLf { byte: 3 })
        );

        // Clamping backs out of \r|\n to offset 2
        assert_eq!(snap.clamp_byte_to_char_boundary(3), 2);
        assert_eq!(snap.clamp_point(Point::new(0, 99)), Point::new(0, 2));

        // ColumnOutOfBounds prevents stepping into \r or \n
        assert_eq!(
            snap.point_to_byte(Point::new(0, 3)),
            Err(BufferError::ColumnOutOfBounds {
                row: 0,
                column: 3,
                max_column: 2,
            })
        );
        assert_eq!(
            snap.utf16_point_to_byte(Utf16Point::new(0, 3)),
            Err(BufferError::ColumnOutOfBounds {
                row: 0,
                column: 3,
                max_column: 2,
            })
        );

        // Offset 4 starts line 1 ("cd")
        assert_eq!(snap.byte_to_point(4).unwrap(), Point::new(1, 0));
        assert_eq!(snap.char_at(4).unwrap(), 'c');
    }

    #[test]
    fn lone_cr_and_unicode_separators_do_not_split_lines() {
        let text = "a\rb\u{2028}c\u{2029}d\ne";
        let buf = Buffer::from_text(text);
        let snap = buf.snapshot();

        assert_eq!(snap.len_lines(), 2);
        assert_eq!(
            snap.line_slice(0).unwrap().to_string(),
            "a\rb\u{2028}c\u{2029}d"
        );
        assert_eq!(snap.line_slice(1).unwrap().to_string(), "e");
    }

    #[test]
    fn apply_edit_cannot_split_crlf_but_can_delete_full_crlf() {
        let mut buf = Buffer::from_text("foo\r\nbar");
        assert_eq!(
            buf.apply_edit(4..4, "x"),
            Err(BufferError::SplitsCrLf { byte: 4 })
        );
        assert_eq!(buf.snapshot().text(), "foo\r\nbar");

        // Deleting the entire \r\n (bytes 3..5) succeeds
        buf.apply_edit(3..5, "\n").unwrap();
        assert_eq!(buf.snapshot().text(), "foo\nbar");
    }

    #[test]
    fn apply_edits_supports_adjacent_non_empty_and_rejects_overlaps_atomically() {
        let mut buf = Buffer::from_text("abcdef");
        // Adjacent ranges [0..3) and [3..6) must succeed
        let v1 = buf
            .apply_edits(&[TextEdit::new(3..6, "XYZ"), TextEdit::new(0..3, "12")])
            .unwrap();
        assert_eq!(v1, BufferVersion::new(1));
        assert_eq!(buf.snapshot().text(), "12XYZ");

        // Overlapping edits must fail atomically without mutating buffer or version
        let err = buf
            .apply_edits(&[TextEdit::new(0..3, "A"), TextEdit::new(2..4, "B")])
            .unwrap_err();
        assert_eq!(
            err,
            BufferError::OverlappingEdits {
                first_end: 3,
                second_start: 2,
            }
        );
        assert_eq!(buf.version(), BufferVersion::new(1));
        assert_eq!(buf.snapshot().text(), "12XYZ");

        // Duplicate empty insertions at the same offset fail atomically
        let err_dup = buf
            .apply_edits(&[TextEdit::insert(2, "a"), TextEdit::insert(2, "b")])
            .unwrap_err();
        assert_eq!(
            err_dup,
            BufferError::OverlappingEdits {
                first_end: 2,
                second_start: 2,
            }
        );
        assert_eq!(buf.version(), BufferVersion::new(1));
    }

    #[test]
    fn empty_buffer_and_no_op_edit_semantics() {
        let mut buf = Buffer::new();
        let snap = buf.snapshot();
        assert!(snap.is_empty());
        assert_eq!(snap.len_lines(), 1);
        assert_eq!(snap.byte_to_point(0).unwrap(), Point::zero());
        assert_eq!(snap.point_to_byte(Point::zero()).unwrap(), 0);
        assert_eq!(snap.byte_to_utf16_point(0).unwrap(), Utf16Point::zero());

        let v = buf.apply_edit(0..0, "").unwrap();
        assert_eq!(v, BufferVersion::new(0));
        let v_empty_batch = buf.apply_edits(&[]).unwrap();
        assert_eq!(v_empty_batch, BufferVersion::new(0));
    }
}
