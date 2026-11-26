//! [`gpui::EntityInputHandler`] for [`EditorView`]: typed text, dead keys and IME composition.
//!
//! The OS speaks document-wide UTF-16 offsets; [`EditorState`](crate::editor::state::EditorState)
//! converts them through the rope's UTF-16 metric in $O(\log N)$, so no handler scans the buffer.

use std::ops::Range;
use std::time::Instant;

use gpui::{
    Bounds, Context, EntityInputHandler, Pixels, Point, UTF16Selection, Window, point, px, size,
};

use crate::editor::EditorView;

impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<String> {
        let (text, used) = self.state.text_for_utf16_range(&range_utf16);
        *adjusted_range = Some(used);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<UTF16Selection> {
        let (range, reversed) = self.state.primary_selection_utf16();
        Some(UTF16Selection { range, reversed })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<Range<usize>> {
        self.state.marked_range_utf16()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.state.unmark() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let started = Instant::now();
        self.reset_cursor_blink(window, cx);
        if self.vim_owns_keys() {
            // Vim NORMAL / VISUAL: committed text only completes `f{char}` / `r{char}`.
            self.vim_text_input(new_text, started, cx);
            return;
        }
        let _changed = self
            .state
            .replace_text_in_range(range_utf16.as_ref(), new_text);
        self.finish_input(started, true, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.vim_owns_keys() {
            // No composition preview in Vim NORMAL / VISUAL; the commit arrives above.
            return;
        }
        let started = Instant::now();
        self.reset_cursor_blink(window, cx);
        let _changed = self.state.replace_and_mark_text_in_range(
            range_utf16.as_ref(),
            new_text,
            new_selected_range_utf16.as_ref(),
        );
        self.finish_input(started, true, cx);
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "row index fits cleanly within f32 mantissa"
    )]
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<Bounds<Pixels>> {
        let snap = self.state.snapshot();
        let start = snap
            .byte_to_point(self.state.utf16_to_byte(range_utf16.start))
            .ok()?;
        let end = snap
            .byte_to_point(self.state.utf16_to_byte(range_utf16.end))
            .ok()?;

        let x_start = self.x_for_row_col(start.row, start.column);
        let x_end = if end.row == start.row {
            self.x_for_row_col(end.row, end.column)
        } else {
            x_start
        };
        let lh = self.last_viewport.line_height;
        let y = start.row as f32 * lh - self.scroll_offset.y;
        let width = (x_end - x_start).max(self.last_viewport.char_advance);
        Some(Bounds::new(
            point(
                element_bounds.origin.x + px(x_start),
                element_bounds.origin.y + px(y),
            ),
            size(px(width), px(lh)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point_px: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<usize> {
        let (_, byte, _) = self.hit_test_point(point_px);
        Some(self.state.byte_to_utf16(byte))
    }
}
