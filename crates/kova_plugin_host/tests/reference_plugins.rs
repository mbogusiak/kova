//! Integration tests loading the reference plugins (KOVA-048) into `kova_plugin_host`.
//!
//! The plugin crates under `plugins/` are compiled once per test binary to
//! `wasm32-wasip2` components (into `CARGO_TARGET_TMPDIR`, so the nested
//! cargo never contends for the outer build lock). Each bundle is assembled
//! from the committed `plugins/<id>/plugin.toml` plus the freshly built
//! `.wasm`, then loaded through [`PluginManager`] exactly as installed plugins.
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
    AstSymbol, InMemoryEditorBridge, PluginEvent, PluginManager, PluginManifest, PluginStatus,
    SandboxPolicy, SemanticIcon, StyleVariant, UiEvent, UiEventKind, WidgetKind, WidgetNode,
    WidgetTree, validate_widget_tree,
};
use tempfile::TempDir;

/// Reference plugin ids; each equals its cargo package name and `plugins/<id>` directory.
const REFERENCE_PLUGINS: [&str; 4] = [
    "kova-ai",
    "kova-database",
    "kova-kotlin-lsp",
    "kova-scala-metals",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root must exist")
}

/// Builds every reference plugin to `wasm32-wasip2` once and returns the artifact directory.
fn built_wasm_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let target_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("reference-plugins");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let mut cmd = Command::new(cargo);
        cmd.current_dir(workspace_root())
            .args([
                "build",
                "--locked",
                "--target",
                "wasm32-wasip2",
                "--target-dir",
            ])
            .arg(&target_dir);
        for id in REFERENCE_PLUGINS {
            cmd.args(["-p", id]);
        }
        let output = cmd.output().expect("failed to spawn cargo");
        assert!(
            output.status.success(),
            "building reference plugins for wasm32-wasip2 failed \
             (is the target installed? `rustup target add wasm32-wasip2`):\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        target_dir.join("wasm32-wasip2/debug")
    })
}

/// Copies `plugins/<id>/plugin.toml` and the built component into `root/<id>/`.
fn install_bundle(root: &Path, id: &str) -> PathBuf {
    let source_manifest = workspace_root()
        .join("plugins")
        .join(id)
        .join("plugin.toml");
    let raw = fs::read_to_string(&source_manifest).unwrap();
    let manifest =
        PluginManifest::from_toml_str(&raw).expect("committed plugin.toml must validate");
    assert_eq!(manifest.id, id);

    let wasm_name = format!("{}.wasm", id.replace('-', "_"));
    assert_eq!(manifest.entrypoint, PathBuf::from(&wasm_name));

    let dir = root.join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plugin.toml"), raw).unwrap();
    fs::copy(built_wasm_dir().join(&wasm_name), dir.join(&wasm_name)).unwrap();
    dir
}

fn java_bridge(workspace: &Path) -> Arc<InMemoryEditorBridge> {
    let bridge = Arc::new(InMemoryEditorBridge::new());
    bridge.set_workspace_root(Some(workspace.to_path_buf()));
    bridge.set_active_buffer(
        Some(PathBuf::from("src/main/java/App.java")),
        Some("java".to_owned()),
        "public class App {\n    public static void main(String[] args) {}\n}\n",
    );
    bridge.set_jvm_metadata(
        vec![AstSymbol {
            name: "App".to_owned(),
            kind: "class".to_owned(),
            line: 0,
            column: 13,
        }],
        2,
    );
    bridge
}

fn manager_with(ids: &[&str], bridge: Arc<InMemoryEditorBridge>) -> (TempDir, PluginManager) {
    let tmp = TempDir::new().unwrap();
    for id in ids {
        install_bundle(tmp.path(), id);
    }
    let mut manager =
        PluginManager::with_plugins_dir(tmp.path().to_path_buf(), SandboxPolicy::default(), bridge)
            .unwrap();
    let outcomes = manager.load_all();
    assert_eq!(outcomes.len(), ids.len());
    for outcome in &outcomes {
        assert!(outcome.result.is_ok(), "plugin failed to load: {outcome:?}");
    }
    for id in ids {
        assert_eq!(manager.plugin_status(id), Some(&PluginStatus::Active));
    }
    (tmp, manager)
}

fn node<'a>(tree: &'a WidgetTree, id: &str) -> &'a WidgetNode {
    tree.nodes
        .iter()
        .find(|n| n.id == id)
        .unwrap_or_else(|| panic!("widget `{id}` missing from tree"))
}

fn kind_names(tree: &WidgetTree) -> Vec<&'static str> {
    tree.nodes
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
        .collect()
}

fn assert_has_kinds(tree: &WidgetTree, expected: &[&str]) {
    let kinds = kind_names(tree);
    for kind in expected {
        assert!(kinds.contains(kind), "missing `{kind}` widget in {kinds:?}");
    }
}

fn ui_event(widget_id: &str, kind: UiEventKind) -> UiEvent {
    UiEvent {
        widget_id: widget_id.to_owned(),
        kind,
    }
}

#[test]
fn all_reference_plugins_load_side_by_side() {
    let tmp_ws = TempDir::new().unwrap();
    let (_tmp, manager) = manager_with(&REFERENCE_PLUGINS, java_bridge(tmp_ws.path()));

    let panels = manager.all_tool_windows();
    assert_eq!(panels.len(), REFERENCE_PLUGINS.len());
    for (plugin_id, panel) in &panels {
        assert!(
            panel.id.starts_with(&format!("{plugin_id}.")),
            "tool window `{}` not namespaced by `{plugin_id}`",
            panel.id
        );
    }
    for (plugin_id, command) in manager.all_commands() {
        assert!(command.id.starts_with(&format!("{plugin_id}.")));
    }
}

#[test]
fn kova_ai_renders_chat_with_editor_context_and_handles_send() {
    let tmp_ws = TempDir::new().unwrap();
    let (_tmp, mut manager) = manager_with(&["kova-ai"], java_bridge(tmp_ws.path()));

    let panels = manager.all_tool_windows();
    assert_eq!(panels[0].1.id, "kova-ai.chat");
    assert_eq!(panels[0].1.icon, SemanticIcon::Sparkle);
    assert_eq!(manager.all_commands().len(), 2);

    let tree = manager.render_ui("kova-ai", "kova-ai.chat").unwrap();
    validate_widget_tree("kova-ai", &tree).unwrap();
    assert_has_kinds(
        &tree,
        &[
            "split",
            "markdown",
            "code-block",
            "form",
            "text-input",
            "button",
            "select",
        ],
    );
    match &node(&tree, "kova-ai.context").kind {
        WidgetKind::CodeBlock(cb) => {
            assert_eq!(cb.language, "java");
            assert!(cb.code.starts_with("public class App"), "{}", cb.code);
        }
        other => panic!("unexpected context widget {other:?}"),
    }
    match &node(&tree, "kova-ai.send").kind {
        WidgetKind::Button(b) => assert!(b.disabled, "empty draft must disable Send"),
        other => panic!("unexpected send widget {other:?}"),
    }

    let typed = manager
        .dispatch_ui_event(
            "kova-ai",
            "kova-ai.chat",
            &ui_event(
                "kova-ai.prompt",
                UiEventKind::TextChanged("What is App?".to_owned()),
            ),
        )
        .unwrap()
        .expect("typing re-renders the panel");
    match &node(&typed, "kova-ai.send").kind {
        WidgetKind::Button(b) => assert!(!b.disabled),
        other => panic!("unexpected send widget {other:?}"),
    }

    let sent = manager
        .dispatch_ui_event(
            "kova-ai",
            "kova-ai.chat",
            &ui_event("kova-ai.send", UiEventKind::Clicked),
        )
        .unwrap()
        .expect("sending re-renders the panel");
    validate_widget_tree("kova-ai", &sent).unwrap();
    match &node(&sent, "kova-ai.transcript").kind {
        WidgetKind::Markdown(md) => {
            assert!(
                md.content.contains("**You:** What is App?"),
                "{}",
                md.content
            );
            assert!(
                md.content.contains("src/main/java/App.java"),
                "{}",
                md.content
            );
            assert!(
                md.content.contains("AST symbols: class App"),
                "{}",
                md.content
            );
        }
        other => panic!("unexpected transcript widget {other:?}"),
    }

    let unchanged = manager
        .dispatch_ui_event(
            "kova-ai",
            "kova-ai.chat",
            &ui_event("unknown-widget", UiEventKind::Clicked),
        )
        .unwrap();
    assert!(unchanged.is_none());

    manager
        .invoke_command("kova-ai", "kova-ai.clear-chat")
        .unwrap();
    assert!(manager.invoke_command("kova-ai", "kova-ai.nope").is_err());
    assert!(manager.render_ui("kova-ai", "other.panel").is_err());
    assert_eq!(
        manager.plugin_status("kova-ai"),
        Some(&PluginStatus::Active)
    );
}

#[test]
fn kova_ai_without_editor_read_degrades_gracefully() {
    let tmp = TempDir::new().unwrap();
    let dir = install_bundle(tmp.path(), "kova-ai");
    let manifest = fs::read_to_string(dir.join("plugin.toml"))
        .unwrap()
        .replace("editor_read   = true", "editor_read   = false");
    fs::write(dir.join("plugin.toml"), manifest).unwrap();

    let tmp_ws = TempDir::new().unwrap();
    let mut manager = PluginManager::with_plugins_dir(
        tmp.path().to_path_buf(),
        SandboxPolicy::default(),
        java_bridge(tmp_ws.path()),
    )
    .unwrap();
    manager.load_all();
    let tree = manager.render_ui("kova-ai", "kova-ai.chat").unwrap();
    match &node(&tree, "kova-ai.context").kind {
        WidgetKind::CodeBlock(cb) => {
            assert!(cb.code.contains("context unavailable"), "{}", cb.code);
            assert!(!cb.code.contains("public class App"));
        }
        other => panic!("unexpected context widget {other:?}"),
    }
}

#[test]
fn kova_database_renders_explorer_tree_and_virtualized_table() {
    let tmp_ws = TempDir::new().unwrap();
    let (_tmp, mut manager) = manager_with(&["kova-database"], java_bridge(tmp_ws.path()));
    let panels = manager.all_tool_windows();
    assert_eq!(panels[0].1.id, "kova-database.explorer");
    assert_eq!(panels[0].1.icon, SemanticIcon::Database);

    let panel = "kova-database.explorer";
    let tree = manager.render_ui("kova-database", panel).unwrap();
    validate_widget_tree("kova-database", &tree).unwrap();
    assert_has_kinds(
        &tree,
        &[
            "split",
            "tree",
            "table",
            "form",
            "code-block",
            "text-input",
            "button",
            "badge",
        ],
    );
    match &node(&tree, "kova-database.schema-tree").kind {
        WidgetKind::Tree(t) => {
            assert_eq!(t.items.len(), 4);
            assert_eq!(t.items[3].id, "table:local-postgres/public/orders");
            assert_eq!(t.items[3].depth, 2);
            assert!(!t.items[3].expanded);
        }
        other => panic!("unexpected tree widget {other:?}"),
    }

    let expanded = manager
        .dispatch_ui_event(
            "kova-database",
            panel,
            &ui_event(
                "kova-database.schema-tree",
                UiEventKind::TreeToggled("table:local-postgres/public/orders".to_owned()),
            ),
        )
        .unwrap()
        .expect("toggling re-renders the explorer");
    match &node(&expanded, "kova-database.schema-tree").kind {
        WidgetKind::Tree(t) => assert_eq!(t.items.len(), 8, "4 order columns revealed"),
        other => panic!("unexpected tree widget {other:?}"),
    }

    let selected = manager
        .dispatch_ui_event(
            "kova-database",
            panel,
            &ui_event(
                "kova-database.schema-tree",
                UiEventKind::TreeSelected("table:local-postgres/public/customers".to_owned()),
            ),
        )
        .unwrap()
        .expect("selecting a table runs a query");
    match &node(&selected, "kova-database.results").kind {
        WidgetKind::Table(t) => {
            let titles: Vec<&str> = t.columns.iter().map(|c| c.title.as_str()).collect();
            assert_eq!(titles, ["id", "name", "country"]);
            assert_eq!(t.rows.len(), 50);
            assert_eq!(t.rows[0], ["1", "Customer 1", "PL"]);
        }
        other => panic!("unexpected table widget {other:?}"),
    }
}

#[test]
fn kova_database_streams_large_results_and_resets_on_refresh() {
    let tmp_ws = TempDir::new().unwrap();
    let (_tmp, mut manager) = manager_with(&["kova-database"], java_bridge(tmp_ws.path()));
    let panel = "kova-database.explorer";

    // A full-table query streams thousands of rows into the virtualized `Table`.
    manager
        .dispatch_ui_event(
            "kova-database",
            panel,
            &ui_event(
                "kova-database.query",
                UiEventKind::TextChanged("SELECT id, status FROM public.orders".to_owned()),
            ),
        )
        .unwrap();
    let big = manager
        .dispatch_ui_event(
            "kova-database",
            panel,
            &ui_event("kova-database.run", UiEventKind::Clicked),
        )
        .unwrap()
        .expect("running re-renders the results");
    validate_widget_tree("kova-database", &big).unwrap();
    match &node(&big, "kova-database.results").kind {
        WidgetKind::Table(t) => {
            assert_eq!(t.columns.len(), 2);
            assert_eq!(t.rows.len(), 5_000);
        }
        other => panic!("unexpected table widget {other:?}"),
    }
    match &node(&big, "kova-database.status") {
        WidgetNode {
            kind: WidgetKind::Badge(b),
            style,
            ..
        } => {
            assert_eq!(b.label, "5000 rows");
            assert_eq!(*style, StyleVariant::Accent);
        }
        other => panic!("unexpected status widget {other:?}"),
    }

    manager
        .invoke_command("kova-database", "kova-database.refresh")
        .unwrap();
    let reset = manager.render_ui("kova-database", panel).unwrap();
    match &node(&reset, "kova-database.results").kind {
        WidgetKind::Table(t) => assert_eq!(t.rows.len(), 0),
        other => panic!("unexpected table widget {other:?}"),
    }
    assert_eq!(
        manager.plugin_status("kova-database"),
        Some(&PluginStatus::Active)
    );
}

#[test]
fn language_packs_detect_their_buffers_and_render_status() {
    let tmp_ws = TempDir::new().unwrap();
    let bridge = java_bridge(tmp_ws.path());
    let (_tmp, mut manager) =
        manager_with(&["kova-kotlin-lsp", "kova-scala-metals"], bridge.clone());

    let commands: Vec<String> = manager
        .all_commands()
        .into_iter()
        .map(|(_, c)| c.id)
        .collect();
    for expected in [
        "kova-kotlin-lsp.restart-server",
        "kova-kotlin-lsp.show-status",
        "kova-scala-metals.restart-server",
        "kova-scala-metals.show-status",
    ] {
        assert!(commands.iter().any(|c| c == expected), "missing {expected}");
    }

    let badge = |tree: &WidgetTree, id: &str| match &node(tree, id).kind {
        WidgetKind::Badge(b) => b.label.clone(),
        other => panic!("unexpected badge widget {other:?}"),
    };

    // A Java buffer matches neither pack.
    let kotlin = manager
        .render_ui("kova-kotlin-lsp", "kova-kotlin-lsp.status")
        .unwrap();
    validate_widget_tree("kova-kotlin-lsp", &kotlin).unwrap();
    assert_has_kinds(
        &kotlin,
        &["form", "markdown", "code-block", "list", "button", "badge"],
    );
    assert!(badge(&kotlin, "kova-kotlin-lsp.state").starts_with("Idle"));
    match &node(&kotlin, "kova-kotlin-lsp.launch").kind {
        WidgetKind::CodeBlock(cb) => assert_eq!(cb.code, "kotlin-lsp --stdio"),
        other => panic!("unexpected launch widget {other:?}"),
    }

    // Opening a Kotlin buffer (snapshot) flips only the Kotlin pack to ready.
    bridge.set_active_buffer(
        Some(PathBuf::from("src/main/kotlin/App.kt")),
        Some("kotlin".to_owned()),
        "fun main() {}\n",
    );
    let kotlin = manager
        .render_ui("kova-kotlin-lsp", "kova-kotlin-lsp.status")
        .unwrap();
    assert_eq!(badge(&kotlin, "kova-kotlin-lsp.state"), "Ready to attach");
    let scala = manager
        .render_ui("kova-scala-metals", "kova-scala-metals.status")
        .unwrap();
    assert!(badge(&scala, "kova-scala-metals.state").starts_with("Idle"));

    // Events without a snapshot read: a saved `.sbt` file activates the Scala pack.
    manager.dispatch_event(&PluginEvent::BufferSaved("build.sbt".to_owned()));
    bridge.set_active_buffer(None, None, "");
    let scala = manager
        .render_ui("kova-scala-metals", "kova-scala-metals.status")
        .unwrap();
    assert_eq!(badge(&scala, "kova-scala-metals.state"), "Ready to attach");

    let restarted = manager
        .dispatch_ui_event(
            "kova-scala-metals",
            "kova-scala-metals.status",
            &ui_event("kova-scala-metals.restart", UiEventKind::Clicked),
        )
        .unwrap()
        .expect("restart re-renders the status panel");
    match &node(&restarted, "kova-scala-metals.summary").kind {
        WidgetKind::Markdown(md) => {
            assert!(md.content.contains("Restart requests: 1"), "{}", md.content);
        }
        other => panic!("unexpected summary widget {other:?}"),
    }
    assert!(
        manager
            .plugin_logs("kova-scala-metals")
            .iter()
            .any(|log| log.message.contains("restart requested: metals"))
    );
    assert!(
        manager
            .dispatch_ui_event(
                "kova-scala-metals",
                "kova-scala-metals.status",
                &ui_event("kova-scala-metals.state", UiEventKind::Clicked),
            )
            .unwrap()
            .is_none()
    );
}
