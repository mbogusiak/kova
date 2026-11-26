//! Workspace-wide *Find in Files* (`Cmd+Shift+F` / `Ctrl+Shift+F`) engine, state and modal.
//!
//! Disk scanning runs off the UI thread (`cx.background_executor()`) with a monotonic
//! generation counter that discards stale search results before touching UI state.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use gpui::{
    ClickEvent, Context, Div, FocusHandle, FontWeight, HighlightStyle, InteractiveElement as _,
    MouseButton, MouseDownEvent, ParentElement as _, ScrollStrategy, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, StyledText, UniformListScrollHandle, div, px,
    uniform_list,
};

use crate::controls::{ControlPalette, TextInputEvent, TextInputState};
use crate::editor::find::find_all_matches;
use crate::shell::WorkspaceShell;
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Maximum number of matches returned by one *Find in Files* query.
pub const MAX_FIND_IN_FILES_HITS: usize = 500;
/// Maximum disk file size scanned by *Find in Files* (`2 MiB`).
const MAX_SCAN_FILE_BYTES: u64 = 2 * 1024 * 1024;
const ROW_HEIGHT_PX: f32 = 34.0;
const MAX_VISIBLE_ROWS: usize = 10;
const MODAL_MIN_WIDTH_PX: f32 = 440.0;
const MODAL_MAX_WIDTH_PX: f32 = 760.0;
const MAX_PREVIEW_BYTES: usize = 180;

/// Single line match produced by [`search_text_in_buffers_and_paths`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindInFilesHit {
    /// Project-relative (or open document) path.
    pub path: PathBuf,
    /// 1-based line number.
    pub line: usize,
    /// 1-based character column of the match start.
    pub col: usize,
    /// Trimmed line preview shown in the result list.
    pub line_preview: String,
    /// Valid UTF-8 byte range inside `line_preview` to highlight.
    pub match_range: Range<usize>,
}

/// Searches `query` across `open_buffers` (in-memory edits take priority) and remaining
/// `workspace_files` on disk under `project_root`, skipping binary/non-UTF-8/oversized files
/// and capping results at [`MAX_FIND_IN_FILES_HITS`].
#[must_use]
pub fn search_text_in_buffers_and_paths(
    query: &str,
    case_sensitive: bool,
    open_buffers: &[(PathBuf, String)],
    project_root: Option<&Path>,
    workspace_files: &[PathBuf],
) -> Vec<FindInFilesHit> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(open_buffers.len());

    let mut sorted_open: Vec<&(PathBuf, String)> = open_buffers.iter().collect();
    sorted_open.sort_by(|a, b| a.0.cmp(&b.0));

    for (path, text) in sorted_open {
        seen.insert(path.clone());
        collect_hits_in_text(path, text, query, case_sensitive, &mut hits);
        if hits.len() >= MAX_FIND_IN_FILES_HITS {
            return hits;
        }
    }

    for rel_path in workspace_files {
        if hits.len() >= MAX_FIND_IN_FILES_HITS {
            break;
        }
        if !seen.insert(rel_path.clone()) {
            continue;
        }
        let abs = project_root.map_or_else(|| rel_path.clone(), |root| root.join(rel_path));
        let Ok(meta) = std::fs::metadata(&abs) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_SCAN_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        collect_hits_in_text(rel_path, text, query, case_sensitive, &mut hits);
    }

    hits
}

fn collect_hits_in_text(
    path: &Path,
    text: &str,
    query: &str,
    case_sensitive: bool,
    out: &mut Vec<FindInFilesHit>,
) {
    for (row_idx, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            continue;
        }
        let matches = find_all_matches(line, query, case_sensitive);
        for range in matches {
            let col = line[..range.start].chars().count() + 1;
            let (line_preview, match_range) = build_line_preview(line, range);
            out.push(FindInFilesHit {
                path: path.to_path_buf(),
                line: row_idx + 1,
                col,
                line_preview,
                match_range,
            });
            if out.len() >= MAX_FIND_IN_FILES_HITS {
                return;
            }
        }
    }
}

fn build_line_preview(line: &str, range: Range<usize>) -> (String, Range<usize>) {
    let leading_ws = line.len() - line.trim_start().len();
    let trimmed = line.trim_start();
    let rel_start = range.start.saturating_sub(leading_ws).min(trimmed.len());
    let rel_end = range.end.saturating_sub(leading_ws).min(trimmed.len());

    if trimmed.len() <= MAX_PREVIEW_BYTES {
        return (trimmed.to_owned(), rel_start..rel_end);
    }

    let window_start = if rel_start > 40 {
        floor_char_boundary(trimmed, rel_start - 40)
    } else {
        0
    };
    let window_end = ceil_char_boundary(
        trimmed,
        (window_start + MAX_PREVIEW_BYTES).min(trimmed.len()),
    );
    let slice = &trimmed[window_start..window_end];
    let adj_start = rel_start.saturating_sub(window_start).min(slice.len());
    let adj_end = rel_end.saturating_sub(window_start).min(slice.len());
    (slice.to_owned(), adj_start..adj_end)
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Outcome of handling one keystroke inside [`FindInFilesState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindInFilesKeyOutcome {
    /// Key was not applicable to the modal.
    Ignored,
    /// Selection or cursor inside query moved (no new search needed).
    Consumed,
    /// Query text or case-sensitivity changed: caller should schedule background search.
    QueryChanged,
    /// `Escape` closed the modal.
    Close,
    /// `Enter` or `Shift+Enter` activated `index` (`split = true` opens in right split).
    Activate {
        /// Index into [`FindInFilesState::hits`].
        index: usize,
        /// Whether `Shift+Enter` was pressed.
        split: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SearchStatus {
    #[default]
    Idle,
    Searching,
    Ready {
        truncated: bool,
    },
}

/// Headless state and keyboard state machine for *Find in Files*.
#[derive(Debug, Default)]
pub struct FindInFilesState {
    open: bool,
    query: TextInputState,
    case_sensitive: bool,
    hits: Vec<FindInFilesHit>,
    selected: usize,
    generation: u64,
    status: SearchStatus,
}

impl FindInFilesState {
    /// Creates a closed [`FindInFilesState`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the modal is currently visible.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Current search query input state.
    #[must_use]
    pub const fn query(&self) -> &TextInputState {
        &self.query
    }

    /// Whether case-sensitive matching (`Aa`) is enabled.
    #[must_use]
    pub const fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    /// Current matched hits.
    #[must_use]
    pub fn hits(&self) -> &[FindInFilesHit] {
        &self.hits
    }

    /// Currently selected hit index.
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// Monotonic generation used to cancel/discard stale background scans.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a background search is currently in flight.
    #[must_use]
    pub const fn is_searching(&self) -> bool {
        matches!(self.status, SearchStatus::Searching)
    }

    /// Whether the current result list hit [`MAX_FIND_IN_FILES_HITS`].
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        matches!(self.status, SearchStatus::Ready { truncated: true })
    }

    /// Opens the modal (optionally prefilling single-line `prefill`) and starts a new generation.
    pub fn open(&mut self, prefill: Option<&str>) -> u64 {
        self.open = true;
        if let Some(text) = prefill
            && !text.is_empty()
            && !text.contains('\n')
        {
            self.query = TextInputState::new(text);
        }
        self.selected = 0;
        self.begin_search()
    }

    /// Closes the modal and increments generation to cancel pending background results.
    pub fn close(&mut self) {
        self.open = false;
        self.hits.clear();
        self.query.clear();
        self.selected = 0;
        self.status = SearchStatus::Idle;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Starts a new search generation (clears hits immediately when the query is empty).
    pub fn begin_search(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        if self.query.text().is_empty() {
            self.hits.clear();
            self.selected = 0;
            self.status = SearchStatus::Idle;
        } else {
            self.status = SearchStatus::Searching;
        }
        self.generation
    }

    /// Toggles `Aa` case-sensitivity and starts a new search generation.
    pub fn toggle_case_sensitive(&mut self) -> u64 {
        self.case_sensitive = !self.case_sensitive;
        self.begin_search()
    }

    /// Applies results produced off-thread if `generation` matches the current generation.
    pub fn set_results(&mut self, generation: u64, hits: Vec<FindInFilesHit>) -> bool {
        if generation != self.generation || !self.open {
            return false;
        }
        let truncated = hits.len() >= MAX_FIND_IN_FILES_HITS;
        self.status = SearchStatus::Ready { truncated };
        self.hits = hits;
        if self.selected >= self.hits.len() {
            self.selected = self.hits.len().saturating_sub(1);
        }
        true
    }

    /// Moves `selected` by `delta` with wrap-around.
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

    /// Processes a single GPUI keystroke inside the modal.
    pub fn handle_key(&mut self, keystroke: &gpui::Keystroke) -> FindInFilesKeyOutcome {
        let mods = &keystroke.modifiers;
        let key = keystroke.key.as_str();

        if key == "escape" && !mods.platform && !mods.control && !mods.alt {
            return FindInFilesKeyOutcome::Close;
        }
        if key == "enter" && !mods.platform && !mods.control && !mods.alt {
            return if self.hits.is_empty() {
                FindInFilesKeyOutcome::Consumed
            } else {
                FindInFilesKeyOutcome::Activate {
                    index: self.selected,
                    split: mods.shift,
                }
            };
        }

        let move_down = (key == "down" && !mods.platform && !mods.control && !mods.alt)
            || (mods.control && !mods.platform && !mods.alt && !mods.shift && key == "n");
        if move_down {
            self.move_selection(1);
            return FindInFilesKeyOutcome::Consumed;
        }

        let move_up = (key == "up" && !mods.platform && !mods.control && !mods.alt)
            || (mods.control && !mods.platform && !mods.alt && !mods.shift && key == "p");
        if move_up {
            self.move_selection(-1);
            return FindInFilesKeyOutcome::Consumed;
        }

        if key == "backspace" && (mods.platform || mods.control) && !mods.alt {
            if !self.query.text().is_empty() {
                self.query.clear();
                self.selected = 0;
                return FindInFilesKeyOutcome::QueryChanged;
            }
            return FindInFilesKeyOutcome::Consumed;
        }

        if mods.platform || mods.control {
            return FindInFilesKeyOutcome::Ignored;
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
                return FindInFilesKeyOutcome::QueryChanged;
            }
            return FindInFilesKeyOutcome::Consumed;
        }

        let is_character_key = key.chars().count() == 1 || key == "space";
        if (!mods.alt || is_character_key)
            && let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text
                .chars()
                .any(|ch| ch.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&ch))
            && self.query.insert(text) == TextInputEvent::Edited
        {
            self.selected = 0;
            return FindInFilesKeyOutcome::QueryChanged;
        }

        if !mods.alt && key == "space" && self.query.insert(" ") == TextInputEvent::Edited {
            self.selected = 0;
            return FindInFilesKeyOutcome::QueryChanged;
        }

        FindInFilesKeyOutcome::Ignored
    }
}

/// GPUI-backed *Find in Files* modal overlay owned by [`WorkspaceShell`].
#[derive(Debug)]
pub struct FindInFilesModal {
    state: FindInFilesState,
    focus_handle: FocusHandle,
    return_focus: Option<FocusHandle>,
    scroll: UniformListScrollHandle,
}

impl FindInFilesModal {
    /// Creates a closed modal bound to `focus_handle`.
    #[must_use]
    pub fn new(focus_handle: FocusHandle) -> Self {
        Self {
            state: FindInFilesState::new(),
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

    /// Modal's [`FocusHandle`].
    #[must_use]
    pub const fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Headless modal state.
    #[must_use]
    pub const fn state(&self) -> &FindInFilesState {
        &self.state
    }

    /// Mutable headless modal state.
    #[must_use]
    pub const fn state_mut(&mut self) -> &mut FindInFilesState {
        &mut self.state
    }

    /// Opens the modal, returning the new search generation.
    pub fn open(&mut self, prefill: Option<&str>, return_focus: Option<FocusHandle>) -> u64 {
        self.return_focus = return_focus;
        let generation = self.state.open(prefill);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        generation
    }

    /// Closes the modal and yields the saved [`FocusHandle`].
    pub fn close(&mut self) -> Option<FocusHandle> {
        self.state.close();
        self.return_focus.take()
    }

    /// Returns the hit at `index`.
    #[must_use]
    pub fn hit(&self, index: usize) -> Option<&FindInFilesHit> {
        self.state.hits().get(index)
    }

    /// Processes a keystroke and keeps the virtualized list scrolled to the selected item.
    pub fn handle_key(&mut self, keystroke: &gpui::Keystroke) -> FindInFilesKeyOutcome {
        let outcome = self.state.handle_key(keystroke);
        if matches!(
            outcome,
            FindInFilesKeyOutcome::Consumed | FindInFilesKeyOutcome::QueryChanged
        ) {
            self.scroll
                .scroll_to_item(self.state.selected(), ScrollStrategy::Top);
        }
        outcome
    }

    /// Renders the *Find in Files* scrim and floating card.
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
        let mut backdrop = hsla_from_rgba(theme.surfaces().overlay);
        backdrop.a = backdrop.a.min(0.45);

        let width = (viewport_px.0 - 80.0).clamp(MODAL_MIN_WIDTH_PX, MODAL_MAX_WIDTH_PX);
        let top_offset = (viewport_px.1 * 0.12).clamp(44.0, 110.0);
        let rows_visible = self.state.hits().len().clamp(1, MAX_VISIBLE_ROWS);
        #[allow(
            clippy::cast_precision_loss,
            reason = "MAX_VISIBLE_ROWS is <= 10, exact in f32"
        )]
        let list_height = (rows_visible as f32) * ROW_HEIGHT_PX + 8.0;

        let header = self.render_header(&p, border_subtle, cx);
        let input_bar = self.render_input_bar(&p, border_subtle);
        let results = self.render_results(list_height, &p, cx);
        let footer = Self::render_footer(&p, border_subtle);

        let card = div()
            .id("find-in-files-panel")
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
            .child(header)
            .child(input_bar)
            .child(results)
            .child(footer);

        div()
            .id("find-in-files-overlay")
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
                    .id("find-in-files-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_find_in_files(true, window, cx);
                        }),
                    ),
            )
            .child(card)
    }

    fn render_header(
        &self,
        p: &ControlPalette,
        border_subtle: gpui::Hsla,
        cx: &Context<'_, WorkspaceShell>,
    ) -> Div {
        let summary = if self.state.query().text().is_empty() {
            "Type to search across project files".to_owned()
        } else if self.state.is_searching() {
            "Searching…".to_owned()
        } else if self.state.is_truncated() {
            format!("{}+ matches", self.state.hits().len())
        } else {
            format!("{} matches", self.state.hits().len())
        };

        div()
            .h(px(34.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(border_subtle)
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text)
                    .child("Find in Files"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(p.text_muted)
                            .child(summary),
                    )
                    .child(render_case_button(self.state.case_sensitive(), p, cx)),
            )
    }

    fn render_footer(p: &ControlPalette, border_subtle: gpui::Hsla) -> Div {
        div()
            .h(px(28.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .border_t_1()
            .border_color(border_subtle)
            .text_size(px(11.0))
            .text_color(p.text_muted)
            .child("↑↓ Navigate · Enter Open · Shift+Enter Split · Esc Close")
    }

    fn render_input_bar(&self, p: &ControlPalette, border_subtle: gpui::Hsla) -> Div {
        let (before, after) = self.state.query().split_at_cursor();
        let empty = before.is_empty() && after.is_empty();

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
                    .child("Search text across workspace files…"),
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
            let msg = if self.state.query().text().is_empty() {
                "Enter search text to find matches across files"
            } else if self.state.is_searching() {
                "Searching workspace…"
            } else {
                "No matches found"
            };
            return div()
                .h(px(list_height))
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.5))
                .text_color(p.text_muted)
                .child(msg);
        }

        div().h(px(list_height)).w_full().child(
            uniform_list(
                "find-in-files-list",
                self.state.hits().len(),
                cx.processor(WorkspaceShell::render_find_in_files_rows),
            )
            .track_scroll(self.scroll.clone())
            .size_full()
            .py(px(4.0)),
        )
    }
}

fn render_case_button(
    active: bool,
    p: &ControlPalette,
    cx: &Context<'_, WorkspaceShell>,
) -> Stateful<Div> {
    let mut btn = div()
        .id("find-in-files-case-btn")
        .h(px(22.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .border_1()
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            this.toggle_find_in_files_case_sensitive(window, cx);
        }));
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
    btn.child("Aa")
}

/// Renders one row of the *Find in Files* `uniform_list`.
pub(crate) fn render_find_in_files_row(
    row: usize,
    hit: &FindInFilesHit,
    selected: bool,
    theme: &KovaTheme,
    cx: &Context<'_, WorkspaceShell>,
) -> Stateful<Div> {
    let p = ControlPalette::from_theme(theme);
    let active_bg = hsla_from_rgba(theme.surfaces().active);
    let file_name = hit.path.file_name().map_or_else(
        || hit.path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let location = format!("{file_name}:{}", hit.line);

    let highlights = if !hit.match_range.is_empty()
        && hit.match_range.end <= hit.line_preview.len()
        && hit.line_preview.is_char_boundary(hit.match_range.start)
        && hit.line_preview.is_char_boundary(hit.match_range.end)
    {
        vec![(
            hit.match_range.clone(),
            HighlightStyle {
                color: Some(p.primary.subtle_fg),
                font_weight: Some(FontWeight::BOLD),
                ..HighlightStyle::default()
            },
        )]
    } else {
        Vec::new()
    };

    let mut el = div()
        .id(("find-in-files-row", row))
        .h(px(ROW_HEIGHT_PX))
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
            this.activate_find_in_files_hit(row, event.modifiers().shift, window, cx);
        }));

    if selected {
        el = el.bg(active_bg);
    }

    el.child(
        div()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(12.5))
            .text_color(p.text)
            .child(StyledText::new(hit.line_preview.clone()).with_highlights(highlights)),
    )
    .child(
        div()
            .flex_none()
            .px(px(6.0))
            .py(px(1.0))
            .rounded(px(4.0))
            .bg(p.input)
            .text_size(px(10.5))
            .text_color(p.text_muted)
            .child(location),
    )
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "unit tests abort on invalid keystroke or temp-dir fixtures"
    )]

    use std::time::{SystemTime, UNIX_EPOCH};

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
    fn search_prefers_open_buffers_skips_binary_and_respects_case_and_limit() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("kova_find_in_files_{nanos}"));
        std::fs::create_dir_all(root.join("src")).expect("temp dir");

        std::fs::write(
            root.join("src/App.java"),
            "class App {\n    // stale on disk\n}\n",
        )
        .expect("write App.java");
        std::fs::write(
            root.join("src/Order.java"),
            "    String zażółć = \"Zażółć\";\n",
        )
        .expect("write Order.java");
        std::fs::write(root.join("src/bin.dat"), "\x00Zażółć\x00".as_bytes())
            .expect("write binary");

        let open_buffers = vec![(
            PathBuf::from("src/App.java"),
            "class App {\n    String label = \"Zażółć\";\n}\n".to_owned(),
        )];
        let workspace_files = vec![
            PathBuf::from("src/App.java"),
            PathBuf::from("src/Order.java"),
            PathBuf::from("src/bin.dat"),
        ];

        let insensitive = search_text_in_buffers_and_paths(
            "zażółć",
            false,
            &open_buffers,
            Some(&root),
            &workspace_files,
        );
        // App.java uses open buffer (1 hit), Order.java has 2 hits on line 1, bin.dat skipped.
        assert_eq!(insensitive.len(), 3);
        assert_eq!(insensitive[0].path, PathBuf::from("src/App.java"));
        assert_eq!(insensitive[0].line, 2);
        assert_eq!(
            &insensitive[0].line_preview[insensitive[0].match_range.clone()],
            "Zażółć"
        );

        let sensitive = search_text_in_buffers_and_paths(
            "zażółć",
            true,
            &open_buffers,
            Some(&root),
            &workspace_files,
        );
        assert_eq!(sensitive.len(), 1);
        assert_eq!(sensitive[0].path, PathBuf::from("src/Order.java"));

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn state_drops_stale_generations_and_handles_keys() {
        let mut st = FindInFilesState::new();
        let gen1 = st.open(Some("order"));
        assert!(st.is_open());
        assert_eq!(st.query().text(), "order");

        assert_eq!(
            st.handle_key(&ks_char("a", "a")),
            FindInFilesKeyOutcome::QueryChanged
        );
        let gen2 = st.begin_search();
        assert_ne!(gen1, gen2);

        let hit = FindInFilesHit {
            path: PathBuf::from("Order.java"),
            line: 3,
            col: 5,
            line_preview: "ordera".to_owned(),
            match_range: 0..6,
        };
        assert!(!st.set_results(gen1, vec![hit.clone()]));
        assert!(st.set_results(gen2, vec![hit]));
        assert_eq!(st.hits().len(), 1);

        assert_eq!(
            st.handle_key(&ks_char("enter", "\n")),
            FindInFilesKeyOutcome::Activate {
                index: 0,
                split: false
            }
        );
        assert_eq!(st.handle_key(&ks("escape")), FindInFilesKeyOutcome::Close);
        st.close();
        assert!(!st.is_open());
    }
}
