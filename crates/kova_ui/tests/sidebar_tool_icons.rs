//! Integration tests for the `IntelliJ` New UI sidebar vector icons, stripe layout,
//! and hover tooltips (`KOVA-059`).

use std::sync::Arc;

use kova_theme::{AetherTheme, BuiltinTheme};
use kova_ui::dock::{
    DockLayoutState, DockSide, DockToolId, FocusedRegion, LEFT_STRIPE_BOTTOM_TOOLS,
    LEFT_STRIPE_TOP_TOOLS, LayoutDensity, PaneId, RIGHT_STRIPE_BOTTOM_TOOLS,
    RIGHT_STRIPE_TOP_TOOLS, SplitTree, ToolStripeTooltip, compute_region_rects, render_tool_icon,
    stripe_icon_color,
};
use kova_ui::{KovaTheme, hsla_from_rgba};

#[test]
fn stripe_rails_include_debug_and_preserve_island_geometry() {
    assert!(LEFT_STRIPE_TOP_TOOLS.contains(&DockToolId::Debug));
    assert_eq!(
        LEFT_STRIPE_TOP_TOOLS,
        [
            DockToolId::Project,
            DockToolId::Commit,
            DockToolId::Run,
            DockToolId::Debug,
        ]
    );
    assert_eq!(
        LEFT_STRIPE_BOTTOM_TOOLS,
        [
            DockToolId::Problems,
            DockToolId::Tests,
            DockToolId::Terminal,
        ]
    );
    assert_eq!(RIGHT_STRIPE_TOP_TOOLS, [DockToolId::Structure]);
    assert_eq!(RIGHT_STRIPE_BOTTOM_TOOLS.as_slice(), []);

    let mut layout = DockLayoutState::default();
    layout.toggle_tool(DockToolId::Debug, PaneId(1));
    assert_eq!(layout.bottom_open, Some(DockToolId::Debug));
    assert_eq!(layout.focused, FocusedRegion::BottomPanel);
    assert!(layout.is_tool_active(DockToolId::Debug));

    layout.toggle_tool(DockToolId::Structure, PaneId(1));
    assert_eq!(layout.right_open, Some(DockToolId::Structure));
    assert_eq!(DockToolId::Structure.side(), DockSide::Right);

    let split = SplitTree::new(PaneId(1));
    let rects = compute_region_rects(
        1440.0,
        900.0,
        &layout,
        &split,
        LayoutDensity::Islands,
        false,
    );
    assert!(rects.iter().any(|(r, _)| *r == FocusedRegion::ProjectDock));
    assert!(
        rects
            .iter()
            .any(|(r, _)| *r == FocusedRegion::StructureDock)
    );
    assert!(rects.iter().any(|(r, _)| *r == FocusedRegion::BottomPanel));
    assert!(
        rects
            .iter()
            .any(|(r, _)| *r == FocusedRegion::EditorPane(PaneId(1)))
    );
}

#[test]
fn tool_icons_and_tooltips_resolve_dark_and_light_theme_tokens() {
    let dark = KovaTheme::islands_dark();
    let light = KovaTheme::from_aether(Arc::new(AetherTheme::builtin(BuiltinTheme::KovaLight)), 1);

    for theme in [&dark, &light] {
        let primary = hsla_from_rgba(theme.text().primary);
        let secondary = hsla_from_rgba(theme.text().secondary);
        let muted = hsla_from_rgba(theme.text().muted);

        for tool in DockToolId::ALL {
            let tip = ToolStripeTooltip::new(tool, theme);
            assert_eq!(tip.formatted_label(), tool.tooltip_label());
            assert_eq!(tip.bg, hsla_from_rgba(theme.surfaces().elevated));
            assert_eq!(tip.border, hsla_from_rgba(theme.borders().strong));
            assert_eq!(tip.text_primary, primary);
            assert_eq!(tip.text_muted, muted);

            let resting = stripe_icon_color(false, false, primary, secondary, muted);
            let hovered = stripe_icon_color(false, true, primary, secondary, muted);
            let active = stripe_icon_color(true, false, primary, secondary, muted);
            drop(render_tool_icon(tool, resting));
            drop(render_tool_icon(tool, hovered));
            drop(render_tool_icon(tool, active));
        }
    }
}
