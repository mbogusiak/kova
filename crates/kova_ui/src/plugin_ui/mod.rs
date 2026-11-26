//! Native GPUI declarative widget renderer for sandboxed WASM plugins (`KOVA-045`).
//!
//! Renders [`WidgetNode`] trees returned by [`kova_plugin_host`] at 120 FPS without any
//! `WebView` or HTML, mapping semantic [`kova_plugin_host::ui::StyleVariant`] tokens onto the
//! active [`KovaTheme`] and emitting [`PluginUiEvent`]s back to the plugin host.

pub mod code_block;
pub mod renderer;
pub mod virtual_table;

use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Context, EventEmitter, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, Styled as _, Task, Window, div,
};
use kova_plugin_host::ui::{
    PluginUiEvent, PluginUiEventSender, PluginUiViewHandle, UiEvent, WidgetNode, WidgetTreeStats,
    WidgetValidationError,
};

use crate::plugin_ui::renderer::{
    PluginActionCallback, PluginRendererAction, PluginUiState, render_widget_node,
};
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Optional synchronous callback invoked whenever a plugin widget emits a [`PluginUiEvent`].
pub type PluginUiEventSink = Arc<dyn Fn(&PluginUiEvent) + Send + Sync>;

/// Native GPUI tool-window view hosting a single WASM plugin's declarative [`WidgetNode`] tree.
pub struct PluginToolWindow {
    plugin_id: String,
    root: WidgetNode,
    state: PluginUiState,
    focus_handle: FocusHandle,
    event_tx: Option<PluginUiEventSender>,
    event_sink: Option<PluginUiEventSink>,
    tree_task: Option<Task<()>>,
}

impl fmt::Debug for PluginToolWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginToolWindow")
            .field("plugin_id", &self.plugin_id)
            .field("root_id", &self.root.id())
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<PluginUiEvent> for PluginToolWindow {}

impl PluginToolWindow {
    /// Creates a standalone [`PluginToolWindow`] displaying `initial_root`.
    #[must_use]
    pub fn new(
        plugin_id: impl Into<String>,
        initial_root: WidgetNode,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mut state = PluginUiState::default();
        state.sync_with_tree(&initial_root);
        Self {
            plugin_id: plugin_id.into(),
            root: initial_root,
            state,
            focus_handle: cx.focus_handle(),
            event_tx: None,
            event_sink: None,
            tree_task: None,
        }
    }

    /// Creates a [`PluginToolWindow`] bound to a bidirectional [`PluginUiViewHandle`]
    /// connected to `PluginManager`.
    ///
    /// Incoming [`WidgetNode`] updates from the plugin host wake this view asynchronously
    /// without polling, and user interactions flow back over [`PluginUiEventSender`].
    #[must_use]
    pub fn from_channel(
        plugin_id: impl Into<String>,
        initial_root: WidgetNode,
        handle: PluginUiViewHandle,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let (event_tx, mut tree_rx) = handle.split();
        let mut view = Self::new(plugin_id, initial_root, cx);
        view.event_tx = Some(event_tx);
        view.tree_task = Some(cx.spawn(async move |this, cx| {
            while let Some(tree) = tree_rx.recv_tree().await {
                let updated = this.update(cx, |view, cx| {
                    let _ignored = view.set_root(tree, cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        }));
        view
    }

    /// Attaches an optional synchronous callback receiving every emitted [`PluginUiEvent`].
    #[must_use]
    pub fn with_event_sink(mut self, sink: PluginUiEventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Returns the plugin instance identifier owning this tool window.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Returns the current declarative [`WidgetNode`] tree.
    #[must_use]
    pub const fn root(&self) -> &WidgetNode {
        &self.root
    }

    /// Returns the GPUI [`FocusHandle`] used for keyboard input inside plugin forms.
    #[must_use]
    pub const fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Validates and installs a new [`WidgetNode`] tree, triggering a GPUI re-render.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetValidationError`] if `root` violates host tree depth or size bounds.
    pub fn set_root(
        &mut self,
        root: WidgetNode,
        cx: &mut Context<'_, Self>,
    ) -> Result<WidgetTreeStats, WidgetValidationError> {
        let stats = root.validate()?;
        self.state.sync_with_tree(&root);
        self.root = root;
        cx.notify();
        Ok(stats)
    }

    fn handle_action(
        &mut self,
        action: PluginRendererAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if matches!(action, PluginRendererAction::FocusTextInput(_)) {
            window.focus(&self.focus_handle);
        }
        self.state.apply_action(&action);
        if let PluginRendererAction::Emit(event) = action {
            self.emit_ui_event(event, cx);
        } else {
            cx.notify();
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let keystroke = &event.keystroke;
        let command = keystroke.modifiers.platform || keystroke.modifiers.control;
        if let Some(ui_event) = self.state.handle_key_stroke(
            keystroke.key.as_str(),
            keystroke.key_char.as_deref(),
            command,
        ) {
            self.emit_ui_event(ui_event, cx);
        } else {
            cx.notify();
        }
    }

    /// Emits a [`UiEvent`] to the host channel, optional sink, and GPUI observers.
    pub fn emit_ui_event(&mut self, event: UiEvent, cx: &mut Context<'_, Self>) {
        let envelope = PluginUiEvent {
            plugin_id: self.plugin_id.clone(),
            event,
        };
        if let Some(tx) = &self.event_tx {
            let _ignored = tx.send_event(envelope.event.clone());
        }
        if let Some(sink) = &self.event_sink {
            sink(&envelope);
        }
        cx.emit(envelope);
        cx.notify();
    }
}

impl Render for PluginToolWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.state.prepare_for_tree(&self.root);
        let theme = cx.global::<KovaTheme>().clone();
        let bg = hsla_from_rgba(theme.surfaces().island);
        let fg = hsla_from_rgba(theme.text().primary);
        let on_action: PluginActionCallback = Rc::new(cx.listener(
            |this, action: &PluginRendererAction, window, cx| {
                this.handle_action(action.clone(), window, cx);
            },
        ));

        div()
            .size_full()
            .bg(bg)
            .text_color(fg)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(render_widget_node(
                &self.root,
                &self.state,
                &theme,
                &on_action,
            ))
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests assert declarative tree validation and bidirectional UI events"
)]
mod tests {
    use kova_plugin_host::ui::{
        BadgeWidget, ButtonWidget, CodeBlockWidget, FormControl, FormField, FormWidget, ListItem,
        ListWidget, MarkdownWidget, PluginUiChannel, SelectOption, SelectWidget, SplitDirection,
        SplitWidget, StyleVariant, TabNode, TableColumn, TableRow, TableWidget, TabsWidget,
        TextAlign, TextInputWidget, ToggleWidget, TreeNode, TreeWidget,
    };

    use super::*;

    #[allow(
        clippy::too_many_lines,
        reason = "test fixture exercising all 9 declarative WidgetNode types"
    )]
    fn full_sample_tree() -> WidgetNode {
        WidgetNode::Split(SplitWidget {
            id: "db-root".to_owned(),
            axis: SplitDirection::Horizontal,
            ratio: 0.28,
            first: Box::new(WidgetNode::Tree(TreeWidget {
                id: "db-tree".to_owned(),
                selected_id: None,
                nodes: vec![TreeNode {
                    id: "schema".to_owned(),
                    label: "public".to_owned(),
                    secondary: Some("2 tables".to_owned()),
                    badge: Some(BadgeWidget {
                        id: "pg".to_owned(),
                        label: "PG".to_owned(),
                        variant: StyleVariant::Accent,
                    }),
                    variant: StyleVariant::Primary,
                    expanded: true,
                    children: Vec::new(),
                }],
            })),
            second: Box::new(WidgetNode::Tabs(TabsWidget {
                id: "db-tabs".to_owned(),
                active_index: 0,
                variant: StyleVariant::Primary,
                tabs: vec![
                    TabNode {
                        id: "results".to_owned(),
                        title: "Results".to_owned(),
                        badge: None,
                        content: WidgetNode::Split(SplitWidget {
                            id: "query-area".to_owned(),
                            axis: SplitDirection::Vertical,
                            ratio: 0.4,
                            first: Box::new(WidgetNode::Form(FormWidget {
                                id: "params".to_owned(),
                                title: Some("Query".to_owned()),
                                variant: StyleVariant::Primary,
                                fields: vec![
                                    FormField {
                                        label: Some("SQL".to_owned()),
                                        hint: None,
                                        control: FormControl::TextInput(TextInputWidget {
                                            id: "sql-in".to_owned(),
                                            value: "SELECT 1".to_owned(),
                                            placeholder: "SQL...".to_owned(),
                                            disabled: false,
                                            variant: StyleVariant::Primary,
                                        }),
                                    },
                                    FormField {
                                        label: Some("Mode".to_owned()),
                                        hint: None,
                                        control: FormControl::Select(SelectWidget {
                                            id: "mode-sel".to_owned(),
                                            options: vec![SelectOption {
                                                value: "read".to_owned(),
                                                label: "Read-Only".to_owned(),
                                            }],
                                            selected_value: Some("read".to_owned()),
                                            disabled: false,
                                            variant: StyleVariant::Muted,
                                        }),
                                    },
                                    FormField {
                                        label: Some("Explain".to_owned()),
                                        hint: None,
                                        control: FormControl::Toggle(ToggleWidget {
                                            id: "explain".to_owned(),
                                            checked: false,
                                            disabled: false,
                                            variant: StyleVariant::Accent,
                                        }),
                                    },
                                    FormField {
                                        label: None,
                                        hint: None,
                                        control: FormControl::Button(ButtonWidget {
                                            id: "exec".to_owned(),
                                            label: "Execute".to_owned(),
                                            variant: StyleVariant::Primary,
                                            ghost: false,
                                            disabled: false,
                                        }),
                                    },
                                ],
                            })),
                            second: Box::new(WidgetNode::Table(TableWidget {
                                id: "rows".to_owned(),
                                columns: vec![TableColumn {
                                    id: "c1".to_owned(),
                                    header: "val".to_owned(),
                                    weight: 1,
                                    align: TextAlign::Right,
                                }],
                                rows: vec![TableRow::from_strings("r1", ["1"])],
                                selected_row: Some(0),
                                empty_message: None,
                            })),
                        }),
                    },
                    TabNode {
                        id: "history".to_owned(),
                        title: "History".to_owned(),
                        badge: None,
                        content: WidgetNode::List(ListWidget {
                            id: "hist".to_owned(),
                            items: vec![ListItem {
                                id: "h1".to_owned(),
                                title: "SELECT 1".to_owned(),
                                subtitle: None,
                                badge: None,
                                variant: StyleVariant::Muted,
                            }],
                            selected_index: None,
                            empty_message: None,
                        }),
                    },
                    TabNode {
                        id: "ddl".to_owned(),
                        title: "DDL".to_owned(),
                        badge: None,
                        content: WidgetNode::CodeBlock(CodeBlockWidget {
                            id: "ddl-code".to_owned(),
                            code: "{\"ok\": true}".to_owned(),
                            language: Some("json".to_owned()),
                            show_line_numbers: true,
                            variant: StyleVariant::Muted,
                        }),
                    },
                    TabNode {
                        id: "notes".to_owned(),
                        title: "Notes".to_owned(),
                        badge: None,
                        content: WidgetNode::Markdown(MarkdownWidget {
                            id: "md".to_owned(),
                            content: "# Notes\nReady.".to_owned(),
                            variant: StyleVariant::Accent,
                        }),
                    },
                ],
            })),
        })
    }

    #[test]
    fn prepare_for_tree_caches_code_blocks_and_validates_all_nine_components() {
        let tree = full_sample_tree();
        let stats = tree
            .validate()
            .expect("tree with all 9 components is valid");
        assert!(stats.node_count >= 9);

        let mut ui_state = PluginUiState::default();
        ui_state.prepare_for_tree(&tree);
        ui_state.sync_with_tree(&tree);
        assert_eq!(ui_state.focused_input(), None);
        assert_eq!(ui_state.open_select(), None);
    }

    #[test]
    fn channel_roundtrips_ui_events_and_updated_trees() {
        let (mut host, view) = PluginUiChannel::pair("kova-db");
        let (tx, mut rx) = view.split();
        let tree = full_sample_tree();
        host.send_tree(tree.clone()).expect("valid tree");
        assert_eq!(rx.drain_latest_tree(), Some(tree));

        tx.send_event(UiEvent::TabChanged {
            widget_id: "db-tabs".to_owned(),
            index: 1,
            tab_id: "history".to_owned(),
        })
        .expect("send event");
        assert_eq!(
            host.try_recv_event(),
            Some(PluginUiEvent {
                plugin_id: "kova-db".to_owned(),
                event: UiEvent::TabChanged {
                    widget_id: "db-tabs".to_owned(),
                    index: 1,
                    tab_id: "history".to_owned(),
                },
            })
        );
    }
}
