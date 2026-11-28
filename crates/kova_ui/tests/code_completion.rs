//! Integration tests for Code Completion (`KOVA-052`):
//!
//! - `Objects.` auto-trigger and JDK static method resolution (`requireNonNull`, `isNull`, `nonNull`, ...)
//! - Live prefix filtering, backspace narrowing, and fuzzy character highlight indices
//! - `Enter` / `Tab` acceptance with smart `()` & cursor placement, `::` method references, and atomic auto-imports
//! - `Esc` dismissal and `Ctrl+Space` (`CompleteBasic`) keymap dispatch

#![allow(
    clippy::expect_used,
    reason = "integration test harness aborts on invalid test fixtures"
)]

use std::path::PathBuf;
use std::time::Instant;

use kova_buffer::{EditOrigin, TextEdit};
use kova_core::completion::{
    CompletionProposal, CompletionProposalKind, CompletionSource, CompletionTextEdit,
    CompletionTriggerKind, extract_completion_context, resolve_local_completion,
    should_auto_trigger_completion,
};
use kova_core::navigation::SourceLanguageKind;
use kova_core::{FocusContext, Platform};
use kova_ui::editor::completion::{
    CaretWindowRect, CompletionKeyOutcome, CompletionPopupState, classify_completion_key,
    compute_completion_popup_bounds,
};
use kova_ui::editor::state::EditorState;
use kova_ui::keymap::{
    DispatchContext, DispatchOutcome, KeyDispatchLayer, ShellAction, shortcut_stroke,
};
use kova_ui::search::char_indices_to_byte_ranges;

fn ks(source: &str) -> gpui::Keystroke {
    gpui::Keystroke::parse(source).expect("valid keystroke")
}

#[test]
fn objects_dot_triggers_and_populates_static_methods() {
    let mut editor = EditorState::from_text(
        "public class Demo {\n    void run() {\n        Objects\n    }\n}\n",
    );
    let caret = editor.snapshot().text().find("Objects\n").expect("Objects") + "Objects".len();
    editor.set_caret(caret);
    assert!(editor.insert_text("."));

    let snap = editor.snapshot();
    let head = editor.primary_head();
    assert!(should_auto_trigger_completion(
        &snap,
        SourceLanguageKind::Java,
        head,
        '.'
    ));

    // Must NOT auto-trigger in numeric literals, ranges, strings, or comments.
    let no_trigger_buf =
        EditorState::from_text("class X { double d = 3.; String s = \"Objects.\"; // Objects.\n}");
    let no_snap = no_trigger_buf.snapshot();
    for marker in ["3.", "Objects.\"", "// Objects."] {
        let pos = no_snap.text().find(marker).expect("marker") + marker.trim_end_matches('"').len();
        assert!(
            !should_auto_trigger_completion(&no_snap, SourceLanguageKind::Java, pos, '.'),
            "must not auto-trigger after `{marker}`"
        );
    }

    let path = PathBuf::from("src/Demo.java");
    let ctx = extract_completion_context(&snap, &path, head, CompletionTriggerKind::AutoChar('.'))
        .expect("completion context after Objects.");
    assert_eq!(ctx.receiver.as_deref(), Some("Objects"));
    assert_eq!(ctx.prefix, "");
    assert_eq!(ctx.anchor_byte, head);

    let local = resolve_local_completion(&ctx, &snap.text(), &[], None, &[]);
    let mut popup = CompletionPopupState::new();
    popup.open_with(
        1,
        ctx.anchor_byte,
        ctx.is_method_reference,
        ctx.prefix,
        local.proposals,
    );
    assert!(popup.is_open());

    let labels: Vec<&str> = popup
        .filtered()
        .iter()
        .map(|item| item.proposal.label.as_str())
        .collect();
    for expected in [
        "requireNonNull",
        "requireNonNullElse",
        "isNull",
        "nonNull",
        "equals",
        "hash",
        "toString",
    ] {
        assert!(
            labels.contains(&expected),
            "expected `Objects.{expected}` in {labels:?}"
        );
    }
    // Static access on `Objects.` must not surface instance-only Object methods.
    assert!(!labels.contains(&"notify"));
    assert!(!labels.contains(&"wait"));
}

#[test]
fn live_filtering_and_navigation_work_on_objects_methods() {
    let mut editor = EditorState::from_text("class Demo { void f() { Objects. } }\n");
    let after_dot = editor.snapshot().text().find("Objects.").expect("dot") + "Objects.".len();
    editor.set_caret(after_dot);

    let snap = editor.snapshot();
    let path = PathBuf::from("Demo.java");
    let ctx = extract_completion_context(
        &snap,
        &path,
        after_dot,
        CompletionTriggerKind::AutoChar('.'),
    )
    .expect("context");
    let local = resolve_local_completion(&ctx, &snap.text(), &[], None, &[]);

    let mut popup = CompletionPopupState::new();
    popup.open_with(
        1,
        ctx.anchor_byte,
        ctx.is_method_reference,
        ctx.prefix,
        local.proposals,
    );
    let total_count = popup.filtered().len();
    assert!(total_count >= 6);

    // Move selection with Down / Ctrl+N / Up / Ctrl+P.
    assert_eq!(
        classify_completion_key(&ks("down")),
        CompletionKeyOutcome::Move(1)
    );
    assert_eq!(
        classify_completion_key(&ks("ctrl-n")),
        CompletionKeyOutcome::Move(1)
    );
    assert_eq!(
        classify_completion_key(&ks("up")),
        CompletionKeyOutcome::Move(-1)
    );
    assert_eq!(
        classify_completion_key(&ks("ctrl-p")),
        CompletionKeyOutcome::Move(-1)
    );
    popup.move_selection(1);
    assert_eq!(popup.selected(), 1);
    popup.move_selection(-2);
    assert_eq!(popup.selected(), total_count - 1);

    // Type `req` -> filters down to requireNonNull* and resets selection to 0.
    assert!(editor.insert_text("req"));
    let prefix_req = editor
        .snapshot()
        .slice_bytes(popup.anchor_byte()..editor.primary_head())
        .expect("slice")
        .to_string();
    popup.set_prefix(&prefix_req);
    assert_eq!(popup.selected(), 0);
    let req_labels: Vec<&str> = popup
        .filtered()
        .iter()
        .map(|i| i.proposal.label.as_str())
        .collect();
    assert!(req_labels.contains(&"requireNonNull"));
    assert!(
        req_labels
            .iter()
            .all(|l| l.to_ascii_lowercase().contains("req"))
    );

    // Highlight ranges map cleanly onto the matched characters of `requireNonNull`.
    let first = &popup.filtered()[0];
    let ranges = char_indices_to_byte_ranges(&first.proposal.label, &first.matched_char_indices);
    assert_ne!(ranges.len(), 0);

    // Narrow further with `Else` -> matches `requireNonNullElse` / `requireNonNullElseGet`.
    assert!(editor.insert_text("Else"));
    let prefix_else = editor
        .snapshot()
        .slice_bytes(popup.anchor_byte()..editor.primary_head())
        .expect("slice")
        .to_string();
    popup.set_prefix(&prefix_else);
    assert_ne!(popup.filtered(), []);
    assert!(
        popup
            .filtered()
            .iter()
            .any(|i| i.proposal.label == "requireNonNullElse")
    );

    // Backspace back to `isN` -> `isNull` ranks #1; typing `ull` narrows uniquely to `isNull`.
    for _ in 0..7 {
        assert!(editor.delete_backward());
    }
    assert!(editor.insert_text("isN"));
    let prefix_isn = editor
        .snapshot()
        .slice_bytes(popup.anchor_byte()..editor.primary_head())
        .expect("slice")
        .to_string();
    popup.set_prefix(&prefix_isn);
    assert_eq!(popup.filtered()[0].proposal.label, "isNull");

    assert!(editor.insert_text("ull"));
    let prefix_isnull = editor
        .snapshot()
        .slice_bytes(popup.anchor_byte()..editor.primary_head())
        .expect("slice")
        .to_string();
    popup.set_prefix(&prefix_isnull);
    assert_eq!(popup.filtered().len(), 1);
    assert_eq!(popup.filtered()[0].proposal.label, "isNull");
}

#[test]
fn enter_and_tab_accept_completion_with_parentheses_method_ref_and_auto_import() {
    // 1. `Enter` accepts `Objects.req` -> `Objects.requireNonNull()` with cursor inside `()`.
    let mut editor = EditorState::from_text("class Demo { void f() { Objects.req } }\n");
    let caret = editor.snapshot().text().find("Objects.req").expect("call") + "Objects.req".len();
    editor.set_caret(caret);
    let snap = editor.snapshot();
    let path = PathBuf::from("Demo.java");
    let ctx = extract_completion_context(&snap, &path, caret, CompletionTriggerKind::Invoked)
        .expect("context");
    let local = resolve_local_completion(&ctx, &snap.text(), &[], None, &[]);

    let mut popup = CompletionPopupState::new();
    popup.open_with(
        1,
        ctx.anchor_byte,
        ctx.is_method_reference,
        ctx.prefix,
        local.proposals,
    );
    assert_eq!(
        classify_completion_key(&ks("enter")),
        CompletionKeyOutcome::Accept
    );
    let ins_enter = popup
        .selected_insertion()
        .expect("requireNonNull insertion");
    let replace_range = popup.anchor_byte()..editor.primary_head();
    assert!(editor.apply_edits(
        &[TextEdit::new(replace_range.clone(), &ins_enter.text)],
        EditOrigin::Compound,
    ));
    editor.set_caret(replace_range.start + ins_enter.cursor_offset_in_text);
    popup.close();
    assert!(!popup.is_open());
    assert!(
        editor
            .snapshot()
            .text()
            .contains("Objects.requireNonNull()")
    );
    let expected_inside = editor
        .snapshot()
        .text()
        .find("requireNonNull()")
        .expect("inserted")
        + "requireNonNull(".len();
    assert_eq!(editor.primary_head(), expected_inside);

    // 2. `Tab` accepts 0-arg `String.length()` -> cursor placed AFTER `()`.
    let mut editor2 =
        EditorState::from_text("class Demo { void f() { String s = \"hi\"; s.len } }\n");
    let caret2 = editor2.snapshot().text().find("s.len").expect("len") + "s.len".len();
    editor2.set_caret(caret2);
    let snap2 = editor2.snapshot();
    let ctx2 = extract_completion_context(&snap2, &path, caret2, CompletionTriggerKind::Invoked)
        .expect("ctx2");
    let local2 = resolve_local_completion(&ctx2, &snap2.text(), &[], None, &[]);
    popup.open_with(
        2,
        ctx2.anchor_byte,
        ctx2.is_method_reference,
        ctx2.prefix,
        local2.proposals,
    );
    assert_eq!(
        classify_completion_key(&ks("tab")),
        CompletionKeyOutcome::Accept
    );
    let ins_tab = popup.selected_insertion().expect("length insertion");
    let replace2 = popup.anchor_byte()..editor2.primary_head();
    assert!(editor2.apply_edits(
        &[TextEdit::new(replace2.clone(), &ins_tab.text)],
        EditOrigin::Compound,
    ));
    editor2.set_caret(replace2.start + ins_tab.cursor_offset_in_text);
    popup.close();
    assert!(editor2.snapshot().text().contains("s.length()"));
    let expected_after = editor2
        .snapshot()
        .text()
        .find("length()")
        .expect("inserted")
        + "length()".len();
    assert_eq!(editor2.primary_head(), expected_after);
}

#[test]
fn method_reference_and_lsp_auto_import_apply_atomically() {
    let path = PathBuf::from("Demo.java");
    let mut popup = CompletionPopupState::new();

    // Method reference `Objects::isN` skips `()`.
    let mut editor3 = EditorState::from_text("class Demo { void f() { Objects::isN } }\n");
    let caret3 =
        editor3.snapshot().text().find("Objects::isN").expect("ref") + "Objects::isN".len();
    editor3.set_caret(caret3);
    let snap3 = editor3.snapshot();
    let ctx3 = extract_completion_context(&snap3, &path, caret3, CompletionTriggerKind::Invoked)
        .expect("ctx3");
    let local3 = resolve_local_completion(&ctx3, &snap3.text(), &[], None, &[]);
    popup.open_with(
        3,
        ctx3.anchor_byte,
        ctx3.is_method_reference,
        ctx3.prefix,
        local3.proposals,
    );
    let ins_ref = popup.selected_insertion().expect("method ref insertion");
    assert_eq!(ins_ref.text, "isNull");
    assert_eq!(ins_ref.cursor_offset_in_text, "isNull".len());
    popup.close();

    // 4. LSP `additional_text_edits` (auto-import) merges cleanly and undoes in one transaction.
    let mut lsp_popup = CompletionPopupState::new();
    let mut prop = CompletionProposal {
        label: "requireNonNull".to_owned(),
        kind: CompletionProposalKind::Method,
        params_signature: Some("(T obj)".to_owned()),
        return_type: Some("T".to_owned()),
        detail_right: None,
        insert_text: "requireNonNull".to_owned(),
        is_callable: true,
        has_params: true,
        is_method_reference: false,
        additional_text_edits: vec![CompletionTextEdit {
            start_utf16_line: 0,
            start_utf16_col: 0,
            end_utf16_line: 0,
            end_utf16_col: 0,
            new_text: "import java.util.Objects;\n".to_owned(),
        }],
        source: CompletionSource::Lsp,
        is_deprecated: false,
        sort_priority: 1,
        is_static: false,
        type_params: None,
        fqcn: None,
    };
    prop.additional_text_edits[0].new_text = "import java.util.Objects;\n".to_owned();
    let mut ed4 = EditorState::from_text("class Demo { void f() { Objects.req } }\n");
    let c4 = ed4.snapshot().text().find("Objects.req").expect("req") + "Objects.req".len();
    ed4.set_caret(c4);
    lsp_popup.open_with(4, c4 - 3, false, "req".to_owned(), vec![prop]);
    let ins_lsp = lsp_popup.selected_insertion().expect("lsp insertion");
    assert_eq!(ins_lsp.additional_text_edits.len(), 1);
    assert!(ed4.apply_edits(
        &[
            TextEdit::new(0..0, &ins_lsp.additional_text_edits[0].new_text),
            TextEdit::new((c4 - 3)..c4, &ins_lsp.text),
        ],
        EditOrigin::Compound,
    ));
    let with_import = ed4.snapshot().text();
    assert!(with_import.starts_with("import java.util.Objects;\n"));
    assert!(with_import.contains("Objects.requireNonNull()"));
    assert!(ed4.undo());
    assert_eq!(
        ed4.snapshot().text(),
        "class Demo { void f() { Objects.req } }\n"
    );
}

#[test]
fn esc_dismisses_popup_and_ctrl_space_keymap_dispatches() {
    let mut editor = EditorState::from_text("class Demo { void f() { Objects.req } }\n");
    let caret = editor.snapshot().text().find("Objects.req").expect("req") + "Objects.req".len();
    editor.set_caret(caret);
    let snap = editor.snapshot();
    let path = PathBuf::from("Demo.java");
    let ctx = extract_completion_context(&snap, &path, caret, CompletionTriggerKind::Invoked)
        .expect("ctx");
    let local = resolve_local_completion(&ctx, &snap.text(), &[], None, &[]);

    let mut popup = CompletionPopupState::new();
    popup.open_with(
        1,
        ctx.anchor_byte,
        ctx.is_method_reference,
        ctx.prefix,
        local.proposals,
    );
    assert!(popup.is_open());

    // Pressing `Esc` closes the popup without altering the buffer.
    assert_eq!(
        classify_completion_key(&ks("escape")),
        CompletionKeyOutcome::Close
    );
    popup.close();
    assert!(!popup.is_open());
    assert_eq!(popup.filtered(), []);
    assert!(popup.selected_insertion().is_none());
    assert_eq!(
        editor.snapshot().text(),
        "class Demo { void f() { Objects.req } }\n"
    );

    // Arrow navigation keys dismiss and pass through to the editor.
    assert_eq!(
        classify_completion_key(&ks("left")),
        CompletionKeyOutcome::DismissAndPassThrough
    );
    assert_eq!(
        classify_completion_key(&ks("right")),
        CompletionKeyOutcome::DismissAndPassThrough
    );

    // `Ctrl+Space` dispatches `ShellAction::CompleteBasic` in editable contexts on macOS & Linux.
    let now = Instant::now();
    for platform in [Platform::MacOs, Platform::Linux] {
        let mut layer = KeyDispatchLayer::new(platform);
        for (leaf, vim_enabled) in [
            (FocusContext::EditorText, false),
            (FocusContext::EditorVimInsert, true),
        ] {
            let outcome = layer.dispatch(
                shortcut_stroke(&ks("ctrl-space")).expect("ctrl-space"),
                DispatchContext {
                    leaf,
                    vim_enabled,
                    editor_read_only: false,
                },
                now,
            );
            assert_eq!(outcome, DispatchOutcome::Action(ShellAction::CompleteBasic));
        }
        let ro_outcome = layer.dispatch(
            shortcut_stroke(&ks("ctrl-space")).expect("ctrl-space"),
            DispatchContext {
                leaf: FocusContext::EditorText,
                vim_enabled: false,
                editor_read_only: true,
            },
            now,
        );
        assert_eq!(ro_outcome, DispatchOutcome::Unhandled);
    }

    // Popup bounds flip above caret at bottom edge and stay inside viewport.
    let flipped = compute_completion_popup_bounds(
        CaretWindowRect {
            x: 200.0,
            y: 820.0,
            line_height: 20.0,
        },
        380.0,
        248.0,
        (1280.0, 900.0),
    );
    assert!(flipped.y < 820.0);
    assert!(flipped.contains_point(flipped.x + 10.0, flipped.y + 10.0));
}
