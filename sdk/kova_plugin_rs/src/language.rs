//! Data-only template for optional language packs (Kotlin LSP, Scala Metals, ...).
//!
//! WIT API `0.1` has no language-server registration call and the default
//! sandbox policy forbids `process_spawn`, so a language pack describes its
//! server as data. The pack detects matching buffers through the
//! `editor_read` snapshot, registers its commands and a status Tool Window,
//! and leaves launching the server to a host-side launcher in a later API.

use crate::ui::{self, UiBuilder};
use crate::{
    CommandDescriptor, ListItem, SemanticIcon, SplitDirection, StyleVariant, ToolWindowDescriptor,
    WidgetTree,
};

/// Static description of a language server shipped as an optional pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageServerDescriptor {
    /// Plugin id this descriptor belongs to (e.g. `"kova-kotlin-lsp"`).
    pub plugin_id: &'static str,
    /// Human-readable language name (e.g. `"Kotlin"`).
    pub language_name: &'static str,
    /// Language identifiers as reported by the editor snapshot (lowercase).
    pub language_ids: &'static [&'static str],
    /// File extensions (without the dot) handled by this server.
    pub file_extensions: &'static [&'static str],
    /// Human-readable server name (e.g. `"Kotlin LSP"`).
    pub server_name: &'static str,
    /// Executable and arguments the host launcher should start (stdio transport).
    pub server_command: &'static [&'static str],
    /// Build/workspace marker files that indicate a project for this language.
    pub workspace_markers: &'static [&'static str],
}

impl LanguageServerDescriptor {
    /// Id of the status Tool Window (`<plugin-id>.status`).
    #[must_use]
    pub fn tool_window_id(&self) -> String {
        format!("{}.status", self.plugin_id)
    }

    /// Tool Window registered by the pack.
    #[must_use]
    pub fn tool_window(&self) -> ToolWindowDescriptor {
        ToolWindowDescriptor {
            id: self.tool_window_id(),
            title: self.server_name.to_owned(),
            icon: SemanticIcon::Code,
        }
    }

    /// Commands registered by the pack (`restart-server`, `show-status`).
    #[must_use]
    pub fn commands(&self) -> Vec<CommandDescriptor> {
        vec![
            CommandDescriptor {
                id: format!("{}.restart-server", self.plugin_id),
                title: format!("Restart {}", self.server_name),
                category: self.language_name.to_owned(),
            },
            CommandDescriptor {
                id: format!("{}.show-status", self.plugin_id),
                title: format!("Show {} Status", self.server_name),
                category: self.language_name.to_owned(),
            },
        ]
    }

    /// Whether a buffer (by language id and/or path) belongs to this language.
    #[must_use]
    pub fn matches(&self, language: Option<&str>, path: Option<&str>) -> bool {
        let by_language = language.is_some_and(|lang| {
            self.language_ids
                .iter()
                .any(|id| id.eq_ignore_ascii_case(lang))
        });
        let by_extension = path
            .and_then(|p| p.rsplit_once('.'))
            .is_some_and(|(_, ext)| {
                self.file_extensions
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(ext))
            });
        by_language || by_extension
    }

    /// Shell-like rendering of [`Self::server_command`].
    #[must_use]
    pub fn command_line(&self) -> String {
        self.server_command.join(" ")
    }
}

/// Runtime status tracked by a language pack.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanguagePackState {
    /// Workspace root reported by the last `workspace-opened` event or snapshot.
    pub workspace_root: Option<String>,
    /// Last active file that matched this language.
    pub last_matching_file: Option<String>,
    /// Number of `restart-server` requests issued by the user.
    pub restart_requests: u32,
}

impl LanguagePackState {
    /// Records the active buffer if it matches `descriptor`; returns whether it matched.
    pub fn observe_buffer(
        &mut self,
        descriptor: &LanguageServerDescriptor,
        language: Option<&str>,
        path: Option<&str>,
    ) -> bool {
        let matched = descriptor.matches(language, path);
        if matched {
            self.last_matching_file = path.map(str::to_owned);
        }
        matched
    }
}

/// Renders the declarative status panel of a language pack.
#[must_use]
pub fn render_status_panel(
    descriptor: &LanguageServerDescriptor,
    state: &LanguagePackState,
) -> WidgetTree {
    let id = |suffix: &str| format!("{}.{suffix}", descriptor.plugin_id);
    let mut ui = UiBuilder::new();

    let (status_label, status_style) = if state.last_matching_file.is_some() {
        ("Ready to attach", StyleVariant::Accent)
    } else {
        ("Idle — no matching file opened", StyleVariant::Muted)
    };
    let status = ui.styled(id("state"), ui::badge(status_label), status_style);

    let summary = ui.leaf(
        id("summary"),
        ui::markdown(format!(
            "## {server}\n\nOptional {language} pack installed outside Kova core.\n\n\
             - Extensions: `{extensions}`\n\
             - Workspace root: `{root}`\n\
             - Active {language} file: `{file}`\n\
             - Restart requests: {restarts}",
            server = descriptor.server_name,
            language = descriptor.language_name,
            extensions = descriptor.file_extensions.join("`, `"),
            root = state.workspace_root.as_deref().unwrap_or("none"),
            file = state.last_matching_file.as_deref().unwrap_or("none"),
            restarts = state.restart_requests,
        )),
    );

    let launch = ui.leaf(
        id("launch"),
        ui::code_block("shell", &descriptor.command_line()),
    );

    let markers = ui.leaf(
        id("markers"),
        ui::list(
            descriptor
                .workspace_markers
                .iter()
                .map(|marker| ListItem {
                    id: (*marker).to_owned(),
                    label: (*marker).to_owned(),
                    detail: Some("workspace marker".to_owned()),
                    style: StyleVariant::Muted,
                })
                .collect(),
            None,
        ),
    );

    let restart = ui.leaf(
        id("restart"),
        ui::button(&format!("Restart {}", descriptor.server_name), false),
    );
    let actions = ui.container(id("actions"), ui::form(Some("Server")), &[restart]);

    let details = ui.container(
        id("details"),
        ui::split(SplitDirection::Vertical, 50),
        &[launch, markers],
    );
    let root = ui.container(
        id("root"),
        ui::form(Some(descriptor.server_name)),
        &[status, summary, details, actions],
    );
    ui.finish(root)
}

#[cfg(test)]
mod tests {
    use super::{LanguagePackState, LanguageServerDescriptor, render_status_panel};
    use crate::WidgetKind;
    use crate::ui::find_node;

    const DEMO: LanguageServerDescriptor = LanguageServerDescriptor {
        plugin_id: "demo-lang",
        language_name: "Demo",
        language_ids: &["demo"],
        file_extensions: &["dm", "dms"],
        server_name: "Demo LS",
        server_command: &["demo-ls", "--stdio"],
        workspace_markers: &["demo.toml"],
    };

    #[test]
    fn matches_by_language_or_extension() {
        assert!(DEMO.matches(Some("DEMO"), None));
        assert!(DEMO.matches(None, Some("src/a.dms")));
        assert!(!DEMO.matches(Some("java"), Some("Main.java")));
        assert!(!DEMO.matches(None, None));
    }

    #[test]
    fn exposes_namespaced_commands_and_tool_window() {
        let ids: Vec<String> = DEMO.commands().into_iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec!["demo-lang.restart-server", "demo-lang.show-status"]
        );
        assert_eq!(DEMO.tool_window().id, "demo-lang.status");
        assert_eq!(DEMO.command_line(), "demo-ls --stdio");
    }

    #[test]
    fn status_panel_reflects_observed_buffer() {
        let mut state = LanguagePackState::default();
        assert!(!state.observe_buffer(&DEMO, Some("java"), Some("Main.java")));
        let idle = render_status_panel(&DEMO, &state);
        assert!(matches!(
            find_node(&idle, "demo-lang.state").map(|n| &n.kind),
            Some(WidgetKind::Badge(b)) if b.label.starts_with("Idle")
        ));

        assert!(state.observe_buffer(&DEMO, None, Some("src/a.dm")));
        let ready = render_status_panel(&DEMO, &state);
        assert!(matches!(
            find_node(&ready, "demo-lang.state").map(|n| &n.kind),
            Some(WidgetKind::Badge(b)) if b.label == "Ready to attach"
        ));
        assert_eq!(
            ready.nodes.last().map(|n| n.id.as_str()),
            Some("demo-lang.root")
        );
    }
}
