//! `kova-ai` — optional reference plugin with an AI chat Tool Window (KOVA-048).
//!
//! The plugin proves that AI features live entirely outside the Kova core:
//! it reads editor context only through the standard `get-editor-snapshot`
//! host API (`editor_read` permission) and describes its UI declaratively
//! (`Split`, `Markdown`, `CodeBlock`, `Form`, `Select`, `TextInput`, `Button`).
//!
//! Network access is declared in `plugin.toml` (local Ollama, `OpenAI` and
//! Anthropic endpoints), but WIT API `0.1` has no outgoing-request call yet,
//! so sending a prompt records it in the transcript together with the
//! provider and context it would be sent with; no model is called.

pub mod chat;

use std::sync::{Mutex, PoisonError};

use kova_plugin_rs::{
    CommandDescriptor, Guest, LogLevel, PluginEvent, SemanticIcon, ToolWindowDescriptor, UiEvent,
    WidgetTree, export_plugin, host,
};

use crate::chat::{CHAT_PANEL_ID, ChatState, EditorContext};

/// Plugin identifier (must match `plugin.toml`).
pub const PLUGIN_ID: &str = "kova-ai";
/// Command asking about the active file.
pub const CMD_ASK_ABOUT_FILE: &str = "kova-ai.ask-about-file";
/// Command clearing the conversation.
pub const CMD_CLEAR_CHAT: &str = "kova-ai.clear-chat";

static STATE: Mutex<ChatState> = Mutex::new(ChatState::new());

fn with_state<R>(f: impl FnOnce(&mut ChatState) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard)
}

/// Refreshes the chat context from the host editor snapshot.
fn refresh_context(state: &mut ChatState) {
    match host::get_editor_snapshot() {
        Ok(snapshot) => state.set_context(Some(EditorContext::from_snapshot(&snapshot))),
        Err(message) => state.set_context_error(message),
    }
}

/// Command and Tool Window descriptors registered during `init`.
#[must_use]
pub fn registrations() -> (Vec<CommandDescriptor>, ToolWindowDescriptor) {
    let commands = vec![
        CommandDescriptor {
            id: CMD_ASK_ABOUT_FILE.to_owned(),
            title: "AI: Ask About Current File".to_owned(),
            category: "AI".to_owned(),
        },
        CommandDescriptor {
            id: CMD_CLEAR_CHAT.to_owned(),
            title: "AI: Clear Chat".to_owned(),
            category: "AI".to_owned(),
        },
    ];
    let panel = ToolWindowDescriptor {
        id: CHAT_PANEL_ID.to_owned(),
        title: "AI Assistant".to_owned(),
        icon: SemanticIcon::Sparkle,
    };
    (commands, panel)
}

fn ensure_panel(panel_id: &str) -> Result<(), String> {
    if panel_id == CHAT_PANEL_ID {
        Ok(())
    } else {
        Err(format!("{PLUGIN_ID}: unknown tool window `{panel_id}`"))
    }
}

/// Component entrypoint exported to `kova_plugin_host`.
#[derive(Debug)]
pub struct KovaAi;

impl Guest for KovaAi {
    fn init() -> Result<(), String> {
        let (commands, panel) = registrations();
        for command in commands {
            host::register_command(&command)?;
        }
        host::register_tool_window(&panel)?;
        host::log(LogLevel::Info, "kova-ai ready (BYOK / Ollama / ACP)");
        Ok(())
    }

    fn shutdown() -> Result<(), String> {
        with_state(|state| *state = ChatState::new());
        Ok(())
    }

    fn on_event(event: PluginEvent) -> Result<(), String> {
        if matches!(
            event,
            PluginEvent::WorkspaceOpened(_) | PluginEvent::BufferChanged(_)
        ) {
            with_state(refresh_context);
        }
        Ok(())
    }

    fn on_command(command_id: String) -> Result<(), String> {
        match command_id.as_str() {
            CMD_ASK_ABOUT_FILE => with_state(|state| {
                refresh_context(state);
                state.ask_about_active_file();
            }),
            CMD_CLEAR_CHAT => with_state(ChatState::clear),
            other => return Err(format!("{PLUGIN_ID}: unknown command `{other}`")),
        }
        Ok(())
    }

    fn render_ui(panel_id: String) -> Result<WidgetTree, String> {
        ensure_panel(&panel_id)?;
        Ok(with_state(|state| {
            refresh_context(state);
            state.render()
        }))
    }

    fn on_ui_event(panel_id: String, event: UiEvent) -> Result<Option<WidgetTree>, String> {
        ensure_panel(&panel_id)?;
        Ok(with_state(|state| {
            state.handle_ui_event(&event).then(|| state.render())
        }))
    }
}

export_plugin!(KovaAi);

#[cfg(test)]
mod tests {
    use super::{CMD_ASK_ABOUT_FILE, PLUGIN_ID, ensure_panel, registrations};
    use crate::chat::CHAT_PANEL_ID;

    #[test]
    fn registers_namespaced_commands_and_panel() {
        let (commands, panel) = registrations();
        assert!(
            commands
                .iter()
                .all(|c| c.id.starts_with(&format!("{PLUGIN_ID}.")))
        );
        assert!(commands.iter().any(|c| c.id == CMD_ASK_ABOUT_FILE));
        assert_eq!(panel.id, CHAT_PANEL_ID);
    }

    #[test]
    fn rejects_unknown_panels() {
        assert!(ensure_panel(CHAT_PANEL_ID).is_ok());
        assert!(ensure_panel("other.panel").is_err());
    }

    #[test]
    fn manifest_declares_matching_id_and_network_allowlist() {
        let manifest = include_str!("../plugin.toml");
        assert!(manifest.contains(&format!("id          = \"{PLUGIN_ID}\"")));
        assert!(manifest.contains("editor_read   = true"));
        assert!(manifest.contains("localhost:11434"));
    }
}
