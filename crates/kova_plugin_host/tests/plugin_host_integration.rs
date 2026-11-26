//! Headless integration tests for `kova_plugin_host` (KOVA-044).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert invariants directly"
)]

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kova_plugin_host::{
    AstSymbol, BadgeProps, EditorContextBridge, InMemoryEditorBridge, LogLevel, PluginEvent,
    PluginHostError, PluginManager, PluginManifest, PluginResourceLimiter, PluginStatus,
    PluginStoreState, SandboxPolicy, StyleVariant, UiEvent, UiEventKind, WidgetKind, WidgetNode,
    WidgetTree, build_wasi_ctx, is_network_host_allowed, is_socket_addr_allowed, resolve_fs_grants,
    validate_widget_tree,
};
use tempfile::TempDir;
use wasmtime_wasi::filesystem::WasiFilesystemView;
use wasmtime_wasi::p2::bindings::filesystem::preopens::Host as PreopensHost;

fn build_init_wat(init_mode: &str, call_host_in_init: bool) -> &'static str {
    match init_mode {
        "loop" => "(loop $inf (br $inf)) (i32.const 0)",
        "trap" => "(unreachable)",
        "err" => {
            r"
            (i32.store (i32.const 256) (i32.const 1))
            (i32.store (i32.const 260) (i32.const 128))
            (i32.store (i32.const 264) (i32.const 17))
            (i32.const 256)
            "
        }
        "grow_fail" => {
            r"
            (if (result i32) (i32.eq (memory.grow (i32.const 64)) (i32.const -1))
              (then
                (i32.store (i32.const 256) (i32.const 1))
                (i32.store (i32.const 260) (i32.const 160))
                (i32.store (i32.const 264) (i32.const 18))
                (i32.const 256)
              )
              (else
                (i32.store (i32.const 256) (i32.const 0))
                (i32.const 256)
              )
            )
            "
        }
        _ if call_host_in_init => {
            r"
            (call $host_log (i32.const 1) (i32.const 320) (i32.const 15))
            (call $host_register_command
                (i32.const 336) (i32.const 8)
                (i32.const 344) (i32.const 8)
                (i32.const 352) (i32.const 4)
                (i32.const 384))
            (call $host_register_tool_window
                (i32.const 356) (i32.const 10)
                (i32.const 366) (i32.const 10)
                (i32.const 0)
                (i32.const 400))
            (i32.store (i32.const 256) (i32.const 0))
            (i32.const 256)
            "
        }
        _ => {
            r"
            (i32.store (i32.const 256) (i32.const 0))
            (i32.const 256)
            "
        }
    }
}

const CORE_PLUGIN_WAT_TEMPLATE: &str = r#"
(module
  (import "kova:plugin/host@0.1.0" "log" (func $host_log (param i32 i32 i32)))
  (import "kova:plugin/host@0.1.0" "get-editor-snapshot" (func $host_get_snapshot (param i32)))
  (import "kova:plugin/host@0.1.0" "apply-editor-edit" (func $host_apply_edit (param i64 i64 i32 i32 i32)))
  (import "kova:plugin/host@0.1.0" "register-command" (func $host_register_command (param i32 i32 i32 i32 i32 i32 i32)))
  (import "kova:plugin/host@0.1.0" "register-tool-window" (func $host_register_tool_window (param i32 i32 i32 i32 i32 i32)))

  (memory (export "memory") 1)

  (data (i32.const 128) "guest-init-failed")
  (data (i32.const 160) "memory-grow-denied")
  (data (i32.const 320) "hello-from-wasm")
  (data (i32.const 336) "demo.run")
  (data (i32.const 344) "Run Demo")
  (data (i32.const 352) "Demo")
  (data (i32.const 356) "demo.panel")
  (data (i32.const 366) "Demo Panel")
  (data (i32.const 700) "status-badge")
  (data (i32.const 720) "Ready")
  (data (i32.const 740) "patched")

  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
    (i32.const 4096)
  )

  (func (export "kova:plugin/plugin@0.1.0#init") (result i32)
    __INIT_BODY__
  )

  (func (export "kova:plugin/plugin@0.1.0#shutdown") (result i32)
    (i32.store (i32.const 272) (i32.const 0))
    (i32.const 272)
  )

  (func (export "kova:plugin/plugin@0.1.0#on-event") (param i32 i32 i32) (result i32)
    (i32.store (i32.const 272) (i32.const 0))
    (i32.const 272)
  )

  (func (export "kova:plugin/plugin@0.1.0#on-command") (param $ptr i32) (param $len i32) (result i32)
    (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 116))
      (then (unreachable))
    )
    (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 115))
      (then (loop $spin (br $spin)))
    )
    (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 99))
      (then
        (call $host_get_snapshot (i32.const 1024))
        (if (i32.load8_u (i32.const 1024))
          (then
            (i32.store (i32.const 288) (i32.const 1))
            (i32.store (i32.const 292) (i32.load (i32.const 1032)))
            (i32.store (i32.const 296) (i32.load (i32.const 1036)))
            (return (i32.const 288))
          )
        )
      )
    )
    (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 97))
      (then
        (call $host_apply_edit (i64.const 0) (i64.const 5) (i32.const 740) (i32.const 7) (i32.const 1024))
        (if (i32.load8_u (i32.const 1024))
          (then
            (i32.store (i32.const 288) (i32.const 1))
            (i32.store (i32.const 292) (i32.load (i32.const 1032)))
            (i32.store (i32.const 296) (i32.load (i32.const 1036)))
            (return (i32.const 288))
          )
        )
      )
    )
    (i32.store (i32.const 288) (i32.const 0))
    (i32.const 288)
  )

  (func (export "kova:plugin/plugin@0.1.0#render-ui") (param i32 i32) (result i32)
    (i32.store (i32.const 600) (i32.const 700))
    (i32.store (i32.const 604) (i32.const 12))
    (i32.store (i32.const 608) (i32.const 12))
    (i32.store (i32.const 612) (i32.const 720))
    (i32.store (i32.const 616) (i32.const 5))
    (i32.store (i32.const 636) (i32.const 4))
    (i32.store (i32.const 640) (i32.const 0))
    (i32.store (i32.const 644) (i32.const 0))
    (i32.store (i32.const 512) (i32.const 0))
    (i32.store (i32.const 516) (i32.const 0))
    (i32.store (i32.const 520) (i32.const 600))
    (i32.store (i32.const 524) (i32.const 1))
    (i32.const 512)
  )

  (func (export "kova:plugin/plugin@0.1.0#on-ui-event") (param i32 i32 i32 i32 i32 i32 i32) (result i32)
    (i32.store (i32.const 544) (i32.const 0))
    (i32.store (i32.const 548) (i32.const 0))
    (i32.const 544)
  )
)
"#;

/// Compiles a core WAT module implementing `kova:plugin/plugin@0.1.0` into a
/// real Component Model `.wasm` binary using `wit_component`.
fn build_test_component_bytes(init_mode: &str, call_host_in_init: bool) -> Vec<u8> {
    let init_body = build_init_wat(init_mode, call_host_in_init);
    let core_wat = CORE_PLUGIN_WAT_TEMPLATE.replace("__INIT_BODY__", init_body);

    let mut wasm = wat::parse_str(&core_wat).expect("core WAT must parse");
    let wit_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("wit/kova-plugin.wit");
    let mut resolve = wit_parser::Resolve::default();
    let pkg = resolve.push_file(&wit_path).expect("WIT must parse");
    let world = resolve
        .select_world(&[pkg], Some("kova-plugin"))
        .expect("kova-plugin world must exist");

    wit_component::embed_component_metadata(
        &mut wasm,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        true,
    )
    .expect("embed_component_metadata");

    wit_component::ComponentEncoder::default()
        .module(&wasm)
        .expect("set module")
        .validate(true)
        .encode()
        .expect("encode component")
}

fn write_plugin_bundle(
    root: &std::path::Path,
    id: &str,
    toml_extra: &str,
    component_bytes: &[u8],
) -> PathBuf {
    let dir = root.join(id);
    fs::create_dir_all(&dir).unwrap();
    let manifest = format!(
        r#"id = "{id}"
name = "Test Plugin {id}"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"
{toml_extra}
"#
    );
    fs::write(dir.join("plugin.toml"), manifest).unwrap();
    fs::write(dir.join("plugin.wasm"), component_bytes).unwrap();
    dir
}

#[test]
fn wit_contract_parses_cleanly_with_wit_parser() {
    let wit_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("wit/kova-plugin.wit");
    let mut resolve = wit_parser::Resolve::default();
    let pkg_id = resolve
        .push_file(&wit_path)
        .expect("wit/kova-plugin.wit must parse and resolve cleanly");
    let pkg = &resolve.packages[pkg_id];
    assert_eq!(pkg.name.namespace, "kova");
    assert_eq!(pkg.name.name, "plugin");
    assert!(pkg.interfaces.contains_key("types"));
    assert!(pkg.interfaces.contains_key("host"));
    assert!(pkg.interfaces.contains_key("plugin"));
    assert!(pkg.worlds.contains_key("kova-plugin"));
}

#[test]
fn manifest_validation_rejects_unsafe_paths_wildcards_and_bad_api() {
    // Valid minimal manifest
    let valid = PluginManifest::from_toml_str(
        r#"
id = "kova-db"
name = "Kova Database"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"

[permissions]
fs_read = ["workspace"]
fs_write = false
network = ["localhost:5432", "127.0.0.1:8080"]
process_spawn = false
editor_read = true
editor_write = false
"#,
    )
    .expect("valid manifest must parse");
    assert_eq!(valid.id, "kova-db");
    assert_eq!(valid.permissions.fs_write, Vec::<String>::new());
    assert!(valid.permissions.editor_read);

    // Path traversal in entrypoint
    let traversal = PluginManifest::from_toml_str(
        r#"
id = "bad-entry"
name = "Bad"
version = "0.1.0"
entrypoint = "../escape.wasm"
api_version = "0.1.0"
"#,
    );
    assert!(matches!(
        traversal,
        Err(PluginHostError::InvalidEntrypoint { .. })
    ));

    // Root directory `/` in fs_read
    let root_fs = PluginManifest::from_toml_str(
        r#"
id = "bad-fs"
name = "Bad FS"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"

[permissions]
fs_read = ["/"]
"#,
    );
    assert!(matches!(
        root_fs,
        Err(PluginHostError::InvalidManifest { .. })
    ));

    // Wildcard in network
    let wildcard_net = PluginManifest::from_toml_str(
        r#"
id = "bad-net"
name = "Bad Net"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"

[permissions]
network = ["*.example.com"]
"#,
    );
    assert!(matches!(
        wildcard_net,
        Err(PluginHostError::InvalidManifest { .. })
    ));

    // Incompatible API version
    let bad_api = PluginManifest::from_toml_str(
        r#"
id = "old-api"
name = "Old"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "9.0.0"
"#,
    );
    assert!(matches!(
        bad_api,
        Err(PluginHostError::UnsupportedApiVersion { .. })
    ));
}

#[test]
fn sandbox_policy_enforces_ceilings_and_disallows_process_spawn() {
    let policy = SandboxPolicy::default();
    let spawn_manifest = PluginManifest::from_toml_str(
        r#"
id = "spawn-plugin"
name = "Spawner"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"

[permissions]
process_spawn = true
"#,
    )
    .unwrap();
    assert!(matches!(
        policy.resolve_limits(&spawn_manifest),
        Err(PluginHostError::PolicyViolation { .. })
    ));

    let mem_manifest = PluginManifest::from_toml_str(
        r#"
id = "oom-plugin"
name = "Greedy"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"

[limits]
max_memory_mb = 2048
"#,
    )
    .unwrap();
    assert!(matches!(
        policy.resolve_limits(&mem_manifest),
        Err(PluginHostError::PolicyViolation { .. })
    ));
}

#[test]
fn widget_tree_validation_catches_cycles_and_invalid_indices() {
    let valid_tree = WidgetTree {
        root: 0,
        nodes: vec![WidgetNode {
            id: "badge-1".to_owned(),
            kind: WidgetKind::Badge(BadgeProps {
                label: "OK".to_owned(),
            }),
            style: StyleVariant::Accent,
            children: vec![],
        }],
    };
    validate_widget_tree("p1", &valid_tree).expect("valid tree passes");

    let cyclic_tree = WidgetTree {
        root: 0,
        nodes: vec![WidgetNode {
            id: "self-loop".to_owned(),
            kind: WidgetKind::Badge(BadgeProps {
                label: "Loop".to_owned(),
            }),
            style: StyleVariant::Error,
            children: vec![0],
        }],
    };
    assert!(matches!(
        validate_widget_tree("p1", &cyclic_tree),
        Err(PluginHostError::InvalidWidgetTree { .. })
    ));
}

#[test]
fn loads_valid_wasm_component_calls_init_and_host_imports() {
    let tmp = TempDir::new().unwrap();
    let wasm = build_test_component_bytes("ok", true);
    write_plugin_bundle(
        tmp.path(),
        "demo-plugin",
        "[permissions]\neditor_read = true\n",
        &wasm,
    );

    let bridge = Arc::new(InMemoryEditorBridge::new());
    bridge.set_workspace_root(Some(tmp.path().to_path_buf()));
    bridge.set_active_buffer(
        Some(PathBuf::from("src/Main.java")),
        Some("java".to_owned()),
        "class Main {}",
    );
    bridge.set_jvm_metadata(
        vec![AstSymbol {
            name: "Main".to_owned(),
            kind: "class".to_owned(),
            line: 0,
            column: 0,
        }],
        3,
    );

    let mut manager =
        PluginManager::with_plugins_dir(tmp.path().to_path_buf(), SandboxPolicy::default(), bridge)
            .unwrap();
    let outcomes = manager.load_all();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        manager.plugin_status("demo-plugin"),
        Some(&PluginStatus::Active)
    );

    let cmds = manager.all_commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].1.id, "demo.run");

    let panels = manager.all_tool_windows();
    assert_eq!(panels.len(), 1);
    assert_eq!(panels[0].1.id, "demo.panel");

    let logs = manager.plugin_logs("demo-plugin");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].level, LogLevel::Info);
    assert_eq!(logs[0].message, "hello-from-wasm");

    // Verify render-ui returns a valid declarative WidgetTree and on-ui-event works.
    let tree = manager.render_ui("demo-plugin", "demo.panel").unwrap();
    assert_eq!(tree.nodes.len(), 1);
    assert_eq!(tree.nodes[0].id, "status-badge");
    assert_eq!(tree.nodes[0].style, StyleVariant::Accent);

    let updated = manager
        .dispatch_ui_event(
            "demo-plugin",
            "demo.panel",
            &UiEvent {
                widget_id: "status-badge".to_owned(),
                kind: UiEventKind::Clicked,
            },
        )
        .unwrap();
    assert!(updated.is_none());

    manager.dispatch_event(&PluginEvent::BufferSaved("src/Main.java".to_owned()));
    assert_eq!(
        manager.plugin_status("demo-plugin"),
        Some(&PluginStatus::Active)
    );
}

#[test]
fn enforces_editor_permissions_and_rope_snapshot_updates() {
    let tmp = TempDir::new().unwrap();
    let wasm = build_test_component_bytes("ok", false);
    // Plugin has NO editor_read and NO editor_write permissions.
    write_plugin_bundle(tmp.path(), "restricted-editor", "", &wasm);

    let bridge = Arc::new(InMemoryEditorBridge::new());
    bridge.set_active_buffer(
        Some(PathBuf::from("App.java")),
        Some("java".to_owned()),
        "hello world",
    );

    let bridge_dyn: Arc<dyn EditorContextBridge> = bridge.clone();
    let mut manager = PluginManager::with_plugins_dir(
        tmp.path().to_path_buf(),
        SandboxPolicy::default(),
        bridge_dyn,
    )
    .unwrap();
    let _outcomes = manager.load_all();

    // Calling command "check-snapshot" must fail with guest permission-denied error.
    let read_err = manager
        .invoke_command("restricted-editor", "check-snapshot")
        .unwrap_err();
    assert!(
        matches!(read_err, PluginHostError::PluginReturnedError { ref message, .. } if message.contains("editor_read")),
        "unexpected error: {read_err:?}"
    );

    // Calling command "apply-edit" must also fail.
    let write_err = manager
        .invoke_command("restricted-editor", "apply-edit")
        .unwrap_err();
    assert!(
        matches!(write_err, PluginHostError::PluginReturnedError { ref message, .. } if message.contains("editor_write")),
        "unexpected error: {write_err:?}"
    );
    assert_eq!(bridge.active_text().as_deref(), Some("hello world"));

    // Grant editor_read + editor_write via plugin.toml and hot-reload.
    fs::write(
        tmp.path().join("restricted-editor/plugin.toml"),
        r#"id = "restricted-editor"
name = "Granted Editor"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"

[permissions]
editor_read = true
editor_write = true
"#,
    )
    .unwrap();
    let status = manager.reload_plugin("restricted-editor").unwrap();
    assert_eq!(status, PluginStatus::Active);

    manager
        .invoke_command("restricted-editor", "check-snapshot")
        .expect("editor_read now granted");
    manager
        .invoke_command("restricted-editor", "apply-edit")
        .expect("editor_write now granted");
    assert_eq!(bridge.active_text().as_deref(), Some("patched world"));
}

#[test]
fn interrupts_infinite_loop_via_epoch_without_hanging_host() {
    let tmp = TempDir::new().unwrap();
    let wasm = build_test_component_bytes("loop", false);
    write_plugin_bundle(
        tmp.path(),
        "infinite-loop",
        "[limits]\nexecution_timeout_ms = 60\n",
        &wasm,
    );

    let policy = SandboxPolicy {
        epoch_tick_interval: Duration::from_millis(5),
        ..SandboxPolicy::default()
    };
    let bridge = Arc::new(InMemoryEditorBridge::new());
    let mut manager =
        PluginManager::with_plugins_dir(tmp.path().to_path_buf(), policy, bridge).unwrap();

    let started = Instant::now();
    let outcomes = manager.load_all();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "epoch interruption took too long: {elapsed:?}"
    );
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcomes[0].result, Err(PluginHostError::Timeout { .. })),
        "expected Timeout error, got {:?}",
        outcomes[0].result
    );
    assert!(matches!(
        manager.plugin_status("infinite-loop"),
        Some(PluginStatus::Faulted { .. })
    ));
}

#[test]
fn isolates_guest_trap_to_faulted_without_panicking_host() {
    let tmp = TempDir::new().unwrap();
    let wasm_ok = build_test_component_bytes("ok", false);
    let wasm_trap = build_test_component_bytes("trap", false);
    write_plugin_bundle(tmp.path(), "healthy-plugin", "", &wasm_ok);
    write_plugin_bundle(tmp.path(), "trapping-plugin", "", &wasm_trap);

    let bridge = Arc::new(InMemoryEditorBridge::new());
    let mut manager =
        PluginManager::with_plugins_dir(tmp.path().to_path_buf(), SandboxPolicy::default(), bridge)
            .unwrap();
    let outcomes = manager.load_all();
    assert_eq!(outcomes.len(), 2);

    assert_eq!(
        manager.plugin_status("healthy-plugin"),
        Some(&PluginStatus::Active)
    );
    assert!(matches!(
        manager.plugin_status("trapping-plugin"),
        Some(PluginStatus::Faulted { .. })
    ));

    // Also test runtime trap during command invocation on an active plugin.
    let err = manager
        .invoke_command("healthy-plugin", "trap-now")
        .unwrap_err();
    assert!(matches!(err, PluginHostError::ExecutionTrap { .. }));
    assert!(matches!(
        manager.plugin_status("healthy-plugin"),
        Some(PluginStatus::Faulted { .. })
    ));
}

#[test]
fn enforces_linear_memory_limit_via_resource_limiter() {
    let tmp = TempDir::new().unwrap();
    let wasm = build_test_component_bytes("grow_fail", false);
    // Limit linear memory to 1 MiB; guest tries to grow by 4 MiB (64 pages) and fails.
    write_plugin_bundle(
        tmp.path(),
        "mem-capped",
        "[limits]\nmax_memory_mb = 1\n",
        &wasm,
    );

    let bridge = Arc::new(InMemoryEditorBridge::new());
    let mut manager =
        PluginManager::with_plugins_dir(tmp.path().to_path_buf(), SandboxPolicy::default(), bridge)
            .unwrap();
    let outcomes = manager.load_all();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(
            outcomes[0].result,
            Err(PluginHostError::PluginReturnedError { .. })
        ),
        "expected memory.grow to be denied by ResourceLimiter, got {:?}",
        outcomes[0].result
    );
}

#[test]
fn blocks_unauthorized_fs_and_allows_explicit_fs_grants() {
    let tmp = TempDir::new().unwrap();
    let workspace_dir = tmp.path().join("workspace");
    let forbidden_dir = tmp.path().join("secret");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::create_dir_all(&forbidden_dir).unwrap();

    let locked_down = PluginManifest::from_toml_str(
        r#"
id = "locked-down"
name = "Locked Down"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"
"#,
    )
    .unwrap();
    assert_eq!(
        resolve_fs_grants(&locked_down, Some(&workspace_dir)),
        Vec::new()
    );
    let wasi_denied = build_wasi_ctx(&locked_down, Some(&workspace_dir)).unwrap();
    let bridge = Arc::new(InMemoryEditorBridge::new());
    let mut denied_state = PluginStoreState::new(
        locked_down.id.clone(),
        locked_down.permissions.clone(),
        wasi_denied,
        PluginResourceLimiter::new(64 * 1024 * 1024, 1024),
        bridge.clone(),
        128,
        16,
    );
    assert!(
        PreopensHost::get_directories(&mut denied_state.filesystem())
            .unwrap()
            .is_empty(),
        "locked-down plugin must receive zero WASI preopened directories"
    );

    let granted = PluginManifest::from_toml_str(
        r#"
id = "granted"
name = "Granted"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"

[permissions]
fs_read = ["workspace"]
"#,
    )
    .unwrap();
    let grants = resolve_fs_grants(&granted, Some(&workspace_dir));
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].host_path, workspace_dir);
    assert_eq!(grants[0].guest_path, "/workspace");
    assert!(!grants[0].writable);

    let wasi_granted = build_wasi_ctx(&granted, Some(&workspace_dir)).unwrap();
    let mut granted_state = PluginStoreState::new(
        granted.id.clone(),
        granted.permissions.clone(),
        wasi_granted,
        PluginResourceLimiter::new(64 * 1024 * 1024, 1024),
        bridge,
        128,
        16,
    );
    let preopens = PreopensHost::get_directories(&mut granted_state.filesystem()).unwrap();
    assert_eq!(preopens.len(), 1);
    assert_eq!(preopens[0].1, "/workspace");
}

#[test]
fn blocks_unauthorized_network_and_allows_explicit_network_rules() {
    let locked_down = PluginManifest::from_toml_str(
        r#"
id = "locked-net"
name = "Locked Net"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"
"#,
    )
    .unwrap();
    let _wasi_denied = build_wasi_ctx(&locked_down, None).unwrap();
    let rules_empty = locked_down.parsed_network_rules().unwrap();
    assert!(!is_socket_addr_allowed(
        &rules_empty,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
    ));
    assert!(!is_network_host_allowed(
        &rules_empty,
        "example.com",
        Some(443)
    ));

    let granted = PluginManifest::from_toml_str(
        r#"
id = "granted-net"
name = "Granted Net"
version = "0.1.0"
entrypoint = "plugin.wasm"
api_version = "0.1.0"

[permissions]
network = ["api.example.com:443", "127.0.0.1:11434"]
"#,
    )
    .unwrap();
    let _wasi_granted = build_wasi_ctx(&granted, None).unwrap();
    let rules = granted.parsed_network_rules().unwrap();
    assert!(is_network_host_allowed(
        &rules,
        "api.example.com",
        Some(443)
    ));
    assert!(!is_network_host_allowed(
        &rules,
        "api.example.com",
        Some(80)
    ));
    assert!(!is_network_host_allowed(
        &rules,
        "evil.example.com",
        Some(443)
    ));
    assert!(is_socket_addr_allowed(
        &rules,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 11434)
    ));
    assert!(!is_socket_addr_allowed(
        &rules,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9999)
    ));
}

#[tokio::test]
async fn supports_async_discovery_disable_and_enable_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let wasm = build_test_component_bytes("ok", true);
    write_plugin_bundle(tmp.path(), "async-plugin", "", &wasm);

    let bridge = Arc::new(InMemoryEditorBridge::new());
    let (mut manager, outcomes) = PluginManager::load_all_async(
        tmp.path().to_path_buf(),
        None,
        SandboxPolicy::default(),
        bridge,
    )
    .await
    .unwrap();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        manager.plugin_status("async-plugin"),
        Some(&PluginStatus::Active)
    );

    manager.disable_plugin("async-plugin").unwrap();
    assert_eq!(
        manager.plugin_status("async-plugin"),
        Some(&PluginStatus::Disabled)
    );
    assert_eq!(manager.all_commands(), Vec::new());

    let err = manager
        .invoke_command("async-plugin", "demo.run")
        .unwrap_err();
    assert!(matches!(err, PluginHostError::PluginDisabled { .. }));

    let status = manager.enable_plugin("async-plugin").unwrap();
    assert_eq!(status, PluginStatus::Active);
    assert_eq!(manager.all_commands().len(), 1);
}
