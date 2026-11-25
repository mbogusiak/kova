//! Virtualized 120 FPS code editor view (`EditorView`).

pub mod element;
pub mod line_cache;
pub mod viewport;

use std::sync::Arc;

use gpui::{
    Context, FocusHandle, Font, FontFallbacks, FontFeatures, FontStyle, FontWeight,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollWheelEvent, ShapedLine, SharedString,
    Styled, Window, div, px,
};
use kova_buffer::{Buffer, BufferSnapshot, SelectionSet};
use smallvec::SmallVec;

use crate::editor::element::EditorCanvasElement;
use crate::editor::line_cache::ShapedLineCache;
use crate::editor::viewport::{
    DEFAULT_EDITOR_FONT_SIZE_PX, SCROLLBAR_RIGHT_MARGIN_PX, SCROLLBAR_THUMB_WIDTH_PX,
    ViewportMetrics,
};

const SCROLL_LINES_MULTIPLIER: f32 = 3.0;
const SCROLLBAR_HIT_WIDTH_PX: f32 = 12.0;

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

/// Virtualized GPUI code editor view operating strictly on $O(1)$ [`BufferSnapshot`]s.
#[derive(Debug)]
pub struct EditorView {
    buffer: Buffer,
    selections: SelectionSet,
    scroll_offset: Point<f32>,
    max_scroll_x: f32,
    last_viewport: ViewportMetrics,
    last_gutter_width_px: f32,
    font_size_px: f32,
    font_family: SharedString,
    line_cache: ShapedLineCache<ShapedLine>,
    line_scratch: String,
    scrollbar_hovered: bool,
    scrollbar_drag: Option<ScrollbarDrag>,
    focus_handle: FocusHandle,
}

impl EditorView {
    /// Creates a new [`EditorView`] backed by `buffer`.
    #[must_use]
    pub fn new(buffer: Buffer, cx: &mut Context<'_, Self>) -> Self {
        let line_height = ViewportMetrics::quantized_line_height(DEFAULT_EDITOR_FONT_SIZE_PX);
        Self {
            buffer,
            selections: SelectionSet::caret(0),
            scroll_offset: Point::new(0.0, 0.0),
            max_scroll_x: 0.0,
            last_viewport: ViewportMetrics::new(line_height, 7.8, 800.0, 600.0),
            last_gutter_width_px: 92.0,
            font_size_px: DEFAULT_EDITOR_FONT_SIZE_PX,
            font_family: SharedString::from("JetBrains Mono"),
            line_cache: ShapedLineCache::default(),
            line_scratch: String::with_capacity(256),
            scrollbar_hovered: false,
            scrollbar_drag: None,
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
        self.buffer.snapshot()
    }

    /// Returns the total line count of the active buffer (`>= 1`).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.buffer.snapshot().len_lines().max(1)
    }

    /// Returns the 1-based `(line, column)` of the primary cursor for status-bar display.
    #[must_use]
    pub fn primary_cursor_position(&self) -> (usize, usize) {
        let snap = self.buffer.snapshot();
        let head = self.selections.primary().head().min(snap.len_bytes());
        let clamped = snap.clamp_byte_to_char_boundary(head);
        snap.byte_to_point(clamped)
            .map_or((1, 1), |pt| (pt.row + 1, pt.column + 1))
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
        for sel in self.selections.as_slice() {
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

    pub(crate) const fn is_scrollbar_active(&self) -> bool {
        self.scrollbar_hovered || self.scrollbar_drag.is_some()
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

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mouse_x = f32::from(event.position.x);
        let mouse_y = f32::from(event.position.y);
        let total_lines = self.line_count();
        if self
            .last_viewport
            .vertical_scrollbar_thumb(self.scroll_offset.y, total_lines)
            .is_some()
            && self.is_in_scrollbar_hit_zone(mouse_x)
        {
            self.scrollbar_drag = Some(ScrollbarDrag {
                start_mouse_y: mouse_y,
                start_scroll_y: self.scroll_offset.y,
            });
            cx.notify();
        }
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mouse_x = f32::from(event.position.x);
        let mouse_y = f32::from(event.position.y);

        if let Some(drag) = self.scrollbar_drag {
            let total_lines = self.line_count();
            if let Some((_thumb_y, thumb_height)) = self
                .last_viewport
                .vertical_scrollbar_thumb(self.scroll_offset.y, total_lines)
            {
                let track_span = (self.last_viewport.viewport_height - thumb_height).max(1.0);
                let max_scroll = self.last_viewport.max_scroll_y(total_lines);
                let dy = mouse_y - drag.start_mouse_y;
                let scroll_delta = (dy / track_span) * max_scroll;
                self.scroll_offset.y = self
                    .last_viewport
                    .clamp_scroll_y(drag.start_scroll_y + scroll_delta, total_lines);
                cx.notify();
            }
            return;
        }

        let hovered = self.is_in_scrollbar_hit_zone(mouse_x)
            && self.last_viewport.max_scroll_y(self.line_count()) > 0.0;
        if hovered != self.scrollbar_hovered {
            self.scrollbar_hovered = hovered;
            cx.notify();
        }
    }

    fn handle_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.scrollbar_drag.take().is_some() {
            cx.notify();
        }
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let _ = SCROLLBAR_THUMB_WIDTH_PX;
        div()
            .size_full()
            .relative()
            .track_focus(&self.focus_handle)
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
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
