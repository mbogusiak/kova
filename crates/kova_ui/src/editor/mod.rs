//! Virtualized 120 FPS code editor view (`EditorView`) with cursors, selections,
//! keyboard, IME, mouse and clipboard input (`< 5 ms` dispatch).

pub mod actions;
pub mod element;
pub mod ime;
pub mod input;
pub mod line_cache;
pub mod shared;
pub mod state;
pub mod viewport;

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Font, FontFallbacks,
    FontFeatures, FontStyle, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point,
    Render, ScrollWheelEvent, ShapedLine, SharedString, Styled as _, Window, div, px,
};
use kova_buffer::{Buffer, BufferSnapshot, BufferVersion, Motion, SelectionSet};
use smallvec::SmallVec;

use crate::editor::element::EditorCanvasElement;
use crate::editor::input::{
    CursorShape, EditorKeyCommand, InputLatencyStats, KeyMods, map_editor_key,
};
use crate::editor::line_cache::ShapedLineCache;
pub use crate::editor::shared::SharedBuffer;
use crate::editor::state::EditorState;
use crate::editor::viewport::{
    DEFAULT_EDITOR_FONT_SIZE_PX, SCROLLBAR_RIGHT_MARGIN_PX, ViewportMetrics,
};

const SCROLL_LINES_MULTIPLIER: f32 = 3.0;
const SCROLLBAR_HIT_WIDTH_PX: f32 = 12.0;
/// Caret blink half-period (`editor_ux_guidelines.md` §4).
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);
/// Drag auto-scroll tick (~120 Hz).
const AUTOSCROLL_TICK: Duration = Duration::from_millis(8);
/// Upper bound for drag auto-scroll speed, in lines per tick.
const AUTOSCROLL_MAX_LINES_PER_TICK: f32 = 4.0;
/// Text inserted by `Tab` in Phase 0 (4 spaces; configurable indentation lands in Phase 1).
const TAB_INSERT: &str = "    ";

/// Built-in Phase 0 welcome buffer displayed before CLI file loading (`KOVA-016`).
const WELCOME_DEMO_TEXT: &str = "\
// Kova IDE — Phase 0 Virtualized Editor (120 FPS)
// Backed by O(1) BufferSnapshot (ropey::Rope) & Kova Islands Dark

package dev.kova.demo;

import java.util.List;

public final class KovaApplication {
    private static final String ENGINE = \"GPUI 120 FPS\";

    public static void main(String[] args) {
        var features = List.of(
            \"O(1) Rope snapshots\",
            \"Virtualized viewport + gutter\",
            \"Zero full-file UI allocations\"
        );
        features.forEach(System.out::println);
    }
}
";

#[derive(Debug, Clone, Copy)]
struct ScrollbarDrag {
    start_mouse_y: f32,
    start_scroll_y: f32,
}

/// Active left-button drag selection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragMode {
    /// No selection drag in progress.
    None,
    /// Character-wise drag started in the text area.
    Text,
    /// Whole-line drag started in the gutter, anchored at `anchor_row`.
    Lines { anchor_row: usize },
}

/// A visible line cached from the last prepaint: `(row, shaped, window_byte_start, x_offset)`.
pub(crate) type VisibleShapedLine = (usize, ShapedLine, usize, f32);

/// Whether an [`EditorView`] belongs to the active split pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneRole {
    Active,
    Inactive,
}

/// Events emitted by [`EditorView`] to its owner (the workspace shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorEvent {
    /// The shared document was edited through this view.
    Edited,
    /// The view was clicked and took keyboard focus.
    Focused,
}

/// Virtualized GPUI code editor view operating strictly on $O(1)$ [`BufferSnapshot`]s.
#[derive(Debug)]
pub struct EditorView {
    state: EditorState,
    pane_role: PaneRole,
    last_emitted_version: BufferVersion,
    scroll_offset: Point<f32>,
    max_scroll_x: f32,
    last_viewport: ViewportMetrics,
    last_gutter_width_px: f32,
    last_canvas_bounds: Bounds<Pixels>,
    visible_shaped_lines: Vec<VisibleShapedLine>,
    font_size_px: f32,
    font_family: SharedString,
    line_cache: ShapedLineCache<ShapedLine>,
    line_scratch: String,
    scrollbar_hovered: bool,
    scrollbar_drag: Option<ScrollbarDrag>,
    drag_mode: DragMode,
    last_drag_position: Option<Point<Pixels>>,
    autoscroll_running: bool,
    cursor_shape: CursorShape,
    cursor_visible: bool,
    blink_epoch: u64,
    input_latency: InputLatencyStats,
    focus_handle: FocusHandle,
}

impl EditorView {
    /// Creates a new [`EditorView`] backed by `buffer`.
    #[must_use]
    pub fn new(buffer: Buffer, cx: &mut Context<'_, Self>) -> Self {
        Self::with_shared_buffer(SharedBuffer::new(buffer), cx)
    }

    /// Creates an [`EditorView`] over an existing [`SharedBuffer`] (the same file in another
    /// split). The new view has its own caret at the top and an independent scroll offset.
    #[must_use]
    pub fn with_shared_buffer(buffer: SharedBuffer, cx: &mut Context<'_, Self>) -> Self {
        let line_height = ViewportMetrics::quantized_line_height(DEFAULT_EDITOR_FONT_SIZE_PX);
        Self {
            last_emitted_version: buffer.version(),
            state: EditorState::with_shared_buffer(buffer),
            pane_role: PaneRole::Active,
            scroll_offset: Point::new(0.0, 0.0),
            max_scroll_x: 0.0,
            last_viewport: ViewportMetrics::new(line_height, 7.8, 800.0, 600.0),
            last_gutter_width_px: 92.0,
            last_canvas_bounds: Bounds::default(),
            visible_shaped_lines: Vec::new(),
            font_size_px: DEFAULT_EDITOR_FONT_SIZE_PX,
            font_family: SharedString::from("JetBrains Mono"),
            line_cache: ShapedLineCache::default(),
            line_scratch: String::with_capacity(256),
            scrollbar_hovered: false,
            scrollbar_drag: None,
            drag_mode: DragMode::None,
            last_drag_position: None,
            autoscroll_running: false,
            cursor_shape: CursorShape::Bar,
            cursor_visible: true,
            blink_epoch: 0,
            input_latency: InputLatencyStats::default(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// Creates an [`EditorView`] initialized from an in-memory UTF-8 string.
    #[must_use]
    pub fn from_text(text: &str, cx: &mut Context<'_, Self>) -> Self {
        Self::new(Buffer::from_text(text), cx)
    }

    /// Creates an [`EditorView`] preloaded with the Phase 0 welcome buffer.
    #[must_use]
    pub fn welcome_demo(cx: &mut Context<'_, Self>) -> Self {
        Self::from_text(WELCOME_DEMO_TEXT, cx)
    }

    /// Returns an immutable $O(1)$ [`BufferSnapshot`] of the underlying buffer.
    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        self.state.snapshot()
    }

    /// Returns the headless editing state (buffer, history, selections).
    #[must_use]
    pub const fn state(&self) -> &EditorState {
        &self.state
    }

    /// Returns a clone of the [`SharedBuffer`] handle backing this view.
    #[must_use]
    pub fn shared_buffer(&self) -> SharedBuffer {
        self.state.shared_buffer()
    }

    /// Returns whether this view belongs to the active split pane.
    #[must_use]
    pub const fn is_active_pane(&self) -> bool {
        matches!(self.pane_role, PaneRole::Active)
    }

    /// Marks this view as belonging to the active (or an inactive) split pane.
    pub fn set_active_pane(&mut self, active: bool, cx: &mut Context<'_, Self>) {
        let role = if active {
            PaneRole::Active
        } else {
            PaneRole::Inactive
        };
        if self.pane_role != role {
            self.pane_role = role;
            cx.notify();
        }
    }

    /// Moves the primary caret to the 1-based `(line, col)` and scrolls it into view.
    ///
    /// Used by *Structure* and *Problems* navigation; out-of-range positions are clamped.
    pub fn set_cursor_position(&mut self, line: usize, col: usize, cx: &mut Context<'_, Self>) {
        self.state.set_line_col(line, col);
        self.ensure_primary_cursor_visible();
        cx.notify();
    }

    /// Returns the total line count of the active buffer (`>= 1`).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.state.snapshot().len_lines().max(1)
    }

    /// Returns the 1-based `(line, column)` of the primary cursor for status-bar display.
    #[must_use]
    pub fn primary_cursor_position(&self) -> (usize, usize) {
        self.state.primary_line_col()
    }

    /// Returns the number of active cursors (`>= 1`).
    #[must_use]
    pub fn cursor_count(&self) -> usize {
        self.state.cursor_count()
    }

    /// Returns the active font size in logical pixels.
    #[must_use]
    pub const fn font_size_px(&self) -> f32 {
        self.font_size_px
    }

    /// Returns the current scroll offset `(x, y)` in logical pixels.
    #[must_use]
    pub const fn scroll_offset(&self) -> Point<f32> {
        self.scroll_offset
    }

    /// Returns the most recent input-dispatch latency stats.
    #[must_use]
    pub const fn input_latency(&self) -> InputLatencyStats {
        self.input_latency
    }

    /// Scrolls the viewport by `(delta_x, delta_y)` in logical pixels, clamping to buffer bounds.
    pub fn scroll_by(&mut self, delta_x: Pixels, delta_y: Pixels, cx: &mut Context<'_, Self>) {
        let total_lines = self.line_count();
        let next_x = self
            .last_viewport
            .clamp_scroll_x(self.scroll_offset.x + f32::from(delta_x), self.max_scroll_x);
        let next_y = self
            .last_viewport
            .clamp_scroll_y(self.scroll_offset.y + f32::from(delta_y), total_lines);

        if (next_x - self.scroll_offset.x).abs() > f32::EPSILON
            || (next_y - self.scroll_offset.y).abs() > f32::EPSILON
        {
            self.scroll_offset = Point::new(next_x, next_y);
            cx.notify();
        }
    }

    /// Scrolls the viewport so `line_idx` (0-based) sits at the top edge of the view.
    #[allow(
        clippy::cast_precision_loss,
        reason = "line index fits cleanly within f32 mantissa"
    )]
    pub fn scroll_to_line(&mut self, line_idx: usize, cx: &mut Context<'_, Self>) {
        let total_lines = self.line_count();
        let target_y =
            (line_idx.min(total_lines.saturating_sub(1)) as f32) * self.last_viewport.line_height;
        self.scroll_offset.y = self.last_viewport.clamp_scroll_y(target_y, total_lines);
        cx.notify();
    }

    /// Places the caret at the 1-based `line` / `column` (characters, clamped to the
    /// buffer; column `1` when `None`) and centers that line vertically in the viewport
    /// (`kova <file>:<line>:<column>`, `KOVA-016`).
    #[allow(
        clippy::cast_precision_loss,
        reason = "line index fits cleanly within f32 mantissa"
    )]
    pub fn go_to_line_column(
        &mut self,
        line: usize,
        column: Option<usize>,
        cx: &mut Context<'_, Self>,
    ) {
        self.state.set_line_col(line, column.unwrap_or(1));
        let total_lines = self.line_count();
        let row = line.saturating_sub(1).min(total_lines.saturating_sub(1));
        let lh = self.last_viewport.line_height.max(1.0);
        let target = row as f32 * lh - (self.last_viewport.viewport_height - lh) / 2.0;
        self.scroll_offset.y = self.last_viewport.clamp_scroll_y(target, total_lines);
        self.ensure_primary_cursor_visible();
        cx.notify();
    }

    pub(crate) fn editor_font(&self) -> Font {
        Font {
            family: self.font_family.clone(),
            features: FontFeatures::default(),
            fallbacks: Some(FontFallbacks::from_fonts(vec![
                "SF Mono".to_owned(),
                "Cascadia Mono".to_owned(),
                "Menlo".to_owned(),
                "ui-monospace".to_owned(),
                "monospace".to_owned(),
            ])),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        }
    }

    pub(crate) fn gutter_font(&self, bold: bool) -> Font {
        Font {
            family: self.font_family.clone(),
            features: FontFeatures(Arc::new(vec![("tnum".to_owned(), 1)])),
            fallbacks: Some(FontFallbacks::from_fonts(vec![
                "SF Mono".to_owned(),
                "Menlo".to_owned(),
                "ui-monospace".to_owned(),
                "monospace".to_owned(),
            ])),
            weight: if bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: FontStyle::Normal,
        }
    }

    pub(crate) fn active_lines(&self, snapshot: &BufferSnapshot) -> SmallVec<[usize; 4]> {
        let mut rows = SmallVec::<[usize; 4]>::new();
        for sel in self.state.selections().as_slice() {
            let head = snapshot.clamp_byte_to_char_boundary(sel.head().min(snapshot.len_bytes()));
            if let Ok(pt) = snapshot.byte_to_point(head)
                && !rows.contains(&pt.row)
            {
                rows.push(pt.row);
            }
        }
        rows
    }

    pub(crate) const fn line_cache_mut(&mut self) -> &mut ShapedLineCache<ShapedLine> {
        &mut self.line_cache
    }

    pub(crate) fn line_scratch_mut(&mut self) -> &mut String {
        &mut self.line_scratch
    }

    pub(crate) fn update_layout_metrics(&mut self, metrics: ViewportMetrics, gutter_width_px: f32) {
        self.last_viewport = metrics;
        self.last_gutter_width_px = gutter_width_px;
    }

    pub(crate) fn set_scroll_bounds_and_clamp(&mut self, max_scroll_x: f32, clamped_scroll_y: f32) {
        self.max_scroll_x = max_scroll_x.max(0.0);
        self.scroll_offset.x = self
            .last_viewport
            .clamp_scroll_x(self.scroll_offset.x, self.max_scroll_x);
        self.scroll_offset.y = clamped_scroll_y;
    }

    /// Returns the GPUI focus handle of this editor.
    #[must_use]
    pub const fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub(crate) const fn selections(&self) -> &SelectionSet {
        self.state.selections()
    }

    pub(crate) const fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    pub(crate) const fn is_cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub(crate) fn marked_range(&self) -> Option<Range<usize>> {
        self.state.marked_range()
    }

    pub(crate) fn visible_shaped_lines(&self) -> &[VisibleShapedLine] {
        &self.visible_shaped_lines
    }

    /// Stores the canvas bounds and the shaped lines of the current `visible_range` only
    /// (replaced on every prepaint, so memory stays $O(\text{viewport})$).
    pub(crate) fn set_render_cache(
        &mut self,
        bounds: Bounds<Pixels>,
        visible_shaped_lines: Vec<VisibleShapedLine>,
    ) {
        self.last_canvas_bounds = bounds;
        self.visible_shaped_lines = visible_shaped_lines;
    }

    pub(crate) const fn is_scrollbar_active(&self) -> bool {
        self.scrollbar_hovered || self.scrollbar_drag.is_some()
    }

    /// Returns the x position (relative to the canvas origin) of byte column `col` in `row`,
    /// using the shaped line when visible and a monospace estimate otherwise.
    #[allow(
        clippy::cast_precision_loss,
        reason = "column counts fit cleanly within f32 mantissa"
    )]
    fn x_for_row_col(&self, row: usize, col: usize) -> f32 {
        let base = self.last_gutter_width_px - self.scroll_offset.x;
        if let Some((_, shaped, byte_start, x_offset)) =
            self.visible_shaped_lines.iter().find(|(r, ..)| *r == row)
        {
            let rel = col.saturating_sub(*byte_start).min(shaped.text.len());
            return base + x_offset + f32::from(shaped.x_for_index(rel));
        }
        let snap = self.state.snapshot();
        let line_start = snap
            .rope()
            .line_to_byte(row.min(snap.len_lines().saturating_sub(1)));
        let chars = snap
            .slice_bytes(line_start..snap.clamp_byte_to_char_boundary(line_start + col))
            .map_or(col, |s| s.len_chars());
        base + chars as f32 * self.last_viewport.char_advance
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let line_step = px(self.last_viewport.line_height * SCROLL_LINES_MULTIPLIER);
        let pixel_delta = event.delta.pixel_delta(line_step);
        // In GPUI, scrolling down yields negative delta.y, so negate to advance viewport downward.
        self.scroll_by(-pixel_delta.x, -pixel_delta.y, cx);
    }

    fn is_in_scrollbar_hit_zone(&self, local_x: f32) -> bool {
        let total_width = self.last_gutter_width_px + self.last_viewport.viewport_width;
        let hit_left = total_width - SCROLLBAR_HIT_WIDTH_PX - SCROLLBAR_RIGHT_MARGIN_PX;
        local_x >= hit_left && local_x <= total_width + SCROLLBAR_RIGHT_MARGIN_PX
    }

    /// Makes the caret solid and restarts the `530 ms` blink loop.
    ///
    /// The loop stops on its own when the editor loses focus or a newer reset happens,
    /// so an unfocused editor causes no background wake-ups.
    fn reset_cursor_blink(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        self.cursor_visible = true;
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        let epoch = self.blink_epoch;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(CURSOR_BLINK_INTERVAL).await;
                let keep_going = this
                    .update_in(cx, |editor, window, cx| {
                        if editor.blink_epoch != epoch {
                            return false;
                        }
                        if !editor.focus_handle.is_focused(window) {
                            editor.cursor_visible = true;
                            return false;
                        }
                        editor.cursor_visible = !editor.cursor_visible;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Scrolls by the minimal delta that keeps the primary cursor inside the viewport
    /// with a 2-line vertical and 4-character horizontal margin.
    #[allow(
        clippy::cast_precision_loss,
        reason = "line indices fit cleanly within f32 mantissa"
    )]
    fn ensure_primary_cursor_visible(&mut self) {
        let total_lines = self.line_count();
        let (row, col) = self.state.primary_point();

        let lh = self.last_viewport.line_height.max(1.0);
        let vh = self.last_viewport.viewport_height.max(lh);
        let margin_y = (lh * 2.0).min(vh * 0.4);
        let cursor_top = row as f32 * lh;
        let cursor_bottom = cursor_top + lh;

        if cursor_top < self.scroll_offset.y + margin_y {
            self.scroll_offset.y = self
                .last_viewport
                .clamp_scroll_y(cursor_top - margin_y, total_lines);
        } else if cursor_bottom > self.scroll_offset.y + vh - margin_y {
            self.scroll_offset.y = self
                .last_viewport
                .clamp_scroll_y(cursor_bottom + margin_y - vh, total_lines);
        }

        let char_advance = self.last_viewport.char_advance.max(1.0);
        let vw = self.last_viewport.viewport_width.max(char_advance);
        let margin_x = (char_advance * 4.0).min(vw * 0.25);
        // Content-space x of the caret (independent of the current horizontal scroll).
        let cursor_x =
            self.x_for_row_col(row, col) - self.last_gutter_width_px + self.scroll_offset.x;
        // Allow scrolling past the last measured line width while typing at the line end.
        let max_x = self.max_scroll_x.max(cursor_x + margin_x - vw);
        if cursor_x < self.scroll_offset.x + margin_x {
            self.scroll_offset.x = (cursor_x - margin_x).clamp(0.0, max_x.max(0.0));
        } else if cursor_x > self.scroll_offset.x + vw - margin_x {
            self.scroll_offset.x = (cursor_x + margin_x - vw).clamp(0.0, max_x.max(0.0));
        }
    }

    /// Maps a window position to `(row, byte_offset, in_gutter)`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped non-negative viewport coordinates fit cleanly in usize"
    )]
    fn hit_test_point(&self, pos: Point<Pixels>) -> (usize, usize, bool) {
        let snap = self.state.snapshot();
        let total_lines = snap.len_lines().max(1);
        let local_x = f32::from(pos.x) - f32::from(self.last_canvas_bounds.origin.x);
        let local_y = f32::from(pos.y) - f32::from(self.last_canvas_bounds.origin.y);
        let lh = self.last_viewport.line_height.max(1.0);
        let row = (((local_y + self.scroll_offset.y).max(0.0) / lh).floor() as usize)
            .min(total_lines.saturating_sub(1));
        let in_gutter = local_x < self.last_gutter_width_px;

        let line_start = snap.rope().line_to_byte(row);
        let line_len = snap.line_len_bytes(row).unwrap_or(0);
        let content_x = local_x - self.last_gutter_width_px + self.scroll_offset.x;
        let byte = if in_gutter {
            line_start
        } else if let Some((_, shaped, byte_start, x_offset)) =
            self.visible_shaped_lines.iter().find(|(r, ..)| *r == row)
        {
            let rel_idx = shaped.closest_index_for_x(px((content_x - *x_offset).max(0.0)));
            (line_start + byte_start + rel_idx).min(line_start + line_len)
        } else {
            let approx_chars =
                (content_x.max(0.0) / self.last_viewport.char_advance.max(1.0)).round() as usize;
            let line_start_char = snap.byte_to_char(line_start).unwrap_or(0);
            let line_chars = snap
                .slice_bytes(line_start..line_start + line_len)
                .map_or(0, |s| s.len_chars());
            snap.char_to_byte(line_start_char + approx_chars.min(line_chars))
                .unwrap_or(line_start)
        };
        (row, byte, in_gutter)
    }

    /// Finishes an input dispatch: auto-scroll (if the caret moved), latency sample, repaint.
    fn finish_input(
        &mut self,
        started: Instant,
        scroll_to_cursor: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if scroll_to_cursor {
            self.ensure_primary_cursor_visible();
        }
        self.input_latency.record(started.elapsed());
        self.emit_if_edited(cx);
        cx.notify();
    }

    /// Emits [`EditorEvent::Edited`] once per document version change made through this view,
    /// so sibling splits sharing the buffer (and the tab dirty badge) refresh immediately.
    fn emit_if_edited(&mut self, cx: &mut Context<'_, Self>) {
        let version = self.state.snapshot().version();
        if version != self.last_emitted_version {
            self.last_emitted_version = version;
            cx.emit(EditorEvent::Edited);
        }
    }

    fn run_edit(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
        edit: impl FnOnce(&mut EditorState, &mut App) -> bool,
    ) {
        let started = Instant::now();
        self.reset_cursor_blink(window, cx);
        self.state.sync_external_edits();
        let changed = edit(&mut self.state, cx);
        self.finish_input(started, changed, cx);
    }

    fn copy_action(&mut self, _: &actions::Copy, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(text) = self.state.copy() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut_action(&mut self, _: &actions::Cut, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.run_edit(window, cx, |state, cx| {
            state.cut().is_some_and(|text| {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            })
        });
    }

    fn paste_action(
        &mut self,
        _: &actions::Paste,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_edit(window, cx, |state, cx| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| state.paste(&text))
        });
    }

    fn select_all_action(
        &mut self,
        _: &actions::SelectAll,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_edit(window, cx, |state, _| {
            state.select_all();
            false
        });
    }

    fn undo_action(&mut self, _: &actions::Undo, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.run_edit(window, cx, |state, _| state.undo());
    }

    fn redo_action(&mut self, _: &actions::Redo, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.run_edit(window, cx, |state, _| state.redo());
    }

    fn escape_action(
        &mut self,
        _: &actions::Escape,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_edit(window, cx, |state, _| {
            state.escape();
            false
        });
    }

    fn motion(
        &mut self,
        motion: Motion,
        extend: bool,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_edit(window, cx, |state, _| {
            state.move_selections(motion, extend);
            true
        });
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "visible line count is a small non-negative value"
    )]
    fn page_lines(&self) -> usize {
        ((self.last_viewport.viewport_height / self.last_viewport.line_height.max(1.0)).floor()
            as usize)
            .saturating_sub(1)
            .max(1)
    }

    /// Handles navigation and editing keys via [`map_editor_key`]. Printable characters are
    /// left to `EntityInputHandler`; clipboard, Select All and Undo/Redo go through actions.
    /// Propagation is stopped only for consumed keys, so the IME never inserts twice.
    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let m = event.keystroke.modifiers;
        let macos = cfg!(target_os = "macos");
        let mods = KeyMods {
            primary: if macos { m.platform } else { m.control },
            alt: m.alt,
            shift: m.shift,
            secondary: if macos { m.control } else { m.platform },
        };
        let Some(command) = map_editor_key(event.keystroke.key.as_str(), mods, macos) else {
            return;
        };
        match command {
            EditorKeyCommand::Escape => self.run_edit(window, cx, |state, _| {
                state.escape();
                false
            }),
            EditorKeyCommand::Newline => {
                self.run_edit(window, cx, |state, _| state.insert_text("\n"));
            }
            EditorKeyCommand::Tab => {
                self.run_edit(window, cx, |state, _| state.insert_text(TAB_INSERT));
            }
            EditorKeyCommand::DeleteBackward => {
                self.run_edit(window, cx, |state, _| state.delete_backward());
            }
            EditorKeyCommand::DeleteForward => {
                self.run_edit(window, cx, |state, _| state.delete_forward());
            }
            EditorKeyCommand::Move { motion, extend } => self.motion(motion, extend, window, cx),
            EditorKeyCommand::AddCursor { up } => self.run_edit(window, cx, |state, _| {
                state.add_cursor_vertical(up);
                true
            }),
            EditorKeyCommand::Page { down, extend } => self.page(down, extend, window, cx),
        }
        cx.stop_propagation();
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "page line count fits cleanly within f32"
    )]
    fn page(&mut self, down: bool, extend: bool, window: &Window, cx: &mut Context<'_, Self>) {
        let lines = self.page_lines();
        let delta = lines as f32 * self.last_viewport.line_height;
        let total_lines = self.line_count();
        self.scroll_offset.y = self.last_viewport.clamp_scroll_y(
            self.scroll_offset.y + if down { delta } else { -delta },
            total_lines,
        );
        self.run_edit(window, cx, |state, _| {
            state.move_vertical_page(lines, down, extend);
            true
        });
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let started = Instant::now();
        window.focus(&self.focus_handle);
        self.reset_cursor_blink(window, cx);
        self.state.sync_external_edits();
        cx.emit(EditorEvent::Focused);

        let local_x = f32::from(event.position.x) - f32::from(self.last_canvas_bounds.origin.x);
        let total_lines = self.line_count();
        if self
            .last_viewport
            .vertical_scrollbar_thumb(self.scroll_offset.y, total_lines)
            .is_some()
            && self.is_in_scrollbar_hit_zone(local_x)
        {
            self.scrollbar_drag = Some(ScrollbarDrag {
                start_mouse_y: f32::from(event.position.y),
                start_scroll_y: self.scroll_offset.y,
            });
            self.finish_input(started, false, cx);
            return;
        }

        let (row, byte, in_gutter) = self.hit_test_point(event.position);
        let mods = event.modifiers;
        if in_gutter {
            let anchor_row = if mods.shift {
                self.state.primary_point().0
            } else {
                row
            };
            self.state.select_lines(anchor_row, row);
            self.drag_mode = DragMode::Lines { anchor_row };
        } else if event.click_count >= 3 {
            self.state.select_lines(row, row);
        } else if event.click_count == 2 {
            self.state.select_word_at(byte);
        } else if mods.alt {
            self.state.add_cursor_at(byte);
        } else if mods.shift {
            self.state.extend_primary_to(byte);
            self.drag_mode = DragMode::Text;
        } else {
            self.state.set_caret(byte);
            self.drag_mode = DragMode::Text;
        }
        cx.stop_propagation();
        self.finish_input(started, false, cx);
    }

    /// Handles hover over the scrollbar zone (only while the pointer is over the editor).
    fn handle_hover_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.scrollbar_drag.is_some() || self.is_selecting() {
            return;
        }
        let local_x = f32::from(event.position.x) - f32::from(self.last_canvas_bounds.origin.x);
        let hovered = self.is_in_scrollbar_hit_zone(local_x)
            && self.last_viewport.max_scroll_y(self.line_count()) > 0.0;
        if hovered != self.scrollbar_hovered {
            self.scrollbar_hovered = hovered;
            cx.notify();
        }
    }

    const fn is_selecting(&self) -> bool {
        !matches!(self.drag_mode, DragMode::None)
    }

    /// Window-level drag handler (registered in paint), so dragging keeps working and
    /// auto-scrolls when the pointer leaves the editor bounds.
    pub(crate) fn handle_drag_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(drag) = self.scrollbar_drag {
            let total_lines = self.line_count();
            if let Some((_thumb_y, thumb_height)) = self
                .last_viewport
                .vertical_scrollbar_thumb(self.scroll_offset.y, total_lines)
            {
                let track_span = (self.last_viewport.viewport_height - thumb_height).max(1.0);
                let max_scroll = self.last_viewport.max_scroll_y(total_lines);
                let dy = f32::from(event.position.y) - drag.start_mouse_y;
                let scroll_delta = (dy / track_span) * max_scroll;
                self.scroll_offset.y = self
                    .last_viewport
                    .clamp_scroll_y(drag.start_scroll_y + scroll_delta, total_lines);
                cx.notify();
            }
            return;
        }
        if !self.is_selecting() || !event.dragging() {
            return;
        }
        self.last_drag_position = Some(event.position);
        self.extend_drag_selection(event.position);
        cx.notify();
        if self.autoscroll_delta(event.position) != 0.0 && !self.autoscroll_running {
            self.start_drag_autoscroll(window, cx);
        }
    }

    fn extend_drag_selection(&mut self, position: Point<Pixels>) {
        let (row, byte, _) = self.hit_test_point(position);
        if let DragMode::Lines { anchor_row } = self.drag_mode {
            self.state.select_lines(anchor_row, row);
        } else {
            self.state.extend_primary_to(byte);
        }
    }

    /// Returns the per-tick vertical auto-scroll delta for a drag at `position`
    /// (`0.0` while the pointer is inside the viewport).
    fn autoscroll_delta(&self, position: Point<Pixels>) -> f32 {
        let top = f32::from(self.last_canvas_bounds.origin.y);
        let bottom = top + self.last_viewport.viewport_height;
        let y = f32::from(position.y);
        let lh = self.last_viewport.line_height;
        if y < top {
            -(lh * (1.0 + (top - y) / lh).min(AUTOSCROLL_MAX_LINES_PER_TICK))
        } else if y > bottom {
            lh * (1.0 + (y - bottom) / lh).min(AUTOSCROLL_MAX_LINES_PER_TICK)
        } else {
            0.0
        }
    }

    /// Smoothly scrolls every frame tick while a drag selection is held outside the viewport.
    fn start_drag_autoscroll(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        self.autoscroll_running = true;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTOSCROLL_TICK).await;
                let keep_going = this
                    .update(cx, |editor, cx| {
                        let Some(pos) = editor.last_drag_position else {
                            editor.autoscroll_running = false;
                            return false;
                        };
                        let delta = editor.autoscroll_delta(pos);
                        if !editor.is_selecting() || delta == 0.0 {
                            editor.autoscroll_running = false;
                            return false;
                        }
                        let total_lines = editor.line_count();
                        editor.scroll_offset.y = editor
                            .last_viewport
                            .clamp_scroll_y(editor.scroll_offset.y + delta, total_lines);
                        editor.extend_drag_selection(pos);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Window-level mouse-up handler (registered in paint): ends drags even outside the editor.
    pub(crate) fn handle_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = self.scrollbar_drag.take().is_some() || self.is_selecting();
        self.drag_mode = DragMode::None;
        self.last_drag_position = None;
        if changed {
            cx.notify();
        }
    }
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // A sibling split may have edited the shared document since the last frame.
        if self.state.sync_external_edits() {
            self.last_emitted_version = self.state.snapshot().version();
        }
        div()
            .size_full()
            .relative()
            .key_context(actions::EDITOR_KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_action(cx.listener(Self::copy_action))
            .on_action(cx.listener(Self::cut_action))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::select_all_action))
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .on_action(cx.listener(Self::escape_action))
            .on_action(cx.listener(|this, _: &actions::MoveLeft, window, cx| {
                this.motion(Motion::Left, false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::MoveRight, window, cx| {
                this.motion(Motion::Right, false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::MoveUp, window, cx| {
                this.motion(Motion::Up, false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::MoveDown, window, cx| {
                this.motion(Motion::Down, false, window, cx);
            }))
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
            .on_mouse_move(cx.listener(Self::handle_hover_move))
            .child(EditorCanvasElement::new(cx.entity()))
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use kova_buffer::Buffer;

    use crate::editor::line_cache::{LineCacheKey, ShapedLineCache};
    use crate::editor::viewport::ViewportMetrics;

    #[test]
    fn virtualizes_one_million_line_snapshot_in_o_viewport() {
        let mut big = String::with_capacity(22 * 1_000_000);
        for i in 0..1_000_000usize {
            let _ = writeln!(big, "line {i:07} content");
        }
        let buffer = Buffer::from_text(&big);
        drop(big);

        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.len_lines(), 1_000_001);

        let metrics = ViewportMetrics::new(19.0, 7.8, 1200.0, 800.0);
        let scroll_y = 500_000.0 * 19.0;
        let visible = metrics.visible_line_range(scroll_y, snapshot.len_lines());
        assert!(visible.len() <= 60);

        let mut scratch = String::with_capacity(128);
        let mut cache = ShapedLineCache::new(512);
        cache.sync_context(snapshot.version().as_u64(), 0, 13.0);

        for row in visible.clone() {
            scratch.clear();
            let slice = snapshot.line_slice(row).expect("visible row in bounds");
            for chunk in slice.chunks() {
                scratch.push_str(chunk);
            }
            let (range, _) = ViewportMetrics::visible_utf8_window(&scratch, 0.0, 7.8, 1200.0);
            cache.insert(
                LineCacheKey::new(row, range.start, range.end),
                scratch.len(),
            );
        }

        assert_eq!(cache.len(), visible.len());
        assert!(cache.len() <= 512);
    }
}
