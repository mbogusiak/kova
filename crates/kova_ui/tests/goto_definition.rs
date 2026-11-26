//! Integration tests for *Go to Definition*, Back/Forward history, Vim `gd`,
//! read-only tabs, and modifier safety (`KOVA-051`).

#![allow(
    clippy::expect_used,
    reason = "integration test harness aborts on invalid test fixtures"
)]

use std::path::PathBuf;
use std::time::Instant;

use kova_buffer::Buffer;
use kova_core::navigation::{
    NavLocation, NavigationHistory, SourceLanguageKind, extract_symbol_query,
    navigable_identifier_at_byte, resolve_local_definition,
};
use kova_core::{FocusContext, KeyCode, KeyStroke, Modifiers, Platform};
use kova_ui::editor::state::EditorState;
use kova_ui::editor::vim::{VimBridge, VimKeyResult};
use kova_ui::keymap::{
    DispatchContext, DispatchOutcome, KeyDispatchLayer, ShellAction, shortcut_stroke,
};
use kova_ui::{TabGroupModel, is_go_to_def_modifiers};

#[test]
fn java_import_same_file_and_back_forward_history_end_to_end() {
    let app_path = PathBuf::from("src/main/java/dev/kova/App.java");
    let svc_path = PathBuf::from("src/main/java/dev/kova/OrderService.java");
    let app_src = "\
package dev.kova;

import dev.kova.OrderService;

public class App {
    public void run() {
        OrderService svc = new OrderService();
        svc.submit();
    }
}
";
    let svc_src = "\
package dev.kova;

public class OrderService {
    public void submit() {}
}
";

    let app_buf = Buffer::from_text(app_src);
    let snap = app_buf.snapshot();
    let open = vec![
        (app_path.clone(), app_src.to_owned()),
        (svc_path.clone(), svc_src.to_owned()),
    ];
    let files = vec![app_path.clone(), svc_path.clone()];

    // Jump 1: `OrderService` -> `OrderService.java:3`
    let origin1 = NavLocation::new(&app_path, 7, 9);
    let pos1 = app_src.find("OrderService svc").expect("ident");
    let q1 = extract_symbol_query(&snap, &app_path, pos1).expect("query1");
    let hit1 = resolve_local_definition(&q1, app_src, &open, None, &files).expect("hit1");
    assert_eq!(hit1.path, svc_path);
    assert_eq!(hit1.line, 3);

    let mut history = NavigationHistory::new();
    history.push(origin1.clone());

    // Jump 2: `svc.submit()` -> `OrderService.java:4`
    let origin2 = NavLocation::new(&app_path, 8, 13);
    let pos2 = app_src.find("submit()").expect("ident");
    let q2 = extract_symbol_query(&snap, &app_path, pos2).expect("query2");
    let hit2 = resolve_local_definition(&q2, app_src, &open, None, &files).expect("hit2");
    assert_eq!(hit2.path, svc_path);
    assert_eq!(hit2.line, 4);
    history.push(origin2.clone());

    let at_hit2 = NavLocation::new(&hit2.path, hit2.line, hit2.column);
    assert_eq!(history.go_back(at_hit2.clone()), Some(origin2.clone()));
    assert_eq!(history.go_back(origin2.clone()), Some(origin1));
    assert_eq!(
        history.go_forward(NavLocation::new(&app_path, 7, 9)),
        Some(origin2)
    );
}

#[test]
fn cmd_click_modifiers_preserve_plain_clicks_gutter_and_alt_multicursor() {
    // Plain click and Alt+Click (multi-cursor) never activate Go to Definition.
    assert!(!is_go_to_def_modifiers(gpui::Modifiers::none(), true));
    assert!(!is_go_to_def_modifiers(gpui::Modifiers::alt(), true));
    assert!(!is_go_to_def_modifiers(gpui::Modifiers::shift(), true));
    assert!(!is_go_to_def_modifiers(gpui::Modifiers::none(), false));
    assert!(!is_go_to_def_modifiers(gpui::Modifiers::alt(), false));

    // Only platform primary modifier activates Go to Definition.
    assert!(is_go_to_def_modifiers(gpui::Modifiers::command(), true));
    assert!(is_go_to_def_modifiers(gpui::Modifiers::control(), false));

    // EditorState plain clicks and Alt+Click multi-cursor behave identically as before.
    let mut state = EditorState::from_text("Order a;\nOrder b;\n");
    state.set_caret(0);
    assert_eq!(state.cursor_count(), 1);
    state.add_cursor_at(9);
    assert_eq!(state.cursor_count(), 2);

    // Keywords are never underlined/clickable as Go to Definition targets.
    let snap = state.snapshot();
    let java_buf = Buffer::from_text("public class Foo {}");
    assert_eq!(
        navigable_identifier_at_byte(&java_buf.snapshot(), 0, SourceLanguageKind::Java),
        None
    );
    assert!(navigable_identifier_at_byte(&snap, 0, SourceLanguageKind::Java).is_some());
}

#[test]
fn keymap_vim_gd_and_read_only_tab_contract_work_together() {
    let now = Instant::now();
    let mut layer = KeyDispatchLayer::new(Platform::MacOs);
    let ro_ctx = DispatchContext {
        leaf: FocusContext::EditorText,
        vim_enabled: false,
        editor_read_only: true,
    };

    assert_eq!(
        layer.dispatch(
            shortcut_stroke(&gpui::Keystroke::parse("cmd-b").expect("ks")).expect("stroke"),
            ro_ctx,
            now
        ),
        DispatchOutcome::Action(ShellAction::GoToDefinition)
    );
    assert_eq!(
        layer.dispatch(
            shortcut_stroke(&gpui::Keystroke::parse("cmd-[").expect("ks")).expect("stroke"),
            ro_ctx,
            now
        ),
        DispatchOutcome::Action(ShellAction::NavigateBack)
    );
    assert_eq!(
        layer.dispatch(
            shortcut_stroke(&gpui::Keystroke::parse("cmd-]").expect("ks")).expect("stroke"),
            ro_ctx,
            now
        ),
        DispatchOutcome::Action(ShellAction::NavigateForward)
    );

    let mut state = EditorState::from_text("public final class String {}");
    state.set_read_only(true);
    let mut bridge = VimBridge::new(&mut state);
    assert_eq!(
        bridge.handle_key(&mut state, KeyStroke::unmodified(KeyCode::Char('g'))),
        VimKeyResult::Handled { edited: false }
    );
    assert_eq!(
        bridge.handle_key(&mut state, KeyStroke::unmodified(KeyCode::Char('d'))),
        VimKeyResult::GoToDefinition
    );

    let mut tabs = TabGroupModel::new(kova_ui::PaneId(1));
    let tab_id = tabs.open_or_focus(
        Some(std::path::Path::new("/cache/java/lang/String.java")),
        "String.java",
        "Java",
    );
    tabs.set_read_only(tab_id, true);
    assert!(tabs.active_tab().is_some_and(|t| t.is_read_only));
    let _ = Modifiers::NONE;
}
