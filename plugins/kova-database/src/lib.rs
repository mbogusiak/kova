//! `kova-database` — reference plugin with a database explorer Tool Window (KOVA-048).
//!
//! Demonstrates the declarative widgets a database client needs while the
//! Kova core stays free of database code: a schema explorer (`Tree`), a query
//! editor (`Form` with `CodeBlock`, `TextInput` and `Button`) and a result
//! grid rendered by the host as a virtualized GPUI `Table`.
//!
//! The network allowlist in `plugin.toml` covers local `PostgreSQL`/`MySQL`,
//! but WIT API `0.1` has no socket call yet, so queries run against the
//! built-in sample catalog ([`catalog::SAMPLE_CATALOG`]).

pub mod catalog;
pub mod explorer;

use std::sync::{LazyLock, Mutex, PoisonError};

use kova_plugin_rs::{
    CommandDescriptor, Guest, LogLevel, PluginEvent, SemanticIcon, ToolWindowDescriptor, UiEvent,
    WidgetTree, export_plugin, host,
};

use crate::catalog::SAMPLE_CATALOG;
use crate::explorer::{EXPLORER_PANEL_ID, ExplorerState};

/// Plugin identifier (must match `plugin.toml`).
pub const PLUGIN_ID: &str = "kova-database";
/// Command executing the current query.
pub const CMD_RUN_QUERY: &str = "kova-database.run-query";
/// Command resetting the explorer.
pub const CMD_REFRESH: &str = "kova-database.refresh";

static STATE: LazyLock<Mutex<ExplorerState>> =
    LazyLock::new(|| Mutex::new(ExplorerState::new(SAMPLE_CATALOG)));

fn with_state<R>(f: impl FnOnce(&mut ExplorerState) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard)
}

/// Command and Tool Window descriptors registered during `init`.
#[must_use]
pub fn registrations() -> (Vec<CommandDescriptor>, ToolWindowDescriptor) {
    let commands = vec![
        CommandDescriptor {
            id: CMD_RUN_QUERY.to_owned(),
            title: "Database: Run Query".to_owned(),
            category: "Database".to_owned(),
        },
        CommandDescriptor {
            id: CMD_REFRESH.to_owned(),
            title: "Database: Refresh Explorer".to_owned(),
            category: "Database".to_owned(),
        },
    ];
    let panel = ToolWindowDescriptor {
        id: EXPLORER_PANEL_ID.to_owned(),
        title: "Database".to_owned(),
        icon: SemanticIcon::Database,
    };
    (commands, panel)
}

fn ensure_panel(panel_id: &str) -> Result<(), String> {
    if panel_id == EXPLORER_PANEL_ID {
        Ok(())
    } else {
        Err(format!("{PLUGIN_ID}: unknown tool window `{panel_id}`"))
    }
}

/// Component entrypoint exported to `kova_plugin_host`.
#[derive(Debug)]
pub struct KovaDatabase;

impl Guest for KovaDatabase {
    fn init() -> Result<(), String> {
        let (commands, panel) = registrations();
        for command in commands {
            host::register_command(&command)?;
        }
        host::register_tool_window(&panel)?;
        host::log(LogLevel::Info, "kova-database ready (sample catalog)");
        Ok(())
    }

    fn shutdown() -> Result<(), String> {
        with_state(|state| *state = ExplorerState::new(SAMPLE_CATALOG));
        Ok(())
    }

    fn on_event(_event: PluginEvent) -> Result<(), String> {
        Ok(())
    }

    fn on_command(command_id: String) -> Result<(), String> {
        match command_id.as_str() {
            CMD_RUN_QUERY => with_state(ExplorerState::run_query),
            CMD_REFRESH => with_state(|state| *state = ExplorerState::new(SAMPLE_CATALOG)),
            other => return Err(format!("{PLUGIN_ID}: unknown command `{other}`")),
        }
        Ok(())
    }

    fn render_ui(panel_id: String) -> Result<WidgetTree, String> {
        ensure_panel(&panel_id)?;
        Ok(with_state(|state| state.render()))
    }

    fn on_ui_event(panel_id: String, event: UiEvent) -> Result<Option<WidgetTree>, String> {
        ensure_panel(&panel_id)?;
        Ok(with_state(|state| {
            state.handle_ui_event(&event).then(|| state.render())
        }))
    }
}

export_plugin!(KovaDatabase);

#[cfg(test)]
mod tests {
    use super::{PLUGIN_ID, ensure_panel, registrations};
    use crate::explorer::EXPLORER_PANEL_ID;

    #[test]
    fn registers_namespaced_commands_and_panel() {
        let (commands, panel) = registrations();
        assert!(
            commands
                .iter()
                .all(|c| c.id.starts_with(&format!("{PLUGIN_ID}.")))
        );
        assert_eq!(panel.id, EXPLORER_PANEL_ID);
        assert!(ensure_panel(EXPLORER_PANEL_ID).is_ok());
        assert!(ensure_panel("kova-ai.chat").is_err());
    }

    #[test]
    fn manifest_allowlists_only_local_database_ports() {
        let manifest = include_str!("../plugin.toml");
        assert!(manifest.contains(&format!("id          = \"{PLUGIN_ID}\"")));
        assert!(manifest.contains(r#"network       = ["localhost:5432", "localhost:3306"]"#));
    }
}
