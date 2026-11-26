//! `hello_kova` — minimal example plugin for the `kova_plugin_rs` SDK (KOVA-046).
//!
//! One Tool Window with two tabs that together use every declarative widget
//! kind of WIT API `0.1`:
//!
//! - **Greet**: a form with a name input, a language select, a "loud" toggle,
//!   a greet button, a status badge, the greeting as markdown and a code block
//!   with the SDK snippet that exports this plugin;
//! - **Widgets**: a split with a tree of SDK concepts, a table of widget kinds
//!   and a list with the greeting history.
//!
//! Build it with `cargo build --release --target wasm32-wasip2 -p hello_kova`
//! and install `hello_kova.wasm` next to `plugin.toml` (see the SDK README).

use kova_plugin_rs::prelude::*;
use kova_plugin_rs::{ListItem, TreeItem};

/// Plugin identifier (must match `plugin.toml`).
pub const PLUGIN_ID: &str = "hello-kova";
/// The only Tool Window of this plugin.
pub const PANEL_ID: &str = "hello-kova.panel";
/// Command greeting the current name (or "Kova" when empty).
pub const SAY_HELLO: &str = "hello-kova.say-hello";
/// Command clearing the form and the history.
pub const RESET: &str = "hello-kova.reset";

/// Greeting languages offered by the select, in order.
const LANGUAGES: [&str; 3] = ["English", "Polski", "Deutsch"];

/// Snippet shown in the code block: the whole wiring of a plugin.
const SNIPPET: &str = "#[derive(Default)]\nstruct HelloKova { /* state */ }\n\n\
impl KovaPlugin for HelloKova {\n    fn render_ui(&mut self, panel_id: &str) -> Result<WidgetTree, String> {\n        /* ... */\n    }\n}\n\n\
export_plugin!(HelloKova);";

/// Static tree of SDK concepts: `(id, label, parent)`.
const CONCEPTS: [(&str, &str, Option<&str>); 7] = [
    ("sdk", "kova_plugin_rs", None),
    ("sdk/trait", "KovaPlugin", Some("sdk")),
    ("sdk/ui", "ui::Element", Some("sdk")),
    ("sdk/macro", "export_plugin!", Some("sdk")),
    ("host", "kova_plugin_host", None),
    ("host/manager", "PluginManager", Some("host")),
    ("host/sandbox", "Wasmtime sandbox", Some("host")),
];

/// Widget kinds and the UI events they emit (rows of the table).
const WIDGETS: [(&str, &str); 13] = [
    ("split", "-"),
    ("tabs", "tab-changed"),
    ("tree", "tree-toggled, tree-selected"),
    ("table", "row-selected"),
    ("list", "selected-index"),
    ("form", "submitted"),
    ("text-input", "text-changed, submitted"),
    ("select", "selected-index"),
    ("toggle", "toggled"),
    ("button", "clicked"),
    ("code-block", "-"),
    ("markdown", "-"),
    ("badge", "-"),
];

/// Builds the namespaced widget id `hello-kova.<suffix>`.
fn wid(suffix: &str) -> String {
    format!("{PLUGIN_ID}.{suffix}")
}

/// State of the example plugin (kept by the SDK between calls).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelloKova {
    name: String,
    language: usize,
    loud: bool,
    active_tab: u32,
    expanded: Vec<String>,
    selected_concept: Option<String>,
    selected_row: Option<u32>,
    history: Vec<(String, &'static str)>,
}

impl HelloKova {
    /// Greeting for `name` in the selected language.
    #[must_use]
    pub fn greeting(&self, name: &str) -> String {
        let base = match self.language {
            1 => format!("Cześć, {name}!"),
            2 => format!("Hallo, {name}!"),
            _ => format!("Hello, {name}!"),
        };
        if self.loud { base.to_uppercase() } else { base }
    }

    /// Records a greeting for the current name (or `fallback` when it is empty).
    fn greet(&mut self, fallback: &str) -> String {
        let name = if self.name.trim().is_empty() {
            fallback.to_owned()
        } else {
            self.name.trim().to_owned()
        };
        let text = self.greeting(&name);
        let language = LANGUAGES.get(self.language).copied().unwrap_or("English");
        self.history.push((text.clone(), language));
        text
    }

    fn greet_tab(&self) -> Element {
        let status = if self.history.is_empty() {
            ui::badge("No greetings yet")
                .id(wid("status"))
                .style(StyleVariant::Muted)
        } else {
            ui::badge(format!("{} greeting(s)", self.history.len()))
                .id(wid("status"))
                .style(StyleVariant::Accent)
        };
        let last = self.history.last().map_or_else(
            || "_Type a name and press **Greet**._".to_owned(),
            |(text, _)| format!("## {text}"),
        );
        let language = u32::try_from(self.language).ok();

        ui::form(Some("Say hello"))
            .id(wid("form"))
            .child(ui::text_input("Name", &self.name, "Your name").id(wid("name")))
            .child(ui::select("Language", &LANGUAGES, language).id(wid("language")))
            .child(ui::toggle("Loud", self.loud).id(wid("loud")))
            .child(ui::button("Greet", self.name.trim().is_empty()).id(wid("greet")))
            .child(status)
            .child(ui::markdown(last).id(wid("greeting")))
            .child(ui::code_block("rust", SNIPPET).id(wid("snippet")))
    }

    fn concept_items(&self) -> Vec<TreeItem> {
        CONCEPTS
            .iter()
            .filter(|(_, _, parent)| parent.is_none_or(|p| self.expanded.iter().any(|e| e == p)))
            .map(|(id, label, parent)| {
                let has_children = CONCEPTS.iter().any(|(_, _, p)| *p == Some(*id));
                let mut item = ui::tree_item(id, label, u32::from(parent.is_some()), has_children);
                item.expanded = self.expanded.iter().any(|e| e == id);
                item.selected = self.selected_concept.as_deref() == Some(*id);
                if parent.is_none() {
                    item.style = StyleVariant::Accent;
                }
                item
            })
            .collect()
    }

    fn widgets_tab(&self) -> Element {
        let rows = WIDGETS
            .iter()
            .map(|(kind, events)| vec![(*kind).to_owned(), (*events).to_owned()])
            .collect();
        let history: Vec<ListItem> = self
            .history
            .iter()
            .enumerate()
            .map(|(index, (text, language))| {
                ui::list_item(&format!("greeting-{index}"), text, Some(language))
            })
            .collect();
        let selected_history = history
            .len()
            .checked_sub(1)
            .and_then(|last| u32::try_from(last).ok());

        ui::split(SplitDirection::Horizontal, 35)
            .id(wid("widgets"))
            .child(ui::tree(self.concept_items()).id(wid("concepts")))
            .child(
                ui::split(SplitDirection::Vertical, 60)
                    .id(wid("details"))
                    .child(
                        ui::table(
                            vec![
                                ui::column("kind", "Widget", 1),
                                ui::column("events", "UI events", 2),
                            ],
                            rows,
                            self.selected_row,
                        )
                        .id(wid("kinds")),
                    )
                    .child(ui::list(history, selected_history).id(wid("history"))),
            )
    }

    /// Full panel tree.
    #[must_use]
    pub fn panel(&self) -> WidgetTree {
        ui::tabs(
            &[("greet", "Greet"), ("widgets", "Widgets")],
            self.active_tab,
        )
        .id(wid("tabs"))
        .child(self.greet_tab())
        .child(self.widgets_tab())
        .build()
    }

    fn toggle_concept(&mut self, id: &str) {
        if let Some(index) = self.expanded.iter().position(|e| e == id) {
            self.expanded.remove(index);
        } else {
            self.expanded.push(id.to_owned());
        }
    }

    /// Applies a UI event; returns whether the panel must be re-rendered.
    pub fn apply(&mut self, event: UiEvent) -> bool {
        let Some(target) = event.widget_id.strip_prefix("hello-kova.") else {
            return false;
        };
        match (target, event.kind) {
            ("name", UiEventKind::TextChanged(text)) => self.name = text,
            ("language", UiEventKind::SelectedIndex(index)) => {
                self.language = usize::try_from(index)
                    .ok()
                    .filter(|i| *i < LANGUAGES.len())
                    .unwrap_or(0);
            }
            ("loud", UiEventKind::Toggled(on)) => self.loud = on,
            ("greet", UiEventKind::Clicked) | ("name" | "form", UiEventKind::Submitted) => {
                if self.name.trim().is_empty() {
                    return false;
                }
                self.greet("Kova");
            }
            ("tabs", UiEventKind::TabChanged(index)) => self.active_tab = index.min(1),
            ("concepts", UiEventKind::TreeToggled(id)) => self.toggle_concept(&id),
            ("concepts", UiEventKind::TreeSelected(id)) => self.selected_concept = Some(id),
            ("kinds", UiEventKind::RowSelected(row)) => self.selected_row = Some(row),
            _ => return false,
        }
        true
    }
}

impl KovaPlugin for HelloKova {
    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![
            command(SAY_HELLO, "Say Hello", "Hello Kova"),
            command(RESET, "Reset Hello Kova", "Hello Kova"),
        ]
    }

    fn tool_windows(&self) -> Vec<ToolWindowDescriptor> {
        vec![tool_window(PANEL_ID, "Hello Kova", SemanticIcon::Plugin)]
    }

    fn init(&mut self) -> Result<(), String> {
        host::log(LogLevel::Info, "hello-kova ready");
        Ok(())
    }

    fn render_ui(&mut self, panel_id: &str) -> Result<WidgetTree, String> {
        if panel_id != PANEL_ID {
            return Err(format!("{PLUGIN_ID}: unknown tool window `{panel_id}`"));
        }
        Ok(self.panel())
    }

    fn on_ui_event(
        &mut self,
        panel_id: &str,
        event: UiEvent,
    ) -> Result<Option<WidgetTree>, String> {
        if panel_id != PANEL_ID {
            return Err(format!("{PLUGIN_ID}: unknown tool window `{panel_id}`"));
        }
        Ok(self.apply(event).then(|| self.panel()))
    }

    fn on_command(&mut self, command_id: &str) -> Result<(), String> {
        match command_id {
            SAY_HELLO => {
                let text = self.greet("Kova");
                host::log(LogLevel::Info, &text);
            }
            RESET => *self = Self::default(),
            other => return Err(format!("{PLUGIN_ID}: unknown command `{other}`")),
        }
        Ok(())
    }
}

export_plugin!(HelloKova);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, reason = "unit tests")]

    use super::{HelloKova, PANEL_ID, WIDGETS};
    use kova_plugin_rs::prelude::*;
    use kova_plugin_rs::ui::find_node;

    fn event(widget: &str, kind: UiEventKind) -> UiEvent {
        UiEvent {
            widget_id: format!("hello-kova.{widget}"),
            kind,
        }
    }

    #[test]
    fn panel_is_valid_and_uses_every_widget_kind() {
        let tree = HelloKova::default().render_ui(PANEL_ID).unwrap();
        ui::validate(&tree).unwrap();
        let mut kinds: Vec<&str> = tree
            .nodes
            .iter()
            .map(|node| match node.kind {
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
        let mut expected: Vec<&str> = WIDGETS.iter().map(|(kind, _)| *kind).collect();
        expected.sort_unstable();
        assert_eq!(kinds, expected);
        assert!(tree.nodes.iter().all(|n| n.id.starts_with("hello-kova.")));
    }

    #[test]
    fn greeting_follows_name_language_and_loudness() {
        let mut plugin = HelloKova::default();
        assert!(plugin.apply(event("name", UiEventKind::TextChanged("Ada".to_owned()))));
        assert!(plugin.apply(event("language", UiEventKind::SelectedIndex(1))));
        assert!(plugin.apply(event("loud", UiEventKind::Toggled(true))));
        let tree = plugin
            .on_ui_event(PANEL_ID, event("greet", UiEventKind::Clicked))
            .unwrap()
            .unwrap();
        match &find_node(&tree, "hello-kova.greeting").unwrap().kind {
            WidgetKind::Markdown(md) => assert_eq!(md.content, "## CZEŚĆ, ADA!"),
            other => panic!("unexpected {other:?}"),
        }
        let status = find_node(&tree, "hello-kova.status").unwrap();
        assert_eq!(status.style, StyleVariant::Accent);
    }

    #[test]
    fn empty_name_disables_greet_and_ignores_submit() {
        let mut plugin = HelloKova::default();
        let tree = plugin.render_ui(PANEL_ID).unwrap();
        match &find_node(&tree, "hello-kova.greet").unwrap().kind {
            WidgetKind::Button(button) => assert!(button.disabled),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!plugin.apply(event("name", UiEventKind::Submitted)));
        assert!(!plugin.apply(event("snippet", UiEventKind::Clicked)));
        assert!(
            plugin
                .on_ui_event(
                    PANEL_ID,
                    UiEvent {
                        widget_id: "other.widget".to_owned(),
                        kind: UiEventKind::Clicked,
                    },
                )
                .unwrap()
                .is_none()
        );
        assert!(plugin.render_ui("other.panel").is_err());
    }

    #[test]
    fn tree_expands_and_selects_concepts() {
        let mut plugin = HelloKova::default();
        let count = |plugin: &HelloKova| plugin.concept_items().len();
        assert_eq!(count(&plugin), 2);
        assert!(plugin.apply(event(
            "concepts",
            UiEventKind::TreeToggled("sdk".to_owned())
        )));
        assert_eq!(count(&plugin), 5);
        assert!(plugin.apply(event(
            "concepts",
            UiEventKind::TreeSelected("sdk/ui".to_owned())
        )));
        assert!(
            plugin
                .concept_items()
                .iter()
                .any(|i| i.id == "sdk/ui" && i.selected)
        );
        assert!(plugin.apply(event(
            "concepts",
            UiEventKind::TreeToggled("sdk".to_owned())
        )));
        assert_eq!(count(&plugin), 2);
        assert!(plugin.apply(event("tabs", UiEventKind::TabChanged(1))));
        assert!(plugin.apply(event("kinds", UiEventKind::RowSelected(3))));
        assert_eq!(plugin.selected_row, Some(3));
    }

    #[test]
    fn declares_namespaced_commands_and_panel() {
        let plugin = HelloKova::default();
        let ids: Vec<String> = plugin.commands().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["hello-kova.say-hello", "hello-kova.reset"]);
        assert_eq!(plugin.tool_windows()[0].id, PANEL_ID);
        assert!(HelloKova::default().on_command("hello-kova.reset").is_ok());
        assert!(HelloKova::default().on_command("nope").is_err());
    }
}
