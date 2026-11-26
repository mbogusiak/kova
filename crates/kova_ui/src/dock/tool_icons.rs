//! Crisp `16×16 px` GPUI vector icons and hover tooltips for the `IntelliJ` New UI
//! tool-window stripe rails (`KOVA-059`).
//!
//! Icons follow the monochrome stroke style of `IntelliJ` New UI using active
//! [`KovaTheme`] tokens (`text.muted` → `text.secondary` → `text.primary`) and
//! draw directly on GPUI `canvas` / `PathBuilder` without external SVG assets.

use gpui::{
    AnyElement, Context, FontWeight, Hsla, IntoElement, ParentElement, PathBuilder, Render, Styled,
    Window, div, px,
};

use crate::dock::layout::DockToolId;
use crate::dock::project_icons::{
    paint_folder_outline, paint_outlined_rounded_rect, paint_rounded_rect, pt, stroke_segments,
    vector_icon,
};
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Resolves the monochrome icon stroke color for a stripe button state.
///
/// Active tools always use `text_primary`, hovered inactive tools use
/// `text_secondary`, and resting inactive tools use `text_muted`.
#[must_use]
pub const fn stripe_icon_color(
    is_active: bool,
    is_hovered: bool,
    text_primary: Hsla,
    text_secondary: Hsla,
    text_muted: Hsla,
) -> Hsla {
    if is_active {
        text_primary
    } else if is_hovered {
        text_secondary
    } else {
        text_muted
    }
}

/// Renders a crisp `16×16 px` monochrome vector icon for `tool` using `color`.
#[must_use]
pub fn render_tool_icon(tool: DockToolId, color: Hsla) -> AnyElement {
    vector_icon(16.0, 16.0, move |ox, oy, window| match tool {
        DockToolId::Project => paint_folder_outline(ox, oy, window, 1.35, color),
        DockToolId::Commit => paint_git_branch_icon(ox, oy, window, color),
        DockToolId::Run => paint_play_icon(ox, oy, window, color),
        DockToolId::Debug => paint_bug_icon(ox, oy, window, color),
        DockToolId::Problems => paint_warning_triangle_icon(ox, oy, window, color),
        DockToolId::Tests => paint_flask_icon(ox, oy, window, color),
        DockToolId::Terminal => paint_terminal_icon(ox, oy, window, color),
        DockToolId::Structure => paint_structure_tree_icon(ox, oy, window, color),
    })
}

fn paint_git_branch_icon(ox: f32, oy: f32, window: &mut Window, color: Hsla) {
    // Top, bottom, and right branch commit nodes (`3.4×3.4 px` circles, `rx = 1.7`).
    paint_outlined_rounded_rect(window, (ox, oy), (2.9, 2.3, 3.4, 3.4), 1.7, 1.3, color);
    paint_outlined_rounded_rect(window, (ox, oy), (2.9, 10.3, 3.4, 3.4), 1.7, 1.3, color);
    paint_outlined_rounded_rect(window, (ox, oy), (9.7, 3.7, 3.4, 3.4), 1.7, 1.3, color);

    // Vertical main trunk connecting the top and bottom nodes.
    stroke_segments(window, 1.35, color, &[&[(4.6, 5.7), (4.6, 10.3)]], ox, oy);

    // Smooth cubic Bézier branch from the lower trunk to the bottom of the right node.
    let mut builder = PathBuilder::stroke(px(1.35));
    builder.move_to(pt(ox, oy, 4.6, 10.3));
    builder.cubic_bezier_to(
        pt(ox, oy, 11.4, 7.1),
        pt(ox, oy, 4.6, 8.2),
        pt(ox, oy, 11.4, 9.2),
    );
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn paint_play_icon(ox: f32, oy: f32, window: &mut Window, color: Hsla) {
    let mut builder = PathBuilder::stroke(px(1.35));
    builder.move_to(pt(ox, oy, 4.6, 3.1));
    builder.line_to(pt(ox, oy, 12.8, 8.0));
    builder.line_to(pt(ox, oy, 4.6, 12.9));
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn paint_bug_icon(ox: f32, oy: f32, window: &mut Window, color: Hsla) {
    // Oval body.
    paint_outlined_rounded_rect(window, (ox, oy), (4.8, 5.2, 6.4, 8.0), 3.2, 1.3, color);
    // Head divider, wing line, antennae, and three pairs of legs.
    stroke_segments(
        window,
        1.3,
        color,
        &[
            &[(5.2, 7.6), (10.8, 7.6)],
            &[(8.0, 7.6), (8.0, 13.0)],
            &[(6.4, 5.2), (5.0, 3.1)],
            &[(9.6, 5.2), (11.0, 3.1)],
            &[(4.8, 7.0), (2.5, 6.0)],
            &[(11.2, 7.0), (13.5, 6.0)],
            &[(4.8, 9.4), (2.2, 9.4)],
            &[(11.2, 9.4), (13.8, 9.4)],
            &[(5.0, 11.6), (2.7, 12.8)],
            &[(11.0, 11.6), (13.3, 12.8)],
        ],
        ox,
        oy,
    );
}

fn paint_warning_triangle_icon(ox: f32, oy: f32, window: &mut Window, color: Hsla) {
    let mut builder = PathBuilder::stroke(px(1.35));
    builder.move_to(pt(ox, oy, 8.0, 2.5));
    builder.line_to(pt(ox, oy, 13.8, 13.0));
    builder.line_to(pt(ox, oy, 2.2, 13.0));
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }

    stroke_segments(window, 1.35, color, &[&[(8.0, 6.2), (8.0, 9.5)]], ox, oy);
    paint_rounded_rect(window, (ox, oy), (7.3, 10.6, 1.4, 1.4), 0.7, color);
}

fn paint_flask_icon(ox: f32, oy: f32, window: &mut Window, color: Hsla) {
    stroke_segments(
        window,
        1.35,
        color,
        &[
            &[(5.6, 2.6), (10.4, 2.6)],
            &[
                (6.5, 2.6),
                (6.5, 5.8),
                (2.9, 12.2),
                (3.5, 13.2),
                (12.5, 13.2),
                (13.1, 12.2),
                (9.5, 5.8),
                (9.5, 2.6),
            ],
            &[(4.3, 10.0), (11.7, 10.0)],
        ],
        ox,
        oy,
    );
}

fn paint_terminal_icon(ox: f32, oy: f32, window: &mut Window, color: Hsla) {
    paint_outlined_rounded_rect(window, (ox, oy), (2.0, 3.0, 12.0, 10.0), 2.0, 1.3, color);
    stroke_segments(
        window,
        1.35,
        color,
        &[
            &[(4.4, 5.8), (6.9, 8.0), (4.4, 10.2)],
            &[(8.1, 10.2), (11.5, 10.2)],
        ],
        ox,
        oy,
    );
}

fn paint_structure_tree_icon(ox: f32, oy: f32, window: &mut Window, color: Hsla) {
    paint_outlined_rounded_rect(window, (ox, oy), (2.4, 2.4, 4.0, 4.0), 1.0, 1.3, color);
    paint_outlined_rounded_rect(window, (ox, oy), (8.2, 6.2, 5.4, 3.2), 0.9, 1.3, color);
    paint_outlined_rounded_rect(window, (ox, oy), (8.2, 10.6, 5.4, 3.2), 0.9, 1.3, color);
    stroke_segments(
        window,
        1.3,
        color,
        &[
            &[(4.4, 6.4), (4.4, 12.2)],
            &[(4.4, 7.8), (8.2, 7.8)],
            &[(4.4, 12.2), (8.2, 12.2)],
        ],
        ox,
        oy,
    );
}

/// Lightweight GPUI tooltip view shown when hovering a tool-window stripe button.
#[derive(Debug, Clone, Copy)]
pub struct ToolStripeTooltip {
    /// Human-readable tool window name (for example, `"Project"`).
    pub title: &'static str,
    /// Platform-native keyboard shortcut label (for example, `"⌘1"` or `"Alt+1"`).
    pub shortcut: &'static str,
    /// Tooltip surface background resolved from [`KovaTheme::surfaces`].
    pub bg: Hsla,
    /// Tooltip border color resolved from [`KovaTheme::borders`].
    pub border: Hsla,
    /// Tool title color resolved from [`KovaTheme::text`].
    pub text_primary: Hsla,
    /// Shortcut label color resolved from [`KovaTheme::text`].
    pub text_muted: Hsla,
}

impl ToolStripeTooltip {
    /// Builds a [`ToolStripeTooltip`] for `tool` using the colors of `theme`.
    #[must_use]
    pub fn new(tool: DockToolId, theme: &KovaTheme) -> Self {
        Self {
            title: tool.title(),
            shortcut: tool.display_shortcut(),
            bg: hsla_from_rgba(theme.surfaces().elevated),
            border: hsla_from_rgba(theme.borders().strong),
            text_primary: hsla_from_rgba(theme.text().primary),
            text_muted: hsla_from_rgba(theme.text().muted),
        }
    }

    /// Formats the combined tooltip text as `"<Title> (<Shortcut>)"`.
    #[must_use]
    pub fn formatted_label(&self) -> String {
        format!("{} ({})", self.title, self.shortcut)
    }
}

impl Render for ToolStripeTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .bg(self.bg)
            .border_1()
            .border_color(self.border)
            .shadow_md()
            .child(
                div()
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(self.text_primary)
                    .child(self.title),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(self.text_muted)
                    .child(format!("({})", self.shortcut)),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kova_theme::{AetherTheme, BuiltinTheme};

    use super::*;

    fn light_theme() -> KovaTheme {
        KovaTheme::from_aether(Arc::new(AetherTheme::builtin(BuiltinTheme::KovaLight)), 0)
    }

    #[test]
    fn tooltip_resolves_titles_shortcuts_and_theme_tokens_for_dark_and_light() {
        let dark = KovaTheme::islands_dark();
        let light = light_theme();

        for tool in DockToolId::ALL {
            let dark_tip = ToolStripeTooltip::new(tool, &dark);
            assert_eq!(dark_tip.title, tool.title());
            assert_eq!(dark_tip.shortcut, tool.display_shortcut());
            assert_eq!(dark_tip.formatted_label(), tool.tooltip_label());
            assert_eq!(dark_tip.bg, hsla_from_rgba(dark.surfaces().elevated));
            assert_eq!(dark_tip.border, hsla_from_rgba(dark.borders().strong));
            assert_eq!(dark_tip.text_primary, hsla_from_rgba(dark.text().primary));
            assert_eq!(dark_tip.text_muted, hsla_from_rgba(dark.text().muted));

            let light_tip = ToolStripeTooltip::new(tool, &light);
            assert_eq!(light_tip.title, tool.title());
            assert_eq!(light_tip.shortcut, tool.display_shortcut());
            assert_eq!(light_tip.formatted_label(), tool.tooltip_label());
            assert_eq!(light_tip.bg, hsla_from_rgba(light.surfaces().elevated));
            assert_eq!(light_tip.border, hsla_from_rgba(light.borders().strong));
            assert_eq!(light_tip.text_primary, hsla_from_rgba(light.text().primary));
            assert_eq!(light_tip.text_muted, hsla_from_rgba(light.text().muted));
            assert_ne!(dark_tip.bg, light_tip.bg);
        }
    }

    #[test]
    fn stripe_icon_color_tracks_active_hover_and_resting_states() {
        for theme in [KovaTheme::islands_dark(), light_theme()] {
            let primary = hsla_from_rgba(theme.text().primary);
            let secondary = hsla_from_rgba(theme.text().secondary);
            let muted = hsla_from_rgba(theme.text().muted);

            assert_eq!(
                stripe_icon_color(false, false, primary, secondary, muted),
                muted
            );
            assert_eq!(
                stripe_icon_color(false, true, primary, secondary, muted),
                secondary
            );
            assert_eq!(
                stripe_icon_color(true, false, primary, secondary, muted),
                primary
            );
            assert_eq!(
                stripe_icon_color(true, true, primary, secondary, muted),
                primary
            );
        }
    }

    #[test]
    fn render_tool_icon_handles_every_dock_tool() {
        let color = hsla_from_rgba(KovaTheme::islands_dark().text().primary);
        for tool in DockToolId::ALL {
            let _element = render_tool_icon(tool, color);
        }
    }
}
