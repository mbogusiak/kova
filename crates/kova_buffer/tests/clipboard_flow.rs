//! Integration tests for `kova_buffer::clipboard` covering multi-cursor Copy/Cut/Paste,
//! 1-to-1 distribution, OS multi-line splitting, broadcast fallback, `linewise` mode,
//! clipboard history ring eviction, and atomic `History` undo/redo + `EditLog` integration.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert invariants directly"
)]

use std::time::Instant;

use kova_buffer::{
    Buffer, BufferId, ClipboardEntry, History, HistoryConfig, LineEnding, MultiCursorClipboard,
    Selection, SelectionSet, copy_selections, cut_selections, paste, paste_latest,
    resolve_paste_texts,
};

#[test]
fn clipboard_multicursor_copy_paste_1_to_1_and_undo() {
    let t0 = Instant::now();
    let mut buf = Buffer::from_text("one=alpha;\ntwo=beta;\nthree=gamma;\n");
    let mut history = History::new(HistoryConfig::default());
    let mut clipboard = MultiCursorClipboard::default();

    let snap0 = buf.snapshot();
    let text0 = snap0.text();
    let alpha_start = text0.find("alpha").unwrap();
    let beta_start = text0.find("beta").unwrap();
    let gamma_start = text0.find("gamma").unwrap();
    let sels_copy = SelectionSet::from_selections(
        [
            Selection::new(alpha_start, alpha_start + 5),
            Selection::new(beta_start, beta_start + 4),
            Selection::new(gamma_start, gamma_start + 5),
        ],
        0,
        &snap0,
    );

    let entry = copy_selections(&mut clipboard, &snap0, &sels_copy, true).unwrap();
    assert!(!entry.linewise);
    assert_eq!(entry.segments, vec!["alpha", "beta", "gamma"]);
    assert_eq!(entry.text, "alpha\nbeta\ngamma");
    assert_eq!(clipboard.latest(), Some(&entry));

    // Paste into 3 carets on "one", "two", "three" (at line starts).
    let two_start = text0.find("two").unwrap();
    let three_start = text0.find("three").unwrap();
    let mut sels_paste = SelectionSet::from_selections(
        [
            Selection::caret(0),
            Selection::caret(two_start),
            Selection::caret(three_start),
        ],
        1,
        &snap0,
    );
    let v_after_paste = paste_latest(&clipboard, &mut buf, &mut sels_paste, &mut history, t0)
        .unwrap()
        .unwrap();
    assert_eq!(v_after_paste, buf.version());
    assert_eq!(
        buf.snapshot().text(),
        "alphaone=alpha;\nbetatwo=beta;\ngammathree=gamma;\n"
    );

    // EditLog must record the multi-cursor paste as a single version delta.
    assert_eq!(buf.edit_log_len(), 1);
    let lsp = buf.lsp_changes_since(snap0.version()).unwrap();
    assert_eq!(lsp.len(), 3);

    // Single Undo reverses the entire multi-cursor paste and restores carets.
    assert!(history.undo(&mut buf, &mut sels_paste).unwrap());
    assert_eq!(buf.snapshot().text(), snap0.text());
    assert_eq!(
        sels_paste.as_slice(),
        &[
            Selection::caret(0),
            Selection::caret(two_start),
            Selection::caret(three_start),
        ]
    );
    assert_eq!(sels_paste.primary_index(), 1);
    assert_eq!(buf.edit_log_len(), 2);
}

#[test]
fn clipboard_multicursor_cut_and_undo() {
    let t0 = Instant::now();
    let mut buf = Buffer::from_text("aa[11]bb[22]cc");
    let mut history = History::new(HistoryConfig::default());
    let mut clipboard = MultiCursorClipboard::default();

    let mut sels = SelectionSet::from_selections(
        [Selection::new(2, 6), Selection::new(8, 12)],
        0,
        &buf.snapshot(),
    );

    assert!(cut_selections(&mut clipboard, &mut buf, &mut sels, &mut history, false, t0).unwrap());
    assert_eq!(buf.snapshot().text(), "aabbcc");
    assert_eq!(sels.as_slice(), &[Selection::caret(2), Selection::caret(4)]);

    let entry = clipboard.latest().unwrap();
    assert_eq!(entry.segments, vec!["[11]", "[22]"]);
    assert!(!entry.linewise);

    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(buf.snapshot().text(), "aa[11]bb[22]cc");
    assert_eq!(
        sels.as_slice(),
        &[Selection::new(2, 6), Selection::new(8, 12)]
    );
}

#[test]
fn clipboard_multicursor_linewise_copy_deduplicates_and_cuts_last_line() {
    let t0 = Instant::now();
    let mut buf =
        Buffer::with_line_ending(BufferId::new(1), "line0\nline1\nline2", LineEnding::CrLf);
    let mut history = History::new(HistoryConfig::default());
    let mut clipboard = MultiCursorClipboard::default();

    // Two cursors on line 0 and one cursor on line 2 (last line without trailing CRLF).
    let mut sels = SelectionSet::from_selections(
        [
            Selection::caret(1),
            Selection::caret(4),
            Selection::caret(16),
        ],
        0,
        &buf.snapshot(),
    );

    // With linewise_on_empty = false, copying or cutting empty carets is a no-op.
    assert!(copy_selections(&mut clipboard, &buf.snapshot(), &sels, false).is_none());
    assert!(!cut_selections(&mut clipboard, &mut buf, &mut sels, &mut history, false, t0).unwrap());
    assert!(clipboard.is_empty());

    // With linewise_on_empty = true, line 0 is deduplicated and CRLF joins segments.
    let copied = copy_selections(&mut clipboard, &buf.snapshot(), &sels, true).unwrap();
    assert!(copied.linewise);
    assert_eq!(copied.segments, vec!["line0", "line2"]);
    assert_eq!(copied.text, "line0\r\nline2");

    // Cut line 0 and line 2 -> leaves only "line1" without dangling CRLF.
    assert!(cut_selections(&mut clipboard, &mut buf, &mut sels, &mut history, true, t0).unwrap());
    assert_eq!(buf.snapshot().text(), "line1");
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(buf.snapshot().text(), "line0\r\nline1\r\nline2");

    // Cut solely the last line ("line2") without trailing CRLF -> removes preceding CRLF.
    let mut last_only = SelectionSet::caret(16);
    assert!(
        cut_selections(
            &mut clipboard,
            &mut buf,
            &mut last_only,
            &mut history,
            true,
            t0
        )
        .unwrap()
    );
    assert_eq!(buf.snapshot().text(), "line0\r\nline1");

    // Cut consecutive lines ending at last line without trailing CRLF -> leaves empty buffer.
    let mut both_remaining = SelectionSet::from_selections(
        [Selection::caret(1), Selection::caret(9)],
        0,
        &buf.snapshot(),
    );
    assert!(
        cut_selections(
            &mut clipboard,
            &mut buf,
            &mut both_remaining,
            &mut history,
            true,
            t0
        )
        .unwrap()
    );
    assert_eq!(buf.snapshot().text(), "");

    // Single-line buffer without trailing newline: linewise cut clears content.
    let mut single_buf = Buffer::from_text("only");
    let mut single_sels = SelectionSet::caret(2);
    assert!(
        cut_selections(
            &mut clipboard,
            &mut single_buf,
            &mut single_sels,
            &mut history,
            true,
            t0
        )
        .unwrap()
    );
    assert_eq!(single_buf.snapshot().text(), "");
}

#[test]
fn clipboard_multicursor_external_os_text_and_broadcast() {
    // Rule 2: single OS entry with M lines distributes across M cursors (LF and CRLF, optional trailing EOL).
    let lf_entry = MultiCursorClipboard::from_external_os_text("first\nsecond\nthird");
    assert_eq!(
        resolve_paste_texts(&lf_entry, 3),
        vec!["first", "second", "third"]
    );

    let trailing_crlf = MultiCursorClipboard::from_external_os_text("first\r\nsecond\r\nthird\r\n");
    assert_eq!(
        resolve_paste_texts(&trailing_crlf, 3),
        vec!["first", "second", "third"]
    );

    // Exactly M = 4 cursors with "a\nb\nc\n" (4th line intentionally empty).
    let four_with_empty = MultiCursorClipboard::from_external_os_text("a\nb\nc\n");
    assert_eq!(
        resolve_paste_texts(&four_with_empty, 4),
        vec!["a", "b", "c", ""]
    );

    // Rule 3: mismatched cursor count (M = 2 vs 3 lines) broadcasts full text.
    assert_eq!(
        resolve_paste_texts(&lf_entry, 2),
        vec!["first\nsecond\nthird", "first\nsecond\nthird"]
    );

    // Rule 3: multi-segment entry (N = 2) pasted into M = 3 cursors broadcasts full text.
    let two_seg = ClipboardEntry {
        text: "x\ny".to_owned(),
        segments: vec!["x".to_owned(), "y".to_owned()],
        linewise: false,
    };
    assert_eq!(
        resolve_paste_texts(&two_seg, 3),
        vec!["x\ny", "x\ny", "x\ny"]
    );

    // Mixed empty + non-empty selections copy empty string for empty carets.
    let t0 = Instant::now();
    let mut buf = Buffer::from_text("hello world");
    let mut history = History::new(HistoryConfig::default());
    let mut clipboard = MultiCursorClipboard::default();
    let mixed = SelectionSet::from_selections(
        [Selection::new(0, 5), Selection::caret(11)],
        0,
        &buf.snapshot(),
    );
    let mixed_entry = copy_selections(&mut clipboard, &buf.snapshot(), &mixed, true).unwrap();
    assert!(!mixed_entry.linewise);
    assert_eq!(mixed_entry.segments, vec!["hello", ""]);

    // Broadcast OS paste into 2 cursors in Buffer + History + EditLog.
    let mut target_sels = SelectionSet::from_selections(
        [Selection::caret(0), Selection::caret(6)],
        0,
        &buf.snapshot(),
    );
    let os_single = MultiCursorClipboard::from_external_os_text("Z ");
    paste(&os_single, &mut buf, &mut target_sels, &mut history, t0).unwrap();
    assert_eq!(buf.snapshot().text(), "Z hello Z world");
}

#[test]
fn clipboard_ring_retains_latest_up_to_capacity() {
    let mut clipboard = MultiCursorClipboard::default();
    assert_eq!(clipboard.max_entries(), 32);
    assert!(clipboard.is_empty());

    for i in 0..40 {
        clipboard.push_external_os_text(&format!("item-{i}"));
    }

    assert_eq!(clipboard.len(), 32);
    assert_eq!(clipboard.latest().unwrap().text, "item-39");
    assert_eq!(clipboard.history().first().unwrap().text, "item-39");
    assert_eq!(clipboard.history().last().unwrap().text, "item-8");

    clipboard.clear();
    assert!(clipboard.is_empty());
    assert!(clipboard.latest().is_none());
}
