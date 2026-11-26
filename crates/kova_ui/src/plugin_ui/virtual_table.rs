//! Virtualized data table for plugin declarative UI (`KOVA-045`).
//!
//! Uses GPUI's [`gpui::uniform_list`] so tables with thousands or tens of thousands of
//! rows (e.g. SQL query results from `kova-database`) only materialize and paint the
//! visible viewport slice at constant $O(\text{viewport})$ cost.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Div, FontWeight, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    UniformListScrollHandle, Window, div, px, relative, uniform_list,
};
use kova_plugin_host::ui::{TableColumn, TableRow, TableWidget, TextAlign, UiEvent};

use crate::controls::ControlPalette;
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Fixed row height in logical pixels required by [`gpui::uniform_list`].
pub const TABLE_ROW_HEIGHT_PX: f32 = 24.0;

/// Fixed sticky header height in logical pixels.
pub const TABLE_HEADER_HEIGHT_PX: f32 = 26.0;

/// Callback invoked when the user selects or double-clicks a table row.
pub type TableEventCallback = Rc<dyn Fn(UiEvent, &mut Window, &mut App)>;

/// Normalizes proportional column weights so they sum to `1.0`.
///
/// Zero-weighted columns are treated as weight `1` so no column ever collapses to zero.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "column weights are small u16 values summed within exact f32 precision"
)]
pub fn normalized_column_weights(columns: &[TableColumn]) -> Vec<f32> {
    if columns.is_empty() {
        return Vec::new();
    }
    let total: u64 = columns.iter().map(|c| u64::from(c.weight.max(1))).sum();
    let total_f = total.max(1) as f32;
    columns
        .iter()
        .map(|c| (u64::from(c.weight.max(1)) as f32) / total_f)
        .collect()
}

/// Returns the visible slice `(row_index, &TableRow)` for `range` clamped to `table.rows`.
#[must_use]
pub fn slice_visible_rows(table: &TableWidget, range: Range<usize>) -> Vec<(usize, &TableRow)> {
    let len = table.rows.len();
    let end = range.end.min(len);
    let start = range.start.min(end);
    table.rows[start..end]
        .iter()
        .enumerate()
        .map(|(offset, row)| (start + offset, row))
        .collect()
}

fn align_cell(el: Div, align: TextAlign) -> Div {
    match align {
        TextAlign::Left => el.justify_start(),
        TextAlign::Center => el.justify_center(),
        TextAlign::Right => el.justify_end(),
    }
}

/// Renders a [`TableWidget`] with a sticky header and a virtualized `uniform_list` body.
#[must_use]
pub fn render_virtual_table(
    table: &TableWidget,
    selected_row: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    theme: &KovaTheme,
    on_event: &TableEventCallback,
) -> Div {
    let surface_island = hsla_from_rgba(theme.surfaces().island);
    let surface_editor = hsla_from_rgba(theme.surfaces().editor);
    let border_subtle = hsla_from_rgba(theme.borders().subtle);
    let text_secondary = hsla_from_rgba(theme.text().secondary);
    let text_muted = hsla_from_rgba(theme.text().muted);

    let weights = normalized_column_weights(&table.columns);
    let header = render_table_header(
        &table.columns,
        &weights,
        surface_island,
        border_subtle,
        text_secondary,
    );

    let body: AnyElement = if table.rows.is_empty() {
        let msg = table
            .empty_message
            .as_deref()
            .unwrap_or("No rows to display.")
            .to_owned();
        div()
            .flex_1()
            .w_full()
            .min_h(px(72.0))
            .flex()
            .items_center()
            .justify_center()
            .p(px(12.0))
            .text_size(px(12.0))
            .text_color(text_muted)
            .child(msg)
            .into_any_element()
    } else {
        let widget_id = table.id.clone();
        let columns = table.columns.clone();
        let rows = table.rows.clone();
        let on_event = Rc::clone(on_event);
        let row_count = rows.len();

        uniform_list(
            SharedString::from(format!("plugin-table-{}", table.id)),
            row_count,
            move |range, _window, cx| {
                let theme = cx.global::<KovaTheme>();
                let end = range.end.min(rows.len());
                let start = range.start.min(end);
                (start..end)
                    .map(|row_index| {
                        render_table_row(
                            &widget_id,
                            row_index,
                            &rows[row_index],
                            &columns,
                            &weights,
                            selected_row == Some(row_index),
                            theme,
                            &on_event,
                        )
                    })
                    .collect()
            },
        )
        .track_scroll(scroll_handle)
        .flex_1()
        .w_full()
        .min_h(px(TABLE_ROW_HEIGHT_PX * 3.0))
        .into_any_element()
    };

    div()
        .size_full()
        .min_h(px(TABLE_HEADER_HEIGHT_PX + TABLE_ROW_HEIGHT_PX * 3.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(border_subtle)
        .bg(surface_editor)
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(header)
        .child(body)
}

fn render_table_header(
    columns: &[TableColumn],
    weights: &[f32],
    surface_island: gpui::Hsla,
    border_subtle: gpui::Hsla,
    text_secondary: gpui::Hsla,
) -> Div {
    let mut header = div()
        .w_full()
        .h(px(TABLE_HEADER_HEIGHT_PX))
        .flex_none()
        .flex()
        .items_center()
        .bg(surface_island)
        .border_b_1()
        .border_color(border_subtle)
        .text_size(px(11.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(text_secondary);

    for (col_idx, (col, &weight)) in columns.iter().zip(weights).enumerate() {
        let mut cell = div()
            .w(relative(weight))
            .h_full()
            .px(px(8.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap();
        if col_idx > 0 {
            cell = cell.border_l_1().border_color(border_subtle);
        }
        header = header.child(align_cell(cell, col.align).child(col.header.clone()));
    }
    header
}

#[allow(
    clippy::too_many_arguments,
    reason = "virtualized row renderer forwards row slice state and theme tokens"
)]
fn render_table_row(
    widget_id: &str,
    row_index: usize,
    row_data: &TableRow,
    columns: &[TableColumn],
    weights: &[f32],
    is_selected: bool,
    theme: &KovaTheme,
    on_event: &TableEventCallback,
) -> gpui::Stateful<Div> {
    let palette = ControlPalette::from_theme(theme);
    let surface_island = hsla_from_rgba(theme.surfaces().island);
    let surface_hover = hsla_from_rgba(theme.surfaces().hover);
    let surface_active = hsla_from_rgba(theme.surfaces().active);
    let border_subtle = hsla_from_rgba(theme.borders().subtle);
    let text_primary = hsla_from_rgba(theme.text().primary);

    let row_id = row_data.id.clone();
    let table_id = widget_id.to_owned();
    let cb = Rc::clone(on_event);

    let mut row_el = div()
        .id((SharedString::from(format!("tbl-{widget_id}")), row_index))
        .w_full()
        .h(px(TABLE_ROW_HEIGHT_PX))
        .flex()
        .items_center()
        .border_b_1()
        .border_color(border_subtle)
        .text_size(px(12.0))
        .text_color(text_primary)
        .cursor_pointer()
        .hover(move |s| s.bg(surface_hover))
        .on_click(move |event: &ClickEvent, window, cx| {
            let ui_event = if event.click_count() >= 2 {
                UiEvent::TableRowActivated {
                    widget_id: table_id.clone(),
                    row_index,
                    row_id: row_id.clone(),
                }
            } else {
                UiEvent::TableRowSelected {
                    widget_id: table_id.clone(),
                    row_index,
                    row_id: row_id.clone(),
                }
            };
            cb(ui_event, window, cx);
        });

    if is_selected {
        row_el = row_el.bg(surface_active);
    } else if row_index % 2 == 1 {
        row_el = row_el.bg(surface_island);
    }

    for (col_idx, (col, &weight)) in columns.iter().zip(weights).enumerate() {
        let cell_data = row_data.cells.get(col_idx);
        let cell_text = cell_data.map_or_else(String::new, |c| c.text.clone());
        let cell_color = cell_data
            .and_then(|c| c.variant)
            .map_or(text_primary, |v| palette.variant(v.into()).subtle_fg);

        let mut cell_el = div()
            .w(relative(weight))
            .h_full()
            .px(px(8.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_color(cell_color);
        if col_idx > 0 {
            cell_el = cell_el.border_l_1().border_color(border_subtle);
        }
        row_el = row_el.child(align_cell(cell_el, col.align).child(cell_text));
    }
    row_el
}

#[cfg(test)]
mod tests {
    use kova_plugin_host::ui::{StyleVariant, TableCell};

    use super::*;

    #[test]
    fn column_weights_normalize_to_unit_sum_and_avoid_zero() {
        let cols = vec![
            TableColumn {
                id: "a".to_owned(),
                header: "A".to_owned(),
                weight: 0,
                align: TextAlign::Left,
            },
            TableColumn {
                id: "b".to_owned(),
                header: "B".to_owned(),
                weight: 3,
                align: TextAlign::Right,
            },
        ];
        let weights = normalized_column_weights(&cols);
        assert_eq!(weights.len(), 2);
        assert!((weights[0] - 0.25).abs() < 1e-5);
        assert!((weights[1] - 0.75).abs() < 1e-5);
        assert!((weights.iter().sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn virtual_table_slices_100k_rows_in_o_viewport() {
        let rows: Vec<TableRow> = (0..100_000)
            .map(|i| TableRow {
                id: format!("r-{i}"),
                cells: vec![
                    TableCell::plain(format!("{i}")),
                    TableCell::styled("ok", StyleVariant::Accent),
                ],
            })
            .collect();
        let table = TableWidget {
            id: "big-sql".to_owned(),
            columns: vec![
                TableColumn {
                    id: "id".to_owned(),
                    header: "ID".to_owned(),
                    weight: 1,
                    align: TextAlign::Right,
                },
                TableColumn {
                    id: "status".to_owned(),
                    header: "Status".to_owned(),
                    weight: 2,
                    align: TextAlign::Left,
                },
            ],
            rows,
            selected_row: Some(50_000),
            empty_message: None,
        };

        let slice = slice_visible_rows(&table, 49_990..50_020);
        assert_eq!(slice.len(), 30);
        assert_eq!(slice[0].0, 49_990);
        assert_eq!(slice[0].1.id, "r-49990");
        assert_eq!(slice[29].0, 50_019);

        let clamped = slice_visible_rows(&table, 99_995..100_050);
        assert_eq!(clamped.len(), 5);
        assert_eq!(clamped[4].0, 99_999);
    }
}
