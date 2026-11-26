//! `kova_buffer` — Rope text buffer, multi-cursors, selections and Undo/Redo tree.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod buffer;
pub mod clipboard;
pub mod edit;
pub mod edit_log;
pub mod error;
pub mod grapheme;
pub mod history;
pub mod line_ending;
pub mod motion;
pub mod position;
pub mod selection;
pub mod snapshot;

pub use buffer::Buffer;
pub use clipboard::{
    ClipboardEntry, MultiCursorClipboard, copy_selections, cut_selections, paste, paste_latest,
    resolve_paste_texts,
};
pub use edit::{
    apply_delete_backward, apply_delete_forward, apply_insert_all, apply_insert_per_cursor,
};
pub use edit_log::{
    EditDelta, EditLog, EditLogConfig, LspTextChange, TreeSitterEdit, VersionChanges, VersionDelta,
};
pub use error::{BufferError, HistoryError};
pub use grapheme::{
    BoundaryAffinity, next_grapheme_boundary, next_word_boundary, prev_grapheme_boundary,
    prev_word_boundary, snap_byte_to_grapheme_boundary,
};
pub use history::{
    EditBatch, EditOrigin, History, HistoryConfig, HistoryNode, NodeId, ReversibleEdit,
    TransactionId,
};
pub use line_ending::LineEnding;
pub use motion::{Motion, MotionConfig, byte_at_visual_column, visual_column_at};
pub use position::{BufferId, BufferVersion, Point, PointRange, TextEdit, Utf16Point, Utf16Range};
pub use selection::{Direction, Selection, SelectionSet};
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
        assert_send_sync::<SelectionSet>();
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

        assert_eq!(snap.utf16_to_byte(10).unwrap(), rocket_byte);
        assert_eq!(
            snap.utf16_point_to_byte(Utf16Point::new(1, 3)).unwrap(),
            rocket_byte
        );

        assert_eq!(snap.utf16_to_byte(11).unwrap(), 21);
        assert_eq!(snap.char_at(21).unwrap(), '!');
        assert_eq!(snap.byte_to_utf16_point(21).unwrap(), Utf16Point::new(1, 4));

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

        assert_eq!(snap.byte_to_point(2).unwrap(), Point::new(0, 2));
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

        assert_eq!(snap.clamp_byte_to_char_boundary(3), 2);
        assert_eq!(snap.clamp_point(Point::new(0, 99)), Point::new(0, 2));

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

        buf.apply_edit(3..5, "\n").unwrap();
        assert_eq!(buf.snapshot().text(), "foo\nbar");
    }

    #[test]
    fn apply_edits_supports_adjacent_non_empty_and_rejects_overlaps_atomically() {
        let mut buf = Buffer::from_text("abcdef");
        let v1 = buf
            .apply_edits(&[TextEdit::new(3..6, "XYZ"), TextEdit::new(0..3, "12")])
            .unwrap();
        assert_eq!(v1, BufferVersion::new(1));
        assert_eq!(buf.snapshot().text(), "12XYZ");

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

    #[test]
    fn selection_set_merging_rules_and_primary_tracking() {
        let buf = Buffer::from_text("0123456789");
        let snap = buf.snapshot();

        // Empty fallback becomes caret(0)
        let empty = SelectionSet::from_selections([], 5, &snap);
        assert_eq!(empty.len(), 1);
        assert_eq!(*empty.primary(), Selection::caret(0));

        // Duplicate carets merge; distinct adjacent carets stay separate
        let carets = SelectionSet::from_selections(
            [
                Selection::caret(4),
                Selection::caret(3),
                Selection::caret(4),
            ],
            2,
            &snap,
        );
        assert_eq!(
            carets.as_slice(),
            &[Selection::caret(3), Selection::caret(4)]
        );
        assert_eq!(carets.primary_index(), 1);

        // Adjacent non-empty ranges [0..3) and [3..6) must NOT merge
        let adjacent =
            SelectionSet::from_selections([Selection::new(3, 6), Selection::new(0, 3)], 0, &snap);
        assert_eq!(
            adjacent.as_slice(),
            &[Selection::new(0, 3), Selection::new(3, 6)]
        );
        assert_eq!(adjacent.primary_index(), 1);

        // Caret touching a non-empty selection's boundary merges and preserves primary direction
        let touching =
            SelectionSet::from_selections([Selection::new(3, 0), Selection::caret(3)], 0, &snap);
        assert_eq!(touching.as_slice(), &[Selection::new(3, 0)]);
        assert_eq!(touching.primary().direction(), Direction::Backward);

        // Overlapping ranges [1..5) and [7..4) merge into [1..7) keeping primary's direction
        let overlap =
            SelectionSet::from_selections([Selection::new(1, 5), Selection::new(7, 4)], 1, &snap);
        assert_eq!(overlap.as_slice(), &[Selection::new(7, 1)]);
        assert_eq!(overlap.primary().direction(), Direction::Backward);
    }

    #[test]
    fn goal_column_preserved_across_short_lines_with_tabs_and_cleared_horizontally() {
        let buf = Buffer::from_text("\tabcdef\nhi\n\t1234567\n");
        let snap = buf.snapshot();
        let cfg = MotionConfig::new(4);

        // Start on line 0 after "\tabcd" -> byte 5, visual column 4 + 4 = 8
        let mut sels = SelectionSet::caret(5);
        assert_eq!(visual_column_at(&snap, 5, cfg), 8);

        // Move down to short line "hi" -> snaps to end of "hi", keeps goal_column = Some(8)
        sels.move_by(&snap, Motion::Down, false, cfg);
        assert_eq!(sels.primary().head(), 10); // "\tabcdef\nhi"
        assert_eq!(sels.primary().goal_column(), Some(8));

        // Move down to "\t1234567" -> returns to visual column 8 ("\t1234" = 4 + 4 = byte 5 on line 2)
        sels.move_by(&snap, Motion::Down, false, cfg);
        let line2_start = snap.rope().line_to_byte(2);
        assert_eq!(sels.primary().head(), line2_start + 5);
        assert_eq!(sels.primary().goal_column(), Some(8));

        // Move down on last empty line -> jumps to EOF and clears goal_column
        sels.move_by(&snap, Motion::Down, false, cfg);
        sels.move_by(&snap, Motion::Down, false, cfg);
        assert_eq!(sels.primary().head(), snap.len_bytes());
        assert_eq!(sels.primary().goal_column(), None);

        // Horizontal movement clears goal_column
        sels = SelectionSet::caret(5);
        sels.move_by(&snap, Motion::Down, false, cfg);
        assert_eq!(sels.primary().goal_column(), Some(8));
        sels.move_by(&snap, Motion::Left, false, cfg);
        assert_eq!(sels.primary().goal_column(), None);
    }

    #[test]
    fn multicursor_insert_delete_and_next_occurrence() {
        let mut buf = Buffer::from_text("int alpha = 1;\nint alpha = 2;\nint alpha = 3;\n");
        let snap = buf.snapshot();

        // Caret inside first "alpha" (byte 5)
        let mut sels = SelectionSet::caret(5);
        assert!(sels.add_next_occurrence(&snap)); // selects "alpha" on line 0
        assert_eq!(sels.len(), 1);
        assert_eq!(sels.primary().range(), 4..9);

        assert!(sels.add_next_occurrence(&snap)); // line 1
        assert!(sels.add_next_occurrence(&snap)); // line 2
        assert!(!sels.add_next_occurrence(&snap)); // no more occurrences
        assert_eq!(sels.len(), 3);

        // Replace all three "alpha" selections with "beta_x"
        apply_insert_all(&mut buf, &mut sels, "beta_x").unwrap();
        assert_eq!(
            buf.snapshot().text(),
            "int beta_x = 1;\nint beta_x = 2;\nint beta_x = 3;\n"
        );
        assert_eq!(sels.len(), 3);
        assert!(sels.as_slice().iter().all(Selection::is_empty));

        // Backspace at all 3 cursors deletes "_x" -> "beta"
        apply_delete_backward(&mut buf, &mut sels).unwrap();
        apply_delete_backward(&mut buf, &mut sels).unwrap();
        assert_eq!(
            buf.snapshot().text(),
            "int beta = 1;\nint beta = 2;\nint beta = 3;\n"
        );

        // Per-cursor insert with mismatched count fails cleanly without mutating buffer
        let err = apply_insert_per_cursor(&mut buf, &mut sels, &["one", "two"]).unwrap_err();
        assert_eq!(
            err,
            BufferError::CursorCountMismatch {
                expected: 3,
                actual: 2,
            }
        );

        // Per-cursor insert with matching count updates all 3 cursors independently
        apply_insert_per_cursor(&mut buf, &mut sels, &["1", "2", "3"]).unwrap();
        assert_eq!(
            buf.snapshot().text(),
            "int beta1 = 1;\nint beta2 = 2;\nint beta3 = 3;\n"
        );

        let occ_buf = Buffer::from_text("int item = 1;\nint item = 2;\nint item = 3;\n");
        let mut occ = SelectionSet::caret(4);
        assert!(occ.select_all_occurrences(&occ_buf.snapshot()));
        assert_eq!(occ.len(), 3);
        assert!(!occ.select_all_occurrences(&occ_buf.snapshot()));
        assert!(occ.remove_primary());
        assert_eq!(occ.len(), 2);
        assert!(occ.remove_primary());
        assert_eq!(occ.len(), 1);
        assert!(!occ.remove_primary());
    }

    #[test]
    fn multicursor_delete_graphemes_and_adjacent_cursors() {
        let mut buf = Buffer::from_text("e\u{0301}👨‍👩‍👧‍👦🇵🇱");
        let snap = buf.snapshot();
        // Caret right after 'e\u{0301}' (3) and after flag (end)
        let mut sels =
            SelectionSet::from_selections([Selection::caret(3), Selection::caret(36)], 0, &snap);

        apply_delete_backward(&mut buf, &mut sels).unwrap();
        assert_eq!(buf.snapshot().text(), "👨‍👩‍👧‍👦");
        assert_eq!(
            sels.as_slice(),
            &[Selection::caret(0), Selection::caret(25)]
        );

        // Second Backspace: cursor at 0 is no-op, cursor at 25 deletes the family emoji -> merged at 0
        apply_delete_backward(&mut buf, &mut sels).unwrap();
        assert_eq!(buf.snapshot().text(), "");
        assert_eq!(sels.as_slice(), &[Selection::caret(0)]);
    }

    #[test]
    fn smart_home_and_extend_selection_collapse() {
        let buf = Buffer::from_text("    hello\n");
        let snap = buf.snapshot();
        let cfg = MotionConfig::default();

        let mut sels = SelectionSet::caret(7); // inside "hello"
        sels.move_by(&snap, Motion::LineFirstNonWhitespace, false, cfg);
        assert_eq!(sels.primary().head(), 4);
        sels.move_by(&snap, Motion::LineFirstNonWhitespace, false, cfg);
        assert_eq!(sels.primary().head(), 0);

        // Non-extending Left on [2..6) collapses to 2 without moving further left
        sels = SelectionSet::single(Selection::new(2, 6));
        sels.move_by(&snap, Motion::Left, false, cfg);
        assert_eq!(*sels.primary(), Selection::caret(2));

        // Extending Right from 2 -> [2..3)
        sels.move_by(&snap, Motion::Right, true, cfg);
        assert_eq!(*sels.primary(), Selection::new(2, 3));
    }

    #[test]
    fn map_through_edits_shifts_selections_accurately() {
        let mut buf = Buffer::from_text("alpha beta gamma");
        let mut sels = SelectionSet::from_selections(
            [Selection::new(6, 10), Selection::new(11, 16)],
            0,
            &buf.snapshot(),
        );
        let edits = vec![TextEdit::new(0..5, "ALPHA_LONG")]; // +5 bytes before both selections
        buf.apply_edits(&edits).unwrap();
        sels.map_through_edits(&edits, &buf.snapshot());

        assert_eq!(
            sels.as_slice(),
            &[Selection::new(11, 15), Selection::new(16, 21)]
        );
    }

    #[test]
    fn add_cursor_above_below_collapse_select_all_and_delete_forward() {
        let buf = Buffer::from_text("row0_abcdef\r\nrow1\r\nrow2_123456");
        let snap = buf.snapshot();
        let cfg = MotionConfig::default();

        let mut sels = SelectionSet::caret(8); // row 0, col 8
        sels.add_cursor_below(&snap, cfg); // row 1 clamped to col 4, keeps goal_column = Some(8)
        assert_eq!(sels.len(), 2);
        assert_eq!(sels.primary().goal_column(), Some(8));

        sels.add_cursor_below(&snap, cfg); // row 2, returns to col 8
        assert_eq!(sels.len(), 3);
        let row2_start = snap.rope().line_to_byte(2);
        assert_eq!(sels.primary().head(), row2_start + 8);

        // Above from row 2 hits row 1 and merges with existing cursor on row 1
        sels.add_cursor_above(&snap, cfg);
        assert_eq!(sels.len(), 3);

        sels.collapse_to_primary();
        assert_eq!(sels.len(), 1);

        sels.select_all(&snap);
        assert_eq!(sels.primary().range(), 0..snap.len_bytes());

        // Delete forward across CRLF and emoji
        let mut del_buf = Buffer::from_text("a\r\n🚀b");
        let mut del_sels = SelectionSet::from_selections(
            [Selection::caret(1), Selection::caret(3)],
            0,
            &del_buf.snapshot(),
        );
        apply_delete_forward(&mut del_buf, &mut del_sels).unwrap();
        assert_eq!(del_buf.snapshot().text(), "ab");
    }
}
