//! Oracle tests for `kova_buffer::edit_log`:
//! - Independent UTF-16 string replay simulator verifying bit-for-bit equality with `BufferSnapshot`.
//! - Step-by-step verification of `TreeSitterEdit` byte offsets and `(row, column)` points.
//! - Multi-cursor same-line edits (UTF-8, CJK, emoji, CRLF), `History` undo/redo/branching,
//!   deterministic fuzzing, and `EditLog` retention/eviction (`FullResyncRequired`).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert invariants directly"
)]

use std::time::Instant;

use kova_buffer::{
    Buffer, BufferId, BufferVersion, EditDelta, EditLogConfig, EditOrigin, History, HistoryConfig,
    LspTextChange, Point, Selection, SelectionSet, TextEdit, TreeSitterEdit, VersionChanges,
};

/// Independent reference LSP document simulator operating strictly on raw UTF-16 code units.
fn apply_lsp_changes_to_string(text: &str, changes: &[LspTextChange]) -> String {
    let mut u16_buf: Vec<u16> = text.encode_utf16().collect();

    for change in changes {
        let line_starts = utf16_line_starts(&u16_buf);
        let start_line_base = line_starts[change.range.start.row];
        let end_line_base = line_starts[change.range.end.row];
        let start_idx = start_line_base + change.range.start.column;
        let end_idx = end_line_base + change.range.end.column;

        assert_eq!(
            u32::try_from(end_idx - start_idx).unwrap_or(u32::MAX),
            change.range_length,
            "LspTextChange::range_length mismatch"
        );

        let replacement_u16: Vec<u16> = change.text.encode_utf16().collect();
        u16_buf.splice(start_idx..end_idx, replacement_u16);
    }

    String::from_utf16(&u16_buf).expect("LSP replay must produce valid UTF-16")
}

fn utf16_line_starts(u16_buf: &[u16]) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, &cu) in u16_buf.iter().enumerate() {
        if cu == u16::from(b'\n') {
            starts.push(idx + 1);
        }
    }
    starts
}

fn string_point_at_byte(text: &str, byte: usize) -> Point {
    let prefix = &text[..byte];
    let row = prefix.bytes().filter(|&b| b == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |pos| pos + 1);
    Point::new(row, byte - line_start)
}

/// Verifies every `EditDelta` step-by-step against raw UTF-8 strings for both `TreeSitterEdit`
/// and `LspTextChange`, confirming the final document matches `expected_final` bit-for-bit.
fn verify_deltas_against_oracle(initial: &str, deltas: &[EditDelta], expected_final: &str) {
    let mut current = initial.to_owned();
    for delta in deltas {
        let ts = delta.tree_sitter;
        assert!(ts.start_byte <= ts.old_end_byte);
        assert!(ts.old_end_byte <= current.len());
        assert_eq!(
            ts.start_point,
            string_point_at_byte(&current, ts.start_byte),
            "TreeSitterEdit::start_point mismatch"
        );
        assert_eq!(
            ts.old_end_point,
            string_point_at_byte(&current, ts.old_end_byte),
            "TreeSitterEdit::old_end_point mismatch"
        );

        let mut next = String::with_capacity(
            current.len() - (ts.old_end_byte - ts.start_byte) + delta.lsp.text.len(),
        );
        next.push_str(&current[..ts.start_byte]);
        next.push_str(&delta.lsp.text);
        next.push_str(&current[ts.old_end_byte..]);

        assert_eq!(
            ts.new_end_byte,
            ts.start_byte + delta.lsp.text.len(),
            "TreeSitterEdit::new_end_byte mismatch"
        );
        assert_eq!(
            ts.new_end_point,
            string_point_at_byte(&next, ts.new_end_byte),
            "TreeSitterEdit::new_end_point mismatch"
        );

        // Also check single-step LSP UTF-16 replay agrees with byte splice.
        let lsp_step = apply_lsp_changes_to_string(&current, std::slice::from_ref(&delta.lsp));
        assert_eq!(lsp_step, next, "single-step LSP replay diverged");

        current = next;
    }
    assert_eq!(current, expected_final);
}

#[test]
fn oracle_lsp_replay_single_edits_utf8_emoji_and_crlf() {
    let initial = "zażółć\r\n日本🚀!\r\nend";
    let mut buf = Buffer::from_text(initial);
    let snap0 = buf.snapshot();

    // Replace "🚀" (4-byte UTF-8, 2 UTF-16 code units) with multiline CRLF + ZWJ emoji.
    let rocket_start = initial.find('🚀').unwrap();
    let rocket_end = rocket_start + '🚀'.len_utf8();
    buf.apply_edit(rocket_start..rocket_end, "α\r\n👨‍👩‍👧‍👦").unwrap();
    let snap1 = buf.snapshot();

    // Delete "zażółć\r\n" from line 0.
    buf.apply_edit(0.."zażółć\r\n".len(), "").unwrap();
    let snap2 = buf.snapshot();

    let lsp_0_to_2 = buf.lsp_changes_since(snap0.version()).unwrap();
    assert_eq!(
        apply_lsp_changes_to_string(&snap0.text(), &lsp_0_to_2),
        snap2.text()
    );

    let lsp_1_to_2 = buf.lsp_changes_since(snap1.version()).unwrap();
    assert_eq!(
        apply_lsp_changes_to_string(&snap1.text(), &lsp_1_to_2),
        snap2.text()
    );

    let VersionChanges::Deltas { deltas, .. } = buf.changes_since(snap0.version()) else {
        panic!("expected Deltas");
    };
    let flat_deltas: Vec<EditDelta> = deltas
        .into_iter()
        .flat_map(|vd| vd.edits.iter().cloned())
        .collect();
    verify_deltas_against_oracle(&snap0.text(), &flat_deltas, &snap2.text());
}

#[test]
fn oracle_lsp_replay_multicursor_same_line_10_cursors_utf16() {
    // 10 segments on a single line mixing ASCII, Polish diacritics, CJK, and 4-byte emoji, plus CRLF.
    let line = "a0|ż1|日2|🚀3|b4|ó5|本6|🎉7|c8|ę9";
    let initial = format!("{line}\r\ntail🚀\r\n");
    let mut buf = Buffer::from_text(&initial);
    let snap0 = buf.snapshot();

    // Place 10 edits on the exact same first line (mix of insertions, deletions, replacements).
    let pipe_offsets: Vec<usize> = line.match_indices('|').map(|(i, _)| i).collect();
    assert_eq!(pipe_offsets.len(), 9);

    let mut edits = vec![TextEdit::new(0..2, "🚀X\nZ")];
    for (idx, &pos) in pipe_offsets.iter().enumerate() {
        if idx % 3 == 0 {
            edits.push(TextEdit::delete(pos..(pos + 1)));
        } else if idx % 3 == 1 {
            edits.push(TextEdit::insert(pos + 1, "ż🔥"));
        } else {
            edits.push(TextEdit::new(pos..(pos + 1), "->"));
        }
    }
    assert_eq!(edits.len(), 10);

    buf.apply_edits(&edits).unwrap();
    let snap1 = buf.snapshot();

    // Second multi-cursor batch across multiple lines including trailing CRLF line.
    let snap1_text = snap1.text();
    let tail_pos = snap1_text.find("tail").unwrap();
    buf.apply_edits(&[
        TextEdit::insert(0, "PREFIX:"),
        TextEdit::new(tail_pos..(tail_pos + 4), "HEAD\r\nMID"),
    ])
    .unwrap();
    let snap2 = buf.snapshot();

    let changes_0_to_2 = buf.lsp_changes_since(snap0.version()).unwrap();
    assert_eq!(
        apply_lsp_changes_to_string(&snap0.text(), &changes_0_to_2),
        snap2.text()
    );

    let VersionChanges::Deltas { deltas, .. } = buf.changes_since(snap0.version()) else {
        panic!("expected Deltas");
    };
    let flat_deltas: Vec<EditDelta> = deltas
        .into_iter()
        .flat_map(|vd| vd.edits.iter().cloned())
        .collect();
    verify_deltas_against_oracle(&snap0.text(), &flat_deltas, &snap2.text());
}

#[test]
fn oracle_lsp_replay_undo_redo_and_branching() {
    let t0 = Instant::now();
    let mut buf = Buffer::from_text("alpha 🚀 beta\ngamma ż delta\n");
    let mut sels = SelectionSet::from_selections(
        [Selection::new(0, 5), Selection::new(15, 20)],
        0,
        &buf.snapshot(),
    );
    let mut history = History::new(HistoryConfig::default());
    let mut snapshots = vec![buf.snapshot()];

    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::new(0..5, "ONE"), TextEdit::new(15..20, "TWO")],
            EditOrigin::Compound,
            t0,
        )
        .unwrap();
    snapshots.push(buf.snapshot());

    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(3, "_🔥")],
            EditOrigin::Compound,
            t0,
        )
        .unwrap();
    snapshots.push(buf.snapshot());

    // Undo twice, create alternate branch, undo and redo original branch.
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    snapshots.push(buf.snapshot());
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    snapshots.push(buf.snapshot());

    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(0, "ALT\r\n")],
            EditOrigin::Compound,
            t0,
        )
        .unwrap();
    snapshots.push(buf.snapshot());

    assert!(history.undo(&mut buf, &mut sels).unwrap());
    snapshots.push(buf.snapshot());
    assert!(history.redo_branch(0, &mut buf, &mut sels).unwrap());
    snapshots.push(buf.snapshot());

    let current = buf.snapshot();
    for old_snap in &snapshots {
        let lsp = buf.lsp_changes_since(old_snap.version()).unwrap();
        assert_eq!(
            apply_lsp_changes_to_string(&old_snap.text(), &lsp),
            current.text(),
            "replay failed from version {:?}",
            old_snap.version()
        );

        let VersionChanges::Deltas { .. } = buf.changes_since(old_snap.version()) else {
            assert_eq!(old_snap.version(), current.version());
            continue;
        };
        if let VersionChanges::Deltas { deltas, .. } = buf.changes_since(old_snap.version()) {
            let flat: Vec<EditDelta> = deltas
                .into_iter()
                .flat_map(|vd| vd.edits.iter().cloned())
                .collect();
            verify_deltas_against_oracle(&old_snap.text(), &flat, &current.text());
        }
    }
}

#[test]
fn tree_sitter_edit_multiline_and_crlf_points() {
    let mut buf = Buffer::from_text("abc\r\ndef\r\nghi");
    // Pure no-op must not record into EditLog.
    let v0 = buf.apply_edit(1..1, "").unwrap();
    assert_eq!(v0, BufferVersion::new(0));
    assert_eq!(buf.edit_log_len(), 0);
    assert_eq!(buf.changes_since(v0), VersionChanges::UpToDate);
    assert_eq!(buf.tree_sitter_edits_since(v0), Some(Vec::new()));
    assert_eq!(buf.lsp_changes_since(v0), Some(Vec::new()));

    // Insert multiline text at byte 2 (row 0, col 2) replacing "c\r\nd" (bytes 2..6).
    let v1 = buf.apply_edit(2..6, "X\r\nY\nZ").unwrap();
    assert_eq!(v1, BufferVersion::new(1));
    assert_eq!(buf.snapshot().text(), "abX\r\nY\nZef\r\nghi");

    let ts_edits = buf.tree_sitter_edits_since(BufferVersion::new(0)).unwrap();
    assert_eq!(ts_edits.len(), 1);
    assert_eq!(
        ts_edits[0],
        TreeSitterEdit {
            start_byte: 2,
            old_end_byte: 6,
            new_end_byte: 2 + "X\r\nY\nZ".len(),
            start_point: Point::new(0, 2),
            old_end_point: Point::new(1, 1),
            new_end_point: Point::new(2, 1),
        }
    );
}

#[test]
fn edit_log_retention_eviction_and_full_resync() {
    // 1. Eviction by max_versions
    let mut buf = Buffer::with_edit_log_config(
        BufferId::new(1),
        "0123456789",
        EditLogConfig {
            max_versions: 3,
            max_retained_bytes: 1024,
        },
    );
    for i in 0..5 {
        let len = buf.snapshot().len_bytes();
        buf.apply_edit(len..len, &format!("{i}")).unwrap();
    }
    assert_eq!(buf.version(), BufferVersion::new(5));
    assert_eq!(buf.edit_log_len(), 3);

    // Versions 0 and 1 were evicted -> FullResyncRequired.
    assert_eq!(
        buf.changes_since(BufferVersion::new(1)),
        VersionChanges::FullResyncRequired {
            requested_version: BufferVersion::new(1),
            oldest_available_version: BufferVersion::new(2),
            current_version: BufferVersion::new(5),
        }
    );
    assert_eq!(buf.tree_sitter_edits_since(BufferVersion::new(1)), None);
    assert_eq!(buf.lsp_changes_since(BufferVersion::new(1)), None);

    // Future version -> FullResyncRequired.
    assert_eq!(
        buf.changes_since(BufferVersion::new(99)),
        VersionChanges::FullResyncRequired {
            requested_version: BufferVersion::new(99),
            oldest_available_version: BufferVersion::new(2),
            current_version: BufferVersion::new(5),
        }
    );

    // Version 2 (boundary of retention window) succeeds.
    let changes = buf.lsp_changes_since(BufferVersion::new(2)).unwrap();
    assert_eq!(changes.len(), 3);
    assert_eq!(
        apply_lsp_changes_to_string("012345678901", &changes),
        buf.snapshot().text()
    );

    // 2. Eviction by max_retained_bytes and oversized single delta
    let mut byte_limited = Buffer::with_edit_log_config(
        BufferId::new(2),
        "",
        EditLogConfig {
            max_versions: 100,
            max_retained_bytes: 10,
        },
    );
    byte_limited.apply_edit(0..0, "aaaa").unwrap(); // 4 bytes
    byte_limited.apply_edit(4..4, "bbbb").unwrap(); // 8 bytes total
    byte_limited.apply_edit(8..8, "cccc").unwrap(); // evicts "aaaa" -> 8 bytes retained
    assert_eq!(byte_limited.edit_log_len(), 2);
    assert!(
        byte_limited
            .lsp_changes_since(BufferVersion::new(0))
            .is_none()
    );
    assert_eq!(
        byte_limited
            .lsp_changes_since(BufferVersion::new(1))
            .unwrap()
            .len(),
        2
    );

    // Oversized single edit (> 10 bytes) clears log and sets oldest = current.
    let v_big = byte_limited.apply_edit(12..12, "0123456789ABCDEF").unwrap();
    assert_eq!(byte_limited.edit_log_len(), 0);
    assert_eq!(byte_limited.edit_log().retained_bytes(), 0);
    assert_eq!(
        byte_limited.changes_since(BufferVersion::new(3)),
        VersionChanges::FullResyncRequired {
            requested_version: BufferVersion::new(3),
            oldest_available_version: v_big,
            current_version: v_big,
        }
    );
    assert_eq!(byte_limited.changes_since(v_big), VersionChanges::UpToDate);
}

#[test]
fn oracle_lsp_replay_deterministic_fuzz_property() {
    struct Rng(u64);
    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn gen_range(&mut self, upper: usize) -> usize {
            if upper == 0 {
                return 0;
            }
            usize::try_from(self.next_u64() % u64::try_from(upper).unwrap_or(1)).unwrap_or(0)
        }
    }

    let snippets = [
        "x",
        "żółć",
        "🚀",
        "日本",
        "\n",
        "\r\n",
        "ab\r\ncd",
        "👨‍👩‍👧‍👦",
        "",
    ];

    let mut rng = Rng(0x5EED_C0DE_0123_4567);
    let mut buf = Buffer::from_text("init 🚀 line0\r\nzażółć line1\n日本語 line2");
    let mut sels = SelectionSet::caret(0);
    let mut history = History::new(HistoryConfig::default());
    let mut snapshots = vec![buf.snapshot()];
    let t0 = Instant::now();

    for step in 0..60 {
        let roll = rng.gen_range(10);
        if roll == 0 && history.can_undo() {
            history.undo(&mut buf, &mut sels).unwrap();
        } else if roll == 1 && history.can_redo() {
            history.redo(&mut buf, &mut sels).unwrap();
        } else {
            let snap = buf.snapshot();
            let len = snap.len_bytes();
            let cursor_count = 1 + rng.gen_range(5);

            // Pick sorted non-overlapping boundary offsets.
            let mut bounds = Vec::with_capacity(cursor_count * 2);
            for _ in 0..(cursor_count * 2) {
                let raw = rng.gen_range(len + 1);
                bounds.push(snap.clamp_byte_to_char_boundary(raw));
            }
            bounds.sort_unstable();

            let mut edits = Vec::new();
            for pair in bounds.chunks_exact(2) {
                let start = pair[0];
                let end = pair[1];
                // Avoid duplicate empty insertions at the same offset.
                if edits
                    .last()
                    .is_some_and(|prev: &TextEdit| prev.range.end == start && start == end)
                {
                    continue;
                }
                if edits
                    .last()
                    .is_some_and(|prev: &TextEdit| start < prev.range.end)
                {
                    continue;
                }
                let rep = snippets[rng.gen_range(snippets.len())];
                edits.push(TextEdit::new(start..end, rep));
            }

            if !edits.is_empty() {
                history
                    .record_and_apply(&mut buf, &mut sels, &edits, EditOrigin::Compound, t0)
                    .unwrap();
            }
        }

        if step % 5 == 0 {
            snapshots.push(buf.snapshot());
        }
    }

    let final_snap = buf.snapshot();
    for old in &snapshots {
        let lsp = buf.lsp_changes_since(old.version()).unwrap();
        assert_eq!(
            apply_lsp_changes_to_string(&old.text(), &lsp),
            final_snap.text()
        );
        if let VersionChanges::Deltas { deltas, .. } = buf.changes_since(old.version()) {
            let flat: Vec<EditDelta> = deltas
                .into_iter()
                .flat_map(|vd| vd.edits.iter().cloned())
                .collect();
            verify_deltas_against_oracle(&old.text(), &flat, &final_snap.text());
        }
    }
}
