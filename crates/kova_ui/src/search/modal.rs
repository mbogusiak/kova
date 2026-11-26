//! *Search Everywhere* modal state, keyboard controller, and GPUI overlay renderer.

use std::time::Instant;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    ClickEvent, Context, Div, FocusHandle, FontWeight, HighlightStyle, InteractiveElement as _,
    MouseButton, MouseDownEvent, ParentElement as _, ScrollStrategy, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, StyledText, UniformListScrollHandle, div, px,
    uniform_list,
};

use super::{
    SearchEngine, SearchEntry, SearchEntryKind, SearchHit, SearchScope, char_indices_to_byte_ranges,
};
use crate::controls::{ControlPalette, TextInputEvent, TextInputState};
use crate::shell::WorkspaceShell;
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Fixed height of one result row in logical pixels (`uniform_list` requirement).
pub const SEARCH_ROW_HEIGHT_PX: f32 = 32.0;
const MAX_VISIBLE_ROWS: usize = 9;
const MODAL_MIN_WIDTH_PX: f32 = 380.0;
const MODAL_MAX_WIDTH_PX: f32 = 680.0;

/// Result of feeding one keystroke to [`SearchEverywhereState::handle_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKeyOutcome {
    /// Key was not applicable to the search modal.
    Ignored,
    /// Modal state changed (query text, caret, selection, or scope tab).
    Consumed,
    /// `Escape` requested closing the modal and restoring previous focus.
    Close,
    /// `Enter` or `Shift+Enter` requested activating the hit at `index`.
    Activate {
        /// Index into [`SearchEverywhereState::hits`].
        index: usize,
        /// `true` when `Shift+Enter` asks to open in a right split pane.
        split: bool,
    },
}

/// Pure headless state and keyboard state machine of *Search Everywhere*.
#[derive(Debug)]
pub struct SearchEverywhereState {
    open: bool,
    recent_files_mode: bool,
    scope: SearchScope,
    query: TextInputState,
    entries: Vec<SearchEntry>,
    engine: SearchEngine,
    hits: Vec<SearchHit>,
    selected: usize,
    last_query_ms: f32,
}

impl Default for SearchEverywhereState {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchEverywhereState {
    /// Creates a closed [`SearchEverywhereState`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: false,
            recent_files_mode: false,
            scope: SearchScope::All,
            query: TextInputState::default(),
            entries: Vec::new(),
            engine: SearchEngine::new(),
            hits: Vec::new(),
            selected: 0,
            last_query_ms: 0.0,
        }
    }

    /// Whether the modal is currently open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the modal is showing the MRU *Recent Files* list (`Cmd+E` / `Ctrl+E`).
    #[must_use]
    pub const fn is_recent_files_mode(&self) -> bool {
        self.recent_files_mode
    }

    /// Active scope tab.
    #[must_use]
    pub const fn scope(&self) -> SearchScope {
        self.scope
    }

    /// Current query input state.
    #[must_use]
    pub const fn query(&self) -> &TextInputState {
        &self.query
    }

    /// Ranked search hits for the current scope and query.
    #[must_use]
    pub fn hits(&self) -> &[SearchHit] {
        &self.hits
    }

    /// Selected hit index inside [`Self::hits`].
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// Duration of the last search query in milliseconds.
    #[must_use]
    pub const fn last_query_ms(&self) -> f32 {
        self.last_query_ms
    }

    /// Opens the modal with a fresh candidate snapshot and resets query/selection.
    pub fn open(&mut self, entries: Vec<SearchEntry>) {
        self.open_with_scope(entries, SearchScope::All);
    }

    /// Opens the modal pre-selected on `scope`.
    pub fn open_with_scope(&mut self, entries: Vec<SearchEntry>, scope: SearchScope) {
        self.open = true;
        self.recent_files_mode = false;
        self.scope = scope;
        self.query = TextInputState::default();
        self.entries = entries;
        self.selected = 0;
        self.recompute();
    }

    /// Opens the modal in *Recent Files* MRU mode (`Cmd+E` / `Ctrl+E`), pre-selecting index `1`
    /// when at least two files exist so `Cmd+E` + `Enter` toggles between the last two files.
    pub fn open_recent_files(&mut self, entries: Vec<SearchEntry>) {
        self.open = true;
        self.recent_files_mode = true;
        self.scope = SearchScope::Files;
        self.query = TextInputState::default();
        self.entries = entries;
        self.selected = 0;
        self.recompute();
        if self.hits.len() >= 2 {
            self.selected = 1;
        }
    }

    /// Closes the modal and drops transient results.
    pub fn close(&mut self) {
        self.open = false;
        self.recent_files_mode = false;
        self.hits.clear();
        self.entries.clear();
        self.query.clear();
        self.selected = 0;
    }

    /// Switches the active scope tab while keeping the typed query and resets selection to `0`.
    pub fn set_scope(&mut self, scope: SearchScope) {
        if self.scope != scope {
            self.scope = scope;
            self.selected = 0;
            self.recompute();
        }
    }

    /// Advances to the next scope tab (`Shift+Shift` while already open, or `Tab`).
    pub fn next_scope(&mut self) {
        self.set_scope(self.scope.next());
    }

    /// Moves selection by `delta` (`+1` down, `-1` up) with wrap-around.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.hits.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let len_i = isize::try_from(len).unwrap_or(isize::MAX);
        let cur_i = isize::try_from(self.selected.min(len - 1)).unwrap_or(0);
        let next = (cur_i + delta).rem_euclid(len_i);
        self.selected = usize::try_from(next).unwrap_or(0);
    }

    /// Processes a single GPUI keystroke inside the open modal.
    pub fn handle_key(&mut self, keystroke: &gpui::Keystroke) -> SearchKeyOutcome {
        let mods = &keystroke.modifiers;
        let key = keystroke.key.as_str();

        // Named control keys are handled before `key_char` so macOS `\n` / `\t` never leak into
        // the text input.
        if key == "escape" && !mods.platform && !mods.control && !mods.alt {
            return SearchKeyOutcome::Close;
        }
        if key == "enter" && !mods.platform && !mods.control && !mods.alt {
            return if self.hits.is_empty() {
                SearchKeyOutcome::Consumed
            } else {
                SearchKeyOutcome::Activate {
                    index: self.selected,
                    split: mods.shift,
                }
            };
        }
        if key == "tab" && !mods.platform && !mods.control && !mods.alt {
            if !self.recent_files_mode {
                let next = if mods.shift {
                    self.scope.prev()
                } else {
                    self.scope.next()
                };
                self.set_scope(next);
            }
            return SearchKeyOutcome::Consumed;
        }

        let move_down = (key == "down" && !mods.platform && !mods.control && !mods.alt)
            || (mods.control && !mods.platform && !mods.alt && !mods.shift && key == "n");
        if move_down {
            self.move_selection(1);
            return SearchKeyOutcome::Consumed;
        }

        let move_up = (key == "up" && !mods.platform && !mods.control && !mods.alt)
            || (mods.control && !mods.platform && !mods.alt && !mods.shift && key == "p");
        if move_up {
            self.move_selection(-1);
            return SearchKeyOutcome::Consumed;
        }

        if key == "backspace" && (mods.platform || mods.control) && !mods.alt {
            if !self.query.text().is_empty() {
                self.query.clear();
                self.selected = 0;
                self.recompute();
            }
            return SearchKeyOutcome::Consumed;
        }

        if mods.platform || mods.control {
            return SearchKeyOutcome::Ignored;
        }

        if !mods.alt
            && matches!(
                key,
                "backspace" | "delete" | "left" | "right" | "home" | "end"
            )
        {
            let previous = self.query.text().to_owned();
            if self.query.handle_named_key(key) == TextInputEvent::Edited
                && self.query.text() != previous
            {
                self.selected = 0;
                self.recompute();
            }
            return SearchKeyOutcome::Consumed;
        }

        // `Option+<key>` on macOS (`mods.alt`) produces national letters (e.g. `ą`, `ę`) when
        // paired with a single character key; ignore named function/navigation keys (`Alt+Up`).
        let is_character_key = key.chars().count() == 1 || key == "space";
        if (!mods.alt || is_character_key)
            && let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text
                .chars()
                .any(|ch| ch.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&ch))
        {
            if self.query.insert(text) == TextInputEvent::Edited {
                self.selected = 0;
                self.recompute();
            }
            return SearchKeyOutcome::Consumed;
        }

        if !mods.alt && key == "space" && self.query.insert(" ") == TextInputEvent::Edited {
            self.selected = 0;
            self.recompute();
            return SearchKeyOutcome::Consumed;
        }

        SearchKeyOutcome::Ignored
    }

    fn recompute(&mut self) {
        let start = Instant::now();
        self.hits = self
            .engine
            .search(&self.entries, self.scope, self.query.text());
        #[allow(
            clippy::cast_possible_truncation,
            reason = "sub-second search latency fits cleanly into f32 milliseconds"
        )]
        {
            self.last_query_ms = (start.elapsed().as_secs_f64() * 1000.0) as f32;
        }
        if self.selected >= self.hits.len() {
            self.selected = self.hits.len().saturating_sub(1);
        }
    }
}

/// GPUI-backed *Search Everywhere* modal overlay owned by [`WorkspaceShell`].
#[derive(Debug)]
pub struct SearchEverywhereModal {
    state: SearchEverywhereState,
    focus_handle: FocusHandle,
    return_focus: Option<FocusHandle>,
    scroll: UniformListScrollHandle,
}

impl SearchEverywhereModal {
    /// Creates a closed modal bound to `focus_handle`.
    #[must_use]
    pub fn new(focus_handle: FocusHandle) -> Self {
        Self {
            state: SearchEverywhereState::new(),
            focus_handle,
            return_focus: None,
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// Whether the modal is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.state.is_open()
    }

    /// Modal's dedicated [`FocusHandle`] (taking focus away from the active editor).
    #[must_use]
    pub const fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Headless modal state.
    #[must_use]
    pub const fn state(&self) -> &SearchEverywhereState {
        &self.state
    }

    /// Scroll handle of the virtualized result list.
    #[must_use]
    pub fn scroll_handle(&self) -> UniformListScrollHandle {
        self.scroll.clone()
    }

    /// Opens the modal with `entries` and remembers `return_focus` for restoration on close.
    pub fn open(&mut self, entries: Vec<SearchEntry>, return_focus: Option<FocusHandle>) {
        self.return_focus = return_focus;
        self.state.open(entries);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    /// Opens the modal pre-selected on `scope`.
    pub fn open_with_scope(
        &mut self,
        entries: Vec<SearchEntry>,
        scope: SearchScope,
        return_focus: Option<FocusHandle>,
    ) {
        self.return_focus = return_focus;
        self.state.open_with_scope(entries, scope);
        self.scroll
            .scroll_to_item(self.state.selected(), ScrollStrategy::Top);
    }

    /// Opens the modal in *Recent Files* MRU mode.
    pub fn open_recent_files(
        &mut self,
        entries: Vec<SearchEntry>,
        return_focus: Option<FocusHandle>,
    ) {
        self.return_focus = return_focus;
        self.state.open_recent_files(entries);
        self.scroll
            .scroll_to_item(self.state.selected(), ScrollStrategy::Top);
    }

    /// Closes the modal and yields the saved [`FocusHandle`] to restore.
    pub fn close(&mut self) -> Option<FocusHandle> {
        self.state.close();
        self.return_focus.take()
    }

    /// Switches to `scope` while keeping the query and saved `return_focus`.
    pub fn set_scope(&mut self, scope: SearchScope) {
        self.state.set_scope(scope);
        self.scroll
            .scroll_to_item(self.state.selected(), ScrollStrategy::Top);
    }

    /// Advances to the next scope tab while keeping the saved `return_focus`.
    pub fn next_scope(&mut self) {
        self.state.next_scope();
        self.scroll
            .scroll_to_item(self.state.selected(), ScrollStrategy::Top);
    }

    /// Returns the hit at `index`.
    #[must_use]
    pub fn hit(&self, index: usize) -> Option<&SearchHit> {
        self.state.hits().get(index)
    }

    /// Processes a keystroke and keeps the virtualized list scrolled to the selected item.
    pub fn handle_key(&mut self, keystroke: &gpui::Keystroke) -> SearchKeyOutcome {
        let outcome = self.state.handle_key(keystroke);
        if outcome == SearchKeyOutcome::Consumed {
            self.scroll
                .scroll_to_item(self.state.selected(), ScrollStrategy::Top);
        }
        outcome
    }

    /// Renders the translucent scrim and floating *Search Everywhere* card.
    pub(crate) fn render(
        &self,
        theme: &KovaTheme,
        viewport_px: (f32, f32),
        cx: &Context<'_, WorkspaceShell>,
    ) -> Stateful<Div> {
        let p = ControlPalette::from_theme(theme);
        let elevated = hsla_from_rgba(theme.surfaces().elevated);
        let border_strong = hsla_from_rgba(theme.borders().strong);
        let border_subtle = hsla_from_rgba(theme.borders().subtle);
        let active_bg = hsla_from_rgba(theme.surfaces().active);
        let tab_indicator = hsla_from_rgba(theme.borders().tab_indicator);
        let mut backdrop = hsla_from_rgba(theme.surfaces().overlay);
        backdrop.a = backdrop.a.min(0.45);

        let width = (viewport_px.0 - 80.0).clamp(MODAL_MIN_WIDTH_PX, MODAL_MAX_WIDTH_PX);
        let top_offset = (viewport_px.1 * 0.14).clamp(48.0, 120.0);
        let rows_visible = self.state.hits().len().clamp(1, MAX_VISIBLE_ROWS);
        #[allow(
            clippy::cast_precision_loss,
            reason = "MAX_VISIBLE_ROWS is <= 9, exact in f32"
        )]
        let list_height = (rows_visible as f32) * SEARCH_ROW_HEIGHT_PX + 8.0;

        let tabs_bar = self.render_tabs_bar(&p, active_bg, tab_indicator, border_subtle, cx);
        let input_bar = self.render_input_bar(&p, border_subtle);
        let results = self.render_results(list_height, &p, cx);
        let footer = Self::render_footer(self.state.is_recent_files_mode(), &p, border_subtle);

        let card = div()
            .id("search-everywhere-panel")
            .relative()
            .w(px(width))
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .border_1()
            .border_color(border_strong)
            .bg(elevated)
            .overflow_hidden()
            .shadow_lg()
            .occlude()
            .track_focus(&self.focus_handle)
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
            })
            .child(tabs_bar)
            .child(input_bar)
            .child(results)
            .child(footer);

        div()
            .id("search-everywhere-overlay")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(top_offset))
            .bg(backdrop)
            .child(
                div()
                    .id("search-everywhere-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_search_everywhere(true, window, cx);
                        }),
                    ),
            )
            .child(card)
    }

    fn render_tabs_bar(
        &self,
        p: &ControlPalette,
        active_bg: gpui::Hsla,
        tab_indicator: gpui::Hsla,
        border_subtle: gpui::Hsla,
        cx: &Context<'_, WorkspaceShell>,
    ) -> Div {
        let left = if self.state.is_recent_files_mode() {
            div()
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child("Recent Files")
        } else {
            let mut tabs = div().flex().items_center().gap(px(2.0));
            for (idx, scope) in SearchScope::ALL.into_iter().enumerate() {
                let active = scope == self.state.scope();
                let mut tab = div()
                    .id(("se-scope-tab", idx))
                    .h(px(26.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .rounded(px(5.0))
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.set_search_scope(scope, window, cx);
                    }));
                tab = if active {
                    tab.bg(active_bg)
                        .text_color(p.text)
                        .font_weight(FontWeight::SEMIBOLD)
                        .border_b_2()
                        .border_color(tab_indicator)
                } else {
                    tab.text_color(p.text_muted)
                };
                tabs = tabs.child(tab.child(scope.label()));
            }
            tabs
        };

        let summary = format!(
            "{} hits · {:.1} ms",
            self.state.hits().len(),
            self.state.last_query_ms()
        );

        div()
            .h(px(36.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(border_subtle)
            .child(left)
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(p.text_muted)
                    .child(summary),
            )
    }

    fn render_input_bar(&self, p: &ControlPalette, border_subtle: gpui::Hsla) -> Div {
        let (before, after) = self.state.query().split_at_cursor();
        let empty = before.is_empty() && after.is_empty();
        let placeholder = if self.state.is_recent_files_mode() {
            "Filter recent files…"
        } else {
            "Search classes, files, symbols, actions… (or :line)"
        };

        let mut field = div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(14.0))
            .text_color(p.text)
            .child(SharedString::from(before.to_owned()))
            .child(div().w(px(1.5)).h(px(16.0)).bg(p.caret))
            .child(SharedString::from(after.to_owned()));

        if empty {
            field = field.child(
                div()
                    .ml(px(2.0))
                    .text_size(px(13.0))
                    .text_color(p.text_muted)
                    .child(placeholder),
            );
        }

        div()
            .h(px(40.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .bg(p.input)
            .border_b_1()
            .border_color(border_subtle)
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(p.text_muted)
                    .child("⌕"),
            )
            .child(field)
            .child(
                div()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(p.border)
                    .text_size(px(10.0))
                    .text_color(p.text_muted)
                    .child("Esc"),
            )
    }

    fn render_results(
        &self,
        list_height: f32,
        p: &ControlPalette,
        cx: &Context<'_, WorkspaceShell>,
    ) -> Div {
        if self.state.hits().is_empty() {
            return div()
                .h(px(list_height))
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.5))
                .text_color(p.text_muted)
                .child("No matching results");
        }

        div().h(px(list_height)).w_full().child(
            uniform_list(
                "search-everywhere-list",
                self.state.hits().len(),
                cx.processor(WorkspaceShell::render_search_rows),
            )
            .track_scroll(self.scroll.clone())
            .size_full()
            .py(px(4.0)),
        )
    }

    fn render_footer(
        recent_files_mode: bool,
        p: &ControlPalette,
        border_subtle: gpui::Hsla,
    ) -> Div {
        let hints = if recent_files_mode {
            "↑↓ Navigate · Enter Open · Shift+Enter Split · Esc Close"
        } else {
            "↑↓ Navigate · Tab Scope · Enter Open · Shift+Enter Split · Esc Close"
        };
        div()
            .h(px(28.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(border_subtle)
            .text_size(px(11.0))
            .text_color(p.text_muted)
            .child(hints)
    }
}

fn build_highlighted_title(title: &str, matched_char_indices: &[u32], p: &ControlPalette) -> Div {
    if title.is_empty() {
        return div();
    }
    let highlights: Vec<_> = char_indices_to_byte_ranges(title, matched_char_indices)
        .into_iter()
        .map(|range| {
            (
                range,
                HighlightStyle {
                    color: Some(p.primary.subtle_fg),
                    font_weight: Some(FontWeight::BOLD),
                    ..HighlightStyle::default()
                },
            )
        })
        .collect();
    div()
        .text_size(px(12.5))
        .text_color(p.text)
        .whitespace_nowrap()
        .child(StyledText::new(title.to_owned()).with_highlights(highlights))
}

/// Renders one row of the *Search Everywhere* `uniform_list`.
pub(crate) fn render_search_row(
    row: usize,
    hit: &SearchHit,
    selected: bool,
    theme: &KovaTheme,
    cx: &Context<'_, WorkspaceShell>,
) -> Stateful<Div> {
    let p = ControlPalette::from_theme(theme);
    let active_bg = hsla_from_rgba(theme.surfaces().active);
    let icon_color = match hit.entry.kind {
        SearchEntryKind::Class => p.accent.subtle_fg,
        SearchEntryKind::File => p.primary.subtle_fg,
        SearchEntryKind::Symbol => p.warning.subtle_fg,
        SearchEntryKind::Action => p.muted.subtle_fg,
    };
    let title_el = build_highlighted_title(&hit.entry.title, &hit.matched_char_indices, &p);
    let right_meta = hit
        .target_line
        .map(|line| format!(":{line}"))
        .or_else(|| hit.entry.shortcut_hint.clone())
        .unwrap_or_default();

    let mut el = div()
        .id(("search-hit-row", row))
        .h(px(SEARCH_ROW_HEIGHT_PX))
        .w_full()
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(10.0))
        .cursor_pointer()
        .hover(|s| s.bg(p.hover))
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            cx.stop_propagation();
            this.activate_search_hit(row, event.modifiers().shift, window, cx);
        }));

    if selected {
        el = el.bg(active_bg);
    }

    el.child(
        div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .overflow_hidden()
            .child(
                div()
                    .w(px(18.0))
                    .h(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .bg(p.input)
                    .text_size(px(10.5))
                    .font_weight(FontWeight::BOLD)
                    .text_color(icon_color)
                    .child(hit.entry.icon),
            )
            .child(title_el)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(11.5))
                    .text_color(p.text_muted)
                    .child(SharedString::from(hit.entry.subtitle.clone())),
            ),
    )
    .when(!right_meta.is_empty(), |row_div| {
        row_div.child(
            div()
                .flex_none()
                .px(px(6.0))
                .py(px(1.0))
                .rounded(px(4.0))
                .bg(p.input)
                .text_size(px(10.5))
                .text_color(p.text_muted)
                .child(right_meta),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::keymap::ShellAction;
    use crate::search::SearchTarget;

    fn ks(source: &str) -> gpui::Keystroke {
        gpui::Keystroke::parse(source).expect("valid keystroke")
    }

    fn ks_char(source: &str, key_char: &str) -> gpui::Keystroke {
        let mut keystroke = ks(source);
        keystroke.key_char = Some(key_char.to_owned());
        keystroke
    }

    fn sample_entries() -> Vec<SearchEntry> {
        vec![
            SearchEntry {
                kind: SearchEntryKind::Class,
                title: "OrderApp".to_owned(),
                subtitle: "Class · OrderApp.java:10".to_owned(),
                match_text: "OrderApp".to_owned(),
                icon: "C",
                shortcut_hint: None,
                target: SearchTarget::File {
                    path: PathBuf::from("src/OrderApp.java"),
                    line: None,
                },
            },
            SearchEntry {
                kind: SearchEntryKind::File,
                title: "OrderApp.java".to_owned(),
                subtitle: "src".to_owned(),
                match_text: "src/OrderApp.java".to_owned(),
                icon: "F",
                shortcut_hint: None,
                target: SearchTarget::File {
                    path: PathBuf::from("src/OrderApp.java"),
                    line: None,
                },
            },
            SearchEntry {
                kind: SearchEntryKind::Symbol,
                title: "ŻółćHelper".to_owned(),
                subtitle: "Method · 5".to_owned(),
                match_text: "ŻółćHelper".to_owned(),
                icon: "m",
                shortcut_hint: None,
                target: SearchTarget::Symbol {
                    path: None,
                    line: 5,
                    col: 1,
                },
            },
            SearchEntry {
                kind: SearchEntryKind::Action,
                title: "Toggle Vim Mode".to_owned(),
                subtitle: "Action".to_owned(),
                match_text: "Toggle Vim Mode".to_owned(),
                icon: "A",
                shortcut_hint: None,
                target: SearchTarget::Action(ShellAction::ToggleVimMode),
            },
        ]
    }

    #[test]
    fn typing_editing_navigation_tabs_activate_and_escape() {
        let mut modal = SearchEverywhereState::new();
        modal.open(sample_entries());
        assert!(modal.is_open());
        assert_eq!(modal.hits().len(), 4);

        // Type "Ord" -> filters to OrderApp & OrderApp.java
        for (src, ch) in [("o", "O"), ("r", "r"), ("d", "d")] {
            assert_eq!(
                modal.handle_key(&ks_char(src, ch)),
                SearchKeyOutcome::Consumed
            );
        }
        assert_eq!(modal.query().text(), "Ord");
        assert_eq!(modal.hits().len(), 2);

        // Down / Up / Ctrl+N / Ctrl+P navigate with wrap-around.
        assert_eq!(modal.selected(), 0);
        assert_eq!(modal.handle_key(&ks("down")), SearchKeyOutcome::Consumed);
        assert_eq!(modal.selected(), 1);
        assert_eq!(modal.handle_key(&ks("down")), SearchKeyOutcome::Consumed);
        assert_eq!(modal.selected(), 0);
        assert_eq!(modal.handle_key(&ks("ctrl-p")), SearchKeyOutcome::Consumed);
        assert_eq!(modal.selected(), 1);
        assert_eq!(modal.handle_key(&ks("ctrl-n")), SearchKeyOutcome::Consumed);
        assert_eq!(modal.selected(), 0);

        // Tab / Shift+Tab switch scope tabs while preserving the query.
        assert_eq!(
            modal.handle_key(&ks_char("tab", "\t")),
            SearchKeyOutcome::Consumed
        );
        assert_eq!(modal.scope(), SearchScope::Classes);
        assert_eq!(modal.query().text(), "Ord");
        assert_eq!(modal.hits().len(), 1);

        assert_eq!(
            modal.handle_key(&ks_char("shift-tab", "\t")),
            SearchKeyOutcome::Consumed
        );
        assert_eq!(modal.scope(), SearchScope::All);

        // Enter & Shift+Enter activate with split flag (ignoring `\n` in key_char).
        assert_eq!(
            modal.handle_key(&ks_char("enter", "\n")),
            SearchKeyOutcome::Activate {
                index: 0,
                split: false
            }
        );
        assert_eq!(
            modal.handle_key(&ks_char("shift-enter", "\n")),
            SearchKeyOutcome::Activate {
                index: 0,
                split: true
            }
        );

        // Backspace & Cmd+Backspace edit/clear query.
        assert_eq!(
            modal.handle_key(&ks("backspace")),
            SearchKeyOutcome::Consumed
        );
        assert_eq!(modal.query().text(), "Or");
        assert_eq!(
            modal.handle_key(&ks("cmd-backspace")),
            SearchKeyOutcome::Consumed
        );
        assert_eq!(modal.query().text(), "");
        assert_eq!(modal.hits().len(), 4);

        // macOS Option+a (`ą`) inserts Polish diacritics, while Alt+Up does not insert `\u{f700}`.
        assert_eq!(
            modal.handle_key(&ks_char("alt-up", "\u{f700}")),
            SearchKeyOutcome::Ignored
        );
        assert_eq!(modal.query().text(), "");
        assert_eq!(
            modal.handle_key(&ks_char("alt-a", "ą")),
            SearchKeyOutcome::Consumed
        );
        assert_eq!(modal.query().text(), "ą");

        // Left / Home / Delete edit in place, Escape closes.
        assert_eq!(modal.handle_key(&ks("home")), SearchKeyOutcome::Consumed);
        assert_eq!(modal.handle_key(&ks("delete")), SearchKeyOutcome::Consumed);
        assert_eq!(modal.query().text(), "");
        assert_eq!(modal.handle_key(&ks("escape")), SearchKeyOutcome::Close);
    }

    #[test]
    fn open_with_scope_and_recent_files_mode_behave_per_intellij() {
        let mut modal = SearchEverywhereState::new();
        modal.open_with_scope(sample_entries(), SearchScope::Classes);
        assert!(modal.is_open());
        assert!(!modal.is_recent_files_mode());
        assert_eq!(modal.scope(), SearchScope::Classes);
        assert_eq!(modal.hits().len(), 1);

        let files = vec![
            SearchEntry {
                kind: SearchEntryKind::File,
                title: "Current.java".to_owned(),
                subtitle: "src".to_owned(),
                match_text: "src/Current.java".to_owned(),
                icon: "F",
                shortcut_hint: None,
                target: SearchTarget::File {
                    path: PathBuf::from("src/Current.java"),
                    line: None,
                },
            },
            SearchEntry {
                kind: SearchEntryKind::File,
                title: "Previous.java".to_owned(),
                subtitle: "src".to_owned(),
                match_text: "src/Previous.java".to_owned(),
                icon: "F",
                shortcut_hint: None,
                target: SearchTarget::File {
                    path: PathBuf::from("src/Previous.java"),
                    line: None,
                },
            },
        ];
        modal.open_recent_files(files);
        assert!(modal.is_recent_files_mode());
        assert_eq!(modal.scope(), SearchScope::Files);
        assert_eq!(modal.selected(), 1, "pre-selects previous file at index 1");
        // Tab in Recent Files mode keeps scope fixed at Files.
        assert_eq!(
            modal.handle_key(&ks_char("tab", "\t")),
            SearchKeyOutcome::Consumed
        );
        assert_eq!(modal.scope(), SearchScope::Files);
        assert_eq!(
            modal.handle_key(&ks_char("enter", "\n")),
            SearchKeyOutcome::Activate {
                index: 1,
                split: false
            }
        );
    }
}
