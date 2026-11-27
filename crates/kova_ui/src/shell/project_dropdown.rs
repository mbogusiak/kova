//! Titlebar project dropdown with `Recent Projects` (`KOVA-065`, `IntelliJ` New UI).
//!
//! Clicking the titlebar project pill (`<name> ▾`) opens a floating popover anchored right
//! under the pill. It lists every remembered project (name, `~`-abbreviated path, checkmark on
//! the project open in this window) followed by `Open Project…`, `Open in New Window…` and
//! `Clear Recent Projects`.
//!
//! [`ProjectDropdownModel`] is pure headless state (rows, highlight, keyboard navigation) so it
//! is unit-tested without a GPUI window. Activating an entry dispatches the same process-level
//! actions as the native `File` menu, so opening a recent project runs the full `KOVA-054`
//! flow (activate another window showing it, *This Window / New Window*, unsaved changes).

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    Bounds, ClickEvent, Context, Div, FontWeight, InteractiveElement as _, Keystroke, MouseButton,
    MouseDownEvent, ParentElement as _, Pixels, Stateful, StatefulInteractiveElement as _,
    Styled as _, Window, canvas, div, px,
};
use kova_core::RecentProject;

use super::project::canonicalize_or_raw;
use super::{Palette, TITLEBAR_LEFT_PADDING_PX, WorkspaceShell};
use crate::app_menu::{
    ClearRecentProjects, OpenProject, OpenProjectInNewWindow, OpenRecentProject,
};
use crate::dock::focus::TITLEBAR_HEIGHT_PX;
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Preferred width of the dropdown card in logical pixels.
pub const PROJECT_DROPDOWN_WIDTH_PX: f32 = 380.0;
/// Height of one recent-project row (name + path).
const RECENT_ROW_HEIGHT_PX: f32 = 40.0;
/// Height of one action row.
const ACTION_ROW_HEIGHT_PX: f32 = 28.0;
/// Gap between the pill's bottom edge and the dropdown card.
const ANCHOR_GAP_PX: f32 = 4.0;
/// Minimum distance kept between the card and the window edges.
const VIEWPORT_MARGIN_PX: f32 = 8.0;
/// Label of the section header.
pub const RECENT_PROJECTS_HEADER: &str = "Recent Projects";
/// Label shown when no project was opened yet.
pub const NO_RECENT_PROJECTS_PLACEHOLDER: &str = "No Recent Projects";

/// Command triggered by one dropdown entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectDropdownAction {
    /// Open this recent project (full `KOVA-054` routing).
    OpenRecent(PathBuf),
    /// `Open Project…`: native directory picker, then the same routing.
    OpenProject,
    /// `Open in New Window…`: native directory picker, then a new window.
    OpenInNewWindow,
    /// `Clear Recent Projects`.
    ClearRecent,
}

impl ProjectDropdownAction {
    /// Row label of a non-project action (`None` for [`Self::OpenRecent`]).
    #[must_use]
    pub const fn label(&self) -> Option<&'static str> {
        match self {
            Self::OpenRecent(_) => None,
            Self::OpenProject => Some("Open Project…"),
            Self::OpenInNewWindow => Some("Open in New Window…"),
            Self::ClearRecent => Some("Clear Recent Projects"),
        }
    }

    /// Leading glyph of a non-project action row.
    #[must_use]
    pub const fn glyph(&self) -> &'static str {
        match self {
            Self::OpenProject | Self::OpenInNewWindow => "+",
            Self::OpenRecent(_) | Self::ClearRecent => "",
        }
    }
}

/// One project row of the `Recent Projects` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDropdownRow {
    /// Project display name.
    pub name: String,
    /// Canonical project root.
    pub root: PathBuf,
    /// Path shown dimmed under the name (`~/…` when inside the home directory).
    pub display_path: String,
    /// Whether this is the project open in the current window.
    pub is_current: bool,
}

/// Result of feeding one key to [`ProjectDropdownModel::handle_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectDropdownKeyOutcome {
    /// `Esc`: close the dropdown.
    Close,
    /// `Enter` on the highlighted entry.
    Activate(ProjectDropdownAction),
    /// Navigation key handled; repaint.
    Consumed,
    /// Not a dropdown key.
    Ignored,
}

/// Headless state of the titlebar project dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDropdownModel {
    rows: Vec<ProjectDropdownRow>,
    actions: Vec<ProjectDropdownAction>,
    highlighted: Option<usize>,
}

impl ProjectDropdownModel {
    /// Builds the dropdown from `recent` (most recent first).
    ///
    /// `current_root` is the canonical root of the project open in this window and `home`
    /// the user's home directory used to abbreviate paths.
    #[must_use]
    pub fn new(recent: &[RecentProject], current_root: Option<&Path>, home: Option<&Path>) -> Self {
        let rows = recent
            .iter()
            .map(|project| ProjectDropdownRow {
                name: project.name.clone(),
                root: project.root.clone(),
                display_path: abbreviate_home(&project.root, home),
                is_current: current_root.is_some_and(|current| current == project.root),
            })
            .collect::<Vec<_>>();
        let mut actions = vec![
            ProjectDropdownAction::OpenProject,
            ProjectDropdownAction::OpenInNewWindow,
        ];
        if !rows.is_empty() {
            actions.push(ProjectDropdownAction::ClearRecent);
        }
        Self {
            rows,
            actions,
            highlighted: None,
        }
    }

    /// Recent-project rows in display order.
    #[must_use]
    pub fn rows(&self) -> &[ProjectDropdownRow] {
        &self.rows
    }

    /// Non-project actions shown below the separator.
    #[must_use]
    pub fn actions(&self) -> &[ProjectDropdownAction] {
        &self.actions
    }

    /// Number of selectable entries (rows followed by actions).
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.rows.len() + self.actions.len()
    }

    /// Action of the selectable entry at `index` (rows first, then actions).
    #[must_use]
    pub fn entry_action(&self, index: usize) -> Option<ProjectDropdownAction> {
        match self.rows.get(index) {
            Some(row) => Some(ProjectDropdownAction::OpenRecent(row.root.clone())),
            None => self.actions.get(index - self.rows.len()).cloned(),
        }
    }

    /// Keyboard-highlighted entry index.
    #[must_use]
    pub const fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    /// Highlights entry `index` (ignored when out of range).
    pub fn set_highlighted(&mut self, index: Option<usize>) {
        self.highlighted = index.filter(|i| *i < self.entry_count());
    }

    /// Moves the highlight one entry down, wrapping to the top.
    pub fn move_down(&mut self) {
        let count = self.entry_count();
        if count == 0 {
            return;
        }
        self.highlighted = Some(self.highlighted.map_or(0, |i| (i + 1) % count));
    }

    /// Moves the highlight one entry up, wrapping to the bottom.
    pub fn move_up(&mut self) {
        let count = self.entry_count();
        if count == 0 {
            return;
        }
        self.highlighted = Some(match self.highlighted {
            Some(0) | None => count - 1,
            Some(i) => i - 1,
        });
    }

    /// Handles `Esc`, `Up`, `Down` and `Enter` (unmodified).
    pub fn handle_key(&mut self, keystroke: &Keystroke) -> ProjectDropdownKeyOutcome {
        let mods = &keystroke.modifiers;
        if mods.control || mods.platform || mods.alt {
            return ProjectDropdownKeyOutcome::Ignored;
        }
        match keystroke.key.as_str() {
            "escape" => ProjectDropdownKeyOutcome::Close,
            "down" => {
                self.move_down();
                ProjectDropdownKeyOutcome::Consumed
            }
            "up" => {
                self.move_up();
                ProjectDropdownKeyOutcome::Consumed
            }
            "enter" => self.highlighted.and_then(|i| self.entry_action(i)).map_or(
                ProjectDropdownKeyOutcome::Consumed,
                ProjectDropdownKeyOutcome::Activate,
            ),
            _ => ProjectDropdownKeyOutcome::Ignored,
        }
    }
}

/// Shows `path` relative to `home` as `~/…` (or `~`), otherwise the full path.
#[must_use]
pub fn abbreviate_home(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Top-left origin and width of the card anchored under `pill` inside `viewport`.
#[must_use]
pub fn dropdown_origin(pill: Option<Bounds<Pixels>>, viewport: (f32, f32)) -> (f32, f32, f32) {
    let (anchor_x, anchor_y) = pill.map_or((TITLEBAR_LEFT_PADDING_PX, TITLEBAR_HEIGHT_PX), |b| {
        (f32::from(b.origin.x), f32::from(b.bottom()))
    });
    let width = PROJECT_DROPDOWN_WIDTH_PX.min((viewport.0 - 2.0 * VIEWPORT_MARGIN_PX).max(160.0));
    let max_x = (viewport.0 - width - VIEWPORT_MARGIN_PX).max(VIEWPORT_MARGIN_PX);
    (
        anchor_x.clamp(VIEWPORT_MARGIN_PX, max_x),
        anchor_y + ANCHOR_GAP_PX,
        width,
    )
}

/// Home directory used to abbreviate project paths.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Open dropdown: model plus the card bounds captured at paint (outside-click detection).
#[derive(Debug)]
pub(super) struct ProjectDropdownState {
    pub(super) model: ProjectDropdownModel,
    pub(super) bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl WorkspaceShell {
    /// Replaces the `Recent Projects` list shown by the titlebar dropdown (`KOVA-065`).
    ///
    /// Called by the host at window creation and on every list update; an open dropdown is
    /// rebuilt in place (keeping the highlight when still in range).
    pub fn set_recent_projects(&mut self, recent: Vec<RecentProject>, cx: &mut Context<'_, Self>) {
        self.recent_projects = recent;
        if let Some(state) = self.project_dropdown.as_mut() {
            let highlighted = state.model.highlighted();
            state.model = build_model(&self.recent_projects, self.project.root());
            state.model.set_highlighted(highlighted);
        }
        cx.notify();
    }

    /// Latest `Recent Projects` list delivered by the host.
    #[must_use]
    pub fn recent_projects(&self) -> &[RecentProject] {
        &self.recent_projects
    }

    /// State of the open titlebar project dropdown, if any.
    #[must_use]
    pub fn project_dropdown(&self) -> Option<&ProjectDropdownModel> {
        self.project_dropdown.as_ref().map(|state| &state.model)
    }

    /// Opens the dropdown when closed, closes it when open (titlebar pill click).
    pub fn toggle_project_dropdown(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.project_dropdown.is_some() {
            self.close_project_dropdown(cx);
        } else {
            self.open_project_dropdown(window, cx);
        }
    }

    /// Opens the titlebar project dropdown under the project pill.
    pub fn open_project_dropdown(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.close_context_menu(cx);
        self.close_project_options(cx);
        self.project_dropdown = Some(ProjectDropdownState {
            model: build_model(&self.recent_projects, self.project.root()),
            bounds: Rc::new(Cell::new(None)),
        });
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Closes the titlebar project dropdown.
    pub fn close_project_dropdown(&mut self, cx: &mut Context<'_, Self>) {
        if self.project_dropdown.take().is_some() {
            cx.notify();
        }
    }

    /// Runs `action` from the dropdown: closes it and dispatches the matching process-level
    /// action handled by the host (`kova_app::menus`).
    pub fn activate_project_dropdown_action(
        &mut self,
        action: ProjectDropdownAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.close_project_dropdown(cx);
        match action {
            ProjectDropdownAction::OpenRecent(path) => {
                window.dispatch_action(Box::new(OpenRecentProject { path }), cx);
            }
            ProjectDropdownAction::OpenProject => {
                window.dispatch_action(Box::new(OpenProject), cx);
            }
            ProjectDropdownAction::OpenInNewWindow => {
                window.dispatch_action(Box::new(OpenProjectInNewWindow), cx);
            }
            ProjectDropdownAction::ClearRecent => {
                window.dispatch_action(Box::new(ClearRecentProjects), cx);
            }
        }
    }

    /// Capture-phase key handler while the dropdown is open.
    pub(super) fn handle_project_dropdown_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(state) = self.project_dropdown.as_mut() else {
            return false;
        };
        match state.model.handle_key(keystroke) {
            ProjectDropdownKeyOutcome::Close => {
                self.close_project_dropdown(cx);
                cx.stop_propagation();
                true
            }
            ProjectDropdownKeyOutcome::Activate(action) => {
                self.activate_project_dropdown_action(action, window, cx);
                cx.stop_propagation();
                true
            }
            ProjectDropdownKeyOutcome::Consumed => {
                cx.stop_propagation();
                cx.notify();
                true
            }
            ProjectDropdownKeyOutcome::Ignored => false,
        }
    }

    /// Closes the dropdown on a mouse down outside both the card and the pill (the pill's own
    /// click toggles it).
    pub(super) fn handle_project_dropdown_outside_click(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(state) = self.project_dropdown.as_ref() else {
            return;
        };
        let inside = state
            .bounds
            .get()
            .is_some_and(|b| b.contains(&event.position))
            || self
                .project_pill_bounds
                .get()
                .is_some_and(|b| b.contains(&event.position));
        if !inside {
            self.close_project_dropdown(cx);
        }
    }

    /// Renders the floating dropdown card when open.
    pub(super) fn render_project_dropdown_overlay(
        &self,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Option<Stateful<Div>> {
        let state = self.project_dropdown.as_ref()?;
        let theme = cx.global::<KovaTheme>();
        let elevated = hsla_from_rgba(theme.surfaces().elevated);
        let border_strong = hsla_from_rgba(theme.borders().strong);
        let (x, y, width) = dropdown_origin(self.project_pill_bounds.get(), self.viewport_px);
        let max_height = (self.viewport_px.1 - y - VIEWPORT_MARGIN_PX).max(120.0);
        let bounds_slot = Rc::clone(&state.bounds);
        let model = &state.model;
        let highlighted = model.highlighted();

        let mut list = div()
            .id("project-dropdown-recent")
            .flex()
            .flex_col()
            .flex_shrink()
            .min_h(px(0.0))
            .overflow_y_scroll();
        if model.rows().is_empty() {
            list = list.child(
                div()
                    .h(px(ACTION_ROW_HEIGHT_PX))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .text_size(px(12.0))
                    .text_color(p.text_muted)
                    .child(NO_RECENT_PROJECTS_PLACEHOLDER),
            );
        }
        for (idx, row) in model.rows().iter().enumerate() {
            list = list.child(Self::render_recent_row(
                idx,
                row,
                highlighted == Some(idx),
                p,
                cx,
            ));
        }

        let mut card = div()
            .id("project-dropdown")
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(width))
            .max_h(px(max_height))
            .p(px(4.0))
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .bg(elevated)
            .border_1()
            .border_color(border_strong)
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(
                canvas(
                    move |bounds, _, _| {
                        bounds_slot.set(Some(bounds));
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(
                div()
                    .h(px(24.0))
                    .px(px(10.0))
                    .flex()
                    .flex_none()
                    .items_center()
                    .text_size(px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_muted)
                    .child(RECENT_PROJECTS_HEADER),
            )
            .child(list)
            .child(render_separator(p));

        let offset = model.rows().len();
        for (i, action) in model.actions().iter().enumerate() {
            let idx = offset + i;
            card = card.child(Self::render_action_row(
                idx,
                action,
                highlighted == Some(idx),
                p,
                cx,
            ));
        }
        Some(card)
    }

    fn render_recent_row(
        idx: usize,
        row: &ProjectDropdownRow,
        highlighted: bool,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Stateful<Div> {
        let action = ProjectDropdownAction::OpenRecent(row.root.clone());
        let mut name = div()
            .text_size(px(12.0))
            .text_color(p.text_primary)
            .truncate()
            .child(row.name.clone());
        if row.is_current {
            name = name.font_weight(FontWeight::SEMIBOLD);
        }
        let mut el = div()
            .id(("project-dropdown-recent-row", idx))
            .h(px(RECENT_ROW_HEIGHT_PX))
            .flex_none()
            .w_full()
            .px(px(8.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .hover(move |s| s.bg(p.hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.activate_project_dropdown_action(action.clone(), window, cx);
            }))
            .child(
                div()
                    .w(px(14.0))
                    .flex_none()
                    .text_size(px(12.0))
                    .text_color(p.tab_indicator)
                    .child(if row.is_current { "✓" } else { "" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .child(name)
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(p.text_muted)
                            .truncate()
                            .child(row.display_path.clone()),
                    ),
            );
        if highlighted {
            el = el.bg(p.active);
        }
        el
    }

    fn render_action_row(
        idx: usize,
        action: &ProjectDropdownAction,
        highlighted: bool,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Stateful<Div> {
        let label = action.label().unwrap_or_default();
        let glyph = action.glyph();
        let on_click = action.clone();
        let mut el = div()
            .id(("project-dropdown-action", idx))
            .h(px(ACTION_ROW_HEIGHT_PX))
            .flex_none()
            .w_full()
            .px(px(8.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(12.0))
            .text_color(p.text_primary)
            .cursor_pointer()
            .hover(move |s| s.bg(p.hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.activate_project_dropdown_action(on_click.clone(), window, cx);
            }))
            .child(
                div()
                    .w(px(14.0))
                    .flex_none()
                    .text_color(p.text_muted)
                    .child(glyph),
            )
            .child(label);
        if highlighted {
            el = el.bg(p.active);
        }
        el
    }
}

/// Horizontal rule between the `Recent Projects` list and the actions.
fn render_separator(p: Palette) -> Div {
    div()
        .h(px(9.0))
        .flex_none()
        .flex()
        .items_center()
        .px(px(6.0))
        .child(div().h(px(1.0)).w_full().bg(p.border_subtle))
}

/// Builds the model for `recent`, marking the row whose root matches `current_root`.
fn build_model(recent: &[RecentProject], current_root: Option<&Path>) -> ProjectDropdownModel {
    let current = current_root.map(canonicalize_or_raw);
    ProjectDropdownModel::new(recent, current.as_deref(), home_dir().as_deref())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "test harness: failures must abort the test with context"
    )]

    use std::path::{Path, PathBuf};

    use gpui::{Bounds, Keystroke, point, px, size};
    use kova_core::RecentProject;

    use super::{
        PROJECT_DROPDOWN_WIDTH_PX, ProjectDropdownAction, ProjectDropdownKeyOutcome,
        ProjectDropdownModel, abbreviate_home, dropdown_origin,
    };

    fn recent(name: &str, root: &str) -> RecentProject {
        RecentProject {
            name: name.to_owned(),
            root: PathBuf::from(root),
        }
    }

    fn key(name: &str) -> Keystroke {
        Keystroke::parse(name).expect("keystroke")
    }

    fn sample() -> ProjectDropdownModel {
        ProjectDropdownModel::new(
            &[
                recent("kova-orders", "/Users/dev/work/kova-orders"),
                recent("billing", "/Users/dev/work/billing"),
                recent("tmp", "/opt/tmp"),
            ],
            Some(Path::new("/Users/dev/work/billing")),
            Some(Path::new("/Users/dev")),
        )
    }

    #[test]
    fn rows_keep_order_abbreviate_home_and_mark_current() {
        let model = sample();
        let rows = model.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "kova-orders");
        assert_eq!(rows[0].display_path, "~/work/kova-orders");
        assert!(!rows[0].is_current);
        assert!(rows[1].is_current);
        assert_eq!(rows[2].display_path, "/opt/tmp");
        assert_eq!(rows.iter().filter(|r| r.is_current).count(), 1);
    }

    #[test]
    fn actions_include_clear_only_when_list_is_non_empty() {
        assert_eq!(
            sample().actions(),
            &[
                ProjectDropdownAction::OpenProject,
                ProjectDropdownAction::OpenInNewWindow,
                ProjectDropdownAction::ClearRecent,
            ]
        );
        let empty = ProjectDropdownModel::new(&[], None, None);
        assert_eq!(empty.rows(), []);
        assert_eq!(
            empty.actions(),
            &[
                ProjectDropdownAction::OpenProject,
                ProjectDropdownAction::OpenInNewWindow,
            ]
        );
    }

    #[test]
    fn no_current_project_marks_nothing() {
        let model = ProjectDropdownModel::new(&[recent("a", "/w/a")], None, None);
        assert!(!model.rows()[0].is_current);
    }

    #[test]
    fn entries_map_rows_then_actions() {
        let model = sample();
        assert_eq!(model.entry_count(), 6);
        assert_eq!(
            model.entry_action(1),
            Some(ProjectDropdownAction::OpenRecent(PathBuf::from(
                "/Users/dev/work/billing"
            )))
        );
        assert_eq!(
            model.entry_action(3),
            Some(ProjectDropdownAction::OpenProject)
        );
        assert_eq!(
            model.entry_action(5),
            Some(ProjectDropdownAction::ClearRecent)
        );
        assert_eq!(model.entry_action(6), None);
    }

    #[test]
    fn keyboard_navigation_wraps_and_enter_activates() {
        let mut model = sample();
        assert_eq!(
            model.handle_key(&key("enter")),
            ProjectDropdownKeyOutcome::Consumed
        );
        assert_eq!(
            model.handle_key(&key("down")),
            ProjectDropdownKeyOutcome::Consumed
        );
        assert_eq!(model.highlighted(), Some(0));
        model.handle_key(&key("up"));
        assert_eq!(model.highlighted(), Some(5));
        model.handle_key(&key("down"));
        assert_eq!(model.highlighted(), Some(0));
        model.handle_key(&key("down"));
        assert_eq!(
            model.handle_key(&key("enter")),
            ProjectDropdownKeyOutcome::Activate(ProjectDropdownAction::OpenRecent(PathBuf::from(
                "/Users/dev/work/billing"
            )))
        );
        model.set_highlighted(Some(4));
        assert_eq!(
            model.handle_key(&key("enter")),
            ProjectDropdownKeyOutcome::Activate(ProjectDropdownAction::OpenInNewWindow)
        );
    }

    #[test]
    fn escape_closes_and_other_keys_pass_through() {
        let mut model = sample();
        assert_eq!(
            model.handle_key(&key("escape")),
            ProjectDropdownKeyOutcome::Close
        );
        assert_eq!(
            model.handle_key(&key("a")),
            ProjectDropdownKeyOutcome::Ignored
        );
        assert_eq!(
            model.handle_key(&key("cmd-down")),
            ProjectDropdownKeyOutcome::Ignored
        );
    }

    #[test]
    fn set_highlighted_ignores_out_of_range() {
        let mut model = ProjectDropdownModel::new(&[], None, None);
        model.set_highlighted(Some(7));
        assert_eq!(model.highlighted(), None);
        model.set_highlighted(Some(1));
        assert_eq!(model.highlighted(), Some(1));
    }

    #[test]
    fn abbreviate_home_handles_home_itself_and_siblings() {
        let home = Some(Path::new("/Users/dev"));
        assert_eq!(abbreviate_home(Path::new("/Users/dev"), home), "~");
        assert_eq!(abbreviate_home(Path::new("/Users/dev/a/b"), home), "~/a/b");
        assert_eq!(
            abbreviate_home(Path::new("/Users/devops/x"), home),
            "/Users/devops/x"
        );
        assert_eq!(
            abbreviate_home(Path::new("/Users/dev/a"), None),
            "/Users/dev/a"
        );
    }

    #[test]
    fn dropdown_is_anchored_under_the_pill_and_clamped() {
        let pill = Bounds::new(point(px(120.0), px(9.0)), size(px(110.0), px(22.0)));
        let (x, y, w) = dropdown_origin(Some(pill), (1400.0, 900.0));
        assert!((x - 120.0).abs() < f32::EPSILON);
        assert!((y - 35.0).abs() < f32::EPSILON);
        assert!((w - PROJECT_DROPDOWN_WIDTH_PX).abs() < f32::EPSILON);

        let far_right = Bounds::new(point(px(1300.0), px(9.0)), size(px(90.0), px(22.0)));
        let (x, _, w) = dropdown_origin(Some(far_right), (1400.0, 900.0));
        assert!(x + w <= 1400.0 - 8.0 + f32::EPSILON);
    }
}
