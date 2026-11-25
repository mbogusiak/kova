//! Custom GPUI [`Element`] (`EditorCanvasElement`) rendering only visible lines at 120 FPS.

use std::fmt::Write as _;
use std::ops::Range;
use std::panic::Location;

use gpui::{
    App, Bounds, ContentMask, Element, ElementId, Entity, GlobalElementId, InspectorElementId,
    LayoutId, Pixels, ShapedLine, SharedString, Style, TextRun, Window, fill, point, px, relative,
    size,
};
use smallvec::SmallVec;

use crate::editor::EditorView;
use crate::editor::line_cache::LineCacheKey;
use crate::editor::viewport::{
    GUTTER_DIGITS_GAP_PX, GUTTER_FOLD_SLOT_PX, GUTTER_GIT_BAR_SLOT_PX, GUTTER_RIGHT_PAD_PX,
    GUTTER_RUN_SLOT_PX, SCROLLBAR_RIGHT_MARGIN_PX, SCROLLBAR_THUMB_RADIUS_PX,
    SCROLLBAR_THUMB_WIDTH_PX, ViewportMetrics,
};
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Prepaint payload passed from [`EditorCanvasElement::prepaint`] to [`EditorCanvasElement::paint`].
#[derive(Debug)]
pub struct EditorPrepaintState {
    bounds: Bounds<Pixels>,
    gutter_width_px: f32,
    line_height_px: f32,
    scroll_x: f32,
    scroll_y: f32,
    visible_range: Range<usize>,
    active_lines: SmallVec<[usize; 4]>,
    shaped_lines: Vec<(usize, ShapedLine, f32)>,
    shaped_gutter_numbers: Vec<(usize, ShapedLine)>,
    scrollbar_thumb: Option<(f32, f32)>,
    scrollbar_active: bool,
}

/// Virtualized GPUI canvas element for [`EditorView`].
#[derive(Debug)]
pub struct EditorCanvasElement {
    view: Entity<EditorView>,
}

impl EditorCanvasElement {
    /// Creates a new [`EditorCanvasElement`] bound to `view`.
    #[must_use]
    pub const fn new(view: Entity<EditorView>) -> Self {
        Self { view }
    }
}

impl gpui::IntoElement for EditorCanvasElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorCanvasElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport line counts and widths fit cleanly within f32"
    )]
    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = cx.global::<KovaTheme>().clone();
        let editor_fg = hsla_from_rgba(theme.editor().foreground);
        let gutter_fg = hsla_from_rgba(theme.gutter().line_number);
        let gutter_active_fg = hsla_from_rgba(theme.gutter().line_number_active);

        self.view.update(cx, |editor, _cx| {
            let snapshot = editor.snapshot();
            let total_lines = snapshot.len_lines().max(1);
            let font_size_px = editor.font_size_px();
            let gutter_font_size_px = (font_size_px - 1.0).max(9.0);
            let line_height_px = ViewportMetrics::quantized_line_height(font_size_px);

            let editor_font = editor.editor_font();
            let gutter_font = editor.gutter_font(false);
            let gutter_font_active = editor.gutter_font(true);

            let editor_font_size = px(font_size_px);
            let gutter_font_size = px(gutter_font_size_px);

            let char_advance =
                measure_char_advance(window, editor_font.clone(), editor_font_size, editor_fg);
            let gutter_char_advance =
                measure_char_advance(window, gutter_font.clone(), gutter_font_size, gutter_fg);

            let gutter_width_px =
                ViewportMetrics::gutter_width_px(total_lines, gutter_char_advance);
            let viewport_width_px = (f32::from(bounds.size.width) - gutter_width_px).max(0.0);
            let viewport_height_px = f32::from(bounds.size.height).max(0.0);

            let metrics = ViewportMetrics::new(
                line_height_px,
                char_advance,
                viewport_width_px,
                viewport_height_px,
            );
            editor.update_layout_metrics(metrics, gutter_width_px);

            let scroll_y = metrics.clamp_scroll_y(editor.scroll_offset().y, total_lines);
            let visible_range = metrics.visible_line_range(scroll_y, total_lines);
            let active_lines = editor.active_lines(&snapshot);

            editor.line_cache_mut().sync_context(
                snapshot.version().as_u64(),
                theme.revision,
                font_size_px,
            );

            let mut shaped_lines = Vec::with_capacity(visible_range.len());
            let mut shaped_gutter_numbers = Vec::with_capacity(visible_range.len());
            let mut max_visible_width_px: f32 = 0.0;

            let mut num_scratch = String::with_capacity(16);
            let scroll_x_raw = editor.scroll_offset().x.max(0.0);

            for row in visible_range.clone() {
                if let Some((shaped, x_offset)) = shape_visible_line(
                    editor,
                    &snapshot,
                    row,
                    scroll_x_raw,
                    char_advance,
                    viewport_width_px,
                    &editor_font,
                    editor_font_size,
                    editor_fg,
                    window,
                ) {
                    max_visible_width_px =
                        max_visible_width_px.max(x_offset + f32::from(shaped.width));
                    shaped_lines.push((row, shaped, x_offset));
                }

                let is_active = active_lines.contains(&row);
                let shaped_num = shape_gutter_number(
                    &mut num_scratch,
                    row,
                    is_active,
                    &gutter_font,
                    &gutter_font_active,
                    gutter_font_size,
                    gutter_fg,
                    gutter_active_fg,
                    window,
                );
                shaped_gutter_numbers.push((row, shaped_num));
            }

            let max_scroll_x =
                (max_visible_width_px + char_advance * 4.0 - viewport_width_px).max(0.0);
            editor.set_scroll_bounds_and_clamp(max_scroll_x, scroll_y);
            let scroll_x = editor.scroll_offset().x;
            let scrollbar_thumb = metrics.vertical_scrollbar_thumb(scroll_y, total_lines);
            let scrollbar_active = editor.is_scrollbar_active();

            EditorPrepaintState {
                bounds,
                gutter_width_px,
                line_height_px,
                scroll_x,
                scroll_y,
                visible_range,
                active_lines,
                shaped_lines,
                shaped_gutter_numbers,
                scrollbar_thumb,
                scrollbar_active,
            }
        })
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "visible row index fits cleanly within f32"
    )]
    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let theme = cx.global::<KovaTheme>().clone();
        let editor_bg = hsla_from_rgba(theme.editor().background);
        let gutter_bg = hsla_from_rgba(theme.gutter().background);
        let line_highlight = hsla_from_rgba(theme.editor().line_highlight);
        let border_subtle = hsla_from_rgba(theme.borders().subtle);
        let scrollbar_color = if prepaint.scrollbar_active {
            hsla_from_rgba(theme.surfaces().scrollbar_thumb_hover)
        } else {
            hsla_from_rgba(theme.surfaces().scrollbar_thumb)
        };

        let bounds = prepaint.bounds;
        let gutter_width = px(prepaint.gutter_width_px);
        let line_height = px(prepaint.line_height_px);

        let gutter_bounds = Bounds::new(bounds.origin, size(gutter_width, bounds.size.height));
        let content_bounds = Bounds::new(
            point(bounds.origin.x + gutter_width, bounds.origin.y),
            size(
                (bounds.size.width - gutter_width).max(px(0.0)),
                bounds.size.height,
            ),
        );

        // Layer 0: Editor and gutter backgrounds.
        window.paint_quad(fill(content_bounds, editor_bg));
        window.paint_quad(fill(gutter_bounds, gutter_bg));

        // Clip all editor painting strictly to the element bounds.
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // Layer 1: Active line highlight spanning both gutter and content area.
            for &active_row in &prepaint.active_lines {
                if prepaint.visible_range.contains(&active_row) {
                    let y = bounds.origin.y
                        + px(active_row as f32 * prepaint.line_height_px - prepaint.scroll_y);
                    let row_bounds = Bounds::new(
                        point(bounds.origin.x, y),
                        size(bounds.size.width, line_height),
                    );
                    window.paint_quad(fill(row_bounds, line_highlight));
                }
            }

            // Layer 4: Visible code lines clipped to content_bounds so horizontal scroll never overlaps gutter.
            window.with_content_mask(
                Some(ContentMask {
                    bounds: content_bounds,
                }),
                |window| {
                    for (row, shaped, x_offset) in &prepaint.shaped_lines {
                        let y = bounds.origin.y
                            + px(*row as f32 * prepaint.line_height_px - prepaint.scroll_y);
                        let x = content_bounds.origin.x + px(*x_offset - prepaint.scroll_x);
                        drop(shaped.paint(point(x, y), line_height, window, cx));
                    }
                },
            );

            // Layer 7a: Gutter line numbers right-aligned before the reserved interactive slots.
            let right_slots_width = GUTTER_DIGITS_GAP_PX
                + GUTTER_RUN_SLOT_PX
                + GUTTER_FOLD_SLOT_PX
                + GUTTER_GIT_BAR_SLOT_PX
                + GUTTER_RIGHT_PAD_PX;
            let digits_right_x = bounds.origin.x + px(prepaint.gutter_width_px - right_slots_width);

            window.with_content_mask(
                Some(ContentMask {
                    bounds: gutter_bounds,
                }),
                |window| {
                    for (row, shaped_num) in &prepaint.shaped_gutter_numbers {
                        let y = bounds.origin.y
                            + px(*row as f32 * prepaint.line_height_px - prepaint.scroll_y);
                        let x = digits_right_x - shaped_num.width;
                        drop(shaped_num.paint(point(x, y), line_height, window, cx));
                    }
                },
            );

            // Conditional 1px gutter separator when horizontally scrolled.
            if prepaint.scroll_x > 0.0 {
                let sep_bounds = Bounds::new(
                    point(bounds.origin.x + gutter_width - px(1.0), bounds.origin.y),
                    size(px(1.0), bounds.size.height),
                );
                window.paint_quad(fill(sep_bounds, border_subtle));
            }

            // Layer 7b: Overlay vertical scrollbar thumb.
            if let Some((thumb_y, thumb_height)) = prepaint.scrollbar_thumb {
                let thumb_x = bounds.origin.x + bounds.size.width
                    - px(SCROLLBAR_RIGHT_MARGIN_PX + SCROLLBAR_THUMB_WIDTH_PX);
                let thumb_bounds = Bounds::new(
                    point(thumb_x, bounds.origin.y + px(thumb_y)),
                    size(px(SCROLLBAR_THUMB_WIDTH_PX), px(thumb_height)),
                );
                let mut quad = fill(thumb_bounds, scrollbar_color);
                quad.corner_radii = px(SCROLLBAR_THUMB_RADIUS_PX).into();
                window.paint_quad(quad);
            }
        });
    }
}

fn measure_char_advance(
    window: &mut Window,
    font: gpui::Font,
    font_size: Pixels,
    color: gpui::Hsla,
) -> f32 {
    let sample_run = TextRun {
        len: 1,
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    f32::from(
        window
            .text_system()
            .shape_line(SharedString::from("0"), font_size, &[sample_run], None)
            .width,
    )
    .max(1.0)
}

#[allow(
    clippy::too_many_arguments,
    reason = "prepaint helper avoids allocating intermediate structs per visible row"
)]
fn shape_visible_line(
    editor: &mut EditorView,
    snapshot: &kova_buffer::BufferSnapshot,
    row: usize,
    scroll_x_raw: f32,
    char_advance: f32,
    viewport_width_px: f32,
    editor_font: &gpui::Font,
    editor_font_size: Pixels,
    editor_fg: gpui::Hsla,
    window: &mut Window,
) -> Option<(ShapedLine, f32)> {
    let line_slice = snapshot.line_slice(row).ok()?;
    let scratch = editor.line_scratch_mut();
    scratch.clear();
    for chunk in line_slice.chunks() {
        scratch.push_str(chunk);
    }

    let (byte_range, x_offset) = ViewportMetrics::visible_utf8_window(
        scratch,
        scroll_x_raw,
        char_advance,
        viewport_width_px,
    );
    let key = LineCacheKey::new(row, byte_range.start, byte_range.end);

    if editor.line_cache_mut().get(key).is_none() {
        let slice_str = &editor.line_scratch_mut()[byte_range];
        let text: SharedString = slice_str.to_owned().into();
        let run = TextRun {
            len: text.len(),
            font: editor_font.clone(),
            color: editor_fg,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window
            .text_system()
            .shape_line(text, editor_font_size, &[run], None);
        editor.line_cache_mut().insert(key, shaped);
    }

    editor
        .line_cache_mut()
        .get(key)
        .map(|shaped| (shaped.clone(), x_offset))
}

#[allow(
    clippy::too_many_arguments,
    reason = "gutter line-number shaping helper reuses a single stack/scratch buffer"
)]
fn shape_gutter_number(
    num_scratch: &mut String,
    row: usize,
    is_active: bool,
    gutter_font: &gpui::Font,
    gutter_font_active: &gpui::Font,
    gutter_font_size: Pixels,
    gutter_fg: gpui::Hsla,
    gutter_active_fg: gpui::Hsla,
    window: &mut Window,
) -> ShapedLine {
    num_scratch.clear();
    let _ = write!(num_scratch, "{}", row + 1);
    let num_text: SharedString = num_scratch.clone().into();
    let num_run = TextRun {
        len: num_text.len(),
        font: if is_active {
            gutter_font_active.clone()
        } else {
            gutter_font.clone()
        },
        color: if is_active {
            gutter_active_fg
        } else {
            gutter_fg
        },
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(num_text, gutter_font_size, &[num_run], None)
}
