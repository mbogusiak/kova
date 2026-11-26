//! End-to-end headless integration tests for the `JetBrains` / `IntelliJ` IDEA keymap
//! integration (`KOVA-053`).

#![allow(
    clippy::expect_used,
    reason = "integration test harness aborts on invalid keystroke or fixture setup"
)]

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use kova_buffer::Buffer;
use kova_core::builtin_actions::{builtin_action_registry_ref, ids};
use kova_core::navigation::SourceLanguageKind;
use kova_core::{FocusContext, Platform, jetbrains_keymap_layer_ref};
use kova_syntax::{Highlighter, LanguageRegistry, SyntaxEngine, SyntaxEngineConfig};
use kova_theme::SyntaxLanguage;
use kova_ui::app_menu::menu_key_bindings;
use kova_ui::dock::{BottomPanelModel, DockToolId, ProblemEntry, ProblemSeverity};
use kova_ui::editor::actions::default_key_bindings;
use kova_ui::editor::find::{FindBarState, FindKeyOutcome};
use kova_ui::editor::folding::FoldMap;
use kova_ui::editor::state::EditorState;
use kova_ui::editor::syntax::EditorSyntax;
use kova_ui::keymap::{
    DispatchContext, DispatchOutcome, ExCommand, ExCommandLine, ExLineEvent, KeyDispatchLayer,
    ShellAction, parse_ex_command, shortcut_stroke,
};
use kova_ui::search::{
    FindInFilesKeyOutcome, FindInFilesState, SearchEntry, SearchEntryKind, SearchEverywhereState,
    SearchKeyOutcome, SearchScope, SearchTarget, search_text_in_buffers_and_paths,
};

fn ks(source: &str) -> gpui::Keystroke {
    gpui::Keystroke::parse(source).expect("valid keystroke")
}

fn ks_char(source: &str, key_char: &str) -> gpui::Keystroke {
    let mut stroke = ks(source);
    stroke.key_char = Some(key_char.to_owned());
    stroke
}

fn dispatch(
    layer: &mut KeyDispatchLayer,
    shortcut: &str,
    leaf: FocusContext,
    vim_enabled: bool,
) -> DispatchOutcome {
    let stroke = shortcut_stroke(&ks(shortcut)).expect("mappable shortcut");
    layer.dispatch(
        stroke,
        DispatchContext {
            leaf,
            vim_enabled,
            editor_read_only: false,
        },
        Instant::now(),
    )
}

#[test]
fn macos_menu_bindings_do_not_collide_with_go_to_class_or_file() {
    let mac_bindings = menu_key_bindings(Platform::MacOs);
    let mac_keys: Vec<String> = mac_bindings
        .iter()
        .flat_map(|b| {
            b.keystrokes()
                .iter()
                .map(gpui::KeybindingKeystroke::unparse)
        })
        .collect();
    assert!(!mac_keys.iter().any(|k| k == "cmd-o"));
    assert!(!mac_keys.iter().any(|k| k == "cmd-shift-o"));

    let mut layer = KeyDispatchLayer::new(Platform::MacOs);
    assert_eq!(
        dispatch(&mut layer, "cmd-o", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::OpenSearchScope(SearchScope::Classes))
    );
    assert_eq!(
        dispatch(&mut layer, "cmd-shift-o", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::OpenSearchScope(SearchScope::Files))
    );
    assert_eq!(
        dispatch(&mut layer, "cmd-shift-a", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::OpenSearchScope(SearchScope::Actions))
    );

    let editor_keys: Vec<String> = default_key_bindings()
        .iter()
        .flat_map(|b| {
            b.keystrokes()
                .iter()
                .map(gpui::KeybindingKeystroke::unparse)
        })
        .collect();
    assert!(!editor_keys.iter().any(|k| k == "ctrl-shift-r"));
}

#[test]
fn cmd_o_cmd_shift_o_cmd_shift_a_and_cmd_e_drive_search_everywhere_and_recent_files() {
    let mut mac = KeyDispatchLayer::new(Platform::MacOs);
    let entries = vec![
        SearchEntry {
            kind: SearchEntryKind::Class,
            title: "OrderService".to_owned(),
            subtitle: "Class".to_owned(),
            match_text: "OrderService".to_owned(),
            icon: "C",
            shortcut_hint: None,
            target: SearchTarget::File {
                path: PathBuf::from("src/OrderService.java"),
                line: None,
            },
        },
        SearchEntry {
            kind: SearchEntryKind::File,
            title: "OrderService.java".to_owned(),
            subtitle: "src".to_owned(),
            match_text: "src/OrderService.java".to_owned(),
            icon: "F",
            shortcut_hint: None,
            target: SearchTarget::File {
                path: PathBuf::from("src/OrderService.java"),
                line: None,
            },
        },
        SearchEntry {
            kind: SearchEntryKind::Action,
            title: "Delete Line".to_owned(),
            subtitle: ids::EDITOR_DELETE_LINE.to_owned(),
            match_text: "Delete Line editor.delete_line".to_owned(),
            icon: "A",
            shortcut_hint: Some("cmd+backspace".to_owned()),
            target: SearchTarget::Action(ShellAction::DeleteLine),
        },
    ];

    let mut modal = SearchEverywhereState::new();

    // Cmd+O -> Go to Class
    let DispatchOutcome::Action(ShellAction::OpenSearchScope(scope)) =
        dispatch(&mut mac, "cmd-o", FocusContext::EditorText, false)
    else {
        panic!("Cmd+O must resolve to OpenSearchScope");
    };
    assert_eq!(scope, SearchScope::Classes);
    modal.open_with_scope(entries.clone(), scope);
    assert_eq!(modal.scope(), SearchScope::Classes);
    assert_eq!(modal.hits()[0].entry.title, "OrderService");

    // Cmd+Shift+O -> Go to File
    let DispatchOutcome::Action(ShellAction::OpenSearchScope(scope)) =
        dispatch(&mut mac, "cmd-shift-o", FocusContext::EditorText, false)
    else {
        panic!("Cmd+Shift+O must resolve to OpenSearchScope");
    };
    assert_eq!(scope, SearchScope::Files);
    modal.open_with_scope(entries.clone(), scope);
    assert_eq!(modal.scope(), SearchScope::Files);
    assert_eq!(modal.hits()[0].entry.title, "OrderService.java");

    // Cmd+Shift+A -> Find Action
    let DispatchOutcome::Action(ShellAction::OpenSearchScope(scope)) =
        dispatch(&mut mac, "cmd-shift-a", FocusContext::EditorText, false)
    else {
        panic!("Cmd+Shift+A must resolve to OpenSearchScope");
    };
    assert_eq!(scope, SearchScope::Actions);
    modal.open_with_scope(entries, scope);
    assert_eq!(modal.scope(), SearchScope::Actions);
    assert_eq!(modal.hits()[0].entry.title, "Delete Line");
}

#[test]
fn cmd_e_recent_files_preselects_previous_file_and_switches_on_enter() {
    let mut mac = KeyDispatchLayer::new(Platform::MacOs);
    let mut modal = SearchEverywhereState::new();
    assert_eq!(
        dispatch(&mut mac, "cmd-e", FocusContext::Workspace, false),
        DispatchOutcome::Action(ShellAction::OpenRecentFiles)
    );
    let recent = vec![
        SearchEntry {
            kind: SearchEntryKind::File,
            title: "Current.java".to_owned(),
            subtitle: "src".to_owned(),
            match_text: "src/Current.java".to_owned(),
            icon: "F",
            shortcut_hint: None,
            target: SearchTarget::File {
                path: PathBuf::from("src/Current.java"),
                line: None,
            },
        },
        SearchEntry {
            kind: SearchEntryKind::File,
            title: "Prev.java".to_owned(),
            subtitle: "src".to_owned(),
            match_text: "src/Prev.java".to_owned(),
            icon: "F",
            shortcut_hint: None,
            target: SearchTarget::File {
                path: PathBuf::from("src/Prev.java"),
                line: None,
            },
        },
    ];
    modal.open_recent_files(recent);
    assert!(modal.is_recent_files_mode());
    assert_eq!(modal.selected(), 1);
    assert_eq!(
        modal.handle_key(&ks_char("enter", "\n")),
        SearchKeyOutcome::Activate {
            index: 1,
            split: false
        }
    );
}

#[test]
fn cmd_l_go_to_line_and_column_works_regardless_of_vim_mode() {
    let mut mac = KeyDispatchLayer::new(Platform::MacOs);
    for vim in [false, true] {
        let leaf = if vim {
            FocusContext::EditorVimNormal
        } else {
            FocusContext::EditorText
        };
        assert_eq!(
            dispatch(&mut mac, "cmd-l", leaf, vim),
            DispatchOutcome::Action(ShellAction::GoToLine)
        );
    }

    let mut prompt = ExCommandLine::new(':');
    assert_eq!(prompt.insert("2:4"), ExLineEvent::Edited);
    let ExLineEvent::Submitted { prefix, text } = prompt.handle_named_key("enter") else {
        panic!("Enter must submit Go to Line");
    };
    assert_eq!(prefix, ':');
    let cmd = parse_ex_command(&text).expect("valid line:col");
    assert_eq!(cmd, ExCommand::GoToLineColumn(2, 4));

    let mut state = EditorState::from_text("alpha\nbeta_line\ngamma\n");
    if let ExCommand::GoToLineColumn(line, col) = cmd {
        state.set_line_col(line, col);
    }
    assert_eq!(state.primary_line_col(), (2, 4));
}

#[test]
fn editing_shortcuts_delete_move_extend_shrink_fold_and_reformat() {
    let mut mac = KeyDispatchLayer::new(Platform::MacOs);

    // Cmd+Backspace -> Delete Line
    assert_eq!(
        dispatch(&mut mac, "cmd-backspace", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::DeleteLine)
    );
    let mut ed = EditorState::from_text("first\nsecond\nthird\n");
    ed.set_line_col(2, 3);
    assert!(ed.delete_lines());
    assert_eq!(ed.snapshot().text(), "first\nthird\n");
    assert!(ed.undo());
    assert_eq!(ed.snapshot().text(), "first\nsecond\nthird\n");

    // Alt+Shift+Up/Down -> Move Line
    assert_eq!(
        dispatch(&mut mac, "alt-shift-up", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::MoveLine(true))
    );
    assert_eq!(
        dispatch(&mut mac, "alt-shift-down", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::MoveLine(false))
    );
    ed.set_line_col(2, 2);
    assert!(ed.move_lines(true));
    assert_eq!(ed.snapshot().text(), "second\nfirst\nthird\n");
    assert!(ed.move_lines(false));
    assert_eq!(ed.snapshot().text(), "first\nsecond\nthird\n");

    // Alt+Up / Alt+Down -> Extend / Shrink Selection over Tree-sitter AST
    assert_eq!(
        dispatch(&mut mac, "alt-up", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::ExtendSelection)
    );
    assert_eq!(
        dispatch(&mut mac, "alt-down", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::ShrinkSelection)
    );

    let java_src = "class Order {\n    void run() {\n        int count = 42;\n    }\n}\n";
    let registry = LanguageRegistry::with_defaults().expect("registry");
    let engine = SyntaxEngine::new(
        registry.clone(),
        SyntaxEngineConfig {
            worker_threads: 1,
            ..SyntaxEngineConfig::default()
        },
    )
    .expect("engine");
    let mut java_ed = EditorState::from_text(java_src);
    let snap = java_ed.snapshot();
    let tree = engine
        .parse_sync(SyntaxLanguage::Java, &snap, None)
        .expect("parse java");
    let syntax = EditorSyntax::build(tree, snap, Highlighter::new(registry));

    let count_pos = java_src.find("count").expect("count") + 2;
    java_ed.set_caret(count_pos);
    assert!(java_ed.extend_selection(Some(&syntax)));
    assert_eq!(&java_src[java_ed.selections().primary().range()], "count");
    assert!(java_ed.extend_selection(Some(&syntax)));
    assert!(java_ed.shrink_selection());
    assert_eq!(&java_src[java_ed.selections().primary().range()], "count");
    assert!(java_ed.shrink_selection());
    assert!(java_ed.selections().primary().is_empty());

    // Cmd+- / Cmd+= -> Fold / Unfold
    assert_eq!(
        dispatch(&mut mac, "cmd--", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::FoldRegion)
    );
    assert_eq!(
        dispatch(&mut mac, "cmd-=", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::UnfoldRegion)
    );
    let mut fold_map = FoldMap::new();
    assert!(fold_map.fold_at(2, syntax.folds()));
    assert!(fold_map.is_hidden(3));
    assert!(fold_map.unfold_at(2));
    assert!(!fold_map.is_hidden(3));

    // Cmd+Alt+L -> Reformat Code
    assert_eq!(
        dispatch(&mut mac, "cmd-alt-l", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::ReformatCode)
    );
    let mut messy = EditorState::from_text("class A {  \nint x = 1;  \n}\n");
    assert!(messy.reformat_code(SourceLanguageKind::Java, 4));
    assert_eq!(messy.snapshot().text(), "class A {\n    int x = 1;\n}\n");
}

#[test]
fn cmd_f_cmd_r_inline_find_replace_and_find_in_files() {
    let mut mac = KeyDispatchLayer::new(Platform::MacOs);
    assert_eq!(
        dispatch(&mut mac, "cmd-f", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::Find)
    );
    assert_eq!(
        dispatch(&mut mac, "cmd-r", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::Replace)
    );
    assert_eq!(
        dispatch(&mut mac, "cmd-g", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::FindNext)
    );
    assert_eq!(
        dispatch(&mut mac, "cmd-shift-g", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::FindPrev)
    );
    assert_eq!(
        dispatch(&mut mac, "cmd-shift-f", FocusContext::Workspace, false),
        DispatchOutcome::Action(ShellAction::FindInFiles)
    );

    let buf = Buffer::from_text("alpha beta alpha gamma");
    let snap = buf.snapshot();
    let mut find_bar = FindBarState::new();
    find_bar.open(true, Some("alpha"), &snap, 0);
    assert_eq!(find_bar.matches(), &[0..5, 11..16]);
    assert_eq!(find_bar.match_counter_label(), "1/2");
    assert_eq!(
        find_bar.handle_key(&ks_char("enter", "\n"), true),
        FindKeyOutcome::ReplaceCurrent
    );
    assert_eq!(
        find_bar.handle_key(&ks("escape"), true),
        FindKeyOutcome::Close
    );

    // Find in Files integration across temp directory and open buffers
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("kova_053_int_{nanos}"));
    std::fs::create_dir_all(&root).expect("temp dir");
    std::fs::write(root.join("Disk.java"), "class Disk { String target; }\n").expect("write");

    let mut fif = FindInFilesState::new();
    let generation = fif.open(Some("target"));
    let hits = search_text_in_buffers_and_paths(
        fif.query().text(),
        fif.case_sensitive(),
        &[(PathBuf::from("Mem.java"), "int target = 1;\n".to_owned())],
        Some(&root),
        &[PathBuf::from("Disk.java")],
    );
    assert!(fif.set_results(generation, hits));
    assert_eq!(fif.hits().len(), 2);
    assert_eq!(
        fif.handle_key(&ks_char("enter", "\n")),
        FindInFilesKeyOutcome::Activate {
            index: 0,
            split: false
        }
    );
    drop(std::fs::remove_dir_all(&root));
}

#[test]
fn f2_problems_and_actions_palette_include_all_new_shell_actions() {
    let mut mac = KeyDispatchLayer::new(Platform::MacOs);
    assert_eq!(
        dispatch(&mut mac, "f2", FocusContext::Workspace, false),
        DispatchOutcome::Action(ShellAction::NextProblem)
    );
    assert_eq!(
        dispatch(&mut mac, "shift-f2", FocusContext::Workspace, false),
        DispatchOutcome::Action(ShellAction::PrevProblem)
    );
    assert_eq!(
        dispatch(&mut mac, "cmd-f12", FocusContext::EditorText, false),
        DispatchOutcome::Action(ShellAction::ToggleTool(DockToolId::Structure))
    );

    let panel = BottomPanelModel::with_problems(vec![
        ProblemEntry {
            file: "A.java".to_owned(),
            line: 5,
            col: 1,
            severity: ProblemSeverity::Error,
            message: "err".to_owned(),
        },
        ProblemEntry {
            file: "B.java".to_owned(),
            line: 2,
            col: 3,
            severity: ProblemSeverity::Warning,
            message: "warn".to_owned(),
        },
    ]);
    assert_eq!(panel.next_problem_index(Some("A.java"), 5, 1), Some(1));
    assert_eq!(panel.prev_problem_index(Some("A.java"), 5, 1), Some(1));

    // Verify all new ShellAction IDs are listed in Search Everywhere -> Actions with preset shortcuts.
    let keymap_layer = jetbrains_keymap_layer_ref(Platform::MacOs);
    let action_ids: Vec<&str> = builtin_action_registry_ref()
        .iter()
        .filter(|d| ShellAction::from_action_id(&d.id).is_some())
        .map(|d| d.id.as_str())
        .collect();
    for expected_id in [
        ids::NAVIGATION_GO_TO_CLASS,
        ids::NAVIGATION_GO_TO_FILE,
        ids::NAVIGATION_FIND_ACTION,
        ids::NAVIGATION_RECENT_FILES,
        ids::NAVIGATION_GO_TO_LINE,
        ids::NAVIGATION_FILE_STRUCTURE,
        ids::NAVIGATION_NEXT_ERROR,
        ids::NAVIGATION_PREV_ERROR,
        ids::EDITOR_DELETE_LINE,
        ids::EDITOR_TOGGLE_BLOCK_COMMENT,
        ids::EDITOR_MOVE_LINE_UP,
        ids::EDITOR_MOVE_LINE_DOWN,
        ids::EDITOR_EXTEND_SELECTION,
        ids::EDITOR_SHRINK_SELECTION,
        ids::EDITOR_REFORMAT_CODE,
        ids::EDITOR_UNSELECT_OCCURRENCE,
        ids::EDITOR_SELECT_ALL_OCCURRENCES,
        ids::EDITOR_FOLD_REGION,
        ids::EDITOR_UNFOLD_REGION,
        ids::EDITOR_FOLD_ALL,
        ids::EDITOR_UNFOLD_ALL,
        ids::EDITOR_FIND,
        ids::EDITOR_REPLACE,
        ids::EDITOR_FIND_NEXT,
        ids::EDITOR_FIND_PREV,
        ids::EDITOR_FIND_IN_FILES,
        ids::RUN_RUN_CONTEXT,
        ids::RUN_RUN_LAST,
    ] {
        assert!(
            action_ids.contains(&expected_id),
            "missing action {expected_id} in Actions palette"
        );
        assert!(
            keymap_layer.chord_for_action(expected_id).is_some(),
            "missing macOS preset shortcut for {expected_id}"
        );
    }
}
