//! Headless shortcut matrix audit and integration tests for the built-in
//! `JetBrains` macOS and Linux keymaps (`KOVA-018`).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests abort with context on invalid test fixtures"
)]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::{Duration, Instant};

use kova_config::KovaPaths;
use kova_core::builtin_actions::{
    builtin_action_ids, builtin_action_registry, builtin_action_registry_ref, ids,
    register_builtin_actions,
};
use kova_core::{
    ActionError, ActionRegistry, BindingTarget, ContextState, FocusContext, FocusStack, KeyChord,
    KeyCode, KeyDispatch, KeyStep, KeyStroke, KeymapLayerKind, KeymapResolver, KeymapSession,
    ModifierTap, Modifiers, Platform, build_effective_keymap, jetbrains_keymap_layer,
};
use tempfile::TempDir;

fn stroke(chord: &KeyChord) -> KeyStroke {
    match chord.steps() {
        [KeyStep::Stroke(s)] => *s,
        other => panic!("expected single-stroke chord, got {other:?}"),
    }
}

fn dispatch_chord_str(
    resolver: &KeymapResolver,
    chord_str: &str,
    leaf: FocusContext,
    state: &ContextState,
) -> KeyDispatch {
    let chord = KeyChord::parse(chord_str, resolver.platform()).expect("valid chord");
    resolver.resolve_steps(chord.steps(), &FocusStack::from_leaf(leaf), state)
}

fn assert_matches_action(
    resolver: &KeymapResolver,
    chord_str: &str,
    leaf: FocusContext,
    state: &ContextState,
    expected_action: &str,
) {
    let chord = KeyChord::parse(chord_str, resolver.platform()).expect("valid chord");
    let dispatch = dispatch_chord_str(resolver, chord_str, leaf, state);
    assert_eq!(
        dispatch,
        KeyDispatch::Matched {
            action: kova_core::ActionId::new(expected_action).expect("valid action id"),
            chord,
        },
        "platform={:?} leaf={leaf:?} chord=`{chord_str}`",
        resolver.platform()
    );
}

fn trigger_double_shift(
    session: &mut KeymapSession,
    resolver: &KeymapResolver,
    leaf: FocusContext,
    state: &ContextState,
) -> KeyDispatch {
    let stack = FocusStack::from_leaf(leaf);
    let t0 = Instant::now();
    assert_eq!(
        session.on_modifier_edge(resolver, ModifierTap::Shift, true, &stack, state, t0),
        KeyDispatch::NotMatched
    );
    assert_eq!(
        session.on_modifier_edge(
            resolver,
            ModifierTap::Shift,
            false,
            &stack,
            state,
            t0 + Duration::from_millis(40)
        ),
        KeyDispatch::NotMatched
    );
    assert_eq!(
        session.on_modifier_edge(
            resolver,
            ModifierTap::Shift,
            true,
            &stack,
            state,
            t0 + Duration::from_millis(100)
        ),
        KeyDispatch::NotMatched
    );
    session.on_modifier_edge(
        resolver,
        ModifierTap::Shift,
        false,
        &stack,
        state,
        t0 + Duration::from_millis(150),
    )
}

const EXPECTED_REQUIRED_IDS: &[&str] = &[
    ids::NAVIGATION_SEARCH_EVERYWHERE,
    ids::NAVIGATION_FIND_ACTION,
    ids::NAVIGATION_GO_TO_CLASS,
    ids::NAVIGATION_GO_TO_FILE,
    ids::NAVIGATION_GO_TO_SYMBOL,
    ids::NAVIGATION_GO_TO_LINE,
    ids::NAVIGATION_GO_TO_DECLARATION,
    ids::NAVIGATION_GO_TO_IMPLEMENTATION,
    ids::NAVIGATION_GO_TO_SUPER,
    ids::NAVIGATION_FIND_USAGES,
    ids::NAVIGATION_BACK,
    ids::NAVIGATION_FORWARD,
    ids::NAVIGATION_RECENT_FILES,
    ids::NAVIGATION_FILE_STRUCTURE,
    ids::NAVIGATION_NEXT_ERROR,
    ids::NAVIGATION_PREV_ERROR,
    ids::NAVIGATION_NEXT_TAB,
    ids::NAVIGATION_PREV_TAB,
    ids::EDITOR_SAVE,
    ids::EDITOR_UNDO,
    ids::EDITOR_REDO,
    ids::EDITOR_CUT,
    ids::EDITOR_COPY,
    ids::EDITOR_PASTE,
    ids::EDITOR_SELECT_ALL,
    ids::EDITOR_ESCAPE,
    ids::EDITOR_DUPLICATE_LINE,
    ids::EDITOR_DELETE_LINE,
    ids::EDITOR_TOGGLE_LINE_COMMENT,
    ids::EDITOR_TOGGLE_BLOCK_COMMENT,
    ids::EDITOR_EXTEND_SELECTION,
    ids::EDITOR_SHRINK_SELECTION,
    ids::EDITOR_MOVE_LINE_UP,
    ids::EDITOR_MOVE_LINE_DOWN,
    ids::EDITOR_MOVE_STATEMENT_UP,
    ids::EDITOR_MOVE_STATEMENT_DOWN,
    ids::EDITOR_REFORMAT_CODE,
    ids::EDITOR_OPTIMIZE_IMPORTS,
    ids::EDITOR_SHOW_INTENTIONS,
    ids::EDITOR_COMPLETE_BASIC,
    ids::EDITOR_COMPLETE_SMART,
    ids::EDITOR_PARAMETER_INFO,
    ids::EDITOR_QUICK_DOC,
    ids::EDITOR_ADD_NEXT_OCCURRENCE,
    ids::EDITOR_UNSELECT_OCCURRENCE,
    ids::EDITOR_SELECT_ALL_OCCURRENCES,
    ids::EDITOR_ADD_CURSOR_ABOVE,
    ids::EDITOR_ADD_CURSOR_BELOW,
    ids::EDITOR_FIND,
    ids::EDITOR_REPLACE,
    ids::EDITOR_FIND_IN_FILES,
    ids::EDITOR_REPLACE_IN_FILES,
    ids::EDITOR_FIND_NEXT,
    ids::EDITOR_FIND_PREV,
    ids::EDITOR_FOLD_REGION,
    ids::EDITOR_UNFOLD_REGION,
    ids::EDITOR_FOLD_ALL,
    ids::EDITOR_UNFOLD_ALL,
    ids::REFACTOR_RENAME,
    ids::REFACTOR_EXTRACT_VARIABLE,
    ids::REFACTOR_EXTRACT_METHOD,
    ids::REFACTOR_EXTRACT_CONSTANT,
    ids::REFACTOR_EXTRACT_FIELD,
    ids::REFACTOR_INLINE,
    ids::REFACTOR_CHANGE_SIGNATURE,
    ids::RUN_RUN_CONTEXT,
    ids::RUN_RUN_LAST,
    ids::RUN_DEBUG_CONTEXT,
    ids::RUN_DEBUG_LAST,
    ids::RUN_STOP,
    ids::DEBUG_TOGGLE_LINE_BREAKPOINT,
    ids::DEBUG_STEP_OVER,
    ids::DEBUG_STEP_INTO,
    ids::DEBUG_STEP_OUT,
    ids::DEBUG_RESUME,
    ids::DEBUG_EVALUATE_EXPRESSION,
    ids::TOOL_WINDOW_PROJECT,
    ids::TOOL_WINDOW_STRUCTURE,
    ids::TOOL_WINDOW_RUN,
    ids::TOOL_WINDOW_DEBUG,
    ids::TOOL_WINDOW_PROBLEMS,
    ids::TOOL_WINDOW_GIT,
    ids::TOOL_WINDOW_TERMINAL,
    ids::TOOL_WINDOW_HIDE_ALL,
    ids::VIEW_TOGGLE_FOCUS_MODE,
    ids::VIEW_OPEN_SETTINGS,
    ids::VIEW_CLOSE_TAB,
    ids::GIT_COMMIT,
    ids::GIT_PUSH,
    ids::GIT_UPDATE_PROJECT,
    ids::GIT_ROLLBACK,
    ids::VIM_TOGGLE,
];

#[test]
fn builtin_action_catalog_covers_all_required_domains() {
    let registry = builtin_action_registry_ref();
    assert_eq!(registry.len(), EXPECTED_REQUIRED_IDS.len());
    assert_eq!(builtin_action_ids().len(), EXPECTED_REQUIRED_IDS.len());
    for action_id in EXPECTED_REQUIRED_IDS {
        assert!(
            registry.contains(action_id),
            "missing required built-in action `{action_id}`"
        );
    }

    let mut fresh = ActionRegistry::new();
    register_builtin_actions(&mut fresh).expect("fresh registration succeeds");
    assert!(matches!(
        register_builtin_actions(&mut fresh),
        Err(ActionError::DuplicateAction(_))
    ));
}

#[test]
fn both_platforms_pass_validation_with_zero_unknown_actions_and_zero_conflicts() {
    let registry = builtin_action_registry();

    for platform in [Platform::MacOs, Platform::Linux] {
        let resolver = build_effective_keymap(platform, None).expect("default preset loads");
        let diagnostics = resolver.validate_bindings(&registry);
        assert!(
            diagnostics.is_empty(),
            "expected zero diagnostics for {platform}, got: {diagnostics:#?}"
        );
    }
}

#[test]
fn exhaustive_focus_context_matrix_is_unambiguous_on_both_platforms() {
    for platform in [Platform::MacOs, Platform::Linux] {
        let layer = jetbrains_keymap_layer(platform);
        let resolver = build_effective_keymap(platform, None).expect("preset loads");
        let state = ContextState::new();

        // Collect all unique chords in the preset and verify each chord is bound
        // to at most one action per (FocusContext, when) in the default preset.
        let mut per_chord_rules: BTreeMap<KeyChord, Vec<_>> = BTreeMap::new();
        for rule in &layer.rules {
            per_chord_rules
                .entry(rule.chord.clone())
                .or_default()
                .push(rule);
        }

        // Every rule in the preset must resolve cleanly to its exact target in its own root context.
        for rule in &layer.rules {
            let leaf = rule.root_context.unwrap_or(FocusContext::Global);
            let stack = FocusStack::from_leaf(leaf);
            let dispatch = resolver.resolve_steps(rule.chord.steps(), &stack, &state);
            match &rule.target {
                BindingTarget::Action(expected_id) => {
                    assert_eq!(
                        dispatch,
                        KeyDispatch::Matched {
                            action: expected_id.clone(),
                            chord: rule.chord.clone(),
                        },
                        "rule #{} in {platform} failed self-resolution in {leaf:?}",
                        rule.id
                    );
                }
                BindingTarget::Unbind => {
                    assert_eq!(dispatch, KeyDispatch::NotMatched);
                }
            }
        }

        // Across all 14 FocusContext variants, check that no two distinct rules match with equal
        // context depth and when specificity, and verify modal barriers never leak Workspace/Editor rules.
        let global_chords: BTreeSet<KeyChord> = layer
            .rules
            .iter()
            .filter(|r| r.root_context == Some(FocusContext::Global))
            .map(|r| r.chord.clone())
            .collect();

        for ctx in FocusContext::ALL {
            let stack = FocusStack::from_leaf(ctx);
            for (chord, rules) in &per_chord_rules {
                let active_matches: Vec<_> = rules
                    .iter()
                    .filter(|r| {
                        r.root_context.is_none_or(|rc| stack.contains(rc))
                            && r.when.as_ref().is_none_or(|w| w.matches(&stack, &state))
                    })
                    .collect();

                if active_matches.len() > 1 {
                    let unique_actions: HashSet<_> =
                        active_matches.iter().map(|r| &r.target).collect();
                    assert_eq!(
                        unique_actions.len(),
                        1,
                        "ambiguous match in {platform} {ctx:?} for `{chord}`: {active_matches:#?}"
                    );
                }

                let dispatch = resolver.resolve_steps(chord.steps(), &stack, &state);
                if matches!(
                    ctx,
                    FocusContext::SearchEverywhere | FocusContext::SettingsModal
                ) {
                    if global_chords.contains(chord) {
                        assert!(matches!(dispatch, KeyDispatch::Matched { .. }));
                    } else {
                        assert_eq!(
                            dispatch,
                            KeyDispatch::NotMatched,
                            "modal barrier `{ctx:?}` leaked non-global chord `{chord}` on {platform}"
                        );
                    }
                }
            }
        }
    }
}

const MAC_FR2_CASES: &[(&str, FocusContext, &str)] = &[
    (
        "cmd+shift+a",
        FocusContext::Global,
        ids::NAVIGATION_FIND_ACTION,
    ),
    (
        "cmd+o",
        FocusContext::Workspace,
        ids::NAVIGATION_GO_TO_CLASS,
    ),
    (
        "cmd+shift+o",
        FocusContext::Workspace,
        ids::NAVIGATION_GO_TO_FILE,
    ),
    (
        "cmd+alt+o",
        FocusContext::Workspace,
        ids::NAVIGATION_GO_TO_SYMBOL,
    ),
    ("cmd+l", FocusContext::Editor, ids::NAVIGATION_GO_TO_LINE),
    (
        "cmd+b",
        FocusContext::EditorText,
        ids::NAVIGATION_GO_TO_DECLARATION,
    ),
    (
        "cmd+alt+b",
        FocusContext::EditorText,
        ids::NAVIGATION_GO_TO_IMPLEMENTATION,
    ),
    (
        "cmd+u",
        FocusContext::EditorText,
        ids::NAVIGATION_GO_TO_SUPER,
    ),
    (
        "alt+f7",
        FocusContext::Workspace,
        ids::NAVIGATION_FIND_USAGES,
    ),
    ("cmd+[", FocusContext::Workspace, ids::NAVIGATION_BACK),
    ("cmd+]", FocusContext::Workspace, ids::NAVIGATION_FORWARD),
    (
        "cmd+e",
        FocusContext::Workspace,
        ids::NAVIGATION_RECENT_FILES,
    ),
    (
        "cmd+f12",
        FocusContext::Editor,
        ids::NAVIGATION_FILE_STRUCTURE,
    ),
    ("f2", FocusContext::Workspace, ids::NAVIGATION_NEXT_ERROR),
    (
        "shift+f2",
        FocusContext::Workspace,
        ids::NAVIGATION_PREV_ERROR,
    ),
    (
        "cmd+shift+]",
        FocusContext::Workspace,
        ids::NAVIGATION_NEXT_TAB,
    ),
    (
        "cmd+shift+[",
        FocusContext::Workspace,
        ids::NAVIGATION_PREV_TAB,
    ),
    ("cmd+s", FocusContext::Workspace, ids::EDITOR_SAVE),
    ("cmd+z", FocusContext::EditorText, ids::EDITOR_UNDO),
    ("cmd+shift+z", FocusContext::EditorText, ids::EDITOR_REDO),
    ("cmd+x", FocusContext::EditorText, ids::EDITOR_CUT),
    ("cmd+c", FocusContext::EditorText, ids::EDITOR_COPY),
    ("cmd+v", FocusContext::EditorText, ids::EDITOR_PASTE),
    ("cmd+a", FocusContext::EditorText, ids::EDITOR_SELECT_ALL),
    ("escape", FocusContext::Editor, ids::EDITOR_ESCAPE),
    (
        "cmd+d",
        FocusContext::EditorText,
        ids::EDITOR_DUPLICATE_LINE,
    ),
    (
        "cmd+backspace",
        FocusContext::EditorText,
        ids::EDITOR_DELETE_LINE,
    ),
    (
        "cmd+/",
        FocusContext::EditorText,
        ids::EDITOR_TOGGLE_LINE_COMMENT,
    ),
    (
        "cmd+alt+/",
        FocusContext::EditorText,
        ids::EDITOR_TOGGLE_BLOCK_COMMENT,
    ),
    (
        "alt+up",
        FocusContext::EditorText,
        ids::EDITOR_EXTEND_SELECTION,
    ),
    (
        "alt+down",
        FocusContext::EditorText,
        ids::EDITOR_SHRINK_SELECTION,
    ),
    (
        "alt+shift+up",
        FocusContext::EditorText,
        ids::EDITOR_MOVE_LINE_UP,
    ),
    (
        "alt+shift+down",
        FocusContext::EditorText,
        ids::EDITOR_MOVE_LINE_DOWN,
    ),
    (
        "cmd+shift+up",
        FocusContext::EditorText,
        ids::EDITOR_MOVE_STATEMENT_UP,
    ),
    (
        "cmd+shift+down",
        FocusContext::EditorText,
        ids::EDITOR_MOVE_STATEMENT_DOWN,
    ),
    (
        "cmd+alt+l",
        FocusContext::EditorText,
        ids::EDITOR_REFORMAT_CODE,
    ),
    (
        "ctrl+alt+o",
        FocusContext::EditorText,
        ids::EDITOR_OPTIMIZE_IMPORTS,
    ),
    (
        "alt+enter",
        FocusContext::EditorText,
        ids::EDITOR_SHOW_INTENTIONS,
    ),
    (
        "ctrl+space",
        FocusContext::EditorText,
        ids::EDITOR_COMPLETE_BASIC,
    ),
    (
        "ctrl+shift+space",
        FocusContext::EditorText,
        ids::EDITOR_COMPLETE_SMART,
    ),
    (
        "cmd+p",
        FocusContext::EditorText,
        ids::EDITOR_PARAMETER_INFO,
    ),
    ("f1", FocusContext::EditorText, ids::EDITOR_QUICK_DOC),
    (
        "ctrl+g",
        FocusContext::EditorText,
        ids::EDITOR_ADD_NEXT_OCCURRENCE,
    ),
    (
        "ctrl+shift+g",
        FocusContext::EditorText,
        ids::EDITOR_UNSELECT_OCCURRENCE,
    ),
    (
        "ctrl+cmd+g",
        FocusContext::EditorText,
        ids::EDITOR_SELECT_ALL_OCCURRENCES,
    ),
    ("cmd+f", FocusContext::Editor, ids::EDITOR_FIND),
    ("cmd+r", FocusContext::Editor, ids::EDITOR_REPLACE),
    (
        "cmd+shift+f",
        FocusContext::Workspace,
        ids::EDITOR_FIND_IN_FILES,
    ),
    (
        "cmd+shift+r",
        FocusContext::Workspace,
        ids::EDITOR_REPLACE_IN_FILES,
    ),
    ("cmd+g", FocusContext::Editor, ids::EDITOR_FIND_NEXT),
    ("cmd+shift+g", FocusContext::Editor, ids::EDITOR_FIND_PREV),
    ("cmd+-", FocusContext::EditorText, ids::EDITOR_FOLD_REGION),
    (
        "cmd+plus",
        FocusContext::EditorText,
        ids::EDITOR_UNFOLD_REGION,
    ),
    ("cmd+=", FocusContext::EditorText, ids::EDITOR_UNFOLD_REGION),
    (
        "cmd+shift+-",
        FocusContext::EditorText,
        ids::EDITOR_FOLD_ALL,
    ),
    (
        "cmd+shift+plus",
        FocusContext::EditorText,
        ids::EDITOR_UNFOLD_ALL,
    ),
    ("shift+f6", FocusContext::ProjectTree, ids::REFACTOR_RENAME),
    (
        "cmd+alt+v",
        FocusContext::EditorText,
        ids::REFACTOR_EXTRACT_VARIABLE,
    ),
    (
        "cmd+alt+m",
        FocusContext::EditorText,
        ids::REFACTOR_EXTRACT_METHOD,
    ),
    (
        "cmd+alt+c",
        FocusContext::EditorText,
        ids::REFACTOR_EXTRACT_CONSTANT,
    ),
    (
        "cmd+alt+f",
        FocusContext::EditorText,
        ids::REFACTOR_EXTRACT_FIELD,
    ),
    ("cmd+alt+n", FocusContext::EditorText, ids::REFACTOR_INLINE),
    (
        "cmd+f6",
        FocusContext::EditorText,
        ids::REFACTOR_CHANGE_SIGNATURE,
    ),
    (
        "ctrl+shift+r",
        FocusContext::Workspace,
        ids::RUN_RUN_CONTEXT,
    ),
    ("ctrl+r", FocusContext::Workspace, ids::RUN_RUN_LAST),
    (
        "ctrl+shift+d",
        FocusContext::Workspace,
        ids::RUN_DEBUG_CONTEXT,
    ),
    ("ctrl+d", FocusContext::Workspace, ids::RUN_DEBUG_LAST),
    ("cmd+f2", FocusContext::Workspace, ids::RUN_STOP),
    (
        "cmd+f8",
        FocusContext::Editor,
        ids::DEBUG_TOGGLE_LINE_BREAKPOINT,
    ),
    ("f8", FocusContext::Workspace, ids::DEBUG_STEP_OVER),
    ("f7", FocusContext::Workspace, ids::DEBUG_STEP_INTO),
    ("shift+f8", FocusContext::Workspace, ids::DEBUG_STEP_OUT),
    ("cmd+alt+r", FocusContext::Workspace, ids::DEBUG_RESUME),
    (
        "alt+f8",
        FocusContext::Workspace,
        ids::DEBUG_EVALUATE_EXPRESSION,
    ),
    ("cmd+1", FocusContext::Workspace, ids::TOOL_WINDOW_PROJECT),
    ("cmd+4", FocusContext::Workspace, ids::TOOL_WINDOW_RUN),
    ("cmd+5", FocusContext::Workspace, ids::TOOL_WINDOW_DEBUG),
    ("cmd+6", FocusContext::Workspace, ids::TOOL_WINDOW_PROBLEMS),
    ("cmd+7", FocusContext::Workspace, ids::TOOL_WINDOW_STRUCTURE),
    ("cmd+9", FocusContext::Workspace, ids::TOOL_WINDOW_GIT),
    (
        "alt+f12",
        FocusContext::Workspace,
        ids::TOOL_WINDOW_TERMINAL,
    ),
    (
        "cmd+shift+f12",
        FocusContext::Workspace,
        ids::TOOL_WINDOW_HIDE_ALL,
    ),
    (
        "ctrl+alt+z",
        FocusContext::Workspace,
        ids::VIEW_TOGGLE_FOCUS_MODE,
    ),
    ("cmd+,", FocusContext::Global, ids::VIEW_OPEN_SETTINGS),
    ("cmd+w", FocusContext::Workspace, ids::VIEW_CLOSE_TAB),
    ("cmd+k", FocusContext::Workspace, ids::GIT_COMMIT),
    ("cmd+shift+k", FocusContext::Workspace, ids::GIT_PUSH),
    ("cmd+t", FocusContext::Workspace, ids::GIT_UPDATE_PROJECT),
    ("ctrl+alt+shift+v", FocusContext::Workspace, ids::VIM_TOGGLE),
];

const LINUX_FR2_CASES: &[(&str, FocusContext, &str)] = &[
    (
        "ctrl+shift+a",
        FocusContext::Global,
        ids::NAVIGATION_FIND_ACTION,
    ),
    (
        "ctrl+n",
        FocusContext::Workspace,
        ids::NAVIGATION_GO_TO_CLASS,
    ),
    (
        "ctrl+shift+n",
        FocusContext::Workspace,
        ids::NAVIGATION_GO_TO_FILE,
    ),
    (
        "ctrl+alt+shift+n",
        FocusContext::Workspace,
        ids::NAVIGATION_GO_TO_SYMBOL,
    ),
    ("ctrl+g", FocusContext::Editor, ids::NAVIGATION_GO_TO_LINE),
    (
        "ctrl+b",
        FocusContext::EditorText,
        ids::NAVIGATION_GO_TO_DECLARATION,
    ),
    (
        "ctrl+alt+b",
        FocusContext::EditorText,
        ids::NAVIGATION_GO_TO_IMPLEMENTATION,
    ),
    (
        "ctrl+u",
        FocusContext::EditorText,
        ids::NAVIGATION_GO_TO_SUPER,
    ),
    (
        "alt+f7",
        FocusContext::Workspace,
        ids::NAVIGATION_FIND_USAGES,
    ),
    (
        "ctrl+alt+left",
        FocusContext::Workspace,
        ids::NAVIGATION_BACK,
    ),
    (
        "ctrl+alt+right",
        FocusContext::Workspace,
        ids::NAVIGATION_FORWARD,
    ),
    (
        "ctrl+e",
        FocusContext::Workspace,
        ids::NAVIGATION_RECENT_FILES,
    ),
    (
        "ctrl+f12",
        FocusContext::Editor,
        ids::NAVIGATION_FILE_STRUCTURE,
    ),
    ("f2", FocusContext::Workspace, ids::NAVIGATION_NEXT_ERROR),
    (
        "shift+f2",
        FocusContext::Workspace,
        ids::NAVIGATION_PREV_ERROR,
    ),
    (
        "alt+right",
        FocusContext::Workspace,
        ids::NAVIGATION_NEXT_TAB,
    ),
    (
        "alt+left",
        FocusContext::Workspace,
        ids::NAVIGATION_PREV_TAB,
    ),
    ("ctrl+s", FocusContext::Workspace, ids::EDITOR_SAVE),
    ("ctrl+z", FocusContext::EditorText, ids::EDITOR_UNDO),
    ("ctrl+shift+z", FocusContext::EditorText, ids::EDITOR_REDO),
    ("ctrl+x", FocusContext::EditorText, ids::EDITOR_CUT),
    ("ctrl+c", FocusContext::EditorText, ids::EDITOR_COPY),
    ("ctrl+v", FocusContext::EditorText, ids::EDITOR_PASTE),
    ("ctrl+a", FocusContext::EditorText, ids::EDITOR_SELECT_ALL),
    ("escape", FocusContext::Editor, ids::EDITOR_ESCAPE),
    (
        "ctrl+d",
        FocusContext::EditorText,
        ids::EDITOR_DUPLICATE_LINE,
    ),
    ("ctrl+y", FocusContext::EditorText, ids::EDITOR_DELETE_LINE),
    (
        "ctrl+/",
        FocusContext::EditorText,
        ids::EDITOR_TOGGLE_LINE_COMMENT,
    ),
    (
        "ctrl+shift+/",
        FocusContext::EditorText,
        ids::EDITOR_TOGGLE_BLOCK_COMMENT,
    ),
    (
        "ctrl+w",
        FocusContext::EditorText,
        ids::EDITOR_EXTEND_SELECTION,
    ),
    (
        "ctrl+shift+w",
        FocusContext::EditorText,
        ids::EDITOR_SHRINK_SELECTION,
    ),
    (
        "alt+shift+up",
        FocusContext::EditorText,
        ids::EDITOR_MOVE_LINE_UP,
    ),
    (
        "alt+shift+down",
        FocusContext::EditorText,
        ids::EDITOR_MOVE_LINE_DOWN,
    ),
    (
        "ctrl+shift+up",
        FocusContext::EditorText,
        ids::EDITOR_MOVE_STATEMENT_UP,
    ),
    (
        "ctrl+shift+down",
        FocusContext::EditorText,
        ids::EDITOR_MOVE_STATEMENT_DOWN,
    ),
    (
        "ctrl+alt+l",
        FocusContext::EditorText,
        ids::EDITOR_REFORMAT_CODE,
    ),
    (
        "ctrl+alt+o",
        FocusContext::EditorText,
        ids::EDITOR_OPTIMIZE_IMPORTS,
    ),
    (
        "alt+enter",
        FocusContext::EditorText,
        ids::EDITOR_SHOW_INTENTIONS,
    ),
    (
        "ctrl+space",
        FocusContext::EditorText,
        ids::EDITOR_COMPLETE_BASIC,
    ),
    (
        "ctrl+shift+space",
        FocusContext::EditorText,
        ids::EDITOR_COMPLETE_SMART,
    ),
    (
        "ctrl+p",
        FocusContext::EditorText,
        ids::EDITOR_PARAMETER_INFO,
    ),
    ("ctrl+q", FocusContext::EditorText, ids::EDITOR_QUICK_DOC),
    (
        "alt+j",
        FocusContext::EditorText,
        ids::EDITOR_ADD_NEXT_OCCURRENCE,
    ),
    (
        "alt+shift+j",
        FocusContext::EditorText,
        ids::EDITOR_UNSELECT_OCCURRENCE,
    ),
    (
        "ctrl+alt+shift+j",
        FocusContext::EditorText,
        ids::EDITOR_SELECT_ALL_OCCURRENCES,
    ),
    ("ctrl+f", FocusContext::Editor, ids::EDITOR_FIND),
    ("ctrl+r", FocusContext::Editor, ids::EDITOR_REPLACE),
    (
        "ctrl+shift+f",
        FocusContext::Workspace,
        ids::EDITOR_FIND_IN_FILES,
    ),
    (
        "ctrl+shift+r",
        FocusContext::Workspace,
        ids::EDITOR_REPLACE_IN_FILES,
    ),
    ("f3", FocusContext::Editor, ids::EDITOR_FIND_NEXT),
    ("shift+f3", FocusContext::Editor, ids::EDITOR_FIND_PREV),
    ("ctrl+-", FocusContext::EditorText, ids::EDITOR_FOLD_REGION),
    (
        "ctrl+plus",
        FocusContext::EditorText,
        ids::EDITOR_UNFOLD_REGION,
    ),
    (
        "ctrl+=",
        FocusContext::EditorText,
        ids::EDITOR_UNFOLD_REGION,
    ),
    (
        "ctrl+shift+-",
        FocusContext::EditorText,
        ids::EDITOR_FOLD_ALL,
    ),
    (
        "ctrl+shift+plus",
        FocusContext::EditorText,
        ids::EDITOR_UNFOLD_ALL,
    ),
    ("shift+f6", FocusContext::ProjectTree, ids::REFACTOR_RENAME),
    (
        "ctrl+alt+v",
        FocusContext::EditorText,
        ids::REFACTOR_EXTRACT_VARIABLE,
    ),
    (
        "ctrl+alt+m",
        FocusContext::EditorText,
        ids::REFACTOR_EXTRACT_METHOD,
    ),
    (
        "ctrl+alt+c",
        FocusContext::EditorText,
        ids::REFACTOR_EXTRACT_CONSTANT,
    ),
    (
        "ctrl+alt+f",
        FocusContext::EditorText,
        ids::REFACTOR_EXTRACT_FIELD,
    ),
    ("ctrl+alt+n", FocusContext::EditorText, ids::REFACTOR_INLINE),
    (
        "ctrl+f6",
        FocusContext::EditorText,
        ids::REFACTOR_CHANGE_SIGNATURE,
    ),
    (
        "ctrl+shift+f10",
        FocusContext::Workspace,
        ids::RUN_RUN_CONTEXT,
    ),
    ("shift+f10", FocusContext::Workspace, ids::RUN_RUN_LAST),
    (
        "ctrl+shift+f9",
        FocusContext::Workspace,
        ids::RUN_DEBUG_CONTEXT,
    ),
    ("shift+f9", FocusContext::Workspace, ids::RUN_DEBUG_LAST),
    ("ctrl+f2", FocusContext::Workspace, ids::RUN_STOP),
    (
        "ctrl+f8",
        FocusContext::Editor,
        ids::DEBUG_TOGGLE_LINE_BREAKPOINT,
    ),
    ("f8", FocusContext::Workspace, ids::DEBUG_STEP_OVER),
    ("f7", FocusContext::Workspace, ids::DEBUG_STEP_INTO),
    ("shift+f8", FocusContext::Workspace, ids::DEBUG_STEP_OUT),
    ("f9", FocusContext::Workspace, ids::DEBUG_RESUME),
    (
        "alt+f8",
        FocusContext::Workspace,
        ids::DEBUG_EVALUATE_EXPRESSION,
    ),
    ("alt+1", FocusContext::Workspace, ids::TOOL_WINDOW_PROJECT),
    ("alt+4", FocusContext::Workspace, ids::TOOL_WINDOW_RUN),
    ("alt+5", FocusContext::Workspace, ids::TOOL_WINDOW_DEBUG),
    ("alt+6", FocusContext::Workspace, ids::TOOL_WINDOW_PROBLEMS),
    ("alt+7", FocusContext::Workspace, ids::TOOL_WINDOW_STRUCTURE),
    ("alt+9", FocusContext::Workspace, ids::TOOL_WINDOW_GIT),
    (
        "alt+f12",
        FocusContext::Workspace,
        ids::TOOL_WINDOW_TERMINAL,
    ),
    (
        "ctrl+shift+f12",
        FocusContext::Workspace,
        ids::TOOL_WINDOW_HIDE_ALL,
    ),
    (
        "ctrl+alt+z",
        FocusContext::Workspace,
        ids::VIEW_TOGGLE_FOCUS_MODE,
    ),
    ("ctrl+alt+s", FocusContext::Global, ids::VIEW_OPEN_SETTINGS),
    ("ctrl+,", FocusContext::Global, ids::VIEW_OPEN_SETTINGS),
    ("ctrl+f4", FocusContext::Workspace, ids::VIEW_CLOSE_TAB),
    ("ctrl+k", FocusContext::Workspace, ids::GIT_COMMIT),
    ("ctrl+shift+k", FocusContext::Workspace, ids::GIT_PUSH),
    ("ctrl+t", FocusContext::Workspace, ids::GIT_UPDATE_PROJECT),
    ("ctrl+alt+shift+v", FocusContext::Workspace, ids::VIM_TOGGLE),
];

#[test]
fn jetbrains_fr2_shortcut_matrix_matches_macos_and_linux_specs() {
    let mac = build_effective_keymap(Platform::MacOs, None).expect("macos preset");
    let linux = build_effective_keymap(Platform::Linux, None).expect("linux preset");
    let state = ContextState::new();

    for &(chord, leaf, expected) in MAC_FR2_CASES {
        assert_matches_action(&mac, chord, leaf, &state, expected);
    }
    for &(chord, leaf, expected) in LINUX_FR2_CASES {
        assert_matches_action(&linux, chord, leaf, &state, expected);
    }

    // Platform-specific context isolation checks from FR-2 & Q-2:
    // On macOS, Cmd+D in ProjectTree is unbound (only EditorText duplicates lines).
    assert_eq!(
        dispatch_chord_str(&mac, "cmd+d", FocusContext::ProjectTree, &state),
        KeyDispatch::NotMatched
    );
    // On Linux, Ctrl+D in ProjectTree is unbound, and Ctrl+W in ProjectTree does NOT close tab.
    assert_eq!(
        dispatch_chord_str(&linux, "ctrl+d", FocusContext::ProjectTree, &state),
        KeyDispatch::NotMatched
    );
    assert_eq!(
        dispatch_chord_str(&linux, "ctrl+w", FocusContext::ProjectTree, &state),
        KeyDispatch::NotMatched
    );
    // When Vim is enabled, unmodified Escape in Editor is left for the Vim state machine.
    let vim_state = ContextState::new().with_flag("vim_enabled", true);
    assert_eq!(
        dispatch_chord_str(&mac, "escape", FocusContext::EditorText, &vim_state),
        KeyDispatch::NotMatched
    );
    assert_eq!(
        dispatch_chord_str(&linux, "escape", FocusContext::EditorText, &vim_state),
        KeyDispatch::NotMatched
    );
}

#[test]
fn double_shift_triggers_search_everywhere_and_ignores_shift_typing_on_both_platforms() {
    let state = ContextState::new();

    for platform in [Platform::MacOs, Platform::Linux] {
        let resolver = build_effective_keymap(platform, None).expect("preset loads");
        let mut session = KeymapSession::new();

        for leaf in [
            FocusContext::Global,
            FocusContext::ProjectTree,
            FocusContext::EditorText,
        ] {
            assert_eq!(
                trigger_double_shift(&mut session, &resolver, leaf, &state),
                KeyDispatch::Matched {
                    action: kova_core::ActionId::from_static(ids::NAVIGATION_SEARCH_EVERYWHERE),
                    chord: KeyChord::double_tap(ModifierTap::Shift),
                },
                "double-shift failed for {platform} in {leaf:?}"
            );
        }

        // Fast typing Shift+A then Shift+B must not trigger Search Everywhere.
        let stack = FocusStack::from_leaf(FocusContext::EditorText);
        let t0 = Instant::now();
        session.on_modifier_edge(&resolver, ModifierTap::Shift, true, &stack, &state, t0);
        session.on_key_down(
            &resolver,
            KeyStroke::new(Modifiers::SHIFT, KeyCode::Char('a')),
            &stack,
            &state,
            t0 + Duration::from_millis(15),
        );
        session.on_modifier_edge(
            &resolver,
            ModifierTap::Shift,
            false,
            &stack,
            &state,
            t0 + Duration::from_millis(30),
        );
        session.on_modifier_edge(
            &resolver,
            ModifierTap::Shift,
            true,
            &stack,
            &state,
            t0 + Duration::from_millis(70),
        );
        session.on_key_down(
            &resolver,
            KeyStroke::new(Modifiers::SHIFT, KeyCode::Char('b')),
            &stack,
            &state,
            t0 + Duration::from_millis(85),
        );
        assert_eq!(
            session.on_modifier_edge(
                &resolver,
                ModifierTap::Shift,
                false,
                &stack,
                &state,
                t0 + Duration::from_millis(100)
            ),
            KeyDispatch::NotMatched
        );
    }
}

#[test]
fn user_overrides_unbind_and_preset_none_work_with_jetbrains_base() {
    let user_override_toml = r#"
schema_version = 1
preset = "jetbrains"

[[bindings]]
context = "EditorText"
key = "primary+d"
action = "editor.reformat_code"

[[bindings]]
key = "shift shift"
action = "none"
"#;

    for (platform, primary_d) in [(Platform::MacOs, "cmd+d"), (Platform::Linux, "ctrl+d")] {
        let resolver =
            build_effective_keymap(platform, Some(user_override_toml)).expect("override parses");
        let state = ContextState::new();

        // User override replaces JetBrains `editor.duplicate_line`.
        assert_matches_action(
            &resolver,
            primary_d,
            FocusContext::EditorText,
            &state,
            ids::EDITOR_REFORMAT_CODE,
        );

        // Unbound `shift shift` no longer triggers Search Everywhere.
        let mut session = KeymapSession::new();
        assert_eq!(
            trigger_double_shift(&mut session, &resolver, FocusContext::EditorText, &state),
            KeyDispatch::NotMatched
        );

        // Other JetBrains defaults remain intact.
        assert_matches_action(
            &resolver,
            "alt+enter",
            FocusContext::EditorText,
            &state,
            ids::EDITOR_SHOW_INTENTIONS,
        );
    }

    // `preset = "none"` strips all JetBrains defaults and keeps only user bindings.
    let minimal_toml = r#"
schema_version = 1
preset = "none"

[[bindings]]
context = "EditorText"
key = "primary+s"
action = "editor.save"
"#;

    let minimal = build_effective_keymap(Platform::Linux, Some(minimal_toml)).expect("preset none");
    assert!(
        minimal
            .rules()
            .iter()
            .all(|r| r.layer == KeymapLayerKind::UserOverride)
    );
    assert_eq!(
        dispatch_chord_str(
            &minimal,
            "alt+enter",
            FocusContext::EditorText,
            &ContextState::new()
        ),
        KeyDispatch::NotMatched
    );
    assert_matches_action(
        &minimal,
        "ctrl+s",
        FocusContext::EditorText,
        &ContextState::new(),
        ids::EDITOR_SAVE,
    );
}

#[test]
fn from_user_file_loads_defaults_without_creating_files_and_applies_existing_overrides() {
    let tmp = TempDir::new().expect("temp dir");
    let cfg_dir = tmp.path().join("cfg");
    let paths = KovaPaths::from_roots(
        cfg_dir.clone(),
        tmp.path().join("data"),
        tmp.path().join("cache"),
    )
    .expect("valid roots");
    let keymap_path = paths.keymap_file();
    assert!(!keymap_path.exists());

    let default_resolver =
        KeymapResolver::from_user_file(&keymap_path, Platform::MacOs).expect("loads defaults");
    assert!(
        !keymap_path.exists(),
        "from_user_file must never create keymap.toml on disk"
    );
    assert_matches_action(
        &default_resolver,
        "cmd+d",
        FocusContext::EditorText,
        &ContextState::new(),
        ids::EDITOR_DUPLICATE_LINE,
    );

    std::fs::create_dir_all(&cfg_dir).expect("create cfg dir");
    std::fs::write(
        &keymap_path,
        r#"
[[bindings]]
context = "Workspace"
key = "ctrl+w"
action = "view.close_tab"
"#,
    )
    .expect("write keymap.toml");

    let custom_resolver =
        KeymapResolver::from_user_file(&keymap_path, Platform::Linux).expect("loads custom file");
    assert_matches_action(
        &custom_resolver,
        "ctrl+w",
        FocusContext::Workspace,
        &ContextState::new(),
        ids::VIEW_CLOSE_TAB,
    );
    // And in EditorText, EditorText depth (3) < UserOverride layer (2 > 1), so UserOverride wins!
    let chord = KeyChord::parse("ctrl+w", Platform::Linux).expect("chord");
    let stroke = stroke(&chord);
    assert_eq!(
        custom_resolver.resolve_key_event(
            stroke,
            &FocusStack::from_leaf(FocusContext::EditorText),
            &ContextState::new()
        ),
        KeyDispatch::Matched {
            action: kova_core::ActionId::from_static(ids::VIEW_CLOSE_TAB),
            chord,
        }
    );
}

#[test]
fn jetbrains_init_and_dispatch_stay_well_under_latency_budget() {
    let init_start = Instant::now();
    let registry = builtin_action_registry();
    let mac = build_effective_keymap(Platform::MacOs, None).expect("macos");
    let linux = build_effective_keymap(Platform::Linux, None).expect("linux");
    let init_elapsed = init_start.elapsed();
    assert!(
        init_elapsed < Duration::from_millis(50),
        "builtin registry + both presets init took {init_elapsed:?}"
    );
    assert!(!registry.is_empty());

    let stack = FocusStack::from_leaf(FocusContext::EditorText);
    let state = ContextState::new();
    let mac_stroke = KeyStroke::new(Modifiers::META, KeyCode::Char('d'));
    let linux_stroke = KeyStroke::new(Modifiers::CTRL, KeyCode::Char('w'));

    let dispatch_start = Instant::now();
    for _ in 0..1_000 {
        assert!(matches!(
            mac.resolve_key_event(mac_stroke, &stack, &state),
            KeyDispatch::Matched { .. }
        ));
        assert!(matches!(
            linux.resolve_key_event(linux_stroke, &stack, &state),
            KeyDispatch::Matched { .. }
        ));
    }
    let avg_dispatch = dispatch_start.elapsed() / 2_000;
    assert!(
        avg_dispatch < Duration::from_micros(50),
        "average dispatch took {avg_dispatch:?}"
    );
}
