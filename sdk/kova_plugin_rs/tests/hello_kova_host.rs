//! End-to-end SDK test (KOVA-046): `examples/hello_kova` is compiled to a
//! `wasm32-wasip2` component and loaded through the real
//! [`kova_plugin_host::PluginManager`], exactly like an installed plugin.
//!
//! The nested cargo build writes into `CARGO_TARGET_TMPDIR`, so it never
//! contends for the outer build lock.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration tests assert invariants directly"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use kova_plugin_host::{
    InMemoryEditorBridge, PluginManager, PluginManifest, PluginStatus, SandboxPolicy, SemanticIcon,
    StyleVariant, UiEvent, UiEventKind, WidgetKind, WidgetNode, WidgetTree, validate_widget_tree,
};
use tempfile::TempDir;

const PLUGIN_ID: &str = "hello-kova";
const PANEL: &str = "hello-kova.panel";
const WASM: &str = "hello_kova.wasm";

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello_kova")
}

/// Builds `hello_kova` for `wasm32-wasip2` once and returns the component path.
fn built_component() -> &'static Path {
    static WASM_PATH: OnceLock<PathBuf> = OnceLock::new();
    WASM_PATH.get_or_init(|| {
        let target_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hello-kova");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let output = Command::new(cargo)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args([
                "build",
                "--locked",
                "--target",
                "wasm32-wasip2",
                "-p",
                "hello_kova",
                "--target-dir",
            ])
            .arg(&target_dir)
            .output()
            .expect("failed to spawn cargo");
        assert!(
            output.status.success(),
            "building hello_kova for wasm32-wasip2 failed \
             (is the target installed? `rustup target add wasm32-wasip2`):\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        target_dir.join("wasm32-wasip2/debug").join(WASM)
    })
}

/// Installs `plugin.toml` + the component into `root/hello-kova/`.
fn install(root: &Path) {
    let raw = fs::read_to_string(example_dir().join("plugin.toml")).unwrap();
    let manifest =
        PluginManifest::from_toml_str(&raw).expect("committed plugin.toml must validate");
    assert_eq!(manifest.id, PLUGIN_ID);
    assert_eq!(manifest.entrypoint, PathBuf::from(WASM));

    let dir = root.join(PLUGIN_ID);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plugin.toml"), raw).unwrap();
    let wasm = fs::read(built_component()).unwrap();
    assert_eq!(&wasm[..4], b"\0asm", "component must be a WASM binary");
    fs::write(dir.join(WASM), wasm).unwrap();
}

fn loaded_manager() -> (TempDir, PluginManager) {
    let tmp = TempDir::new().unwrap();
    install(tmp.path());
    let mut manager = PluginManager::with_plugins_dir(
        tmp.path().to_path_buf(),
        SandboxPolicy::default(),
        Arc::new(InMemoryEditorBridge::new()),
    )
    .unwrap();
    let outcomes = manager.load_all();
    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].result.is_ok(),
        "hello_kova failed to load: {outcomes:?}"
    );
    assert_eq!(
        manager.plugin_status(PLUGIN_ID),
        Some(&PluginStatus::Active)
    );
    (tmp, manager)
}

fn node<'a>(tree: &'a WidgetTree, id: &str) -> &'a WidgetNode {
    tree.nodes
        .iter()
        .find(|n| n.id == id)
        .unwrap_or_else(|| panic!("widget `{id}` missing from tree"))
}

fn event(widget: &str, kind: UiEventKind) -> UiEvent {
    UiEvent {
        widget_id: format!("hello-kova.{widget}"),
        kind,
    }
}

fn dispatch(manager: &mut PluginManager, widget: &str, kind: UiEventKind) -> Option<WidgetTree> {
    manager
        .dispatch_ui_event(PLUGIN_ID, PANEL, &event(widget, kind))
        .unwrap()
}

#[test]
fn hello_kova_registers_through_kova_plugin_trait() {
    let (_tmp, manager) = loaded_manager();

    let panels = manager.all_tool_windows();
    assert_eq!(panels.len(), 1);
    assert_eq!(panels[0].0, PLUGIN_ID);
    assert_eq!(panels[0].1.id, PANEL);
    assert_eq!(panels[0].1.title, "Hello Kova");
    assert_eq!(panels[0].1.icon, SemanticIcon::Plugin);

    let commands: Vec<String> = manager
        .all_commands()
        .into_iter()
        .map(|(_, c)| c.id)
        .collect();
    assert_eq!(commands, ["hello-kova.say-hello", "hello-kova.reset"]);
    assert!(
        manager
            .plugin_logs(PLUGIN_ID)
            .iter()
            .any(|log| log.message.contains("hello-kova ready")),
        "KovaPlugin::init must run after registration"
    );
}

#[test]
fn hello_kova_renders_every_widget_kind_through_the_host() {
    let (_tmp, mut manager) = loaded_manager();
    let tree = manager.render_ui(PLUGIN_ID, PANEL).unwrap();
    validate_widget_tree(PLUGIN_ID, &tree).unwrap();

    let mut kinds: Vec<&str> = tree
        .nodes
        .iter()
        .map(|n| match n.kind {
            WidgetKind::Split(_) => "split",
            WidgetKind::Tabs(_) => "tabs",
            WidgetKind::Tree(_) => "tree",
            WidgetKind::Table(_) => "table",
            WidgetKind::ListView(_) => "list",
            WidgetKind::Form(_) => "form",
            WidgetKind::TextInput(_) => "text-input",
            WidgetKind::Select(_) => "select",
            WidgetKind::Toggle(_) => "toggle",
            WidgetKind::Button(_) => "button",
            WidgetKind::CodeBlock(_) => "code-block",
            WidgetKind::Markdown(_) => "markdown",
            WidgetKind::Badge(_) => "badge",
        })
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(
        kinds.len(),
        13,
        "all WIT widget kinds expected, got {kinds:?}"
    );
    assert_eq!(
        node(&tree, "hello-kova.tabs").id,
        tree.nodes[tree.root as usize].id
    );
    match &node(&tree, "hello-kova.greet").kind {
        WidgetKind::Button(b) => assert!(b.disabled, "empty name must disable Greet"),
        other => panic!("unexpected greet widget {other:?}"),
    }
    assert_eq!(node(&tree, "hello-kova.status").style, StyleVariant::Muted);
    assert!(manager.render_ui(PLUGIN_ID, "other.panel").is_err());
}

#[test]
fn hello_kova_keeps_state_across_ui_events_and_commands() {
    let (_tmp, mut manager) = loaded_manager();

    dispatch(
        &mut manager,
        "name",
        UiEventKind::TextChanged("Ada".to_owned()),
    )
    .expect("typing re-renders");
    dispatch(&mut manager, "language", UiEventKind::SelectedIndex(2)).unwrap();
    let greeted = dispatch(&mut manager, "greet", UiEventKind::Clicked).expect("greet re-renders");
    validate_widget_tree(PLUGIN_ID, &greeted).unwrap();
    match &node(&greeted, "hello-kova.greeting").kind {
        WidgetKind::Markdown(md) => assert_eq!(md.content, "## Hallo, Ada!"),
        other => panic!("unexpected greeting widget {other:?}"),
    }
    assert_eq!(
        node(&greeted, "hello-kova.status").style,
        StyleVariant::Accent
    );

    let expanded = dispatch(
        &mut manager,
        "concepts",
        UiEventKind::TreeToggled("sdk".to_owned()),
    )
    .expect("toggle re-renders");
    match &node(&expanded, "hello-kova.concepts").kind {
        WidgetKind::Tree(t) => assert_eq!(t.items.len(), 5),
        other => panic!("unexpected tree widget {other:?}"),
    }
    assert!(dispatch(&mut manager, "snippet", UiEventKind::Clicked).is_none());

    // Commands share the same plugin instance as UI events.
    manager
        .invoke_command(PLUGIN_ID, "hello-kova.say-hello")
        .unwrap();
    assert!(
        manager
            .plugin_logs(PLUGIN_ID)
            .iter()
            .any(|log| log.message == "Hallo, Ada!")
    );
    let after = manager.render_ui(PLUGIN_ID, PANEL).unwrap();
    match &node(&after, "hello-kova.history").kind {
        WidgetKind::ListView(list) => assert_eq!(list.items.len(), 2),
        other => panic!("unexpected history widget {other:?}"),
    }

    manager
        .invoke_command(PLUGIN_ID, "hello-kova.reset")
        .unwrap();
    let reset = manager.render_ui(PLUGIN_ID, PANEL).unwrap();
    match &node(&reset, "hello-kova.history").kind {
        WidgetKind::ListView(list) => assert_eq!(list.items.len(), 0),
        other => panic!("unexpected history widget {other:?}"),
    }
    assert!(
        manager
            .invoke_command(PLUGIN_ID, "hello-kova.nope")
            .is_err()
    );
    assert_eq!(
        manager.plugin_status(PLUGIN_ID),
        Some(&PluginStatus::Active)
    );
}

#[test]
fn hello_kova_survives_reload_with_fresh_state() {
    let (_tmp, mut manager) = loaded_manager();
    dispatch(
        &mut manager,
        "name",
        UiEventKind::TextChanged("Grace".to_owned()),
    )
    .unwrap();
    assert_eq!(
        manager.reload_plugin(PLUGIN_ID).unwrap(),
        PluginStatus::Active
    );
    let tree = manager.render_ui(PLUGIN_ID, PANEL).unwrap();
    match &node(&tree, "hello-kova.name").kind {
        WidgetKind::TextInput(input) => assert_eq!(input.value, ""),
        other => panic!("unexpected name widget {other:?}"),
    }
}
