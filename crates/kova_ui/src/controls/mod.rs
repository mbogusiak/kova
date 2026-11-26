//! Aether-styled control primitives (`KOVA-029` FR-4): [`ToggleSwitch`], [`NumericInput`],
//! [`TextInput`], [`SelectInput`], [`Button`] and [`Badge`].
//!
//! Every colour comes from [`KovaTheme`] tokens (`surfaces.input`, `borders.input`,
//! `borders.focus`, `editor.diagnostic_error` and the `plugins.*` variants), so the controls
//! look right in both `Kova Islands Dark` and `Kova Light`. The primitives only build
//! elements: the caller attaches click handlers to the returned [`Stateful<Div>`] and owns
//! the state. [`TextInputState`] is the headless single-line editor behind text and numeric
//! inputs.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Div, ElementId, FontWeight, Hsla, InteractiveElement as _, ParentElement as _, SharedString,
    Stateful, Styled, div, px,
};
use kova_theme::PluginVariant;

use crate::theme::{KovaTheme, hsla_from_rgba};

/// Height of every input-like control in logical pixels.
pub const CONTROL_HEIGHT_PX: f32 = 26.0;
const CONTROL_RADIUS_PX: f32 = 5.0;
const TOGGLE_WIDTH_PX: f32 = 34.0;
const TOGGLE_HEIGHT_PX: f32 = 18.0;
const TOGGLE_KNOB_PX: f32 = 12.0;

/// Colours of one [`PluginVariant`] resolved for rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VariantColors {
    /// Filled background.
    pub background: Hsla,
    /// Foreground on `background`.
    pub foreground: Hsla,
    /// Border.
    pub border: Hsla,
    /// Hover background.
    pub hover: Hsla,
    /// Soft background (banners).
    pub subtle_bg: Hsla,
    /// Foreground on `subtle_bg`.
    pub subtle_fg: Hsla,
}

/// Theme tokens used by the controls, resolved once per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlPalette {
    /// `surfaces.input`.
    pub input: Hsla,
    /// `surfaces.elevated` (drop-down lists).
    pub elevated: Hsla,
    /// `surfaces.hover`.
    pub hover: Hsla,
    /// `borders.input`.
    pub border: Hsla,
    /// `borders.focus`.
    pub focus: Hsla,
    /// `text.primary`.
    pub text: Hsla,
    /// `text.secondary`.
    pub text_secondary: Hsla,
    /// `text.muted`.
    pub text_muted: Hsla,
    /// `editor.diagnostic_error`.
    pub error: Hsla,
    /// `editor.cursor`.
    pub caret: Hsla,
    /// `plugins.primary`.
    pub primary: VariantColors,
    /// `plugins.muted`.
    pub muted: VariantColors,
    /// `plugins.warning`.
    pub warning: VariantColors,
    /// `plugins.error`.
    pub danger: VariantColors,
    /// `plugins.accent`.
    pub accent: VariantColors,
}

impl ControlPalette {
    /// Resolves the palette from the active theme.
    #[must_use]
    pub fn from_theme(theme: &KovaTheme) -> Self {
        let variant = |v: PluginVariant| {
            let t = theme.tokens().plugins.get(v);
            VariantColors {
                background: hsla_from_rgba(t.background),
                foreground: hsla_from_rgba(t.foreground),
                border: hsla_from_rgba(t.border),
                hover: hsla_from_rgba(t.hover_bg),
                subtle_bg: hsla_from_rgba(t.subtle_bg),
                subtle_fg: hsla_from_rgba(t.subtle_fg),
            }
        };
        Self {
            input: hsla_from_rgba(theme.surfaces().input),
            elevated: hsla_from_rgba(theme.surfaces().elevated),
            hover: hsla_from_rgba(theme.surfaces().hover),
            border: hsla_from_rgba(theme.borders().input),
            focus: hsla_from_rgba(theme.borders().focus),
            text: hsla_from_rgba(theme.text().primary),
            text_secondary: hsla_from_rgba(theme.text().secondary),
            text_muted: hsla_from_rgba(theme.text().muted),
            error: hsla_from_rgba(theme.editor().diagnostic_error),
            caret: hsla_from_rgba(theme.editor().cursor),
            primary: variant(PluginVariant::Primary),
            muted: variant(PluginVariant::Muted),
            warning: variant(PluginVariant::Warning),
            danger: variant(PluginVariant::Error),
            accent: variant(PluginVariant::Accent),
        }
    }

    /// Colours of `variant`.
    #[must_use]
    pub const fn variant(&self, variant: PluginVariant) -> VariantColors {
        match variant {
            PluginVariant::Primary => self.primary,
            PluginVariant::Muted => self.muted,
            PluginVariant::Warning => self.warning,
            PluginVariant::Error => self.danger,
            PluginVariant::Accent => self.accent,
        }
    }

    fn border_for(&self, focused: bool, invalid: bool) -> Hsla {
        if invalid {
            self.error
        } else if focused {
            self.focus
        } else {
            self.border
        }
    }
}

/// On / off switch (`ON` = `plugins.primary`, `OFF` = `surfaces.input` + `borders.input`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToggleSwitch {
    /// Current state.
    pub on: bool,
    /// Whether the switch has keyboard focus (focus ring).
    pub focused: bool,
}

impl ToggleSwitch {
    /// Builds the switch element.
    #[must_use]
    pub fn render(self, id: impl Into<ElementId>, p: &ControlPalette) -> Stateful<Div> {
        let (track, border) = if self.on {
            (p.primary.background, p.primary.border)
        } else {
            (p.input, p.border)
        };
        let knob = if self.on {
            p.primary.foreground
        } else {
            p.text_muted
        };
        let pad = (TOGGLE_HEIGHT_PX - TOGGLE_KNOB_PX) / 2.0 - 1.0;
        let inner = div()
            .w(px(TOGGLE_WIDTH_PX))
            .h(px(TOGGLE_HEIGHT_PX))
            .rounded(px(TOGGLE_HEIGHT_PX / 2.0))
            .border_1()
            .border_color(border)
            .bg(track)
            .flex()
            .items_center()
            .px(px(pad))
            .when(self.on, Styled::justify_end)
            .child(
                div()
                    .size(px(TOGGLE_KNOB_PX))
                    .rounded(px(TOGGLE_KNOB_PX / 2.0))
                    .bg(knob),
            );
        div()
            .id(id)
            .flex_none()
            .p(px(2.0))
            .rounded(px(TOGGLE_HEIGHT_PX / 2.0 + 2.0))
            .border_1()
            .border_color(if self.focused {
                p.focus
            } else {
                gpui::transparent_black()
            })
            .cursor_pointer()
            .child(inner)
    }
}

/// Single-line text field with a caret while focused and a red border when invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    /// Text left of the caret (or the whole text when unfocused).
    pub before_caret: String,
    /// Text right of the caret.
    pub after_caret: String,
    /// Placeholder shown when the text is empty.
    pub placeholder: String,
    /// Whether the field has keyboard focus.
    pub focused: bool,
    /// Whether the current text was rejected by validation.
    pub invalid: bool,
}

impl TextInput {
    /// Unfocused field showing `text`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            before_caret: text.into(),
            after_caret: String::new(),
            placeholder: String::new(),
            focused: false,
            invalid: false,
        }
    }

    /// Focused field editing `state`.
    #[must_use]
    pub fn editing(state: &TextInputState) -> Self {
        let (before, after) = state.split_at_cursor();
        Self {
            before_caret: before.to_owned(),
            after_caret: after.to_owned(),
            placeholder: String::new(),
            focused: true,
            invalid: false,
        }
    }

    /// Sets the placeholder.
    #[must_use]
    pub fn placeholder(mut self, text: impl Into<String>) -> Self {
        self.placeholder = text.into();
        self
    }

    /// Marks the field invalid.
    #[must_use]
    pub const fn invalid(mut self, invalid: bool) -> Self {
        self.invalid = invalid;
        self
    }

    /// Builds the field element (`width_px` wide).
    #[must_use]
    pub fn render(
        self,
        id: impl Into<ElementId>,
        width_px: f32,
        p: &ControlPalette,
    ) -> Stateful<Div> {
        let empty = self.before_caret.is_empty() && self.after_caret.is_empty();
        let mut content = div()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap();
        if empty && !self.focused {
            content = content
                .text_color(p.text_muted)
                .child(SharedString::from(self.placeholder));
        } else {
            content = content
                .text_color(p.text)
                .child(SharedString::from(self.before_caret));
            if self.focused {
                content = content.child(div().w(px(1.5)).h(px(14.0)).bg(p.caret));
            }
            content = content.child(SharedString::from(self.after_caret));
        }
        div()
            .id(id)
            .w(px(width_px))
            .h(px(CONTROL_HEIGHT_PX))
            .flex_none()
            .flex()
            .items_center()
            .px(px(8.0))
            .rounded(px(CONTROL_RADIUS_PX))
            .bg(p.input)
            .border_1()
            .border_color(p.border_for(self.focused, self.invalid))
            .text_size(px(12.0))
            .cursor_text()
            .child(content)
    }
}

/// Numeric field: a [`TextInput`] with right-aligned digits and an optional unit suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumericInput {
    /// Underlying text field.
    pub field: TextInput,
    /// Optional unit (`MiB`, `px`) shown after the value.
    pub unit: Option<&'static str>,
}

impl NumericInput {
    /// Builds the field element.
    #[must_use]
    pub fn render(self, id: impl Into<ElementId>, p: &ControlPalette) -> Stateful<Div> {
        let unit = self.unit;
        let mut field = self.field.render(id, 110.0, p).justify_between();
        if let Some(unit) = unit {
            field = field.child(
                div()
                    .text_color(p.text_muted)
                    .text_size(px(11.0))
                    .child(unit),
            );
        }
        field
    }
}

/// Drop-down selector showing the current option label and a chevron.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectInput {
    /// Label of the selected option.
    pub label: String,
    /// Whether the selector has keyboard focus.
    pub focused: bool,
    /// Whether the option list is open.
    pub open: bool,
}

impl SelectInput {
    /// Builds the selector element.
    #[must_use]
    pub fn render(self, id: impl Into<ElementId>, p: &ControlPalette) -> Stateful<Div> {
        div()
            .id(id)
            .w(px(200.0))
            .h(px(CONTROL_HEIGHT_PX))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .px(px(8.0))
            .rounded(px(CONTROL_RADIUS_PX))
            .bg(p.input)
            .border_1()
            .border_color(p.border_for(self.focused || self.open, false))
            .text_size(px(12.0))
            .text_color(p.text)
            .cursor_pointer()
            .hover(|s| s.bg(p.hover))
            .child(SharedString::from(self.label))
            .child(
                div()
                    .text_color(p.text_muted)
                    .child(if self.open { "▴" } else { "▾" }),
            )
    }

    /// Builds one row of the open option list (`surfaces.elevated`).
    #[must_use]
    pub fn option_row(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        selected: bool,
        p: &ControlPalette,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .w_full()
            .h(px(24.0))
            .flex()
            .items_center()
            .px(px(8.0))
            .rounded(px(4.0))
            .text_size(px(12.0))
            .text_color(if selected {
                p.primary.subtle_fg
            } else {
                p.text
            })
            .when(selected, |row| row.bg(p.primary.subtle_bg))
            .cursor_pointer()
            .hover(|s| s.bg(p.hover))
            .child(label.into())
    }
}

/// Push button in one of the [`PluginVariant`] styles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    /// Caption.
    pub label: SharedString,
    /// Semantic style.
    pub variant: PluginVariant,
    /// Ghost buttons have no fill until hovered.
    pub ghost: bool,
}

impl Button {
    /// Filled button.
    #[must_use]
    pub fn new(label: impl Into<SharedString>, variant: PluginVariant) -> Self {
        Self {
            label: label.into(),
            variant,
            ghost: false,
        }
    }

    /// Ghost (borderless until hovered) button.
    #[must_use]
    pub fn ghost(label: impl Into<SharedString>, variant: PluginVariant) -> Self {
        Self {
            label: label.into(),
            variant,
            ghost: true,
        }
    }

    /// Builds the button element.
    #[must_use]
    pub fn render(self, id: impl Into<ElementId>, p: &ControlPalette) -> Stateful<Div> {
        let c = p.variant(self.variant);
        let base = div()
            .id(id)
            .flex_none()
            .h(px(CONTROL_HEIGHT_PX - 2.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(CONTROL_RADIUS_PX))
            .text_size(px(12.0))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .child(self.label);
        if self.ghost {
            base.text_color(c.subtle_fg)
                .hover(move |s| s.bg(c.subtle_bg))
        } else {
            base.bg(c.background)
                .border_1()
                .border_color(c.border)
                .text_color(c.foreground)
                .hover(move |s| s.bg(c.hover))
        }
    }
}

/// Small pill label in one of the [`PluginVariant`] styles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    /// Text.
    pub label: SharedString,
    /// Semantic style.
    pub variant: PluginVariant,
}

impl Badge {
    /// Creates a badge.
    #[must_use]
    pub fn new(label: impl Into<SharedString>, variant: PluginVariant) -> Self {
        Self {
            label: label.into(),
            variant,
        }
    }

    /// Builds the badge element.
    #[must_use]
    pub fn render(self, p: &ControlPalette) -> Div {
        let c = p.variant(self.variant);
        div()
            .flex_none()
            .px(px(6.0))
            .py(px(1.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(c.border)
            .bg(c.subtle_bg)
            .text_color(c.subtle_fg)
            .text_size(px(10.0))
            .child(self.label)
    }
}

/// Result of one key in a [`TextInputState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputEvent {
    /// Text or caret changed.
    Edited,
    /// `Enter`: commit the text.
    Submit,
    /// `Escape`: discard the edit.
    Cancel,
    /// Key has no meaning here.
    Ignored,
}

/// Headless single-line editor (text and caret) behind [`TextInput`] / [`NumericInput`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInputState {
    text: String,
    /// Caret position in characters.
    cursor_col: usize,
}

impl TextInputState {
    /// Editor holding `text` with the caret at the end.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor_col = text.chars().count();
        Self { text, cursor_col }
    }

    /// Current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Caret column in characters.
    #[must_use]
    pub const fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    /// Text before and after the caret.
    #[must_use]
    pub fn split_at_cursor(&self) -> (&str, &str) {
        self.text.split_at(self.byte_at(self.cursor_col))
    }

    fn byte_at(&self, col: usize) -> usize {
        self.text
            .char_indices()
            .nth(col)
            .map_or(self.text.len(), |(i, _)| i)
    }

    /// Inserts typed text (control characters are dropped).
    pub fn insert(&mut self, text: &str) -> TextInputEvent {
        let printable: String = text.chars().filter(|c| !c.is_control()).collect();
        if printable.is_empty() {
            return TextInputEvent::Ignored;
        }
        let at = self.byte_at(self.cursor_col);
        self.text.insert_str(at, &printable);
        self.cursor_col += printable.chars().count();
        TextInputEvent::Edited
    }

    /// Handles a named GPUI key (`enter`, `escape`, `backspace`, `delete`, `left`, `right`,
    /// `home`, `end`).
    pub fn handle_named_key(&mut self, key: &str) -> TextInputEvent {
        let len = self.text.chars().count();
        match key {
            "enter" => TextInputEvent::Submit,
            "escape" => TextInputEvent::Cancel,
            "backspace" if self.cursor_col > 0 => {
                let start = self.byte_at(self.cursor_col - 1);
                let end = self.byte_at(self.cursor_col);
                self.text.replace_range(start..end, "");
                self.cursor_col -= 1;
                TextInputEvent::Edited
            }
            "delete" if self.cursor_col < len => {
                let start = self.byte_at(self.cursor_col);
                let end = self.byte_at(self.cursor_col + 1);
                self.text.replace_range(start..end, "");
                TextInputEvent::Edited
            }
            "left" if self.cursor_col > 0 => {
                self.cursor_col -= 1;
                TextInputEvent::Edited
            }
            "right" if self.cursor_col < len => {
                self.cursor_col += 1;
                TextInputEvent::Edited
            }
            "home" => {
                self.cursor_col = 0;
                TextInputEvent::Edited
            }
            "end" => {
                self.cursor_col = len;
                TextInputEvent::Edited
            }
            _ => TextInputEvent::Ignored,
        }
    }

    /// Clears the text (`Cmd+Backspace`).
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_col = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_input_state_edits_unicode_and_reports_submit_cancel() {
        let mut s = TextInputState::new("Iosevka");
        assert_eq!(s.cursor_col(), 7);
        assert_eq!(s.handle_named_key("backspace"), TextInputEvent::Edited);
        assert_eq!(s.text(), "Iosevk");
        s.handle_named_key("home");
        assert_eq!(s.insert("Zażółć "), TextInputEvent::Edited);
        assert_eq!(s.split_at_cursor(), ("Zażółć ", "Iosevk"));
        assert_eq!(s.insert("\u{7f}"), TextInputEvent::Ignored);
        s.handle_named_key("delete");
        assert_eq!(s.text(), "Zażółć osevk");
        assert_eq!(s.handle_named_key("enter"), TextInputEvent::Submit);
        assert_eq!(s.handle_named_key("escape"), TextInputEvent::Cancel);
        assert_eq!(s.handle_named_key("f5"), TextInputEvent::Ignored);
        s.clear();
        assert_eq!(s.text(), "");
        assert_eq!(s.handle_named_key("backspace"), TextInputEvent::Ignored);
    }

    #[test]
    fn controls_resolve_from_both_builtin_themes() {
        for theme in [KovaTheme::islands_dark(), light_theme()] {
            let p = ControlPalette::from_theme(&theme);
            assert_ne!(p.border_for(false, false), p.border_for(true, false));
            assert_eq!(p.border_for(true, true), p.error);
            assert_ne!(p.primary.background, p.input);
        }
    }

    fn light_theme() -> KovaTheme {
        let aether = kova_theme::AetherTheme::builtin(kova_theme::BuiltinTheme::KovaLight);
        KovaTheme::from_aether(std::sync::Arc::new(aether), 1)
    }
}
