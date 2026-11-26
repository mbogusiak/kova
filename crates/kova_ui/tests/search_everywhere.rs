//! Integration tests for Double-Shift (`ModifiersChangedEvent`) dispatch and
//! *Search Everywhere* interactivity (`KOVA-050`).

#![allow(
    clippy::expect_used,
    reason = "integration test harness aborts on invalid test fixtures"
)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use kova_core::builtin_actions::builtin_action_registry_ref;
use kova_core::{FocusContext, Platform, jetbrains_keymap_layer_ref};
use kova_ui::keymap::{
    DispatchContext, DispatchOutcome, KeyDispatchLayer, ShellAction, shortcut_stroke,
};
use kova_ui::search::{
    SearchEntry, SearchEntryKind, SearchEverywhereState, SearchKeyOutcome, SearchScope,
    SearchTarget, char_indices_to_byte_ranges,
};

fn ks(source: &str) -> gpui::Keystroke {
    gpui::Keystroke::parse(source).expect("valid keystroke")
}

fn ks_char(source: &str, key_char: &str) -> gpui::Keystroke {
    let mut stroke = ks(source);
    stroke.key_char = Some(key_char.to_owned());
    stroke
}

fn tap_shift(
    layer: &mut KeyDispatchLayer,
    ctx: DispatchContext,
    press_at: Instant,
    release_at: Instant,
) -> DispatchOutcome {
    assert_eq!(
        layer.dispatch_modifiers_changed(gpui::Modifiers::shift(), false, ctx, press_at),
        DispatchOutcome::Unhandled
    );
    layer.dispatch_modifiers_changed(gpui::Modifiers::none(), false, ctx, release_at)
}

fn sample_entries() -> Vec<SearchEntry> {
    let mut entries = vec![
        SearchEntry {
            kind: SearchEntryKind::File,
            title: "OrderApp.java".to_owned(),
            subtitle: "src/main/java/dev/kova/demo".to_owned(),
            match_text: "src/main/java/dev/kova/demo/OrderApp.java".to_owned(),
            icon: "F",
            shortcut_hint: None,
            target: SearchTarget::File {
                path: PathBuf::from("src/main/java/dev/kova/demo/OrderApp.java"),
                line: None,
            },
        },
        SearchEntry {
            kind: SearchEntryKind::Class,
            title: "OrderApp".to_owned(),
            subtitle: "Class · OrderApp.java:5".to_owned(),
            match_text: "OrderApp".to_owned(),
            icon: "C",
            shortcut_hint: None,
            target: SearchTarget::Symbol {
                path: Some(PathBuf::from("src/main/java/dev/kova/demo/OrderApp.java")),
                line: 5,
                col: 14,
            },
        },
        SearchEntry {
            kind: SearchEntryKind::Symbol,
            title: "żółćHelper".to_owned(),
            subtitle: "Method · OrderApp.java:18".to_owned(),
            match_text: "żółćHelper".to_owned(),
            icon: "m",
            shortcut_hint: None,
            target: SearchTarget::Symbol {
                path: Some(PathBuf::from("src/main/java/dev/kova/demo/OrderApp.java")),
                line: 18,
                col: 10,
            },
        },
    ];

    let keymap_layer = jetbrains_keymap_layer_ref(Platform::MacOs);
    for desc in builtin_action_registry_ref().iter() {
        let Some(action) = ShellAction::from_action_id(&desc.id) else {
            continue;
        };
        entries.push(SearchEntry {
            kind: SearchEntryKind::Action,
            title: desc.title.clone().into_owned(),
            subtitle: desc.id.as_str().to_owned(),
            match_text: format!("{} {}", desc.title, desc.id),
            icon: "A",
            shortcut_hint: keymap_layer
                .chord_for_action(desc.id.as_str())
                .map(ToString::to_string),
            target: SearchTarget::Action(action),
        });
    }
    entries
}

#[test]
fn double_shift_state_machine_triggers_across_contexts_and_platforms() {
    for platform in [Platform::MacOs, Platform::Linux] {
        for (leaf, vim_enabled) in [
            (FocusContext::EditorText, false),
            (FocusContext::EditorVimNormal, true),
            (FocusContext::EditorVimInsert, true),
            (FocusContext::Terminal, false),
            (FocusContext::ProjectTree, false),
            (FocusContext::SearchEverywhere, false),
        ] {
            let mut layer = KeyDispatchLayer::new(platform);
            let ctx = DispatchContext {
                leaf,
                vim_enabled,
                editor_read_only: false,
            };
            let t0 = Instant::now();

            assert_eq!(
                tap_shift(&mut layer, ctx, t0, t0 + Duration::from_millis(40)),
                DispatchOutcome::Unhandled
            );
            assert_eq!(
                tap_shift(
                    &mut layer,
                    ctx,
                    t0 + Duration::from_millis(120),
                    t0 + Duration::from_millis(180),
                ),
                DispatchOutcome::Action(ShellAction::OpenSearchEverywhere),
                "expected Shift+Shift on {platform:?} in {leaf:?}"
            );
        }
    }
}

#[test]
fn double_shift_never_triggers_for_shift_plus_letter_or_shortcut() {
    let t0 = Instant::now();
    let editor_ctx = DispatchContext {
        leaf: FocusContext::EditorText,
        vim_enabled: false,
        editor_read_only: false,
    };

    // Shift↓ -> KeyDown('A') -> Shift↑ -> Shift↓ -> Shift↑ must NOT trigger (only 1 clean tap).
    let mut layer = KeyDispatchLayer::new(Platform::MacOs);
    assert_eq!(
        layer.dispatch_modifiers_changed(gpui::Modifiers::shift(), false, editor_ctx, t0),
        DispatchOutcome::Unhandled
    );
    layer.cancel_modifier_tap();
    let shift_a = shortcut_stroke(&ks_char("shift-a", "A")).expect("shift+a stroke");
    assert_eq!(
        layer.dispatch(shift_a, editor_ctx, t0 + Duration::from_millis(20)),
        DispatchOutcome::Unhandled
    );
    assert_eq!(
        layer.dispatch_modifiers_changed(
            gpui::Modifiers::none(),
            false,
            editor_ctx,
            t0 + Duration::from_millis(40),
        ),
        DispatchOutcome::Unhandled
    );
    assert_eq!(
        tap_shift(
            &mut layer,
            editor_ctx,
            t0 + Duration::from_millis(90),
            t0 + Duration::from_millis(130),
        ),
        DispatchOutcome::Unhandled
    );

    // Cmd+Shift+O shortcut followed by a single Shift tap must NOT trigger.
    let mut layer = KeyDispatchLayer::new(Platform::MacOs);
    layer.dispatch_modifiers_changed(gpui::Modifiers::command_shift(), false, editor_ctx, t0);
    layer.cancel_modifier_tap();
    layer.dispatch_modifiers_changed(
        gpui::Modifiers::none(),
        false,
        editor_ctx,
        t0 + Duration::from_millis(40),
    );
    assert_eq!(
        tap_shift(
            &mut layer,
            editor_ctx,
            t0 + Duration::from_millis(80),
            t0 + Duration::from_millis(120),
        ),
        DispatchOutcome::Unhandled
    );
}

#[test]
fn search_everywhere_modal_filters_files_with_line_suffix_and_opens() {
    let mut modal = SearchEverywhereState::new();
    modal.open(sample_entries());
    assert!(modal.is_open());
    assert_ne!(modal.hits().len(), 0);

    for (source, ch) in [
        ("o", "O"),
        ("r", "r"),
        ("d", "d"),
        ("e", "e"),
        ("r", "r"),
        ("a", "A"),
        ("p", "p"),
        ("p", "p"),
        (":", ":"),
        ("4", "4"),
        ("2", "2"),
    ] {
        assert_eq!(
            modal.handle_key(&ks_char(source, ch)),
            SearchKeyOutcome::Consumed
        );
    }
    assert_eq!(modal.query().text(), "OrderApp:42");
    assert_eq!(modal.hits().len(), 2);

    assert_eq!(
        modal.handle_key(&ks_char("tab", "\t")),
        SearchKeyOutcome::Consumed
    );
    assert_eq!(modal.scope(), SearchScope::Classes);
    assert_eq!(
        modal.handle_key(&ks_char("tab", "\t")),
        SearchKeyOutcome::Consumed
    );
    assert_eq!(modal.scope(), SearchScope::Files);
    assert_eq!(modal.hits().len(), 1);
    assert_eq!(modal.hits()[0].entry.title, "OrderApp.java");
    assert_eq!(
        modal.hits()[0].entry.target,
        SearchTarget::File {
            path: PathBuf::from("src/main/java/dev/kova/demo/OrderApp.java"),
            line: Some(42),
        }
    );
    let highlight_ranges = char_indices_to_byte_ranges(
        &modal.hits()[0].entry.title,
        &modal.hits()[0].matched_char_indices,
    );
    assert_ne!(highlight_ranges.len(), 0);

    assert_eq!(
        modal.handle_key(&ks_char("enter", "\n")),
        SearchKeyOutcome::Activate {
            index: 0,
            split: false,
        }
    );
}

#[test]
fn search_everywhere_modal_supports_option_chars_actions_and_escape() {
    let mut modal = SearchEverywhereState::new();
    modal.open(sample_entries());

    // Switch to Symbols and type Polish `ż` via Option+z.
    for _ in 0..3 {
        assert_eq!(
            modal.handle_key(&ks_char("tab", "\t")),
            SearchKeyOutcome::Consumed
        );
    }
    assert_eq!(modal.scope(), SearchScope::Symbols);
    assert_eq!(
        modal.handle_key(&ks_char("alt-z", "ż")),
        SearchKeyOutcome::Consumed
    );
    assert_eq!(modal.hits().len(), 1);
    assert_eq!(modal.hits()[0].entry.title, "żółćHelper");
    let utf8_ranges = char_indices_to_byte_ranges(
        &modal.hits()[0].entry.title,
        &modal.hits()[0].matched_char_indices,
    );
    assert_eq!(
        utf8_ranges
            .iter()
            .map(|r| &modal.hits()[0].entry.title[r.clone()])
            .collect::<Vec<_>>(),
        vec!["ż"]
    );

    // Clear with Cmd+Backspace, switch to Actions, type "toggle vim", and activate.
    assert_eq!(
        modal.handle_key(&ks("cmd-backspace")),
        SearchKeyOutcome::Consumed
    );
    assert_eq!(modal.query().text(), "");
    assert_eq!(
        modal.handle_key(&ks_char("tab", "\t")),
        SearchKeyOutcome::Consumed
    );
    assert_eq!(modal.scope(), SearchScope::Actions);
    for (src, ch) in [
        ("t", "t"),
        ("o", "o"),
        ("g", "g"),
        ("g", "g"),
        ("l", "l"),
        ("e", "e"),
        ("space", " "),
        ("v", "v"),
        ("i", "i"),
        ("m", "m"),
    ] {
        assert_eq!(
            modal.handle_key(&ks_char(src, ch)),
            SearchKeyOutcome::Consumed
        );
    }
    assert_ne!(modal.hits().len(), 0);
    assert_eq!(
        modal.hits()[0].entry.target,
        SearchTarget::Action(ShellAction::ToggleVimMode)
    );
    assert_eq!(
        modal.handle_key(&ks_char("enter", "\n")),
        SearchKeyOutcome::Activate {
            index: 0,
            split: false,
        }
    );

    assert_eq!(modal.handle_key(&ks("escape")), SearchKeyOutcome::Close);
    modal.close();
    assert!(!modal.is_open());
    assert_eq!(modal.hits().len(), 0);
    assert_eq!(modal.query().text(), "");
}
