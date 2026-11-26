//! Terminal tabs in the bottom dock: open (`Cmd+T`), close (`Cmd+W`, `×`, shell `exit`) and
//! rename (double-click a tab).

use std::collections::HashMap;

use gpui::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, FontWeight,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};

use super::terminal_view::{TerminalLaunch, TerminalView, TerminalViewEvent};
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Height of the terminal tab strip in logical pixels.
pub const TERMINAL_TAB_STRIP_HEIGHT_PX: f32 = 26.0;

/// Stable identifier of a terminal tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TerminalTabId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
struct TabEntry {
    id: TerminalTabId,
    ordinal: u64,
    custom_title: Option<String>,
}

/// Headless tab list of the terminal dock (order, active tab, titles).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalTabs {
    entries: Vec<TabEntry>,
    active: Option<TerminalTabId>,
    next_id: u64,
}

impl TerminalTabs {
    /// Appends a new tab and makes it active.
    pub fn add(&mut self) -> TerminalTabId {
        self.next_id += 1;
        let id = TerminalTabId(self.next_id);
        let ordinal = self.entries.iter().map(|e| e.ordinal).max().unwrap_or(0) + 1;
        self.entries.push(TabEntry {
            id,
            ordinal,
            custom_title: None,
        });
        self.active = Some(id);
        id
    }

    /// Removes `id`; activates its right neighbour (or the new last tab) if it was active.
    /// Returns whether the tab existed.
    pub fn close(&mut self, id: TerminalTabId) -> bool {
        let Some(index) = self.entries.iter().position(|e| e.id == id) else {
            return false;
        };
        self.entries.remove(index);
        if self.active == Some(id) {
            self.active = self
                .entries
                .get(index)
                .or_else(|| self.entries.last())
                .map(|e| e.id);
        }
        true
    }

    /// Activates `id` if it exists.
    pub fn select(&mut self, id: TerminalTabId) {
        if self.entries.iter().any(|e| e.id == id) {
            self.active = Some(id);
        }
    }

    /// Renames `id`; a blank name restores the default title.
    pub fn rename(&mut self, id: TerminalTabId, name: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            let name = name.trim();
            entry.custom_title = (!name.is_empty()).then(|| name.to_owned());
        }
    }

    /// Title shown on the tab: the custom name, else `Local`, `Local (2)`, ….
    #[must_use]
    pub fn title(&self, id: TerminalTabId) -> Option<String> {
        self.entries.iter().find(|e| e.id == id).map(|e| {
            e.custom_title.clone().unwrap_or_else(|| {
                if e.ordinal == 1 {
                    "Local".to_owned()
                } else {
                    format!("Local ({})", e.ordinal)
                }
            })
        })
    }

    /// The active tab.
    #[must_use]
    pub const fn active(&self) -> Option<TerminalTabId> {
        self.active
    }

    /// Tab ids in display order.
    pub fn ids(&self) -> impl Iterator<Item = TerminalTabId> + '_ {
        self.entries.iter().map(|e| e.id)
    }

    /// Number of open tabs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no tab is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Events emitted by the [`TerminalDock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalDockEvent {
    /// The last tab was closed; the workspace hides the terminal.
    AllClosed,
}

/// Multi-tab terminal container shown in the bottom panel.
pub struct TerminalDock {
    tabs: TerminalTabs,
    views: HashMap<TerminalTabId, (Entity<TerminalView>, Subscription)>,
    launch: TerminalLaunch,
    focus_handle: FocusHandle,
    renaming: Option<(TerminalTabId, String)>,
}

impl std::fmt::Debug for TerminalDock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalDock")
            .field("tabs", &self.tabs)
            .field("renaming", &self.renaming)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<TerminalDockEvent> for TerminalDock {}

impl TerminalDock {
    /// Creates the dock with one session started from `launch`.
    pub fn new(launch: TerminalLaunch, cx: &mut Context<'_, Self>) -> Self {
        let mut dock = Self {
            tabs: TerminalTabs::default(),
            views: HashMap::new(),
            launch,
            focus_handle: cx.focus_handle(),
            renaming: None,
        };
        dock.spawn_tab(cx);
        dock
    }

    fn spawn_tab(&mut self, cx: &mut Context<'_, Self>) -> TerminalTabId {
        let id = self.tabs.add();
        let launch = self.launch.clone();
        let view = cx.new(|cx| TerminalView::new(&launch, cx));
        let subscription = cx.subscribe(&view, move |dock, _view, event, cx| match event {
            TerminalViewEvent::Exited => dock.close_tab(id, cx),
        });
        self.views.insert(id, (view, subscription));
        id
    }

    /// The headless tab model.
    #[must_use]
    pub const fn tabs(&self) -> &TerminalTabs {
        &self.tabs
    }

    /// View of the active tab.
    #[must_use]
    pub fn active_view(&self) -> Option<&Entity<TerminalView>> {
        self.tabs
            .active()
            .and_then(|id| self.views.get(&id))
            .map(|(view, _)| view)
    }

    /// Whether keyboard focus is inside the terminal (a session or the rename field).
    #[must_use]
    pub fn contains_focus(&self, window: &Window, cx: &gpui::App) -> bool {
        self.focus_handle.is_focused(window)
            || self
                .views
                .values()
                .any(|(view, _)| view.read(cx).focus_handle().is_focused(window))
    }

    /// Focuses the active session (or the dock when renaming).
    pub fn focus_active(&self, window: &mut Window, cx: &gpui::App) {
        if self.renaming.is_some() {
            window.focus(&self.focus_handle);
        } else if let Some(view) = self.active_view() {
            window.focus(view.read(cx).focus_handle());
        }
    }

    /// Opens a new tab and focuses it (`Cmd+T`).
    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.renaming = None;
        self.spawn_tab(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Closes the active tab (`Cmd+W`).
    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(id) = self.tabs.active() {
            self.close_tab(id, cx);
            self.focus_active(window, cx);
        }
    }

    /// Closes `id`, terminating its shell (the session's `Drop` sends `SIGHUP`).
    pub fn close_tab(&mut self, id: TerminalTabId, cx: &mut Context<'_, Self>) {
        if self.renaming.as_ref().is_some_and(|(r, _)| *r == id) {
            self.renaming = None;
        }
        if self.tabs.close(id) {
            self.views.remove(&id);
            if self.tabs.is_empty() {
                cx.emit(TerminalDockEvent::AllClosed);
            }
            cx.notify();
        }
    }

    /// Activates tab `id` and focuses it.
    pub fn select_tab(
        &mut self,
        id: TerminalTabId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.tabs.select(id);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Starts renaming tab `id` in place.
    pub fn start_rename(
        &mut self,
        id: TerminalTabId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.tabs.select(id);
        let current = self.tabs.title(id).unwrap_or_default();
        self.renaming = Some((id, current));
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Applies new launch options (settings change) to future tabs and the font to open ones.
    pub fn set_launch(&mut self, launch: TerminalLaunch, cx: &mut Context<'_, Self>) {
        let font_changed = launch.font_family != self.launch.font_family
            || (launch.font_size - self.launch.font_size).abs() > f32::EPSILON;
        if font_changed {
            for (view, _) in self.views.values() {
                view.update(cx, |view, cx| {
                    view.set_font(&launch.font_family, launch.font_size, cx);
                });
            }
        }
        self.launch = launch;
        cx.notify();
    }

    /// Runs `f` on the active session view.
    pub fn update_active<R>(
        &self,
        cx: &mut Context<'_, Self>,
        f: impl FnOnce(&mut TerminalView, &mut Context<'_, TerminalView>) -> R,
    ) -> Option<R> {
        let view = self.active_view()?.clone();
        Some(view.update(cx, f))
    }

    fn handle_rename_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((id, buffer)) = self.renaming.as_mut() else {
            return;
        };
        let keystroke = &event.keystroke;
        let m = &keystroke.modifiers;
        if m.platform || m.control {
            return;
        }
        match keystroke.key.as_str() {
            "enter" => {
                let (id, name) = (*id, buffer.clone());
                self.tabs.rename(id, &name);
                self.renaming = None;
                self.focus_active(window, cx);
            }
            "escape" => {
                self.renaming = None;
                self.focus_active(window, cx);
            }
            "backspace" => {
                buffer.pop();
            }
            _ => match keystroke.key_char.as_deref() {
                Some(text) if !text.chars().any(char::is_control) => buffer.push_str(text),
                _ => return,
            },
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for TerminalDock {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.global::<KovaTheme>();
        let text_primary = hsla_from_rgba(theme.text().primary);
        let text_muted = hsla_from_rgba(theme.text().muted);
        let hover = hsla_from_rgba(theme.surfaces().hover);
        let border = hsla_from_rgba(theme.borders().subtle);
        let indicator = hsla_from_rgba(theme.borders().tab_indicator);
        let input_bg = hsla_from_rgba(theme.surfaces().input);
        let active = self.tabs.active();

        let tabs: Vec<_> = self
            .tabs
            .ids()
            .map(|id| {
                let is_active = Some(id) == active;
                let label = match &self.renaming {
                    Some((rid, buffer)) if *rid == id => format!("{buffer}▏"),
                    _ => self.tabs.title(id).unwrap_or_default(),
                };
                let renaming = self.renaming.as_ref().is_some_and(|(rid, _)| *rid == id);
                let mut tab = div()
                    .id(SharedString::from(format!("terminal-tab-{}", id.0)))
                    .h_full()
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .hover(|s| s.bg(hover))
                    .on_click(cx.listener(move |dock, event: &ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            dock.start_rename(id, window, cx);
                        } else {
                            dock.select_tab(id, window, cx);
                        }
                    }));
                tab = if is_active {
                    tab.text_color(text_primary)
                        .font_weight(FontWeight::SEMIBOLD)
                        .border_b_2()
                        .border_color(indicator)
                } else {
                    tab.text_color(text_muted)
                };
                if renaming {
                    tab = tab.bg(input_bg);
                }
                tab.child(label).child(
                    div()
                        .id(SharedString::from(format!("terminal-tab-close-{}", id.0)))
                        .px(px(3.0))
                        .rounded(px(3.0))
                        .text_color(text_muted)
                        .hover(|s| s.text_color(text_primary).bg(hover))
                        .on_click(cx.listener(move |dock, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            dock.close_tab(id, cx);
                            dock.focus_active(window, cx);
                        }))
                        .child("×"),
                )
            })
            .collect();

        let new_tab = div()
            .id("terminal-new-tab")
            .px(px(8.0))
            .h_full()
            .flex()
            .items_center()
            .text_size(px(14.0))
            .text_color(text_muted)
            .hover(|s| s.text_color(text_primary).bg(hover))
            .on_click(cx.listener(|dock, _: &ClickEvent, window, cx| dock.new_tab(window, cx)))
            .child("+");

        let body = self
            .active_view()
            .map(|view| div().flex_1().min_h(px(0.0)).child(view.clone()));

        div()
            .id("terminal-dock")
            .key_context("TerminalDock")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_rename_key))
            .size_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(TERMINAL_TAB_STRIP_HEIGHT_PX))
                    .flex_none()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(border)
                    .children(tabs)
                    .child(new_tab),
            )
            .children(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_open_close_and_reactivate_neighbours() {
        let mut tabs = TerminalTabs::default();
        let a = tabs.add();
        let b = tabs.add();
        let c = tabs.add();
        assert_eq!(tabs.active(), Some(c));
        assert_eq!(tabs.title(a).as_deref(), Some("Local"));
        assert_eq!(tabs.title(b).as_deref(), Some("Local (2)"));
        tabs.select(b);
        assert!(tabs.close(b));
        assert_eq!(tabs.active(), Some(c), "right neighbour becomes active");
        assert!(tabs.close(c));
        assert_eq!(tabs.active(), Some(a), "last tab becomes active");
        assert!(!tabs.close(c), "closing twice is a no-op");
        assert!(tabs.close(a));
        assert!(tabs.is_empty());
        assert_eq!(tabs.active(), None);
    }

    #[test]
    fn rename_and_reset_title() {
        let mut tabs = TerminalTabs::default();
        let id = tabs.add();
        tabs.rename(id, "  gradle  ");
        assert_eq!(tabs.title(id).as_deref(), Some("gradle"));
        tabs.rename(id, " ");
        assert_eq!(tabs.title(id).as_deref(), Some("Local"));
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs.ids().collect::<Vec<_>>(), vec![id]);
    }

    #[test]
    fn ordinals_follow_the_highest_open_tab() {
        let mut tabs = TerminalTabs::default();
        let a = tabs.add();
        let b = tabs.add();
        tabs.close(b);
        let c = tabs.add();
        assert_eq!(tabs.title(c).as_deref(), Some("Local (2)"));
        tabs.close(a);
        tabs.close(c);
        let d = tabs.add();
        assert_eq!(tabs.title(d).as_deref(), Some("Local"));
    }
}
