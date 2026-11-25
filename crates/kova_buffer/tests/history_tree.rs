//! Integration tests for `kova_buffer::history` covering multi-cursor Undo/Redo,
//! non-linear branching, deterministic time-based coalescing, explicit compound groups,
//! save-point tracking, error resilience, and memory-limit pruning.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert invariants directly"
)]

use std::time::{Duration, Instant};

use kova_buffer::{
    Buffer, BufferError, Direction, EditBatch, EditOrigin, History, HistoryConfig, HistoryError,
    NodeId, Selection, SelectionSet, TextEdit, apply_delete_backward, apply_insert_all,
};

#[test]
fn multicursor_undo_redo_restores_exact_selections_and_reversed_ranges() {
    let t0 = Instant::now();
    let initial_text = "line0 alpha\nline1 alpha\nline2 alpha\nline3 alpha\nline4 alpha\n";
    let mut buf = Buffer::from_text(initial_text);
    let snap0 = buf.snapshot();

    // 5 selections across 5 lines: mix of forward, reversed, and caret with goal_column.
    let mut sels = SelectionSet::from_selections(
        [
            Selection::new(11, 6),                            // line0: reversed "alpha"
            Selection::new(18, 23).with_goal_column(Some(9)), // line1: forward "alpha" with goal
            Selection::new(35, 30),                           // line2: reversed "alpha"
            Selection::new(42, 47),                           // line3: forward "alpha"
            Selection::caret(54),                             // line4: caret before "alpha"
        ],
        2, // primary on line2
        &snap0,
    );
    let initial_sels = sels.clone();

    let mut history = History::new(HistoryConfig::default());
    let edits: Vec<TextEdit> = sels
        .as_slice()
        .iter()
        .map(|s| TextEdit::new(s.range(), "BETA"))
        .collect();

    history
        .record_and_apply(&mut buf, &mut sels, &edits, EditOrigin::Compound, t0)
        .expect("multicursor replacement must succeed");

    assert_eq!(
        buf.snapshot().text(),
        "line0 BETA\nline1 BETA\nline2 BETA\nline3 BETA\nline4 BETAalpha\n"
    );
    assert_eq!(sels.len(), 5);
    assert_eq!(sels.primary_index(), 2);
    assert!(sels.as_slice().iter().all(Selection::is_empty));
    let after_first = sels.clone();

    // Also test integration with `edit.rs` helper via `EditBatch::build` + `record_step_with_selections`.
    let pre_snap = buf.snapshot();
    let sels_before_helper = sels.clone();
    apply_insert_all(&mut buf, &mut sels, "_X").expect("apply_insert_all must succeed");
    let helper_edits: Vec<TextEdit> = sels_before_helper
        .as_slice()
        .iter()
        .map(|s| TextEdit::new(s.range(), "_X"))
        .collect();
    let batch = EditBatch::build(&pre_snap, &helper_edits).expect("valid batch");
    history.record_step_with_selections(
        batch,
        EditOrigin::Typing,
        sels_before_helper,
        sels.clone(),
        t0 + Duration::from_millis(50),
    );
    let after_second = sels.clone();
    assert_eq!(
        buf.snapshot().text(),
        "line0 BETA_X\nline1 BETA_X\nline2 BETA_X\nline3 BETA_X\nline4 BETA_Xalpha\n"
    );

    // Undo second step -> exact `after_first` selections and text
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(
        buf.snapshot().text(),
        "line0 BETA\nline1 BETA\nline2 BETA\nline3 BETA\nline4 BETAalpha\n"
    );
    assert_eq!(sels, after_first);

    // Undo first step -> exact `initial_sels` including reversed directions and goal_column
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(buf.snapshot().text(), initial_text);
    assert_eq!(sels, initial_sels);
    assert_eq!(sels.primary().direction(), Direction::Backward);
    assert_eq!(sels.as_slice()[1].goal_column(), Some(9));
    assert_eq!(sels.primary_index(), 2);

    // Redo both steps -> exact `after_first`, then `after_second`
    assert!(history.redo(&mut buf, &mut sels).unwrap());
    assert_eq!(sels, after_first);
    assert!(history.redo(&mut buf, &mut sels).unwrap());
    assert_eq!(sels, after_second);
    assert_eq!(
        buf.snapshot().text(),
        "line0 BETA_X\nline1 BETA_X\nline2 BETA_X\nline3 BETA_X\nline4 BETA_Xalpha\n"
    );
}

#[test]
fn branching_preserves_undone_subtree_and_allows_redo_branch() {
    let t0 = Instant::now();
    let mut buf = Buffer::from_text("base");
    let mut sels = SelectionSet::caret(4);
    let mut history = History::new(HistoryConfig::default());

    // Node A: "base_A"
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(4, "_A")],
            EditOrigin::Paste,
            t0,
        )
        .unwrap();
    let node_a = history.current_id();

    // Node B (branch 0 under A): "base_A_B"
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(6, "_B")],
            EditOrigin::Paste,
            t0 + Duration::from_millis(10),
        )
        .unwrap();
    let node_b = history.current_id();
    assert_eq!(buf.snapshot().text(), "base_A_B");

    // Undo back to A
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(history.current_id(), node_a);
    assert_eq!(buf.snapshot().text(), "base_A");
    assert_eq!(history.branches_at_current(), 1);

    // Edit C creates branch 1 under A while preserving B!
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(6, "_C")],
            EditOrigin::Paste,
            t0 + Duration::from_millis(20),
        )
        .unwrap();
    let node_c = history.current_id();
    assert_eq!(buf.snapshot().text(), "base_A_C");

    // Undo back to A: now 2 branches exist under A, active is C (index 1)
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(history.current_id(), node_a);
    assert_eq!(history.branches_at_current(), 2);
    assert_eq!(history.redo_count(), 1);

    // Default redo follows latest branch C
    assert!(history.redo(&mut buf, &mut sels).unwrap());
    assert_eq!(history.current_id(), node_c);
    assert_eq!(buf.snapshot().text(), "base_A_C");

    // Undo and explicitly switch to older branch 0 (B)
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert!(history.redo_branch(0, &mut buf, &mut sels).unwrap());
    assert_eq!(history.current_id(), node_b);
    assert_eq!(buf.snapshot().text(), "base_A_B");
    assert_eq!(sels, SelectionSet::caret(8));

    // After switching active_child to 0, default redo from A now follows B
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert!(history.redo(&mut buf, &mut sels).unwrap());
    assert_eq!(history.current_id(), node_b);

    // Invalid branch index fails cleanly without changing state
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    let err = history.redo_branch(5, &mut buf, &mut sels).unwrap_err();
    assert_eq!(
        err,
        HistoryError::InvalidBranchIndex {
            index: 5,
            available: 2,
        }
    );
    assert_eq!(history.current_id(), node_a);
    assert_eq!(buf.snapshot().text(), "base_A");
}

#[test]
fn time_based_coalescing_respects_window_break_group_savepoint_and_cursor_count() {
    let t0 = Instant::now();
    let mut buf = Buffer::new();
    let mut sels = SelectionSet::caret(0);
    let mut history = History::new(HistoryConfig {
        grouping_window: Duration::from_millis(500),
        ..HistoryConfig::default()
    });

    // 1. Typing within 500ms coalesces
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(0, "h")],
            EditOrigin::Typing,
            t0,
        )
        .unwrap();
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(1, "i")],
            EditOrigin::Typing,
            t0 + Duration::from_millis(500), // exact boundary `<= grouping_window`
        )
        .unwrap();
    assert_eq!(history.undo_count(), 1);

    // 2. SavePoint seals the saved node against coalescing
    history.mark_saved();
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(2, "!")],
            EditOrigin::Typing,
            t0 + Duration::from_millis(550),
        )
        .unwrap();
    assert_eq!(history.undo_count(), 2);

    // Undo once returns exactly to the SavePoint ("hi")
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(buf.snapshot().text(), "hi");
    assert!(!history.is_dirty());

    // Redo back to "hi!" and test explicit break_group()
    assert!(history.redo(&mut buf, &mut sels).unwrap());
    history.break_group();
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(3, "?")],
            EditOrigin::Typing,
            t0 + Duration::from_millis(560),
        )
        .unwrap();
    assert_eq!(history.undo_count(), 3);

    // 3. Changing cursor count breaks coalescing even within window
    sels.push(Selection::caret(0), false, &buf.snapshot());
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(0, ">"), TextEdit::insert(4, "<")],
            EditOrigin::Typing,
            t0 + Duration::from_millis(570),
        )
        .unwrap();
    assert_eq!(history.undo_count(), 4);
    assert_eq!(buf.snapshot().text(), ">hi!?<");
}

#[test]
fn empty_group_creates_no_node_and_unmatched_end_errors() {
    let t0 = Instant::now();
    let sels = SelectionSet::caret(5);
    let mut history = History::new(HistoryConfig::default());

    assert_eq!(
        history.end_group(&sels),
        Err(HistoryError::UnmatchedEndGroup)
    );

    history.begin_group(EditOrigin::Compound, &sels, t0);
    assert_eq!(history.group_depth(), 1);
    assert_eq!(history.end_group(&sels), Ok(None));
    assert_eq!(history.group_depth(), 0);
    assert_eq!(history.undo_count(), 0);
    assert_eq!(history.node_count(), 1);
}

#[test]
fn nested_explicit_groups_undo_together_and_block_open_navigation() {
    let t0 = Instant::now();
    let mut buf = Buffer::from_text("start");
    let mut sels = SelectionSet::caret(5);
    let mut history = History::new(HistoryConfig::default());

    let before_group = sels.clone();
    history.begin_group(EditOrigin::Compound, &before_group, t0);
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(5, "_one")],
            EditOrigin::Typing,
            t0 + Duration::from_secs(1),
        )
        .unwrap();

    history.begin_group(EditOrigin::Compound, &sels, t0 + Duration::from_secs(2));
    assert_eq!(history.group_depth(), 2);

    assert_eq!(
        history.undo(&mut buf, &mut sels),
        Err(HistoryError::GroupStillOpen)
    );
    assert_eq!(
        history.redo(&mut buf, &mut sels),
        Err(HistoryError::GroupStillOpen)
    );
    assert_eq!(
        history.redo_branch(0, &mut buf, &mut sels),
        Err(HistoryError::GroupStillOpen)
    );

    // No-op edit inside group does not break or corrupt anything
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::new(0..0, "")],
            EditOrigin::Other,
            t0 + Duration::from_secs(3),
        )
        .unwrap();

    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(9, "_two")],
            EditOrigin::Paste,
            t0 + Duration::from_secs(10),
        )
        .unwrap();

    assert_eq!(history.end_group(&sels), Ok(None));
    assert_eq!(history.group_depth(), 1);

    let final_sels = SelectionSet::new_single_for_test(0, 13);
    assert!(history.end_group(&final_sels).unwrap().is_some());
    assert_eq!(history.group_depth(), 0);
    assert_eq!(history.undo_count(), 1);
    assert_eq!(buf.snapshot().text(), "start_one_two");

    // Subsequent edit outside group does NOT coalesce into the sealed compound node
    sels = final_sels.clone();
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(13, "!")],
            EditOrigin::Typing,
            t0 + Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(history.undo_count(), 2);

    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(sels, final_sels);
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(buf.snapshot().text(), "start");
    assert_eq!(sels, before_group);
}

#[test]
fn save_point_tracking_across_edits_undo_redo_and_branches() {
    let t0 = Instant::now();
    let mut buf = Buffer::from_text("doc");
    let mut sels = SelectionSet::caret(3);
    let mut history = History::new(HistoryConfig::default());

    assert!(!history.is_dirty());
    assert_eq!(history.saved_node(), Some(NodeId::ROOT));

    // Edit 1 -> dirty
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(3, "_1")],
            EditOrigin::Paste,
            t0,
        )
        .unwrap();
    assert!(history.is_dirty());

    // Undo back to root -> clean again
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert!(!history.is_dirty());

    // Redo to Edit 1 and mark_saved
    assert!(history.redo(&mut buf, &mut sels).unwrap());
    history.mark_saved();
    assert!(!history.is_dirty());

    // Edit 2 -> dirty
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(5, "_2")],
            EditOrigin::Paste,
            t0 + Duration::from_millis(10),
        )
        .unwrap();
    assert!(history.is_dirty());

    // Undo to Edit 1 -> clean
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert!(!history.is_dirty());

    // Undo to root -> dirty; create branch B -> dirty; mark_saved at B -> clean
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert!(history.is_dirty());
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(3, "_B")],
            EditOrigin::Paste,
            t0 + Duration::from_millis(20),
        )
        .unwrap();
    assert!(history.is_dirty());
    history.mark_saved();
    assert!(!history.is_dirty());

    // Switch back to branch 0 (Edit 1) -> dirty again
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert!(history.redo_branch(0, &mut buf, &mut sels).unwrap());
    assert!(history.is_dirty());
}

#[test]
fn error_resilience_leaves_buffer_selections_and_history_consistent() {
    let t0 = Instant::now();
    let mut buf = Buffer::from_text("ab\r\ncd");
    let mut sels = SelectionSet::caret(2);
    let mut history = History::new(HistoryConfig::default());

    // Overlapping edits
    let err = history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::new(0..2, "X"), TextEdit::new(1..2, "Y")],
            EditOrigin::Other,
            t0,
        )
        .unwrap_err();
    assert!(matches!(
        err,
        HistoryError::Buffer(BufferError::OverlappingEdits { .. })
    ));

    // Splitting CRLF
    let err_crlf = history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(3, "Z")],
            EditOrigin::Typing,
            t0,
        )
        .unwrap_err();
    assert_eq!(
        err_crlf,
        HistoryError::Buffer(BufferError::SplitsCrLf { byte: 3 })
    );

    // Out-of-bounds range
    let err_oob = history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::delete(0..99)],
            EditOrigin::Other,
            t0,
        )
        .unwrap_err();
    assert!(matches!(
        err_oob,
        HistoryError::Buffer(BufferError::InvalidByteRange { .. })
    ));

    assert_eq!(buf.snapshot().text(), "ab\r\ncd");
    assert_eq!(sels, SelectionSet::caret(2));
    assert_eq!(history.node_count(), 1);
    assert_eq!(history.current_id(), NodeId::ROOT);
    assert!(!history.can_undo());
}

#[test]
fn pruning_drops_inactive_side_branches_before_active_ancestors() {
    let t0 = Instant::now();
    let mut buf = Buffer::from_text("0");
    let mut sels = SelectionSet::caret(1);
    let mut history = History::new(HistoryConfig {
        max_nodes: 4,
        max_stored_bytes: 1024,
        ..HistoryConfig::default()
    });

    // Root -> A -> B
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(1, "A")],
            EditOrigin::Paste,
            t0,
        )
        .unwrap();
    let node_a = history.current_id();

    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(2, "B")],
            EditOrigin::Paste,
            t0,
        )
        .unwrap();
    let node_b = history.current_id();

    // Undo to A and create C (now 4 nodes: Root, A, B, C)
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(2, "C")],
            EditOrigin::Paste,
            t0,
        )
        .unwrap();
    let node_c = history.current_id();
    assert_eq!(history.node_count(), 4);

    // Mark A as saved, then add D under C -> 5 nodes > max_nodes(4).
    // Inactive side branch B must be pruned first; Root, A (saved), C, D remain!
    assert!(history.undo(&mut buf, &mut sels).unwrap());
    assert_eq!(history.current_id(), node_a);
    history.mark_saved();
    assert!(history.redo(&mut buf, &mut sels).unwrap());
    assert_eq!(history.current_id(), node_c);

    history
        .record_and_apply(
            &mut buf,
            &mut sels,
            &[TextEdit::insert(3, "D")],
            EditOrigin::Paste,
            t0,
        )
        .unwrap();

    assert_eq!(history.node_count(), 4);
    assert!(history.node(node_b).is_none());
    assert!(history.node(node_a).is_some());
    assert_eq!(history.saved_node(), Some(node_a));
    assert_eq!(history.undo_count(), 3);
}

#[test]
fn multicursor_sequence_with_backspace_and_apply_edit_now_roundtrips() {
    let mut buf = Buffer::from_text("foo_1\nfoo_2\nfoo_3\n");
    let mut sels = SelectionSet::from_selections(
        [
            Selection::caret(5),
            Selection::caret(11),
            Selection::caret(17),
        ],
        0,
        &buf.snapshot(),
    );
    let initial_sels = sels.clone();
    let mut history = History::new(HistoryConfig::default());

    // Step 1: apply_edit_now inserts "!" at all 3 cursors
    let edits: Vec<TextEdit> = sels
        .as_slice()
        .iter()
        .map(|s| TextEdit::insert(s.head(), "!"))
        .collect();
    history
        .apply_edit_now(&mut buf, &mut sels, &edits, EditOrigin::Other)
        .unwrap();
    assert_eq!(buf.snapshot().text(), "foo_1!\nfoo_2!\nfoo_3!\n");

    // Step 2: multi-cursor delete backward via `edit.rs` helper + manual batch recording
    let pre_snap = buf.snapshot();
    let sels_before_del = sels.clone();
    apply_delete_backward(&mut buf, &mut sels).unwrap();
    // Compute exact batch corresponding to `apply_delete_backward` (deleted "!" at each cursor)
    let actual_del_edits: Vec<TextEdit> = sels_before_del
        .as_slice()
        .iter()
        .map(|s| TextEdit::delete(s.head() - 1..s.head()))
        .collect();
    let del_batch = EditBatch::build(&pre_snap, &actual_del_edits).unwrap();
    history.record_step_with_selections(
        del_batch,
        EditOrigin::DeleteBackward,
        sels_before_del,
        sels.clone(),
        Instant::now(),
    );
    assert_eq!(buf.snapshot().text(), "foo_1\nfoo_2\nfoo_3\n");

    // Undo both -> back to initial text & initial_sels
    while history.undo(&mut buf, &mut sels).unwrap() {}
    assert_eq!(buf.snapshot().text(), "foo_1\nfoo_2\nfoo_3\n");
    assert_eq!(sels, initial_sels);

    // Redo both
    while history.redo(&mut buf, &mut sels).unwrap() {}
    assert_eq!(buf.snapshot().text(), "foo_1\nfoo_2\nfoo_3\n");
}

trait SelectionSetExt {
    fn new_single_for_test(anchor: usize, head: usize) -> Self;
}

impl SelectionSetExt for SelectionSet {
    fn new_single_for_test(anchor: usize, head: usize) -> Self {
        Self::single(Selection::new(anchor, head))
    }
}
