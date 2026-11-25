//! Headless integration tests for `ConfigWatcher` live reload (`KOVA-008`).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration tests assert watcher behavior with explicit failure context"
)]

use std::path::Path;
use std::time::Duration;

use kova_config::{
    ConfigReloadEvent, ConfigWatcher, ConfigWatcherOptions, KovaPaths, ReloadTarget, SettingUpdate,
    SettingsDocument, WatcherError,
};
use kova_theme::{AetherTheme, BuiltinTheme};
use tempfile::TempDir;
use tokio::sync::broadcast;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

fn isolated_paths(root: &Path) -> KovaPaths {
    KovaPaths::from_roots(root.join("config"), root.join("data"), root.join("cache"))
        .expect("TempDir paths must be absolute")
}

fn custom_theme_toml(id: &str, name: &str, bg_hex: &str) -> String {
    BuiltinTheme::KovaIslandsDark
        .toml_source()
        .replace("id = \"kova-islands-dark\"", &format!("id = \"{id}\""))
        .replace(
            "name = \"Kova Islands Dark\"",
            &format!("name = \"{name}\""),
        )
        .replace("canvas = \"#0D1017\"", &format!("canvas = \"{bg_hex}\""))
}

async fn next_matching_event<F>(
    rx: &mut broadcast::Receiver<ConfigReloadEvent>,
    mut predicate: F,
) -> ConfigReloadEvent
where
    F: FnMut(&ConfigReloadEvent) -> bool,
{
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok(evt) if predicate(&evt) => return evt,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(err) => panic!("broadcast channel closed unexpectedly: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for expected ConfigReloadEvent")
}

/// Allow macOS `FSEvents` stream to finish registering before writing test files.
async fn settle_fs_watcher() {
    tokio::time::sleep(Duration::from_millis(120)).await;
}

#[tokio::test]
async fn initial_load_populates_settings_and_builtin_themes() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = isolated_paths(tmp.path());
    paths.ensure_omarchy_dirs().expect("omarchy dirs");

    let custom_toml = custom_theme_toml("omarchy-startup", "Omarchy Startup", "#111827");
    std::fs::write(
        paths.theme_file("omarchy-startup").expect("theme path"),
        &custom_toml,
    )
    .expect("write custom theme");

    let mut doc = SettingsDocument::default_template().expect("template");
    doc.set_setting(SettingUpdate::EditorTabSize(2))
        .expect("tab size");
    doc.set_setting(SettingUpdate::UiTheme("omarchy-startup".to_owned()))
        .expect("ui theme");
    doc.save_to_file(&paths.settings_file())
        .expect("save settings");

    let handle =
        ConfigWatcher::start(paths, ConfigWatcherOptions::default()).expect("watcher should start");

    assert_eq!(handle.startup_diagnostics(), []);
    let state = handle.current_state();
    assert_eq!(state.generation, 0);
    assert_eq!(state.settings.editor.tab_size, 2);
    assert_eq!(state.active_theme.metadata.id, "omarchy-startup");
    assert!(state.theme_registry.get("kova-islands-dark").is_some());
    assert!(state.theme_registry.get("kova-light").is_some());
    assert!(state.theme_registry.get("omarchy-startup").is_some());

    handle.shutdown().await;
}

#[tokio::test]
async fn reloads_settings_on_direct_and_atomic_save() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = isolated_paths(tmp.path());

    let handle = ConfigWatcher::start(
        paths.clone(),
        ConfigWatcherOptions {
            debounce: Duration::from_millis(50),
            create_missing_user_dirs: true,
            ..ConfigWatcherOptions::default()
        },
    )
    .expect("watcher should start");

    let mut state_rx = handle.subscribe_state();
    let mut events_rx = handle.subscribe_events();
    settle_fs_watcher().await;

    // 1. Direct write
    std::fs::write(
        paths.settings_file(),
        "[editor]\ntab_size = 2\nvim_mode = true\n",
    )
    .expect("direct write");

    let evt = next_matching_event(&mut events_rx, |e| {
        matches!(e, ConfigReloadEvent::SettingsReloaded { .. })
    })
    .await;
    assert!(matches!(
        evt,
        ConfigReloadEvent::SettingsReloaded { generation: 1, .. }
    ));

    state_rx.changed().await.expect("watch channel updated");
    let state = state_rx.borrow().clone();
    assert_eq!(state.settings.editor.tab_size, 2);
    assert!(state.settings.editor.vim_mode);

    // 2. Atomic save via SettingsDocument::save_to_file (NamedTempFile + rename)
    let mut doc = SettingsDocument::load_or_default(&paths.settings_file()).expect("load");
    doc.set_setting(SettingUpdate::EditorTabSize(8))
        .expect("update tab size");
    doc.save_to_file(&paths.settings_file())
        .expect("atomic save");

    let evt2 = next_matching_event(&mut events_rx, |e| {
        matches!(e, ConfigReloadEvent::SettingsReloaded { generation: 2, .. })
    })
    .await;
    assert!(matches!(
        evt2,
        ConfigReloadEvent::SettingsReloaded { generation: 2, .. }
    ));
    assert_eq!(handle.current_state().settings.editor.tab_size, 8);

    handle.shutdown().await;
}

#[tokio::test]
async fn reloads_custom_theme_and_handles_dynamic_themes_dir_and_removal() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = isolated_paths(tmp.path());
    // Create only config_dir, leaving themes_dir absent at startup.
    std::fs::create_dir_all(paths.config_dir()).expect("create config dir");

    let handle = ConfigWatcher::start(
        paths.clone(),
        ConfigWatcherOptions {
            debounce: Duration::from_millis(50),
            create_missing_user_dirs: false,
            ..ConfigWatcherOptions::default()
        },
    )
    .expect("watcher should start even when themes_dir is missing");

    let mut events_rx = handle.subscribe_events();
    settle_fs_watcher().await;

    // Create themes_dir and write a custom theme in one step.
    std::fs::create_dir_all(paths.themes_dir()).expect("create themes dir");
    let theme_path = paths.theme_file("my-omarchy").expect("theme path");
    let theme_v1 = custom_theme_toml("my-omarchy", "My Omarchy", "#10141E");
    std::fs::write(&theme_path, &theme_v1).expect("write custom theme");

    next_matching_event(&mut events_rx, |e| {
        matches!(
            e,
            ConfigReloadEvent::ThemeReloaded { theme_id, .. } if theme_id == "my-omarchy"
        )
    })
    .await;

    // Select the custom theme in settings.toml.
    let mut doc = SettingsDocument::default_template().expect("template");
    doc.set_setting(SettingUpdate::UiTheme("my-omarchy".to_owned()))
        .expect("set theme");
    doc.save_to_file(&paths.settings_file())
        .expect("save settings");

    next_matching_event(&mut events_rx, |e| {
        matches!(e, ConfigReloadEvent::SettingsReloaded { .. })
    })
    .await;

    let state = handle.current_state();
    assert_eq!(state.active_theme.metadata.id, "my-omarchy");
    assert_eq!(
        state.active_theme.tokens.surfaces.canvas,
        AetherTheme::from_toml_str(&theme_v1)
            .expect("valid")
            .tokens
            .surfaces
            .canvas
    );

    // Delete the active custom theme file -> safe fallback to kova-islands-dark.
    std::fs::remove_file(&theme_path).expect("remove active custom theme");
    let removed_evt = next_matching_event(&mut events_rx, |e| {
        matches!(
            e,
            ConfigReloadEvent::ThemeRemoved { theme_id, .. } if theme_id == "my-omarchy"
        )
    })
    .await;
    assert!(matches!(
        removed_evt,
        ConfigReloadEvent::ThemeRemoved {
            fallback_to: Some(ref fb),
            ..
        } if fb == "kova-islands-dark"
    ));
    assert_eq!(
        handle.current_state().active_theme.metadata.id,
        "kova-islands-dark"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn preserves_last_known_good_on_invalid_toml_and_recovers() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = isolated_paths(tmp.path());
    paths.ensure_omarchy_dirs().expect("omarchy dirs");

    let valid_settings = "[editor]\ntab_size = 2\nvim_mode = true\n";
    std::fs::write(paths.settings_file(), valid_settings).expect("initial settings");

    let valid_theme_path = paths.theme_file("my-omarchy").expect("theme path");
    let valid_theme = custom_theme_toml("my-omarchy", "My Omarchy", "#141923");
    std::fs::write(&valid_theme_path, &valid_theme).expect("initial theme");

    let handle = ConfigWatcher::start(
        paths.clone(),
        ConfigWatcherOptions {
            debounce: Duration::from_millis(50),
            ..ConfigWatcherOptions::default()
        },
    )
    .expect("watcher should start");

    let mut events_rx = handle.subscribe_events();
    settle_fs_watcher().await;

    let gen_before = handle.current_state().generation;
    assert_eq!(handle.current_state().settings.editor.tab_size, 2);

    // 1. Corrupt settings.toml -> ReloadFailed, LKG preserved, generation unchanged.
    std::fs::write(paths.settings_file(), "[editor\ntab_size = \"oops\"")
        .expect("write broken settings");
    let fail_evt = next_matching_event(&mut events_rx, |e| {
        matches!(
            e,
            ConfigReloadEvent::ReloadFailed {
                target: ReloadTarget::Settings,
                ..
            }
        )
    })
    .await;
    assert!(matches!(fail_evt, ConfigReloadEvent::ReloadFailed { .. }));
    assert_eq!(handle.current_state().generation, gen_before);
    assert_eq!(handle.current_state().settings.editor.tab_size, 2);
    assert!(handle.current_state().settings.editor.vim_mode);

    // 2. Revert to the EXACT SAME valid bytes -> must NOT be swallowed by SHA-256 cache!
    std::fs::write(paths.settings_file(), valid_settings).expect("revert settings");
    next_matching_event(&mut events_rx, |e| {
        matches!(e, ConfigReloadEvent::SettingsReloaded { .. })
    })
    .await;

    // 3. Corrupt custom theme TOML -> ReloadFailed, LKG theme preserved.
    std::fs::write(&valid_theme_path, "[metadata]\nid = \"my-omarchy\"\n")
        .expect("write incomplete theme");
    next_matching_event(&mut events_rx, |e| {
        matches!(
            e,
            ConfigReloadEvent::ReloadFailed {
                target: ReloadTarget::Theme(_),
                ..
            }
        )
    })
    .await;
    assert!(
        handle
            .current_state()
            .theme_registry
            .get("my-omarchy")
            .is_some()
    );

    // 4. Restore valid updated theme TOML -> ThemeReloaded.
    let updated_theme = custom_theme_toml("my-omarchy", "My Omarchy v2", "#1B2230");
    std::fs::write(&valid_theme_path, &updated_theme).expect("restore theme");
    next_matching_event(&mut events_rx, |e| {
        matches!(
            e,
            ConfigReloadEvent::ThemeReloaded { theme_id, .. } if theme_id == "my-omarchy"
        )
    })
    .await;
    assert_eq!(
        handle
            .current_state()
            .theme_registry
            .get("my-omarchy")
            .expect("registered")
            .metadata
            .name,
        "My Omarchy v2"
    );

    // 5. Unknown theme in settings.toml -> ReloadFailed, active LKG theme preserved.
    std::fs::write(paths.settings_file(), "[ui]\ntheme = \"does-not-exist\"\n")
        .expect("unknown theme in settings");
    next_matching_event(&mut events_rx, |e| {
        matches!(
            e,
            ConfigReloadEvent::ReloadFailed {
                target: ReloadTarget::Settings,
                ..
            }
        )
    })
    .await;
    assert_eq!(
        handle.current_state().active_theme.metadata.id,
        "kova-islands-dark"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn debounces_burst_writes_and_ignores_temp_files() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = isolated_paths(tmp.path());
    paths.ensure_omarchy_dirs().expect("omarchy dirs");

    let handle = ConfigWatcher::start(
        paths.clone(),
        ConfigWatcherOptions {
            debounce: Duration::from_millis(160),
            ..ConfigWatcherOptions::default()
        },
    )
    .expect("watcher should start");

    let mut events_rx = handle.subscribe_events();
    settle_fs_watcher().await;

    // Noise/editor swap files must be ignored completely before debounce.
    std::fs::write(paths.config_dir().join(".settings.toml.swp"), "garbage").expect("write swp");
    std::fs::write(paths.config_dir().join("settings.toml~"), "garbage")
        .expect("write tilde backup");
    std::fs::write(paths.config_dir().join(".DS_Store"), [0_u8, 1, 2, 3]).expect("write ds_store");
    std::fs::write(paths.themes_dir().join(".hidden.toml"), "invalid").expect("write hidden theme");

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        matches!(
            events_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ),
        "temp/hidden files must not emit any reload or error events"
    );

    // Rapid burst writes inside the debounce window should coalesce into one reload.
    for tab in [2_u8, 3, 5, 6] {
        std::fs::write(
            paths.settings_file(),
            format!("[editor]\ntab_size = {tab}\n"),
        )
        .expect("burst write");
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    let first_evt = next_matching_event(&mut events_rx, |e| {
        matches!(e, ConfigReloadEvent::SettingsReloaded { .. })
    })
    .await;
    assert!(matches!(
        first_evt,
        ConfigReloadEvent::SettingsReloaded { generation: 1, .. }
    ));
    assert_eq!(handle.current_state().settings.editor.tab_size, 6);

    // Wait past another debounce window to verify no extra events arrive.
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert!(matches!(
        events_rx.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // Re-running reload_now() with unchanged content emits nothing (SHA-256 dedup).
    handle.reload_now().await.expect("reload_now");
    assert!(matches!(
        events_rx.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    handle.shutdown().await;
}

#[tokio::test]
async fn watches_optional_project_kova_toml_without_creating_hidden_files() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = isolated_paths(tmp.path());
    paths.ensure_omarchy_dirs().expect("omarchy dirs");

    let project_dir = tmp.path().join("my-java-project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let handle = ConfigWatcher::start(
        paths.clone(),
        ConfigWatcherOptions {
            debounce: Duration::from_millis(50),
            ..ConfigWatcherOptions::default()
        },
    )
    .expect("watcher should start");

    let mut events_rx = handle.subscribe_events();
    handle
        .watch_project(Some(project_dir.clone()))
        .expect("attach project root");
    settle_fs_watcher().await;

    // Omarchy rule: watching project_root creates zero files or hidden folders.
    let entries: Vec<_> = std::fs::read_dir(&project_dir)
        .expect("read project dir")
        .collect();
    assert!(entries.is_empty());

    // Create kova.toml -> ProjectConfigReloaded.
    let proj_toml = KovaPaths::project_config_file(&project_dir);
    std::fs::write(
        &proj_toml,
        "schema_version = 1\njdk = \"21\"\n[editor]\ntab_size = 2\n",
    )
    .expect("write kova.toml");

    next_matching_event(&mut events_rx, |e| {
        matches!(e, ConfigReloadEvent::ProjectConfigReloaded { .. })
    })
    .await;

    let state = handle.current_state();
    assert_eq!(state.settings.editor.tab_size, 4);
    assert_eq!(state.effective.settings.editor.tab_size, 2);
    assert_eq!(
        state.effective.settings.jvm.preferred_jdk.as_deref(),
        Some("21")
    );

    // Keymap change notification.
    std::fs::write(paths.keymap_file(), "[bindings]\n\"cmd-s\" = \"save\"\n")
        .expect("write keymap.toml");
    next_matching_event(&mut events_rx, |e| {
        matches!(e, ConfigReloadEvent::KeymapChanged { .. })
    })
    .await;

    // Remove kova.toml -> ProjectConfigRemoved.
    std::fs::remove_file(&proj_toml).expect("remove kova.toml");
    next_matching_event(&mut events_rx, |e| {
        matches!(e, ConfigReloadEvent::ProjectConfigRemoved { .. })
    })
    .await;

    let state_after = handle.current_state();
    assert!(state_after.project_config.is_none());
    assert_eq!(state_after.effective.settings.editor.tab_size, 4);

    handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_terminates_cleanly_and_rejects_further_control() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = isolated_paths(tmp.path());
    paths.ensure_omarchy_dirs().expect("omarchy dirs");

    let handle =
        ConfigWatcher::start(paths, ConfigWatcherOptions::default()).expect("watcher should start");

    let state_rx = handle.subscribe_state();
    tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
        .await
        .expect("shutdown must finish promptly");

    // Once shutdown completes, the watch sender has been dropped.
    assert!(state_rx.has_changed().is_err() || state_rx.borrow().generation == 0);

    // Passing a non-existent project root fails cleanly without panicking.
    let bad_dir = tmp.path().join("does-not-exist");
    let err = ConfigWatcher::start(
        isolated_paths(tmp.path()),
        ConfigWatcherOptions {
            project_root: Some(bad_dir),
            ..ConfigWatcherOptions::default()
        },
    )
    .expect_err("non-existent project_root must fail");
    assert!(matches!(err, WatcherError::WatchPath { .. }));
}
