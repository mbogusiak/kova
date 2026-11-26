//! Ergonomic, stateful plugin trait layered over the raw WIT [`Guest`] exports.
//!
//! The WIT `plugin` interface is a set of free functions: the component keeps
//! no `self` between calls. [`KovaPlugin`] hides that: the SDK keeps one
//! instance of the plugin type (created with [`Default`] on first use) and
//! forwards every export to a `&mut self` method. Commands and Tool Windows
//! returned by [`KovaPlugin::commands`] and [`KovaPlugin::tool_windows`] are
//! registered with the host automatically before [`KovaPlugin::init`] runs.
//!
//! A type implementing [`KovaPlugin`] implements [`Guest`] through a blanket
//! impl, so it is exported with the same [`export_plugin!`](crate::export_plugin)
//! macro as a hand-written [`Guest`] implementation.

use std::any::Any;
use std::cell::RefCell;

use crate::{
    CommandDescriptor, Guest, PluginEvent, SemanticIcon, ToolWindowDescriptor, UiEvent, WidgetTree,
    host,
};

/// A Kova WASM plugin with its own state.
///
/// Only [`KovaPlugin::render_ui`] is required; every other hook has a default.
/// Methods receive plain Rust types and return `Result<_, String>`; an `Err`
/// is reported by the host as a plugin error, without faulting the plugin.
///
/// ```
/// use kova_plugin_rs::prelude::*;
///
/// #[derive(Default)]
/// struct Counter {
///     clicks: u32,
/// }
///
/// impl KovaPlugin for Counter {
///     fn tool_windows(&self) -> Vec<ToolWindowDescriptor> {
///         vec![tool_window("counter.panel", "Counter", SemanticIcon::Plugin)]
///     }
///
///     fn render_ui(&mut self, _panel_id: &str) -> Result<WidgetTree, String> {
///         Ok(ui::form(Some("Counter"))
///             .id("counter.root")
///             .child(ui::badge(self.clicks.to_string()).id("counter.value"))
///             .child(ui::button("Add one", false).id("counter.add"))
///             .build())
///     }
///
///     fn on_ui_event(
///         &mut self,
///         panel_id: &str,
///         event: UiEvent,
///     ) -> Result<Option<WidgetTree>, String> {
///         if event.widget_id != "counter.add" {
///             return Ok(None);
///         }
///         self.clicks += 1;
///         self.render_ui(panel_id).map(Some)
///     }
/// }
///
/// export_plugin!(Counter);
/// # let mut counter = Counter::default();
/// # let tree = counter.render_ui("counter.panel").unwrap();
/// # assert!(ui::validate(&tree).is_ok());
/// ```
pub trait KovaPlugin: Default + 'static {
    /// Commands registered in the action palette before [`KovaPlugin::init`].
    fn commands(&self) -> Vec<CommandDescriptor> {
        Vec::new()
    }

    /// Tool Windows registered with the host before [`KovaPlugin::init`].
    fn tool_windows(&self) -> Vec<ToolWindowDescriptor> {
        Vec::new()
    }

    /// Called once after the commands and Tool Windows are registered.
    ///
    /// # Errors
    ///
    /// Returning an error makes the host mark the plugin as faulted.
    fn init(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Renders the declarative UI of the Tool Window `panel_id`.
    ///
    /// # Errors
    ///
    /// Return an error for an unknown `panel_id` or a failed render.
    fn render_ui(&mut self, panel_id: &str) -> Result<WidgetTree, String>;

    /// Handles a UI interaction; return `Some(tree)` to re-render the panel.
    ///
    /// # Errors
    ///
    /// Return an error when the event cannot be handled.
    fn on_ui_event(
        &mut self,
        panel_id: &str,
        event: UiEvent,
    ) -> Result<Option<WidgetTree>, String> {
        drop((panel_id, event));
        Ok(None)
    }

    /// Runs the command `command_id` registered by this plugin.
    ///
    /// # Errors
    ///
    /// The default implementation rejects every command as unknown.
    fn on_command(&mut self, command_id: &str) -> Result<(), String> {
        Err(format!("unknown command `{command_id}`"))
    }

    /// Reacts to an IDE lifecycle event (workspace opened, buffer saved, ...).
    ///
    /// # Errors
    ///
    /// Return an error when the event cannot be processed.
    fn on_event(&mut self, event: PluginEvent) -> Result<(), String> {
        drop(event);
        Ok(())
    }

    /// Called before the plugin is unloaded or disabled.
    ///
    /// The SDK drops the instance afterwards; a later call starts from
    /// [`Default::default`] again.
    ///
    /// # Errors
    ///
    /// Return an error when cleanup fails.
    fn shutdown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

thread_local! {
    /// The single plugin instance of this component (one component exports one plugin).
    static INSTANCE: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
}

/// Runs `f` on the plugin instance, creating it with [`Default`] when absent.
fn with_instance<T: KovaPlugin, R>(
    f: impl FnOnce(&mut T) -> Result<R, String>,
) -> Result<R, String> {
    INSTANCE.with(|slot| {
        let mut slot = slot
            .try_borrow_mut()
            .map_err(|_| "re-entrant call into the plugin instance".to_owned())?;
        let reuse = slot.as_ref().is_some_and(|boxed| boxed.is::<T>());
        if !reuse {
            *slot = Some(Box::new(T::default()));
        }
        let plugin = slot
            .as_mut()
            .and_then(|boxed| boxed.downcast_mut::<T>())
            .ok_or_else(|| "plugin instance has an unexpected type".to_owned())?;
        f(plugin)
    })
}

/// Drops the plugin instance (after `shutdown`).
fn drop_instance() {
    INSTANCE.with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut() {
            slot.take();
        }
    });
}

impl<T: KovaPlugin> Guest for T {
    fn init() -> Result<(), String> {
        with_instance(|plugin: &mut T| {
            for command in plugin.commands() {
                host::register_command(&command)?;
            }
            for panel in plugin.tool_windows() {
                host::register_tool_window(&panel)?;
            }
            plugin.init()
        })
    }

    fn shutdown() -> Result<(), String> {
        let result = with_instance(|plugin: &mut T| plugin.shutdown());
        drop_instance();
        result
    }

    fn on_event(event: PluginEvent) -> Result<(), String> {
        with_instance(|plugin: &mut T| plugin.on_event(event))
    }

    fn on_command(command_id: String) -> Result<(), String> {
        with_instance(|plugin: &mut T| plugin.on_command(&command_id))
    }

    fn render_ui(panel_id: String) -> Result<WidgetTree, String> {
        with_instance(|plugin: &mut T| plugin.render_ui(&panel_id))
    }

    fn on_ui_event(panel_id: String, event: UiEvent) -> Result<Option<WidgetTree>, String> {
        with_instance(|plugin: &mut T| plugin.on_ui_event(&panel_id, event))
    }
}

/// Builds a [`CommandDescriptor`] for [`KovaPlugin::commands`].
///
/// Prefix `id` with the plugin id (e.g. `"hello-kova.say-hello"`).
#[must_use]
pub fn command(id: &str, title: &str, category: &str) -> CommandDescriptor {
    CommandDescriptor {
        id: id.to_owned(),
        title: title.to_owned(),
        category: category.to_owned(),
    }
}

/// Builds a [`ToolWindowDescriptor`] for [`KovaPlugin::tool_windows`].
///
/// Prefix `id` with the plugin id (e.g. `"hello-kova.panel"`).
#[must_use]
pub fn tool_window(id: &str, title: &str, icon: SemanticIcon) -> ToolWindowDescriptor {
    ToolWindowDescriptor {
        id: id.to_owned(),
        title: title.to_owned(),
        icon,
    }
}

#[cfg(test)]
mod tests {
    use super::{Guest, KovaPlugin, command, tool_window, with_instance};
    use crate::ui::{self, WidgetKindExt};
    use crate::{PluginEvent, SemanticIcon, UiEvent, UiEventKind, WidgetTree};

    #[derive(Debug, Default)]
    struct Probe {
        events: u32,
        clicks: u32,
    }

    impl KovaPlugin for Probe {
        fn render_ui(&mut self, panel_id: &str) -> Result<WidgetTree, String> {
            if panel_id != "probe.panel" {
                return Err(format!("unknown panel `{panel_id}`"));
            }
            Ok(ui::badge(format!("{}/{}", self.events, self.clicks))
                .id("probe.badge")
                .build())
        }

        fn on_ui_event(
            &mut self,
            panel_id: &str,
            event: UiEvent,
        ) -> Result<Option<WidgetTree>, String> {
            if event.kind != UiEventKind::Clicked {
                return Ok(None);
            }
            self.clicks += 1;
            self.render_ui(panel_id).map(Some)
        }

        fn on_event(&mut self, _event: PluginEvent) -> Result<(), String> {
            self.events += 1;
            Ok(())
        }
    }

    fn badge_label(tree: &WidgetTree) -> String {
        match &tree.nodes[0].kind {
            crate::WidgetKind::Badge(props) => props.label.clone(),
            other => panic!("unexpected widget {other:?}"),
        }
    }

    #[test]
    fn guest_exports_share_one_stateful_instance() {
        <Probe as Guest>::on_event(PluginEvent::BufferSaved("a.kt".to_owned())).unwrap();
        let clicked = <Probe as Guest>::on_ui_event(
            "probe.panel".to_owned(),
            UiEvent {
                widget_id: "probe.badge".to_owned(),
                kind: UiEventKind::Clicked,
            },
        )
        .unwrap()
        .expect("click re-renders");
        assert_eq!(badge_label(&clicked), "1/1");

        let rendered = <Probe as Guest>::render_ui("probe.panel".to_owned()).unwrap();
        assert_eq!(badge_label(&rendered), "1/1");
        assert!(<Probe as Guest>::render_ui("other".to_owned()).is_err());

        // Default hooks: unknown commands are rejected, shutdown resets state.
        let err = <Probe as Guest>::on_command("probe.nope".to_owned()).unwrap_err();
        assert!(err.contains("probe.nope"), "{err}");
        <Probe as Guest>::shutdown().unwrap();
        let fresh = <Probe as Guest>::render_ui("probe.panel".to_owned()).unwrap();
        assert_eq!(badge_label(&fresh), "0/0");
    }

    #[test]
    fn re_entrant_access_is_reported_not_panicking() {
        let nested = with_instance(|_: &mut Probe| with_instance(|_: &mut Probe| Ok(())));
        assert!(nested.unwrap_err().contains("re-entrant"));
    }

    #[test]
    fn descriptor_helpers_fill_every_field() {
        let cmd = command("probe.run", "Run Probe", "Probe");
        assert_eq!(
            (cmd.id.as_str(), cmd.title.as_str(), cmd.category.as_str()),
            ("probe.run", "Run Probe", "Probe")
        );
        let panel = tool_window("probe.panel", "Probe", SemanticIcon::Bug);
        assert_eq!(panel.id, "probe.panel");
        assert_eq!(panel.icon, SemanticIcon::Bug);
        assert_eq!(Probe::default().commands().len(), 0);
        assert_eq!(Probe::default().tool_windows().len(), 0);
    }
}
