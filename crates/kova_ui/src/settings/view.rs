//! [`SettingsView`]: the Islands Settings overlay (`Cmd+,`) rendered above the workspace.
//!
//! The view state (category, keyboard focus, the text being typed, the open drop-down) is
//! headless: keys arrive as [`SettingsKey`] and clicks as [`SettingsClick`], and every
//! handler returns a [`SettingsKeyOutcome`] telling the [`WorkspaceShell`] whether to persist
//! and apply the settings, open a config file, or close the overlay. Only `SettingsView::render`
//! touches GPUI.

use std::path::PathBuf;

use gpui::{
    ClickEvent, Context, Div, FontWeight, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, div,
    px,
};
use kova_config::{ConfigError, KovaSettings};
use kova_theme::PluginVariant;

use super::SettingsSource;
use super::model::{
    ControlKind, ReloadOutcome, SettingDescriptor, SettingFieldError, SettingValue,
    SettingsCategory, SettingsFormModel, descriptor,
};
use crate::controls::{
    Badge, Button, ControlPalette, NumericInput, SelectInput, TextInput, TextInputEvent,
    TextInputState, ToggleSwitch,
};
use crate::shell::WorkspaceShell;
use crate::theme::{KovaTheme, hsla_from_rgba};

const NAV_WIDTH_PX: f32 = 200.0;
const PANEL_MAX_WIDTH_PX: f32 = 920.0;
const PANEL_MAX_HEIGHT_PX: f32 = 640.0;
const PANEL_MARGIN_PX: f32 = 40.0;

/// Keyboard focus inside the Settings view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFocus {
    /// The `Search settings...` field.
    Search,
    /// The control of one setting.
    Field(&'static str),
}

/// What the shell must do after the view handled an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsKeyOutcome {
    /// Nothing changed.
    Ignored,
    /// Only the view state changed (repaint).
    Handled,
    /// The settings changed: persist [`SettingsView::model`] and apply it to the IDE.
    Changed,
    /// Open this config file in an editor tab (and close the overlay).
    OpenFile(PathBuf),
    /// Close the overlay.
    Close,
}

impl SettingsKeyOutcome {
    /// Keeps the more important of two outcomes (`Changed` beats `Handled`).
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Ignored | Self::Handled, other) => {
                if other == Self::Ignored {
                    Self::Handled
                } else {
                    other
                }
            }
            (first, _) => first,
        }
    }
}

/// Key press routed to the view (built from a GPUI keystroke by the shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsKey<'a> {
    /// GPUI key name (`enter`, `tab`, `a`, `/`).
    pub key: &'a str,
    /// Typed text, if the key produces any.
    pub text: Option<&'a str>,
    /// Whether `Shift` is held.
    pub shift: bool,
    /// Whether `Cmd` / `Ctrl` (or `Alt`) is held: such keys never insert text.
    pub command: bool,
}

/// Mouse interaction inside the Settings view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsClick {
    /// Backdrop click: close.
    Backdrop,
    /// Navigation entry.
    Category(SettingsCategory),
    /// Search field.
    Search,
    /// Row or control of a setting (focus it).
    Field(&'static str),
    /// Toggle switch.
    Toggle(&'static str),
    /// `−` / `+` stepper of a numeric setting.
    Step(&'static str, bool),
    /// Open / close a drop-down.
    Select(&'static str),
    /// Pick an option of an open drop-down.
    Option(&'static str, String),
    /// *Reset to Omakase default*.
    Reset(&'static str),
    /// `Open settings.toml`.
    OpenSettingsFile,
    /// `Open keymap.toml`.
    OpenKeymapFile,
    /// Close button.
    Close,
}

/// State of the Settings overlay.
#[derive(Debug, Clone)]
pub struct SettingsView {
    model: SettingsFormModel,
    category: SettingsCategory,
    focus: SettingsFocus,
    search: TextInputState,
    /// Field being typed into and its editor.
    editing: Option<(&'static str, TextInputState)>,
    open_select: Option<&'static str>,
    settings_path: Option<PathBuf>,
    keymap_path: Option<PathBuf>,
    /// Last background write failure, shown in the banner.
    write_error: Option<String>,
}

impl SettingsView {
    /// Creates the view over `model`.
    #[must_use]
    pub fn new(model: SettingsFormModel) -> Self {
        Self {
            model,
            category: SettingsCategory::Editor,
            focus: SettingsFocus::Search,
            search: TextInputState::default(),
            editing: None,
            open_select: None,
            settings_path: None,
            keymap_path: None,
            write_error: None,
        }
    }

    /// Builds the view from a host [`SettingsSource`] (defaults when the file is missing or
    /// invalid; an invalid file is reported in the banner).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] only if the built-in Omakase template is invalid.
    pub fn from_source(source: Option<&SettingsSource>) -> Result<Self, ConfigError> {
        let mut model = SettingsFormModel::omakase_default()?;
        let mut load_error = None;
        if let Some(source) = source {
            model.set_theme_ids(source.theme_ids.clone());
            if let Some(contents) = &source.contents
                && let Err(message) = model.reload_from_toml(contents)
            {
                load_error = Some(message);
            }
        }
        let mut view = Self::new(model);
        if let Some(source) = source {
            view.settings_path = Some(source.settings_path.clone());
            view.keymap_path = Some(source.keymap_path.clone());
        }
        if load_error.is_some() {
            view.model.set_load_error(load_error);
        }
        Ok(view)
    }

    /// Headless form model.
    #[must_use]
    pub const fn model(&self) -> &SettingsFormModel {
        &self.model
    }

    /// Current validated settings.
    #[must_use]
    pub fn settings(&self) -> &KovaSettings {
        self.model.settings()
    }

    /// Selected category.
    #[must_use]
    pub const fn category(&self) -> SettingsCategory {
        self.category
    }

    /// Keyboard focus.
    #[must_use]
    pub const fn focus(&self) -> SettingsFocus {
        self.focus
    }

    /// Path of `settings.toml`, once known.
    #[must_use]
    pub const fn settings_path(&self) -> Option<&PathBuf> {
        self.settings_path.as_ref()
    }

    /// Records a failed background write (banner) or clears it.
    pub fn set_write_error(&mut self, message: Option<String>) {
        self.write_error = message;
    }

    /// Feeds a host [`SettingsSource`] (watcher reload) into the form.
    pub fn apply_source(&mut self, source: &SettingsSource) -> ReloadOutcome {
        self.settings_path = Some(source.settings_path.clone());
        self.keymap_path = Some(source.keymap_path.clone());
        self.model.set_theme_ids(source.theme_ids.clone());
        let editing = self.editing.as_ref().map(|(key, _)| *key);
        let outcome = match &source.contents {
            Some(contents) => self.model.apply_watcher_source(contents, editing),
            None => ReloadOutcome::SelfWriteEcho,
        };
        if outcome == ReloadOutcome::Reloaded {
            self.write_error = None;
        }
        outcome
    }

    /// Shows a reload failure reported by the watcher (Last-Known-Good stays active).
    pub fn set_load_error(&mut self, message: String) {
        self.model.set_load_error(Some(message));
    }

    /// Settings listed in the content column: search hits across all categories while a
    /// query is typed, otherwise the selected category.
    #[must_use]
    pub fn fields(&self) -> Vec<&'static SettingDescriptor> {
        if self.model.filter_query().trim().is_empty() {
            self.model.visible_descriptors(self.category)
        } else {
            self.model.search_results()
        }
    }

    /// Handles one key press.
    pub fn handle_key(&mut self, key: SettingsKey<'_>) -> SettingsKeyOutcome {
        if let Some(select_key) = self.open_select {
            return self.handle_select_key(select_key, key);
        }
        match key.key {
            "escape" => return self.escape(),
            "tab" => return self.move_focus(!key.shift),
            "up" | "down" if !key.command => {
                let outcome = self.commit_editing();
                self.category = self.category.cycle(key.key == "down");
                self.model.set_filter_query("");
                self.search = TextInputState::default();
                self.focus = SettingsFocus::Search;
                return outcome.merge(SettingsKeyOutcome::Handled);
            }
            _ => {}
        }
        match self.focus {
            SettingsFocus::Search => self.search_key(key),
            SettingsFocus::Field(field) => self.field_key(field, key),
        }
    }

    fn escape(&mut self) -> SettingsKeyOutcome {
        if let Some((field, _)) = self.editing.take() {
            self.model.discard_draft(field);
            return SettingsKeyOutcome::Handled;
        }
        if !self.search.text().is_empty() {
            self.search.clear();
            self.model.set_filter_query("");
            return SettingsKeyOutcome::Handled;
        }
        SettingsKeyOutcome::Close
    }

    fn search_key(&mut self, key: SettingsKey<'_>) -> SettingsKeyOutcome {
        if key.command {
            return SettingsKeyOutcome::Ignored;
        }
        let event = match key.text {
            Some(text)
                if !text.is_empty()
                    && key.key != "enter"
                    && !text.chars().any(char::is_control) =>
            {
                self.search.insert(text)
            }
            _ => self.search.handle_named_key(key.key),
        };
        match event {
            TextInputEvent::Edited => {
                self.model.set_filter_query(self.search.text());
                SettingsKeyOutcome::Handled
            }
            TextInputEvent::Submit => self.move_focus(true),
            TextInputEvent::Cancel => self.escape(),
            TextInputEvent::Ignored => SettingsKeyOutcome::Ignored,
        }
    }

    fn field_key(&mut self, field: &'static str, key: SettingsKey<'_>) -> SettingsKeyOutcome {
        let Some(desc) = descriptor(field) else {
            return SettingsKeyOutcome::Ignored;
        };
        match desc.control {
            ControlKind::Toggle => match key.key {
                "space" | "enter" => Self::changed(self.model.toggle(field)),
                "/" => self.focus_search(),
                _ => SettingsKeyOutcome::Ignored,
            },
            ControlKind::Select(_) => match key.key {
                "space" | "enter" => {
                    self.open_select = Some(field);
                    SettingsKeyOutcome::Handled
                }
                "left" | "right" => {
                    let result = self.model.cycle_select(field, key.key == "right");
                    Self::changed(result)
                }
                "/" => self.focus_search(),
                _ => SettingsKeyOutcome::Ignored,
            },
            ControlKind::Number { .. } | ControlKind::Text { .. } => {
                if key.command {
                    return SettingsKeyOutcome::Ignored;
                }
                self.ensure_editing(field);
                let Some((_, editor)) = self.editing.as_mut() else {
                    return SettingsKeyOutcome::Ignored;
                };
                let event = match key.text {
                    Some(text)
                        if !text.is_empty()
                            && key.key != "enter"
                            && !text.chars().any(char::is_control) =>
                    {
                        editor.insert(text)
                    }
                    _ => editor.handle_named_key(key.key),
                };
                match event {
                    TextInputEvent::Edited => {
                        let text = editor.text().to_owned();
                        self.model.set_draft(field, text);
                        SettingsKeyOutcome::Handled
                    }
                    TextInputEvent::Submit => {
                        self.commit_editing().merge(SettingsKeyOutcome::Handled)
                    }
                    TextInputEvent::Cancel => self.escape(),
                    TextInputEvent::Ignored => SettingsKeyOutcome::Ignored,
                }
            }
        }
    }

    fn handle_select_key(
        &mut self,
        field: &'static str,
        key: SettingsKey<'_>,
    ) -> SettingsKeyOutcome {
        match key.key {
            "escape" | "space" | "enter" | "tab" => {
                self.open_select = None;
                SettingsKeyOutcome::Handled
            }
            "up" | "down" | "left" | "right" => {
                let forward = matches!(key.key, "down" | "right");
                let result = self.model.cycle_select(field, forward);
                Self::changed(result)
            }
            _ => SettingsKeyOutcome::Handled,
        }
    }

    fn changed(result: Result<String, SettingFieldError>) -> SettingsKeyOutcome {
        result.map_or(SettingsKeyOutcome::Handled, |_| SettingsKeyOutcome::Changed)
    }

    fn focus_search(&mut self) -> SettingsKeyOutcome {
        let outcome = self.commit_editing();
        self.focus = SettingsFocus::Search;
        self.open_select = None;
        outcome.merge(SettingsKeyOutcome::Handled)
    }

    fn ensure_editing(&mut self, field: &'static str) {
        if self.editing.as_ref().is_some_and(|(key, _)| *key == field) {
            return;
        }
        let text = self.model.display_text(field);
        self.editing = Some((field, TextInputState::new(text)));
    }

    /// Commits the field being typed into (`Enter` or focus loss). Invalid text stays as a
    /// draft with its error; nothing reaches `settings.toml`.
    pub fn commit_editing(&mut self) -> SettingsKeyOutcome {
        let Some((field, editor)) = self.editing.take() else {
            return SettingsKeyOutcome::Ignored;
        };
        if !self.model.has_draft(field) {
            return SettingsKeyOutcome::Handled;
        }
        let result = self.model.commit_text(field, editor.text());
        Self::changed(result)
    }

    /// Moves keyboard focus to the next (`forward`) or previous control, committing the
    /// field being left.
    pub fn move_focus(&mut self, forward: bool) -> SettingsKeyOutcome {
        let outcome = self.commit_editing();
        self.open_select = None;
        let fields = self.fields();
        let mut order: Vec<SettingsFocus> = vec![SettingsFocus::Search];
        order.extend(fields.iter().map(|d| SettingsFocus::Field(d.dotted_key)));
        let len = order.len();
        let idx = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        self.focus = order[next];
        outcome.merge(SettingsKeyOutcome::Handled)
    }

    /// Handles a click.
    pub fn click(&mut self, click: SettingsClick) -> SettingsKeyOutcome {
        match click {
            // The shell commits (and persists) the field being typed into when it closes.
            SettingsClick::Backdrop | SettingsClick::Close => SettingsKeyOutcome::Close,
            SettingsClick::Category(category) => {
                let outcome = self.commit_editing();
                self.category = category;
                self.search.clear();
                self.model.set_filter_query("");
                self.open_select = None;
                self.focus = SettingsFocus::Search;
                outcome.merge(SettingsKeyOutcome::Handled)
            }
            SettingsClick::Search => self.focus_search(),
            SettingsClick::Field(field) => self.focus_field(field),
            SettingsClick::Toggle(field) => {
                let outcome = self.focus_field(field);
                let result = self.model.toggle(field);
                outcome.merge(Self::changed(result))
            }
            SettingsClick::Step(field, forward) => {
                let outcome = self.focus_field(field);
                self.editing = None;
                self.model.discard_draft(field);
                let result = self.model.step(field, forward);
                outcome.merge(Self::changed(result))
            }
            SettingsClick::Select(field) => {
                let outcome = self.focus_field(field);
                self.open_select = if self.open_select == Some(field) {
                    None
                } else {
                    Some(field)
                };
                outcome.merge(SettingsKeyOutcome::Handled)
            }
            SettingsClick::Option(field, id) => {
                self.open_select = None;
                let result = self.model.select(field, &id);
                Self::changed(result)
            }
            SettingsClick::Reset(field) => {
                let outcome = self.commit_editing();
                self.model.discard_draft(field);
                let result = self.model.reset_to_default(field);
                outcome.merge(Self::changed(result))
            }
            SettingsClick::OpenSettingsFile => Self::open_file(self.settings_path.clone()),
            SettingsClick::OpenKeymapFile => Self::open_file(self.keymap_path.clone()),
        }
    }

    /// Opening a file closes the overlay, and closing commits the field being typed into.
    fn open_file(path: Option<PathBuf>) -> SettingsKeyOutcome {
        path.map_or(SettingsKeyOutcome::Handled, SettingsKeyOutcome::OpenFile)
    }

    fn focus_field(&mut self, field: &'static str) -> SettingsKeyOutcome {
        if self.focus == SettingsFocus::Field(field) {
            return SettingsKeyOutcome::Handled;
        }
        let outcome = self.commit_editing();
        if self.open_select != Some(field) {
            self.open_select = None;
        }
        self.focus = SettingsFocus::Field(field);
        outcome.merge(SettingsKeyOutcome::Handled)
    }

    /// Renders the overlay (backdrop, navigation island and content island).
    pub(crate) fn render(
        &self,
        theme: &KovaTheme,
        viewport: (f32, f32),
        cx: &Context<'_, WorkspaceShell>,
    ) -> Stateful<Div> {
        let p = ControlPalette::from_theme(theme);
        let island = hsla_from_rgba(theme.surfaces().island);
        let editor_bg = hsla_from_rgba(theme.surfaces().editor);
        let border = hsla_from_rgba(theme.borders().island);
        let active = hsla_from_rgba(theme.surfaces().active);
        let mut backdrop = hsla_from_rgba(theme.surfaces().overlay);
        backdrop.a = backdrop.a.min(0.55);

        let width = (viewport.0 - 2.0 * PANEL_MARGIN_PX).clamp(320.0, PANEL_MAX_WIDTH_PX);
        let height = (viewport.1 - 2.0 * PANEL_MARGIN_PX).clamp(240.0, PANEL_MAX_HEIGHT_PX);

        let panel = div()
            .id("settings-panel")
            .w(px(width))
            .h(px(height))
            .flex()
            .flex_row()
            .rounded(px(10.0))
            .border_1()
            .border_color(border)
            .bg(editor_bg)
            .overflow_hidden()
            .shadow_lg()
            .occlude()
            // Clicks inside the panel must not reach the backdrop.
            .on_click(|_: &ClickEvent, _, cx| cx.stop_propagation())
            .child(self.render_nav(&p, island, active, border, cx))
            .child(self.render_content(&p, editor_bg, cx));

        div()
            .id("settings-overlay")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(backdrop)
            .child(
                div()
                    .id("settings-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .on_click(listener(cx, SettingsClick::Backdrop)),
            )
            .child(panel)
    }

    fn render_nav(
        &self,
        p: &ControlPalette,
        island: gpui::Hsla,
        active: gpui::Hsla,
        border: gpui::Hsla,
        cx: &Context<'_, WorkspaceShell>,
    ) -> Div {
        let searching = !self.model.filter_query().trim().is_empty();
        let mut list = div().flex().flex_col().gap(px(2.0)).p(px(8.0));
        for category in SettingsCategory::ALL {
            let selected = !searching && category == self.category;
            let id = SharedString::from(format!("settings-nav-{}", category.title()));
            let mut row = div()
                .id(id)
                .h(px(28.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .justify_between()
                .rounded(px(6.0))
                .text_size(px(12.5))
                .cursor_pointer()
                .hover(|s| s.bg(p.hover))
                .on_click(listener(cx, SettingsClick::Category(category)))
                .child(category.title())
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(p.text_muted)
                        .child(category.toml_table()),
                );
            row = if selected {
                row.bg(active)
                    .text_color(p.text)
                    .font_weight(FontWeight::SEMIBOLD)
            } else {
                row.text_color(p.text_secondary)
            };
            list = list.child(row);
        }
        div()
            .w(px(NAV_WIDTH_PX))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .justify_between()
            .bg(island)
            .border_r_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(18.0))
                            .pt(px(16.0))
                            .pb(px(6.0))
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text)
                            .child("Settings"),
                    )
                    .child(list),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .p(px(10.0))
                    .child(
                        Button::ghost("Open settings.toml", PluginVariant::Accent)
                            .render("settings-open-toml", p)
                            .on_click(listener(cx, SettingsClick::OpenSettingsFile)),
                    )
                    .child(
                        Button::ghost("Open keymap.toml", PluginVariant::Accent)
                            .render("settings-open-keymap", p)
                            .on_click(listener(cx, SettingsClick::OpenKeymapFile)),
                    ),
            )
    }

    fn render_content(
        &self,
        p: &ControlPalette,
        editor_bg: gpui::Hsla,
        cx: &Context<'_, WorkspaceShell>,
    ) -> Div {
        let search_focused = self.focus == SettingsFocus::Search;
        let search = if search_focused {
            TextInput::editing(&self.search)
        } else {
            TextInput::new(self.search.text())
        }
        .placeholder("Search settings...  ( / )")
        .render("settings-search", 360.0, p)
        .on_click(listener(cx, SettingsClick::Search));

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .px(px(20.0))
            .pt(px(14.0))
            .pb(px(10.0))
            .child(search)
            .child(
                Button::ghost("Esc", PluginVariant::Muted)
                    .render("settings-close", p)
                    .on_click(listener(cx, SettingsClick::Close)),
            );

        let mut body = div()
            .id("settings-body")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(20.0))
            .pb(px(16.0))
            .flex()
            .flex_col()
            .gap(px(2.0));

        if let Some(banner) = self.render_banner(p, cx) {
            body = body.child(banner);
        }

        let searching = !self.model.filter_query().trim().is_empty();
        let fields = self.fields();
        if searching {
            body = body.child(section_title(
                &format!(
                    "{} result(s) for \"{}\"",
                    fields.len(),
                    self.model.filter_query().trim()
                ),
                p,
            ));
        } else {
            body = body.child(section_title(self.category.title(), p));
            if self.category == SettingsCategory::KeymapAndVim {
                body = body.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.text_secondary)
                        .pb(px(6.0))
                        .child("Per-action shortcuts live in ~/.config/kova/keymap.toml (Open keymap.toml)."),
                );
            }
        }
        if fields.is_empty() {
            body = body.child(
                div()
                    .py(px(20.0))
                    .text_size(px(12.0))
                    .text_color(p.text_muted)
                    .child("No settings match the search."),
            );
        }
        for desc in fields {
            body = body.child(self.render_row(desc, searching, p, cx));
        }

        div()
            .flex_1()
            .h_full()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .bg(editor_bg)
            .child(header)
            .child(body)
    }

    fn render_banner(&self, p: &ControlPalette, cx: &Context<'_, WorkspaceShell>) -> Option<Div> {
        let (variant, message) = match (&self.write_error, self.model.load_error()) {
            (Some(err), _) => (
                PluginVariant::Error,
                format!("Could not write settings.toml: {err}"),
            ),
            (None, Some(err)) => (
                PluginVariant::Warning,
                format!("settings.toml has an error — the last valid settings stay active. {err}"),
            ),
            (None, None) => return None,
        };
        let c = p.variant(variant);
        Some(
            div()
                .my(px(6.0))
                .p(px(10.0))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(10.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(c.border)
                .bg(c.subtle_bg)
                .text_color(c.subtle_fg)
                .text_size(px(12.0))
                .child(div().flex_1().child(message))
                .child(
                    Button::new("Open settings.toml", variant)
                        .render("settings-banner-open", p)
                        .on_click(listener(cx, SettingsClick::OpenSettingsFile)),
                ),
        )
    }

    fn render_row(
        &self,
        desc: &'static SettingDescriptor,
        searching: bool,
        p: &ControlPalette,
        cx: &Context<'_, WorkspaceShell>,
    ) -> gpui::AnyElement {
        let key = desc.dotted_key;
        let focused = self.focus == SettingsFocus::Field(key);
        let error = self.model.error_for(key).map(str::to_owned);

        let mut key_line = div().flex().items_center().gap(px(6.0)).child(
            div()
                .text_size(px(10.5))
                .text_color(p.text_muted)
                .child(key),
        );
        if searching {
            key_line =
                key_line.child(Badge::new(desc.category.title(), PluginVariant::Muted).render(p));
        }
        if !self.model.is_default(key) {
            key_line = key_line.child(Badge::new("modified", PluginVariant::Accent).render(p));
        }

        let mut label = div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(p.text)
                    .font_weight(FontWeight::MEDIUM)
                    .child(desc.title),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.text_secondary)
                    .child(desc.description),
            )
            .child(key_line);
        if let Some(error) = &error {
            label = label.child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.error)
                    .child(SharedString::from(error.clone())),
            );
        }

        let control = self.render_control(desc, focused, error.is_some(), p, cx);
        let mut controls = div().flex().items_center().gap(px(6.0)).child(control);
        if !self.model.is_default(key) {
            controls = controls.child(
                Button::ghost("Reset", PluginVariant::Muted)
                    .render(SharedString::from(format!("reset-{key}")), p)
                    .on_click(listener(cx, SettingsClick::Reset(key))),
            );
        }

        let mut row = div()
            .id(SharedString::from(format!("row-{key}")))
            .w_full()
            .flex()
            .flex_col()
            .px(px(12.0))
            .py(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(if focused {
                p.focus
            } else {
                gpui::transparent_black()
            })
            .on_click(listener(cx, SettingsClick::Field(key)))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(16.0))
                    .child(label)
                    .child(controls),
            );
        if self.open_select == Some(key) {
            row = row.child(self.render_options(key, p, cx));
        }
        row.into_any_element()
    }

    fn render_control(
        &self,
        desc: &'static SettingDescriptor,
        focused: bool,
        invalid: bool,
        p: &ControlPalette,
        cx: &Context<'_, WorkspaceShell>,
    ) -> gpui::AnyElement {
        let key = desc.dotted_key;
        match desc.control {
            ControlKind::Toggle => {
                let on = matches!(self.model.value(key), Some(SettingValue::Bool(true)));
                ToggleSwitch { on, focused }
                    .render(SharedString::from(format!("toggle-{key}")), p)
                    .on_click(listener(cx, SettingsClick::Toggle(key)))
                    .into_any_element()
            }
            ControlKind::Select(_) => {
                let current = match self.model.value(key) {
                    Some(SettingValue::Choice(id)) => id,
                    _ => String::new(),
                };
                let label = self
                    .model
                    .select_options(key)
                    .into_iter()
                    .find(|(id, _)| *id == current)
                    .map_or(current, |(_, label)| label);
                SelectInput {
                    label,
                    focused,
                    open: self.open_select == Some(key),
                }
                .render(SharedString::from(format!("select-{key}")), p)
                .on_click(listener(cx, SettingsClick::Select(key)))
                .into_any_element()
            }
            ControlKind::Number { .. } => {
                let field = self.text_field(key, focused, invalid);
                let unit = match key {
                    "jvm.jdtls_max_ram_mb" => Some("MiB"),
                    "ui.sidebar_width" | "ui.bottom_panel_height" => Some("px"),
                    "editor.font_size" | "terminal.font_size" => Some("pt"),
                    "jvm.auto_hibernate_minutes" => Some("min"),
                    _ => None,
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        Button::ghost("−", PluginVariant::Muted)
                            .render(SharedString::from(format!("dec-{key}")), p)
                            .on_click(listener(cx, SettingsClick::Step(key, false))),
                    )
                    .child(
                        NumericInput { field, unit }
                            .render(SharedString::from(format!("num-{key}")), p)
                            .on_click(listener(cx, SettingsClick::Field(key))),
                    )
                    .child(
                        Button::ghost("+", PluginVariant::Muted)
                            .render(SharedString::from(format!("inc-{key}")), p)
                            .on_click(listener(cx, SettingsClick::Step(key, true))),
                    )
                    .into_any_element()
            }
            ControlKind::Text { optional } => {
                let placeholder = if optional { "(system default)" } else { "" };
                self.text_field(key, focused, invalid)
                    .placeholder(placeholder)
                    .render(SharedString::from(format!("text-{key}")), 220.0, p)
                    .on_click(listener(cx, SettingsClick::Field(key)))
                    .into_any_element()
            }
        }
    }

    fn text_field(&self, key: &'static str, focused: bool, invalid: bool) -> TextInput {
        match &self.editing {
            Some((field, editor)) if *field == key => TextInput::editing(editor),
            _ => {
                let mut input = TextInput::new(self.model.display_text(key));
                if focused {
                    input.focused = true;
                }
                input
            }
        }
        .invalid(invalid)
    }

    fn render_options(
        &self,
        key: &'static str,
        p: &ControlPalette,
        cx: &Context<'_, WorkspaceShell>,
    ) -> Div {
        let current = match self.model.value(key) {
            Some(SettingValue::Choice(id)) => id,
            _ => String::new(),
        };
        let mut list = div()
            .mt(px(6.0))
            .ml_auto()
            .w(px(220.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .gap(px(1.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(p.border)
            .bg(p.elevated);
        for (id, label) in self.model.select_options(key) {
            let selected = id == current;
            list = list.child(
                SelectInput::option_row(
                    SharedString::from(format!("opt-{key}-{id}")),
                    label,
                    selected,
                    p,
                )
                .on_click(listener(cx, SettingsClick::Option(key, id))),
            );
        }
        list
    }
}

fn section_title(text: &str, p: &ControlPalette) -> Div {
    div()
        .pt(px(8.0))
        .pb(px(4.0))
        .text_size(px(15.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.text)
        .child(SharedString::from(text.to_owned()))
}

fn listener(
    cx: &Context<'_, WorkspaceShell>,
    click: SettingsClick,
) -> impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    cx.listener(move |shell, _: &ClickEvent, window, cx| {
        cx.stop_propagation();
        shell.settings_click(click.clone(), window, cx);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "# keep me\n[editor]\ntab_size = 4 # tabs\nvim_mode = false\n";

    fn view() -> SettingsView {
        SettingsView::from_source(Some(&SettingsSource {
            settings_path: PathBuf::from("/cfg/settings.toml"),
            keymap_path: PathBuf::from("/cfg/keymap.toml"),
            contents: Some(SOURCE.to_owned()),
            theme_ids: vec!["kova-islands-dark".to_owned()],
        }))
        .expect("view")
    }

    fn key(name: &str) -> SettingsKey<'_> {
        SettingsKey {
            key: name,
            text: None,
            shift: false,
            command: false,
        }
    }

    fn typed(text: &str) -> SettingsKey<'_> {
        SettingsKey {
            key: text,
            text: Some(text),
            shift: false,
            command: false,
        }
    }

    #[test]
    fn typing_in_search_filters_and_escape_clears_then_closes() {
        let mut v = view();
        for ch in ["r", "a", "m"] {
            assert_eq!(v.handle_key(typed(ch)), SettingsKeyOutcome::Handled);
        }
        let keys: Vec<_> = v.fields().iter().map(|d| d.dotted_key).collect();
        assert_eq!(keys, ["jvm.jdtls_max_ram_mb"]);
        assert_eq!(v.handle_key(key("escape")), SettingsKeyOutcome::Handled);
        assert_eq!(v.fields().len(), 9, "editor category again");
        assert_eq!(v.handle_key(key("escape")), SettingsKeyOutcome::Close);
    }

    #[test]
    fn tab_navigation_toggles_and_commits_numeric_fields() {
        let mut v = view();
        // Search → font_family → font_size → line_height → tab_size.
        for _ in 0..4 {
            v.handle_key(key("tab"));
        }
        assert_eq!(v.focus(), SettingsFocus::Field("editor.tab_size"));
        v.handle_key(key("backspace"));
        assert_eq!(v.handle_key(typed("0")), SettingsKeyOutcome::Handled);
        // Invalid: Enter keeps the TOML untouched and shows the error.
        assert_eq!(v.handle_key(key("enter")), SettingsKeyOutcome::Handled);
        assert!(v.model().error_for("editor.tab_size").is_some());
        assert!(v.model().to_toml_string().contains("tab_size = 4 # tabs"));

        v.handle_key(key("backspace"));
        v.handle_key(typed("2"));
        // Leaving the field commits it.
        assert_eq!(v.handle_key(key("tab")), SettingsKeyOutcome::Changed);
        assert!(v.model().to_toml_string().contains("tab_size = 2 # tabs"));
        assert_eq!(v.focus(), SettingsFocus::Field("editor.insert_spaces"));
        assert_eq!(v.handle_key(key("space")), SettingsKeyOutcome::Changed);
        assert!(!v.settings().editor.insert_spaces);
        assert!(v.model().to_toml_string().starts_with("# keep me\n"));
    }

    #[test]
    fn arrows_switch_categories_and_selects_cycle() {
        let mut v = view();
        v.handle_key(key("down"));
        assert_eq!(v.category(), SettingsCategory::Appearance);
        v.handle_key(key("tab"));
        assert_eq!(v.focus(), SettingsFocus::Field("ui.theme"));
        assert_eq!(v.handle_key(key("right")), SettingsKeyOutcome::Changed);
        assert_eq!(v.settings().ui.theme, "kova-light");
        v.handle_key(key("up"));
        v.handle_key(key("up"));
        assert_eq!(v.category(), SettingsCategory::Terminal);
    }

    #[test]
    fn clicks_toggle_step_reset_and_open_files() {
        let mut v = view();
        assert_eq!(
            v.click(SettingsClick::Toggle("editor.vim_mode")),
            SettingsKeyOutcome::Changed
        );
        assert!(v.settings().editor.vim_mode);
        assert_eq!(
            v.click(SettingsClick::Step("editor.tab_size", true)),
            SettingsKeyOutcome::Changed
        );
        assert_eq!(v.settings().editor.tab_size, 5);
        assert_eq!(
            v.click(SettingsClick::Reset("editor.tab_size")),
            SettingsKeyOutcome::Changed
        );
        assert_eq!(v.settings().editor.tab_size, 4);
        assert_eq!(
            v.click(SettingsClick::OpenKeymapFile),
            SettingsKeyOutcome::OpenFile(PathBuf::from("/cfg/keymap.toml"))
        );
        v.click(SettingsClick::Select("editor.keymap"));
        assert_eq!(
            v.click(SettingsClick::Option("editor.keymap", "vim".to_owned())),
            SettingsKeyOutcome::Changed
        );
        assert_eq!(v.settings().editor.keymap, kova_config::KeymapPreset::Vim);
    }

    #[test]
    fn close_and_open_file_clicks_win_over_a_pending_draft() {
        let mut v = view();
        v.click(SettingsClick::Field("editor.font_family"));
        v.handle_key(typed("X"));
        assert_eq!(v.click(SettingsClick::Close), SettingsKeyOutcome::Close);
        // The shell commits the draft while closing.
        assert_eq!(v.commit_editing(), SettingsKeyOutcome::Changed);
        assert_eq!(v.settings().editor.font_family, "JetBrains MonoX");

        v.click(SettingsClick::Field("editor.font_family"));
        v.handle_key(typed("Y"));
        assert_eq!(
            v.click(SettingsClick::OpenSettingsFile),
            SettingsKeyOutcome::OpenFile(PathBuf::from("/cfg/settings.toml"))
        );
        assert_eq!(v.click(SettingsClick::Backdrop), SettingsKeyOutcome::Close);
    }

    #[test]
    fn watcher_sources_reload_unless_they_echo_our_write() {
        let mut v = view();
        v.click(SettingsClick::Toggle("editor.vim_mode"));
        let echo = SettingsSource {
            settings_path: PathBuf::from("/cfg/settings.toml"),
            keymap_path: PathBuf::from("/cfg/keymap.toml"),
            contents: Some(v.model().to_toml_string()),
            theme_ids: Vec::new(),
        };
        assert_eq!(v.apply_source(&echo), ReloadOutcome::SelfWriteEcho);
        let external = SettingsSource {
            contents: Some(SOURCE.replace("tab_size = 4", "tab_size = 8")),
            ..echo
        };
        assert_eq!(v.apply_source(&external), ReloadOutcome::Reloaded);
        assert_eq!(v.settings().editor.tab_size, 8);
        assert!(!v.settings().editor.vim_mode);
    }

    #[test]
    fn invalid_or_missing_source_falls_back_to_defaults() {
        let v = SettingsView::from_source(Some(&SettingsSource {
            settings_path: PathBuf::from("/cfg/settings.toml"),
            keymap_path: PathBuf::from("/cfg/keymap.toml"),
            contents: Some("[editor\n".to_owned()),
            theme_ids: Vec::new(),
        }))
        .expect("view");
        assert!(v.model().load_error().is_some());
        assert_eq!(v.settings(), &KovaSettings::default());
        let v = SettingsView::from_source(None).expect("view");
        assert_eq!(v.settings(), &KovaSettings::default());
        assert_eq!(v.settings_path(), None);
    }
}
