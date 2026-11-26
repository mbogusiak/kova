//! Inline *Find / Replace* bar (`Cmd+F` / `Cmd+R` / `Ctrl+F` / `Ctrl+R`) for [`EditorView`].
//!
//! Splits headless search/replace state and match computation ([`FindBarState`]) from the
//! floating GPUI bar rendered at the top-right of the editor canvas.

use std::ops::Range;

use gpui::{
    ClickEvent, Context, Div, FocusHandle, FontWeight, InteractiveElement as _, MouseButton,
    MouseDownEvent, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, px,
};
use kova_buffer::{BufferSnapshot, BufferVersion};

use crate::controls::{ControlPalette, TextInputState};
use crate::editor::EditorView;
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Which input field inside the Find/Replace bar holds keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FindBarField {
    /// Search pattern input (`Find`).
    #[default]
    Find,
    /// Replacement text input (`Replace`).
    Replace,
}

/// Result of feeding a keystroke to [`FindBarState::handle_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindKeyOutcome {
    /// Key was not handled by the Find/Replace bar.
    Ignored,
    /// Bar state changed (input text, caret, or field focus).
    Consumed,
    /// Close the bar and return focus to the editor (`Esc`).
    Close,
    /// Jump to the next match (`Enter`, `Cmd+G`, `F3`).
    FindNext,
    /// Jump to the previous match (`Shift+Enter`, `Cmd+Shift+G`, `Shift+F3`).
    FindPrev,
    /// Replace the active match and advance to the next (`Enter` in Replace field).
    ReplaceCurrent,
    /// Replace every match in the document.
    ReplaceAll,
    /// Switch to / focus Find mode (`Cmd+F` / `Ctrl+F`).
    OpenFind,
    /// Switch to / focus Replace mode (`Cmd+R` / `Ctrl+R`).
    OpenReplace,
}

/// Pure headless state of the inline *Find / Replace* bar.
#[derive(Debug)]
pub struct FindBarState {
    open: bool,
    replace_mode: bool,
    focus_field: FindBarField,
    case_sensitive: bool,
    find_input: TextInputState,
    replace_input: TextInputState,
    matches: Vec<Range<usize>>,
    current_match: Option<usize>,
    synced_version: BufferVersion,
}

impl Default for FindBarState {
    fn default() -> Self {
        Self::new()
    }
}

impl FindBarState {
    /// Creates a closed [`FindBarState`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: false,
            replace_mode: false,
            focus_field: FindBarField::Find,
            case_sensitive: false,
            find_input: TextInputState::default(),
            replace_input: TextInputState::default(),
            matches: Vec::new(),
            current_match: None,
            synced_version: BufferVersion::new(0),
        }
    }

    /// Whether the inline bar is currently visible.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the Replace row is expanded.
    #[must_use]
    pub const fn is_replace_mode(&self) -> bool {
        self.replace_mode
    }

    /// Currently focused field (`Find` or `Replace`).
    #[must_use]
    pub const fn focus_field(&self) -> FindBarField {
        self.focus_field
    }

    /// Whether case-sensitive matching (`Aa`) is active.
    #[must_use]
    pub const fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    /// Search query input state.
    #[must_use]
    pub const fn find_input(&self) -> &TextInputState {
        &self.find_input
    }

    /// Replacement text input state.
    #[must_use]
    pub const fn replace_input(&self) -> &TextInputState {
        &self.replace_input
    }

    /// Sorted non-overlapping match byte ranges in the active buffer.
    #[must_use]
    pub fn matches(&self) -> &[Range<usize>] {
        &self.matches
    }

    /// Index of the active match inside [`Self::matches`], if any.
    #[must_use]
    pub const fn current_match(&self) -> Option<usize> {
        self.current_match
    }

    /// Active match byte range, if one is selected.
    #[must_use]
    pub fn current_match_range(&self) -> Option<Range<usize>> {
        self.current_match
            .and_then(|idx| self.matches.get(idx).cloned())
    }

    /// Match counter label (`"3/14"`, `"No results"`, or `""` when query is empty).
    #[must_use]
    pub fn match_counter_label(&self) -> String {
        if self.find_input.text().is_empty() {
            return String::new();
        }
        if self.matches.is_empty() {
            return "No results".to_owned();
        }
        let active = self.current_match.map_or(1, |idx| idx + 1);
        format!("{active}/{}", self.matches.len())
    }

    /// Opens (or switches mode of) the bar, optionally prefilling `Find` from `prefill`.
    pub fn open(
        &mut self,
        replace: bool,
        prefill: Option<&str>,
        snapshot: &BufferSnapshot,
        caret_byte: usize,
    ) {
        self.open = true;
        self.replace_mode = replace;
        if let Some(text) = prefill
            && !text.is_empty()
            && !text.contains('\n')
        {
            self.find_input = TextInputState::new(text);
        }
        self.focus_field = if replace && !self.find_input.text().is_empty() {
            FindBarField::Replace
        } else {
            FindBarField::Find
        };
        self.recompute(snapshot, caret_byte);
    }

    /// Closes the bar and clears active match highlights while remembering the last query.
    pub fn close(&mut self) {
        self.open = false;
        self.matches.clear();
        self.current_match = None;
    }

    /// Switches focused field between `Find` and `Replace`.
    pub const fn set_focus_field(&mut self, field: FindBarField) {
        if self.replace_mode {
            self.focus_field = field;
        } else {
            self.focus_field = FindBarField::Find;
        }
    }

    /// Toggles `Aa` case sensitivity and recomputes matches.
    pub fn toggle_case_sensitive(&mut self, snapshot: &BufferSnapshot, prefer_near: usize) {
        self.case_sensitive = !self.case_sensitive;
        self.recompute(snapshot, prefer_near);
    }

    /// Recomputes all matches against `snapshot`, selecting the match nearest `prefer_near`.
    pub fn recompute(&mut self, snapshot: &BufferSnapshot, prefer_near: usize) {
        self.synced_version = snapshot.version();
        let query = self.find_input.text();
        if query.is_empty() {
            self.matches.clear();
            self.current_match = None;
            return;
        }
        let text = snapshot.text();
        self.matches = find_all_matches(&text, query, self.case_sensitive);
        self.current_match = if self.matches.is_empty() {
            None
        } else {
            Some(
                self.matches
                    .iter()
                    .position(|r| r.start >= prefer_near)
                    .unwrap_or(0),
            )
        };
    }

    /// Advances (`forward = true`) or retreats (`forward = false`) to the next/previous match
    /// with wrap-around, recomputing first if the buffer changed.
    pub fn step(
        &mut self,
        forward: bool,
        snapshot: &BufferSnapshot,
        caret_byte: usize,
    ) -> Option<Range<usize>> {
        let needs_sync = self.synced_version != snapshot.version()
            || (self.matches.is_empty() && !self.find_input.text().is_empty());
        if needs_sync {
            self.recompute(snapshot, caret_byte);
        }
        let len = self.matches.len();
        if len == 0 {
            self.current_match = None;
            return None;
        }
        let next_idx = match self.current_match {
            Some(cur) if cur < len => {
                if forward {
                    (cur + 1) % len
                } else {
                    (cur + len - 1) % len
                }
            }
            _ => {
                if forward {
                    self.matches
                        .iter()
                        .position(|r| r.start > caret_byte)
                        .unwrap_or(0)
                } else {
                    self.matches
                        .iter()
                        .rposition(|r| r.start < caret_byte)
                        .unwrap_or(len - 1)
                }
            }
        };
        self.current_match = Some(next_idx);
        self.matches.get(next_idx).cloned()
    }

    /// Feeds one keystroke to the active input field (`Find` or `Replace`).
    pub fn handle_key(&mut self, keystroke: &gpui::Keystroke, macos: bool) -> FindKeyOutcome {
        let mods = &keystroke.modifiers;
        let key = keystroke.key.as_str();
        let primary = if macos { mods.platform } else { mods.control };
        let other_mod = if macos { mods.control } else { mods.platform };

        if key == "escape" && !mods.platform && !mods.control && !mods.alt {
            return FindKeyOutcome::Close;
        }
        if primary && !other_mod && !mods.alt {
            if key == "f" && !mods.shift {
                return FindKeyOutcome::OpenFind;
            }
            if key == "r" && !mods.shift {
                return FindKeyOutcome::OpenReplace;
            }
            if macos && key == "g" {
                return if mods.shift {
                    FindKeyOutcome::FindPrev
                } else {
                    FindKeyOutcome::FindNext
                };
            }
        }
        if !macos && !mods.platform && !mods.control && !mods.alt && key == "f3" {
            return if mods.shift {
                FindKeyOutcome::FindPrev
            } else {
                FindKeyOutcome::FindNext
            };
        }
        if key == "tab" && !mods.platform && !mods.control && !mods.alt {
            if self.replace_mode {
                self.focus_field = match self.focus_field {
                    FindBarField::Find => FindBarField::Replace,
                    FindBarField::Replace => FindBarField::Find,
                };
            }
            return FindKeyOutcome::Consumed;
        }
        if key == "enter" && !mods.platform && !mods.control && !mods.alt {
            return match self.focus_field {
                FindBarField::Replace if !mods.shift => FindKeyOutcome::ReplaceCurrent,
                _ if mods.shift => FindKeyOutcome::FindPrev,
                _ => FindKeyOutcome::FindNext,
            };
        }
        if key == "backspace" && (mods.platform || mods.control) && !mods.alt {
            let input = self.active_input_mut();
            if !input.text().is_empty() {
                input.clear();
            }
            return FindKeyOutcome::Consumed;
        }
        if mods.platform || mods.control {
            return FindKeyOutcome::Ignored;
        }

        if !mods.alt
            && matches!(
                key,
                "backspace" | "delete" | "left" | "right" | "home" | "end"
            )
        {
            let _ = self.active_input_mut().handle_named_key(key);
            return FindKeyOutcome::Consumed;
        }

        let is_character_key = key.chars().count() == 1 || key == "space";
        if (!mods.alt || is_character_key)
            && let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text
                .chars()
                .any(|ch| ch.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&ch))
        {
            let _ = self.active_input_mut().insert(text);
            return FindKeyOutcome::Consumed;
        }

        if !mods.alt && key == "space" {
            let _ = self.active_input_mut().insert(" ");
            return FindKeyOutcome::Consumed;
        }

        FindKeyOutcome::Ignored
    }

    const fn active_input_mut(&mut self) -> &mut TextInputState {
        match self.focus_field {
            FindBarField::Find => &mut self.find_input,
            FindBarField::Replace => &mut self.replace_input,
        }
    }
}

/// Finds all non-overlapping UTF-8 byte ranges of `needle` inside `haystack`.
#[must_use]
pub fn find_all_matches(haystack: &str, needle: &str, case_sensitive: bool) -> Vec<Range<usize>> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    if case_sensitive {
        let mut ranges = Vec::new();
        let mut pos = 0;
        while pos + needle.len() <= haystack.len() {
            if let Some(rel) = haystack[pos..].find(needle) {
                let start = pos + rel;
                let end = start + needle.len();
                ranges.push(start..end);
                pos = end;
            } else {
                break;
            }
        }
        return ranges;
    }

    if needle.is_ascii() {
        let h_bytes = haystack.as_bytes();
        let n_bytes = needle.as_bytes();
        let mut ranges = Vec::new();
        let mut i = 0;
        while i + n_bytes.len() <= h_bytes.len() {
            if h_bytes[i..i + n_bytes.len()].eq_ignore_ascii_case(n_bytes) {
                ranges.push(i..i + n_bytes.len());
                i += n_bytes.len();
            } else {
                i += 1;
            }
        }
        return ranges;
    }

    let needle_chars: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if needle_chars.is_empty() {
        return Vec::new();
    }
    let h_chars: Vec<(usize, char)> = haystack
        .char_indices()
        .map(|(byte, ch)| (byte, ch.to_lowercase().next().unwrap_or(ch)))
        .collect();
    let mut ranges = Vec::new();
    let mut idx = 0usize;
    while idx + needle_chars.len() <= h_chars.len() {
        let matched = h_chars[idx..idx + needle_chars.len()]
            .iter()
            .zip(needle_chars.iter())
            .all(|((_, h), n)| h == n);
        if matched {
            let start = h_chars[idx].0;
            let end = h_chars
                .get(idx + needle_chars.len())
                .map_or(haystack.len(), |(b, _)| *b);
            ranges.push(start..end);
            idx += needle_chars.len();
        } else {
            idx += 1;
        }
    }
    ranges
}

/// Renders the compact inline *Find / Replace* overlay at the top-right of [`EditorView`].
pub(crate) fn render_find_bar(
    state: &FindBarState,
    focus_handle: &FocusHandle,
    read_only: bool,
    theme: &KovaTheme,
    cx: &Context<'_, EditorView>,
) -> Div {
    let p = ControlPalette::from_theme(theme);
    let elevated = hsla_from_rgba(theme.surfaces().elevated);
    let border_strong = hsla_from_rgba(theme.borders().strong);
    let border_subtle = hsla_from_rgba(theme.borders().subtle);

    let mut panel = div()
        .id("editor-find-bar")
        .track_focus(focus_handle)
        .w(px(420.0))
        .p(px(8.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(border_strong)
        .bg(elevated)
        .shadow_md()
        .occlude()
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
        })
        .child(render_find_row(state, &p, cx));

    if state.is_replace_mode() {
        panel = panel.child(render_replace_row(state, read_only, &p, border_subtle, cx));
    }

    div().absolute().top(px(8.0)).right(px(16.0)).child(panel)
}

fn render_find_row(state: &FindBarState, p: &ControlPalette, cx: &Context<'_, EditorView>) -> Div {
    let counter = state.match_counter_label();
    let no_results = !state.find_input().text().is_empty() && state.matches().is_empty();
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(render_bar_input(
            "find-query-input",
            state.find_input(),
            "Find…",
            state.focus_field() == FindBarField::Find,
            no_results,
            p,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.focus_find_field(FindBarField::Find, window, cx);
            }),
        ))
        .child(render_toggle_button(
            "find-case-btn",
            "Aa",
            state.case_sensitive(),
            p,
            cx.listener(|this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.toggle_find_case_sensitive(window, cx);
            }),
        ))
        .child(
            div()
                .min_w(px(54.0))
                .text_size(px(11.0))
                .text_color(if no_results { p.error } else { p.text_muted })
                .child(if counter.is_empty() {
                    SharedString::from(" ")
                } else {
                    SharedString::from(counter)
                }),
        )
        .child(render_icon_button(
            "find-prev-btn",
            "↑",
            false,
            p,
            cx.listener(|this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.find_prev(window, cx);
            }),
        ))
        .child(render_icon_button(
            "find-next-btn",
            "↓",
            false,
            p,
            cx.listener(|this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.find_next(window, cx);
            }),
        ))
        .child(render_icon_button(
            "find-close-btn",
            "×",
            false,
            p,
            cx.listener(|this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.close_find_bar(true, window, cx);
            }),
        ))
}

fn render_replace_row(
    state: &FindBarState,
    read_only: bool,
    p: &ControlPalette,
    border_subtle: gpui::Hsla,
    cx: &Context<'_, EditorView>,
) -> Div {
    let replace_disabled = read_only || state.matches().is_empty();
    div()
        .pt(px(4.0))
        .border_t_1()
        .border_color(border_subtle)
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(render_bar_input(
            "replace-text-input",
            state.replace_input(),
            "Replace…",
            state.focus_field() == FindBarField::Replace,
            false,
            p,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.focus_find_field(FindBarField::Replace, window, cx);
            }),
        ))
        .child(render_icon_button(
            "find-replace-one-btn",
            "Replace",
            replace_disabled,
            p,
            cx.listener(|this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.replace_current_match(window, cx);
            }),
        ))
        .child(render_icon_button(
            "find-replace-all-btn",
            "All",
            replace_disabled,
            p,
            cx.listener(|this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.replace_all_matches(window, cx);
            }),
        ))
}

fn render_bar_input(
    id: &'static str,
    input: &TextInputState,
    placeholder: &'static str,
    focused: bool,
    error: bool,
    p: &ControlPalette,
    on_down: impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    let (before, after) = input.split_at_cursor();
    let empty = before.is_empty() && after.is_empty();
    let border = if error {
        p.danger.border
    } else if focused {
        p.focus
    } else {
        p.border
    };
    let mut row = div()
        .id(id)
        .flex_1()
        .min_w(px(140.0))
        .h(px(26.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .rounded(px(5.0))
        .border_1()
        .border_color(border)
        .bg(p.input)
        .text_size(px(12.5))
        .text_color(p.text)
        .on_mouse_down(MouseButton::Left, on_down)
        .child(SharedString::from(before.to_owned()));
    if focused {
        row = row.child(div().w(px(1.5)).h(px(14.0)).bg(p.caret));
    }
    row = row.child(SharedString::from(after.to_owned()));
    if empty {
        row = row.child(
            div()
                .ml(px(2.0))
                .text_size(px(12.0))
                .text_color(p.text_muted)
                .child(placeholder),
        );
    }
    row
}

fn render_toggle_button(
    id: &'static str,
    label: &'static str,
    active: bool,
    p: &ControlPalette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    let mut btn = div()
        .id(id)
        .h(px(24.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .border_1()
        .text_size(px(11.5))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .on_click(on_click);
    btn = if active {
        btn.bg(p.primary.subtle_bg)
            .border_color(p.primary.border)
            .text_color(p.primary.subtle_fg)
    } else {
        btn.bg(p.input)
            .border_color(p.border)
            .text_color(p.text_muted)
            .hover(|s| s.bg(p.hover))
    };
    btn.child(label)
}

fn render_icon_button(
    id: &'static str,
    label: &'static str,
    disabled: bool,
    p: &ControlPalette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    let mut btn = div()
        .id(id)
        .h(px(24.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .border_1()
        .border_color(p.border)
        .bg(p.input)
        .text_size(px(11.5));
    if disabled {
        btn = btn.text_color(p.text_muted);
    } else {
        btn = btn
            .text_color(p.text)
            .cursor_pointer()
            .hover(|s| s.bg(p.hover))
            .on_click(on_click);
    }
    btn.child(label)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "unit tests abort on invalid keystroke specs"
    )]

    use kova_buffer::Buffer;

    use super::*;

    fn ks(source: &str) -> gpui::Keystroke {
        gpui::Keystroke::parse(source).expect("valid keystroke")
    }

    fn ks_char(source: &str, key_char: &str) -> gpui::Keystroke {
        let mut s = ks(source);
        s.key_char = Some(key_char.to_owned());
        s
    }

    #[test]
    fn recompute_finds_case_insensitive_and_case_sensitive_matches_with_wrapping() {
        let buf = Buffer::from_text("Foo bar foo FOO");
        let snap = buf.snapshot();
        let mut bar = FindBarState::new();
        bar.open(false, Some("foo"), &snap, 5);
        assert!(bar.is_open());
        assert_eq!(bar.matches(), &[0..3, 8..11, 12..15]);
        assert_eq!(bar.current_match(), Some(1));
        assert_eq!(bar.match_counter_label(), "2/3");

        assert_eq!(bar.step(true, &snap, 8), Some(12..15));
        assert_eq!(bar.step(true, &snap, 12), Some(0..3));
        assert_eq!(bar.step(false, &snap, 0), Some(12..15));

        bar.toggle_case_sensitive(&snap, 0);
        assert!(bar.case_sensitive());
        assert_eq!(bar.matches(), std::slice::from_ref(&(8..11)));
        assert_eq!(bar.match_counter_label(), "1/1");
    }

    #[test]
    fn find_all_matches_handles_unicode_case_insensitive_safely() {
        let text = "Zażółć zażółć ZAŻÓŁĆ";
        let ranges = find_all_matches(text, "zażółć", false);
        let slices: Vec<&str> = ranges.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(slices, vec!["Zażółć", "zażółć", "ZAŻÓŁĆ"]);
    }

    #[test]
    fn keyboard_handling_supports_option_chars_tab_enter_and_escape() {
        let buf = Buffer::from_text("zażółć gęślą");
        let snap = buf.snapshot();
        let mut bar = FindBarState::new();
        bar.open(true, Some("zaż"), &snap, 0);
        // Prefilled Find -> Replace mode focuses Replace field.
        assert_eq!(bar.focus_field(), FindBarField::Replace);

        assert_eq!(
            bar.handle_key(&ks_char("alt-z", "ż"), true),
            FindKeyOutcome::Consumed
        );
        assert_eq!(bar.replace_input().text(), "ż");

        assert_eq!(
            bar.handle_key(&ks_char("enter", "\n"), true),
            FindKeyOutcome::ReplaceCurrent
        );
        assert_eq!(
            bar.handle_key(&ks_char("tab", "\t"), true),
            FindKeyOutcome::Consumed
        );
        assert_eq!(bar.focus_field(), FindBarField::Find);
        assert_eq!(
            bar.handle_key(&ks_char("enter", "\n"), true),
            FindKeyOutcome::FindNext
        );
        assert_eq!(
            bar.handle_key(&ks_char("shift-enter", "\n"), true),
            FindKeyOutcome::FindPrev
        );
        assert_eq!(bar.handle_key(&ks("cmd-g"), true), FindKeyOutcome::FindNext);
        assert_eq!(
            bar.handle_key(&ks("cmd-shift-g"), true),
            FindKeyOutcome::FindPrev
        );
        assert_eq!(bar.handle_key(&ks("f3"), false), FindKeyOutcome::FindNext);
        assert_eq!(
            bar.handle_key(&ks("shift-f3"), false),
            FindKeyOutcome::FindPrev
        );
        assert_eq!(
            bar.handle_key(&ks("cmd-backspace"), true),
            FindKeyOutcome::Consumed
        );
        assert_eq!(bar.find_input().text(), "");
        assert_eq!(bar.handle_key(&ks("escape"), true), FindKeyOutcome::Close);
    }
}
