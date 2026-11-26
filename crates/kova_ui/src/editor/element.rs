//! Custom GPUI [`Element`] (`EditorCanvasElement`) rendering only visible lines at 120 FPS.

use std::fmt::Write as _;
use std::ops::Range;
use std::panic::Location;

use gpui::{
    App, Bounds, ContentMask, CursorStyle, DispatchPhase, Element, ElementId, ElementInputHandler,
    Entity, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, LayoutId,
    MouseButton, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, ShapedLine, SharedString,
    Style, TextRun, Window, fill, point, px, relative, size,
};
use kova_buffer::BufferSnapshot;
use kova_syntax::{GutterIconRole, HighlightSpan};
use kova_theme::{RgbaColor, SyntaxTokens};
use smallvec::SmallVec;

use crate::editor::highlight::{build_line_text_runs, segment_line_styles};
use crate::editor::line_cache::LineCacheKey;
use crate::editor::syntax::EditorSyntax;
use crate::editor::viewport::{
    GUTTER_DIGITS_GAP_PX, GUTTER_FOLD_SLOT_PX, GUTTER_GIT_BAR_SLOT_PX, GUTTER_RIGHT_PAD_PX,
    GUTTER_RUN_SLOT_PX, SCROLLBAR_RIGHT_MARGIN_PX, SCROLLBAR_THUMB_RADIUS_PX,
    SCROLLBAR_THUMB_WIDTH_PX, ViewportMetrics,
};
use crate::editor::vim::CursorRenderKind;
use crate::editor::{EditorView, VisibleShapedLine};
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Side length of the gutter `▶` run triangle (`10×10 px`).
const RUN_ICON_SIZE_PX: f32 = 10.0;
/// Corner radius of the run-slot hover background and the fold pill (`rx = 4 px`).
const GUTTER_HOVER_RADIUS_PX: f32 = 4.0;
/// Horizontal padding inside the `... (N lines)` fold pill.
const FOLD_PILL_PAD_X_PX: f32 = 6.0;
/// Half extent of the fold chevron glyph.
const CHEVRON_HALF_PX: f32 = 3.5;

/// Gutter decorations of one visible row.
#[derive(Debug)]
struct GutterMarker {
    visual_row: usize,
    /// `Some(true)` collapsed (`›`), `Some(false)` expandable (`⌄`), `None` not foldable.
    fold: Option<bool>,
    /// `▶` icon role when the row holds a runnable entry point.
    run: Option<GutterIconRole>,
    /// Whether the row holds a caret.
    active: bool,
    run_hitbox: Option<Hitbox>,
    fold_hitbox: Option<Hitbox>,
}

impl GutterMarker {
    /// Returns the marker of buffer `row`, or `None` when it has no chevron and no `▶`.
    fn for_row(
        editor: &EditorView,
        syntax: Option<&EditorSyntax>,
        row: usize,
        visual_row: usize,
        active: bool,
    ) -> Option<Self> {
        let fold = if editor.fold_map().collapsed_at(row).is_some() {
            Some(true)
        } else {
            syntax.is_some_and(|s| s.has_fold_at(row)).then_some(false)
        };
        let run = syntax
            .and_then(|s| s.entry_point_at(row))
            .map(|e| e.icon_role);
        (fold.is_some() || run.is_some()).then_some(Self {
            visual_row,
            fold,
            run,
            active,
            run_hitbox: None,
            fold_hitbox: None,
        })
    }
}

/// Inserts the whole-gutter hitbox (chevron hover reveal) and one hitbox per icon slot.
fn insert_gutter_hitboxes(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    gutter_width_px: f32,
    geom: OverlayGeometry,
    markers: &mut [GutterMarker],
) -> Hitbox {
    let gutter_hitbox = window.insert_hitbox(
        Bounds::new(bounds.origin, size(px(gutter_width_px), bounds.size.height)),
        HitboxBehavior::Normal,
    );
    let (run_slot, fold_slot) = ViewportMetrics::gutter_slot_ranges(gutter_width_px);
    for marker in markers {
        let y = geom.row_y(marker.visual_row);
        let slot = |range: &Range<f32>| {
            Bounds::new(
                point(bounds.origin.x + px(range.start), px(y)),
                size(px(range.end - range.start), px(geom.line_height_px)),
            )
        };
        if marker.run.is_some() {
            marker.run_hitbox = Some(window.insert_hitbox(slot(&run_slot), HitboxBehavior::Normal));
        }
        if marker.fold.is_some() {
            marker.fold_hitbox =
                Some(window.insert_hitbox(slot(&fold_slot), HitboxBehavior::Normal));
        }
    }
    gutter_hitbox
}

/// Shapes and places the `... (N lines)` pills after every visible collapsed fold header.
fn build_fold_pills(
    editor: &EditorView,
    visible: &[VisibleShapedLine],
    label_run: &TextRun,
    font_size: Pixels,
    geom: OverlayGeometry,
    window: &mut Window,
) -> Vec<FoldPill> {
    let mut pills = Vec::new();
    for line in visible {
        let Some(region) = editor.fold_map().collapsed_at(line.row) else {
            continue;
        };
        let text: SharedString =
            format!("... ({} lines)", region.end_line - region.start_line).into();
        let run = TextRun {
            len: text.len(),
            ..label_run.clone()
        };
        let label = window
            .text_system()
            .shape_line(text, font_size, &[run], None);
        let x = geom.content_origin_x + line.x_offset - geom.scroll_x
            + f32::from(line.shaped.width)
            + geom.char_advance;
        let bounds = Bounds::new(
            point(px(x), px(geom.row_y(line.visual_row) + 2.0)),
            size(
                label.width + px(FOLD_PILL_PAD_X_PX * 2.0),
                px((geom.line_height_px - 4.0).max(1.0)),
            ),
        );
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        pills.push(FoldPill {
            row: line.row,
            bounds,
            label,
            hitbox,
        });
    }
    pills
}

/// `... (N lines)` pill painted after a collapsed fold header.
#[derive(Debug)]
struct FoldPill {
    row: usize,
    bounds: Bounds<Pixels>,
    label: ShapedLine,
    hitbox: Hitbox,
}

/// Per-frame syntax highlighting inputs for [`shape_visible_line`].
struct HighlightCtx<'a> {
    syntax: Option<&'a EditorSyntax>,
    rows: &'a [(usize, usize)],
    spans: Option<Vec<HighlightSpan>>,
    tokens: &'a SyntaxTokens,
    default_fg: RgbaColor,
}

impl HighlightCtx<'_> {
    /// Lazily queries highlight spans for every visible buffer row (one query per contiguous
    /// run of rows, so collapsed regions are never scanned). Runs only on a shaped-line cache
    /// miss, so steady-state frames do no Tree-sitter work at all.
    fn spans(&mut self) -> &[HighlightSpan] {
        let syntax = self.syntax;
        let rows = self.rows;
        self.spans.get_or_insert_with(|| {
            let mut out = Vec::new();
            let Some(syntax) = syntax else {
                return out;
            };
            let mut iter = rows.iter().map(|&(_, row)| row).peekable();
            while let Some(start) = iter.next() {
                let mut end = start;
                while let Some(&next) = iter.peek() {
                    if next != end + 1 {
                        break;
                    }
                    end = next;
                    iter.next();
                }
                syntax.highlight_lines_into(start..end + 1, &mut out);
            }
            out
        })
    }
}

/// Prepaint payload passed from [`EditorCanvasElement::prepaint`] to [`EditorCanvasElement::paint`].
#[derive(Debug)]
pub struct EditorPrepaintState {
    bounds: Bounds<Pixels>,
    gutter_width_px: f32,
    line_height_px: f32,
    scroll_x: f32,
    scroll_y: f32,
    visible_range: Range<usize>,
    active_visual_rows: SmallVec<[usize; 4]>,
    shaped_lines: Vec<(usize, ShapedLine, f32)>,
    shaped_gutter_numbers: Vec<(usize, ShapedLine)>,
    gutter_markers: Vec<GutterMarker>,
    fold_pills: Vec<FoldPill>,
    gutter_hitbox: Hitbox,
    overlays: Overlays,
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
        clippy::too_many_lines,
        reason = "viewport line counts and widths fit cleanly within f32; one linear \
                  per-frame pipeline (metrics, shaping, gutter markers, pills, overlays) kept \
                  in order"
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
        let pill_fg = hsla_from_rgba(theme.text().muted);
        let syntax_tokens = theme.tokens().syntax.clone();
        let default_fg = theme.editor().foreground;

        self.view.update(cx, |editor, _cx| {
            let snapshot = editor.snapshot();
            let total_lines = snapshot.len_lines().max(1);
            let total_visual = editor.fold_map().visible_line_count(total_lines).max(1);
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

            let scroll_y = metrics.clamp_scroll_y(editor.scroll_offset().y, total_visual);
            let visible_range = metrics.visible_line_range(scroll_y, total_visual);
            let active_lines = editor.active_lines(&snapshot);
            let rows: Vec<(usize, usize)> = {
                let fold_map = editor.fold_map();
                visible_range
                    .clone()
                    .map(|visual| (visual, fold_map.visual_to_buffer_line(visual)))
                    .filter(|&(_, row)| row < total_lines)
                    .collect()
            };
            let active_visual_rows: SmallVec<[usize; 4]> = active_lines
                .iter()
                .filter(|&&row| !editor.fold_map().is_hidden(row))
                .map(|&row| editor.fold_map().visual_row_of(row))
                .collect();

            let syntax_revision = editor.syntax_revision();
            editor.line_cache_mut().sync_context(
                snapshot.version().as_u64(),
                theme.revision,
                syntax_revision,
                font_size_px,
            );

            let syntax = editor.syntax().cloned();
            let mut highlight = HighlightCtx {
                syntax: syntax.as_deref(),
                rows: &rows,
                spans: None,
                tokens: &syntax_tokens,
                default_fg,
            };

            let mut shaped_lines = Vec::with_capacity(rows.len());
            let mut visible_shaped_cache = Vec::with_capacity(rows.len());
            let mut shaped_gutter_numbers = Vec::with_capacity(rows.len());
            let mut gutter_markers = Vec::new();
            let mut max_visible_width_px: f32 = 0.0;

            let mut num_scratch = String::with_capacity(16);
            let scroll_x_raw = editor.scroll_offset().x.max(0.0);

            for &(visual_row, row) in &rows {
                if let Some((shaped, byte_start, x_offset)) = shape_visible_line(
                    editor,
                    &snapshot,
                    row,
                    scroll_x_raw,
                    char_advance,
                    viewport_width_px,
                    &editor_font,
                    editor_font_size,
                    editor_fg,
                    &mut highlight,
                    window,
                ) {
                    max_visible_width_px =
                        max_visible_width_px.max(x_offset + f32::from(shaped.width));
                    visible_shaped_cache.push(VisibleShapedLine {
                        row,
                        visual_row,
                        shaped: shaped.clone(),
                        byte_start,
                        x_offset,
                    });
                    shaped_lines.push((visual_row, shaped, x_offset));
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
                shaped_gutter_numbers.push((visual_row, shaped_num));

                gutter_markers.extend(GutterMarker::for_row(
                    editor,
                    syntax.as_deref(),
                    row,
                    visual_row,
                    is_active,
                ));
            }

            let max_scroll_x =
                (max_visible_width_px + char_advance * 4.0 - viewport_width_px).max(0.0);
            editor.set_scroll_bounds_and_clamp(max_scroll_x, scroll_y);
            let scroll_x = editor.scroll_offset().x;

            let geom =
                OverlayGeometry::new(bounds, gutter_width_px, &metrics, editor.scroll_offset());

            let gutter_hitbox =
                insert_gutter_hitboxes(window, bounds, gutter_width_px, geom, &mut gutter_markers);
            let pill_font = TextRun {
                len: 0,
                font: editor_font.clone(),
                color: pill_fg,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let fold_pills = build_fold_pills(
                editor,
                &visible_shaped_cache,
                &pill_font,
                gutter_font_size,
                geom,
                window,
            );
            editor.set_fold_pills(fold_pills.iter().map(|p| (p.bounds, p.row)).collect());

            editor.set_render_cache(bounds, visible_shaped_cache);

            let focused = editor.focus_handle().is_focused(window);
            let mut overlays = compute_overlays(editor, &snapshot, geom, focused);
            if focused {
                let inverse = hsla_from_rgba(theme.text().inverse);
                shape_caret_glyphs(
                    &mut overlays,
                    &editor_font,
                    editor_font_size,
                    inverse,
                    window,
                );
            }

            EditorPrepaintState {
                bounds,
                gutter_width_px,
                line_height_px,
                scroll_x,
                scroll_y,
                visible_range,
                active_visual_rows,
                shaped_lines,
                shaped_gutter_numbers,
                gutter_markers,
                fold_pills,
                gutter_hitbox,
                overlays,
                scrollbar_thumb: metrics.vertical_scrollbar_thumb(scroll_y, total_visual),
                scrollbar_active: editor.is_scrollbar_active(),
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
            for &active_row in &prepaint.active_visual_rows {
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

            // Layers 2-6: selections, code lines, IME underline and carets (content area only).
            window.with_content_mask(
                Some(ContentMask {
                    bounds: content_bounds,
                }),
                |window| paint_content(prepaint, content_bounds, &theme, window, cx),
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
                    // Layer 7c: fold chevrons and `▶` run icons in their reserved slots.
                    paint_gutter_markers(prepaint, &theme, window);
                },
            );

            // Layer 7d: `... (N lines)` pills after collapsed fold headers.
            window.with_content_mask(
                Some(ContentMask {
                    bounds: content_bounds,
                }),
                |window| paint_fold_pills(prepaint, &theme, window, cx),
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

        self.register_input_handlers(bounds, window, cx);
        register_hover_refresh(prepaint, window);
    }
}

/// Paints fold chevrons (`›` collapsed, `⌄` expandable on gutter hover or caret line) and
/// `▶` run icons (with hover background and pointing-hand cursor).
#[allow(
    clippy::cast_precision_loss,
    reason = "visible row index fits cleanly within f32"
)]
fn paint_gutter_markers(prepaint: &EditorPrepaintState, theme: &KovaTheme, window: &mut Window) {
    let fold_color = hsla_from_rgba(theme.gutter().fold_marker);
    let run_color = hsla_from_rgba(theme.gutter().run_icon);
    let hover_bg = hsla_from_rgba(theme.surfaces().hover);
    let gutter_hovered = prepaint.gutter_hitbox.is_hovered(window);
    let (run_slot, fold_slot) = ViewportMetrics::gutter_slot_ranges(prepaint.gutter_width_px);
    let origin = prepaint.bounds.origin;
    let lh = prepaint.line_height_px;

    for marker in &prepaint.gutter_markers {
        let top = f32::from(origin.y) + marker.visual_row as f32 * lh - prepaint.scroll_y;
        let center_y = top + lh / 2.0;

        if let Some(role) = marker.run {
            let slot_x = f32::from(origin.x) + run_slot.start;
            let hovered = marker
                .run_hitbox
                .as_ref()
                .is_some_and(|hitbox| hitbox.is_hovered(window));
            if let Some(hitbox) = marker.run_hitbox.as_ref() {
                window.set_cursor_style(CursorStyle::PointingHand, hitbox);
            }
            if hovered {
                let side = GUTTER_RUN_SLOT_PX.min(lh);
                let mut quad = fill(
                    Bounds::new(
                        point(px(slot_x), px(center_y - side / 2.0)),
                        size(px(GUTTER_RUN_SLOT_PX), px(side)),
                    ),
                    hover_bg,
                );
                quad.corner_radii = px(GUTTER_HOVER_RADIUS_PX).into();
                window.paint_quad(quad);
            }
            let icon_x = slot_x + (GUTTER_RUN_SLOT_PX - RUN_ICON_SIZE_PX) / 2.0 + 0.5;
            paint_run_triangle(window, icon_x, center_y, RUN_ICON_SIZE_PX, run_color);
            if role == GutterIconRole::RunAll {
                // Test class (runs every test): a second, smaller chevron marks the group.
                paint_run_triangle(
                    window,
                    icon_x + 4.0,
                    center_y,
                    RUN_ICON_SIZE_PX * 0.6,
                    run_color,
                );
            }
        }

        if let Some(collapsed) = marker.fold {
            if let Some(hitbox) = marker.fold_hitbox.as_ref() {
                window.set_cursor_style(CursorStyle::PointingHand, hitbox);
            }
            if collapsed || gutter_hovered || marker.active {
                let cx = f32::from(origin.x) + f32::midpoint(fold_slot.start, fold_slot.end);
                paint_chevron(window, cx, center_y, collapsed, fold_color);
            }
        }
    }
}

/// Paints a filled right-pointing triangle of height `side` starting at `x`.
fn paint_run_triangle(window: &mut Window, x: f32, center_y: f32, side: f32, color: Hsla) {
    let mut builder = PathBuilder::fill();
    builder.move_to(point(px(x), px(center_y - side / 2.0)));
    builder.line_to(point(px(x + side * 0.866), px(center_y)));
    builder.line_to(point(px(x), px(center_y + side / 2.0)));
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Paints a stroked fold chevron: `›` when `collapsed`, `⌄` otherwise.
fn paint_chevron(window: &mut Window, cx: f32, cy: f32, collapsed: bool, color: Hsla) {
    let h = CHEVRON_HALF_PX;
    let mut builder = PathBuilder::stroke(px(1.4));
    if collapsed {
        builder.move_to(point(px(cx - h / 2.0), px(cy - h)));
        builder.line_to(point(px(cx + h / 2.0), px(cy)));
        builder.line_to(point(px(cx - h / 2.0), px(cy + h)));
    } else {
        builder.move_to(point(px(cx - h), px(cy - h / 2.0)));
        builder.line_to(point(px(cx), px(cy + h / 2.0)));
        builder.line_to(point(px(cx + h), px(cy - h / 2.0)));
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Paints the `... (N lines)` pills (`editor.inlay_hint_bg`, `text.muted`, `rx = 4 px`).
fn paint_fold_pills(
    prepaint: &EditorPrepaintState,
    theme: &KovaTheme,
    window: &mut Window,
    cx: &mut App,
) {
    let bg = hsla_from_rgba(theme.editor().inlay_hint_bg);
    for pill in &prepaint.fold_pills {
        window.set_cursor_style(CursorStyle::PointingHand, &pill.hitbox);
        let mut quad = fill(pill.bounds, bg);
        quad.corner_radii = px(GUTTER_HOVER_RADIUS_PX).into();
        window.paint_quad(quad);
        let text_origin = point(
            pill.bounds.origin.x + px(FOLD_PILL_PAD_X_PX),
            pill.bounds.origin.y,
        );
        drop(
            pill.label
                .paint(text_origin, pill.bounds.size.height, window, cx),
        );
    }
}

/// Repaints when the pointer enters/leaves the gutter or a `▶` slot, so the `⌄` reveal and
/// the run-icon hover background follow the mouse without per-move editor state.
fn register_hover_refresh(prepaint: &EditorPrepaintState, window: &mut Window) {
    let gutter = prepaint.gutter_hitbox.clone();
    let gutter_was = gutter.is_hovered(window);
    let runs: Vec<(Hitbox, bool)> = prepaint
        .gutter_markers
        .iter()
        .filter_map(|m| m.run_hitbox.clone())
        .map(|hitbox| {
            let was = hitbox.is_hovered(window);
            (hitbox, was)
        })
        .collect();
    window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, _cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        let changed = gutter.is_hovered(window) != gutter_was
            || runs
                .iter()
                .any(|(hitbox, was)| hitbox.is_hovered(window) != *was);
        if changed {
            window.refresh();
        }
    });
}

/// Pixel geometry shared by the selection, caret and IME overlay computation.
#[derive(Debug, Clone, Copy)]
struct OverlayGeometry {
    content_origin_x: f32,
    origin_y: f32,
    scroll_x: f32,
    scroll_y: f32,
    line_height_px: f32,
    char_advance: f32,
}

impl OverlayGeometry {
    fn new(
        bounds: Bounds<Pixels>,
        gutter_width_px: f32,
        metrics: &ViewportMetrics,
        scroll: gpui::Point<f32>,
    ) -> Self {
        Self {
            content_origin_x: f32::from(bounds.origin.x) + gutter_width_px,
            origin_y: f32::from(bounds.origin.y),
            scroll_x: scroll.x,
            scroll_y: scroll.y,
            line_height_px: metrics.line_height,
            char_advance: metrics.char_advance,
        }
    }

    /// Window-space y of the top edge of `visual_row`.
    #[allow(
        clippy::cast_precision_loss,
        reason = "visible row indices fit cleanly within f32"
    )]
    fn row_y(&self, visual_row: usize) -> f32 {
        self.origin_y + (visual_row as f32 * self.line_height_px - self.scroll_y)
    }
}

/// Selection, caret and IME-underline rectangles for the visible rows only.
#[derive(Debug)]
struct Overlays {
    selections: Vec<Bounds<Pixels>>,
    /// Caret rectangles with their corner radius.
    carets: Vec<(Bounds<Pixels>, f32)>,
    /// Glyph under each block caret: `(origin, text)` (shaped in prepaint into `caret_glyphs`).
    block_glyphs: Vec<(gpui::Point<Pixels>, SharedString)>,
    /// Glyphs redrawn above block carets in `text.inverse`, bold.
    caret_glyphs: Vec<(gpui::Point<Pixels>, ShapedLine)>,
    ime_underlines: Vec<Bounds<Pixels>>,
    caret_kind: CursorRenderKind,
    focused: bool,
    caret_visible: bool,
}

/// Paints selections, visible code lines, the IME underline and carets (in that order).
#[allow(
    clippy::cast_precision_loss,
    reason = "visible row index fits cleanly within f32"
)]
fn paint_content(
    prepaint: &EditorPrepaintState,
    content_bounds: Bounds<Pixels>,
    theme: &KovaTheme,
    window: &mut Window,
    cx: &mut App,
) {
    let overlays = &prepaint.overlays;
    let selection_color = if overlays.focused {
        hsla_from_rgba(theme.editor().selection)
    } else {
        hsla_from_rgba(theme.editor().selection_inactive)
    };
    for &rect in &overlays.selections {
        window.paint_quad(fill(rect, selection_color));
    }
    let line_height = px(prepaint.line_height_px);
    for (row, shaped, x_offset) in &prepaint.shaped_lines {
        let y = prepaint.bounds.origin.y
            + px(*row as f32 * prepaint.line_height_px - prepaint.scroll_y);
        let x = content_bounds.origin.x + px(*x_offset - prepaint.scroll_x);
        drop(shaped.paint(point(x, y), line_height, window, cx));
    }
    let ime_color = hsla_from_rgba(theme.borders().focus);
    for &rect in &overlays.ime_underlines {
        window.paint_quad(fill(rect, ime_color));
    }
    if overlays.focused && overlays.caret_visible {
        let caret_color = hsla_from_rgba(theme.editor().cursor);
        for &(rect, radius) in &overlays.carets {
            let mut quad = fill(rect, caret_color);
            quad.corner_radii = px(radius).into();
            window.paint_quad(quad);
        }
        // Block caret: redraw the covered glyph in `text.inverse` for WCAG AAA contrast.
        for (origin, glyph) in &overlays.caret_glyphs {
            drop(glyph.paint(*origin, line_height, window, cx));
        }
    }
}

/// Returns the `(x_start, x_end)` of byte range `range` intersected with visible `line`,
/// or `None` when they do not intersect. `x_end` covers the trailing `\n` cell when the
/// range continues past the end of the line and `include_newline` is set.
fn span_x_range(
    snapshot: &BufferSnapshot,
    line: &VisibleShapedLine,
    range: &Range<usize>,
    include_newline: bool,
    geom: OverlayGeometry,
) -> Option<(f32, f32)> {
    let row = line.row;
    let line_start = snapshot.rope().line_to_byte(row);
    let line_len = snapshot.line_len_bytes(row).unwrap_or(0);
    let line_end = line_start + line_len;
    let limit = if include_newline {
        snapshot
            .line_slice_with_ending(row)
            .map_or(line_end, |s| line_start + s.len_bytes())
    } else {
        line_end
    };
    if range.end <= line_start || range.start >= limit {
        return None;
    }
    let local_start = range.start.saturating_sub(line_start).min(line_len);
    let local_end = range.end.saturating_sub(line_start).min(line_len);
    let x = |local| {
        x_for_line_byte(
            &line.shaped,
            line.byte_start,
            line.x_offset,
            local,
            geom.content_origin_x,
            geom.scroll_x,
        )
    };
    let x1 = x(local_start);
    let mut x2 = x(local_end);
    if include_newline && range.end > line_end {
        x2 += geom.char_advance;
    }
    (x2 > x1).then_some((x1, x2))
}

fn compute_overlays(
    editor: &EditorView,
    snapshot: &BufferSnapshot,
    geom: OverlayGeometry,
    focused: bool,
) -> Overlays {
    let mut out = Overlays {
        selections: Vec::new(),
        carets: Vec::new(),
        block_glyphs: Vec::new(),
        caret_glyphs: Vec::new(),
        ime_underlines: Vec::new(),
        caret_kind: editor.cursor_render_kind(),
        focused,
        caret_visible: editor.is_cursor_visible(),
    };
    let lh = geom.line_height_px;
    let row_y = |visual_row: usize| geom.row_y(visual_row);
    let visible = editor.visible_shaped_lines();

    for sel in editor.selections().as_slice() {
        if !sel.is_empty() {
            let range = sel.range();
            for line in visible {
                if let Some((x1, x2)) = span_x_range(snapshot, line, &range, true, geom) {
                    out.selections.push(Bounds::new(
                        point(px(x1), px(row_y(line.visual_row))),
                        size(px(x2 - x1), px(lh)),
                    ));
                }
            }
        }

        let head = snapshot.clamp_byte_to_char_boundary(sel.head().min(snapshot.len_bytes()));
        let Ok(pt) = snapshot.byte_to_point(head) else {
            continue;
        };
        // Hidden (folded) or off-screen rows have no visible shaped line.
        let Some(line) = visible.iter().find(|l| l.row == pt.row) else {
            continue;
        };
        let x = x_for_line_byte(
            &line.shaped,
            line.byte_start,
            line.x_offset,
            pt.column,
            geom.content_origin_x,
            geom.scroll_x,
        );
        let y = row_y(line.visual_row);
        // Width of the glyph under the caret; one `char_advance` at the end of the line.
        let shaped = &line.shaped;
        let rel = pt
            .column
            .saturating_sub(line.byte_start)
            .min(shaped.text.len());
        let glyph = shaped.text[rel..].chars().next().filter(|c| *c != '\n');
        let cell = glyph.map_or(geom.char_advance, |c| {
            let next = rel + c.len_utf8();
            (f32::from(shaped.x_for_index(next)) - f32::from(shaped.x_for_index(rel)))
                .max(geom.char_advance * 0.5)
        });
        let caret = out.caret_kind.rect(x, y, cell, lh);
        out.carets.push((
            Bounds::new(
                point(px(caret.x), px(caret.y)),
                size(px(caret.width), px(caret.height)),
            ),
            caret.radius,
        ));
        if out.caret_kind.redraws_glyph()
            && let Some(c) = glyph.filter(|c| !c.is_whitespace())
        {
            out.block_glyphs
                .push((point(px(x), px(y)), SharedString::from(c.to_string())));
        }
    }

    if let Some(marked) = editor.marked_range() {
        for line in visible {
            if let Some((x1, x2)) = span_x_range(snapshot, line, &marked, false, geom) {
                out.ime_underlines.push(Bounds::new(
                    point(px(x1), px(row_y(line.visual_row) + lh - 1.0)),
                    size(px(x2 - x1), px(1.0)),
                ));
            }
        }
    }
    out
}

impl EditorCanvasElement {
    /// Registers the IME/text input handler for this frame (effective only while focused) and
    /// window-level drag/release listeners, so a selection drag keeps tracking (and
    /// auto-scrolling) after the pointer leaves the editor bounds.
    fn register_input_handlers(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let focus_handle = self.view.read(cx).focus_handle().clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        let drag_view = self.view.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                drag_view.update(cx, |editor, cx| editor.handle_drag_move(event, window, cx));
            }
        });
        let up_view = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                up_view.update(cx, |editor, cx| editor.handle_mouse_up(event, window, cx));
            }
        });
    }
}

/// Shapes the glyphs under block carets in bold `text.inverse` (`KOVA-026` FR-4.2, NFR-3).
fn shape_caret_glyphs(
    overlays: &mut Overlays,
    editor_font: &gpui::Font,
    font_size: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let mut font = editor_font.clone();
    font.weight = gpui::FontWeight::BOLD;
    overlays.caret_glyphs = overlays
        .block_glyphs
        .drain(..)
        .map(|(origin, text)| {
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window
                .text_system()
                .shape_line(text, font_size, &[run], None);
            (origin, shaped)
        })
        .collect();
}

fn measure_char_advance(
    window: &mut Window,
    font: gpui::Font,
    font_size: Pixels,
    color: Hsla,
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

fn x_for_line_byte(
    shaped: &ShapedLine,
    byte_start: usize,
    x_offset: f32,
    local_byte: usize,
    content_origin_x: f32,
    scroll_x: f32,
) -> f32 {
    let rel_byte = local_byte.saturating_sub(byte_start).min(shaped.text.len());
    content_origin_x + x_offset - scroll_x + f32::from(shaped.x_for_index(rel_byte))
}

#[allow(
    clippy::too_many_arguments,
    reason = "prepaint helper avoids allocating intermediate structs per visible row"
)]
fn shape_visible_line(
    editor: &mut EditorView,
    snapshot: &BufferSnapshot,
    row: usize,
    scroll_x_raw: f32,
    char_advance: f32,
    viewport_width_px: f32,
    editor_font: &gpui::Font,
    editor_font_size: Pixels,
    editor_fg: Hsla,
    highlight: &mut HighlightCtx<'_>,
    window: &mut Window,
) -> Option<(ShapedLine, usize, f32)> {
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
    let byte_start = byte_range.start;
    let key = LineCacheKey::new(row, byte_range.start, byte_range.end);

    if editor.line_cache_mut().get(key).is_none() {
        let line_str = editor.line_scratch_mut().as_str();
        let text: SharedString = line_str[byte_range.clone()].to_owned().into();
        // Stale-safe: spans and line start come from the snapshot the tree was parsed from.
        let line_start = highlight.syntax.and_then(|s| s.line_start_byte(row));
        let mut runs = match line_start {
            Some(line_start) => {
                let tokens = highlight.tokens;
                let default_fg = highlight.default_fg;
                let segments = segment_line_styles(
                    line_str,
                    line_start,
                    byte_range,
                    highlight.spans(),
                    tokens,
                    default_fg,
                );
                build_line_text_runs(&segments, editor_font)
            }
            None => Vec::new(),
        };
        if runs.iter().map(|r| r.len).sum::<usize>() != text.len() {
            runs = vec![TextRun {
                len: text.len(),
                font: editor_font.clone(),
                color: editor_fg,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
        }
        let shaped = window
            .text_system()
            .shape_line(text, editor_font_size, &runs, None);
        editor.line_cache_mut().insert(key, shaped);
    }

    editor
        .line_cache_mut()
        .get(key)
        .map(|shaped| (shaped.clone(), byte_start, x_offset))
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
    gutter_fg: Hsla,
    gutter_active_fg: Hsla,
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
