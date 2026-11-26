//! `kova-scala-metals` — optional Scala language pack adapter template (KOVA-048).
//!
//! Registers Scala commands and a status Tool Window and tracks Scala
//! buffers via the `editor_read` snapshot. The Metals process is described
//! as data ([`DESCRIPTOR`]); WIT API `0.1` has no language-server
//! registration call and the default sandbox policy forbids `process_spawn`,
//! so nothing is launched from the plugin.

use std::sync::{Mutex, PoisonError};

use kova_plugin_rs::language::{LanguagePackState, LanguageServerDescriptor, render_status_panel};
use kova_plugin_rs::{
    Guest, LogLevel, PluginEvent, UiEvent, UiEventKind, WidgetTree, export_plugin, host,
};

/// Plugin identifier (must match `plugin.toml`).
pub const PLUGIN_ID: &str = "kova-scala-metals";

/// Metals server description consumed by the host launcher.
pub const DESCRIPTOR: LanguageServerDescriptor = LanguageServerDescriptor {
    plugin_id: PLUGIN_ID,
    language_name: "Scala",
    language_ids: &["scala"],
    file_extensions: &["scala", "sc", "sbt"],
    server_name: "Metals",
    server_command: &["metals"],
    workspace_markers: &["build.sbt", "build.sc", ".bsp"],
};

static STATE: Mutex<LanguagePackState> = Mutex::new(LanguagePackState {
    workspace_root: None,
    last_matching_file: None,
    restart_requests: 0,
});

fn with_state<R>(f: impl FnOnce(&mut LanguagePackState) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard)
}

fn refresh_from_snapshot(state: &mut LanguagePackState) {
    if let Ok(snapshot) = host::get_editor_snapshot() {
        if snapshot.workspace_root.is_some() {
            state.workspace_root = snapshot.workspace_root;
        }
        state.observe_buffer(
            &DESCRIPTOR,
            snapshot.language.as_deref(),
            snapshot.active_file.as_deref(),
        );
    }
}

fn request_restart(state: &mut LanguagePackState) {
    state.restart_requests = state.restart_requests.saturating_add(1);
    host::log(
        LogLevel::Info,
        &format!("restart requested: {}", DESCRIPTOR.command_line()),
    );
}

/// Whether a UI event is a click on the status panel restart button.
#[must_use]
pub fn is_restart_click(event: &UiEvent) -> bool {
    event.widget_id == format!("{PLUGIN_ID}.restart") && event.kind == UiEventKind::Clicked
}

fn ensure_panel(panel_id: &str) -> Result<(), String> {
    if panel_id == DESCRIPTOR.tool_window_id() {
        Ok(())
    } else {
        Err(format!("{PLUGIN_ID}: unknown tool window `{panel_id}`"))
    }
}

/// Component entrypoint exported to `kova_plugin_host`.
#[derive(Debug)]
pub struct KovaScalaMetals;

impl Guest for KovaScalaMetals {
    fn init() -> Result<(), String> {
        for command in DESCRIPTOR.commands() {
            host::register_command(&command)?;
        }
        host::register_tool_window(&DESCRIPTOR.tool_window())?;
        host::log(LogLevel::Info, "kova-scala-metals ready");
        Ok(())
    }

    fn shutdown() -> Result<(), String> {
        with_state(|state| *state = LanguagePackState::default());
        Ok(())
    }

    fn on_event(event: PluginEvent) -> Result<(), String> {
        with_state(|state| match event {
            PluginEvent::WorkspaceOpened(root) => state.workspace_root = Some(root),
            PluginEvent::BufferChanged(path) | PluginEvent::BufferSaved(path) => {
                state.observe_buffer(&DESCRIPTOR, None, Some(&path));
            }
            PluginEvent::CursorMoved(_) | PluginEvent::ThemeChanged(_) => {}
        });
        Ok(())
    }

    fn on_command(command_id: String) -> Result<(), String> {
        let restart = format!("{PLUGIN_ID}.restart-server");
        let status = format!("{PLUGIN_ID}.show-status");
        if command_id == restart {
            with_state(request_restart);
        } else if command_id == status {
            with_state(refresh_from_snapshot);
        } else {
            return Err(format!("{PLUGIN_ID}: unknown command `{command_id}`"));
        }
        Ok(())
    }

    fn render_ui(panel_id: String) -> Result<WidgetTree, String> {
        ensure_panel(&panel_id)?;
        Ok(with_state(|state| {
            refresh_from_snapshot(state);
            render_status_panel(&DESCRIPTOR, state)
        }))
    }

    fn on_ui_event(panel_id: String, event: UiEvent) -> Result<Option<WidgetTree>, String> {
        ensure_panel(&panel_id)?;
        if !is_restart_click(&event) {
            return Ok(None);
        }
        Ok(Some(with_state(|state| {
            request_restart(state);
            render_status_panel(&DESCRIPTOR, state)
        })))
    }
}

export_plugin!(KovaScalaMetals);

#[cfg(test)]
mod tests {
    use kova_plugin_rs::{UiEvent, UiEventKind};

    use super::{DESCRIPTOR, PLUGIN_ID, ensure_panel, is_restart_click};

    #[test]
    fn descriptor_matches_scala_files_only() {
        assert!(DESCRIPTOR.matches(None, Some("src/main/scala/App.scala")));
        assert!(DESCRIPTOR.matches(Some("scala"), None));
        assert!(!DESCRIPTOR.matches(Some("java"), Some("src/Main.java")));
    }

    #[test]
    fn panel_and_restart_button_are_namespaced() {
        assert!(ensure_panel(&format!("{PLUGIN_ID}.status")).is_ok());
        assert!(ensure_panel("kova-ai.chat").is_err());
        let click = UiEvent {
            widget_id: format!("{PLUGIN_ID}.restart"),
            kind: UiEventKind::Clicked,
        };
        assert!(is_restart_click(&click));
    }

    #[test]
    fn manifest_keeps_process_spawn_disabled() {
        let manifest = include_str!("../plugin.toml");
        assert!(manifest.contains(&format!("id          = \"{PLUGIN_ID}\"")));
        assert!(manifest.contains("process_spawn = false"));
    }
}
