//! WCAG 2.1 contrast validation tests for built-in Aether themes.

use kova_theme::{
    AetherTheme, BuiltinTheme, PluginVariant, RgbaColor, SyntaxTokenKind,
    color::wcag::{WcagLevel, contrast_ratio, meets_wcag},
};

fn assert_contrast(theme_id: &str, label: &str, fg: RgbaColor, bg: RgbaColor, level: WcagLevel) {
    let ratio = contrast_ratio(fg, bg);
    assert!(
        meets_wcag(fg, bg, level),
        "[{theme_id}] WCAG check failed for `{label}`: {fg} on {bg} has contrast {ratio:.2}:1 (required >= {:.1}:1)",
        level.threshold()
    );
}

fn verify_surfaces_and_editor(theme: &AetherTheme) {
    let id = &theme.metadata.id;
    let t = &theme.tokens;

    for (label, fg, bg) in [
        (
            "editor.foreground vs editor.background",
            t.editor.foreground,
            t.editor.background,
        ),
        (
            "editor.foreground vs editor.line_highlight",
            t.editor.foreground,
            t.editor.line_highlight,
        ),
        (
            "text.primary vs surfaces.island",
            t.text.primary,
            t.surfaces.island,
        ),
        (
            "text.primary vs surfaces.editor",
            t.text.primary,
            t.surfaces.editor,
        ),
        (
            "text.primary vs surfaces.elevated",
            t.text.primary,
            t.surfaces.elevated,
        ),
    ] {
        assert_contrast(id, label, fg, bg, WcagLevel::AaaNormal);
    }

    for (label, fg, bg) in [
        (
            "editor.foreground vs editor.selection",
            t.editor.foreground,
            t.editor.selection,
        ),
        (
            "editor.foreground vs editor.find_match",
            t.editor.foreground,
            t.editor.find_match,
        ),
        (
            "editor.foreground vs editor.find_match_current",
            t.editor.foreground,
            t.editor.find_match_current,
        ),
        (
            "editor.inlay_hint_fg vs editor.inlay_hint_bg",
            t.editor.inlay_hint_fg,
            t.editor.inlay_hint_bg,
        ),
        (
            "text.secondary vs surfaces.island",
            t.text.secondary,
            t.surfaces.island,
        ),
        (
            "text.secondary vs surfaces.editor",
            t.text.secondary,
            t.surfaces.editor,
        ),
        (
            "text.muted vs surfaces.island",
            t.text.muted,
            t.surfaces.island,
        ),
        (
            "text.muted vs surfaces.editor",
            t.text.muted,
            t.surfaces.editor,
        ),
        (
            "text.inverse vs borders.focus",
            t.text.inverse,
            t.borders.focus,
        ),
        (
            "gutter.line_number vs gutter.background",
            t.gutter.line_number,
            t.gutter.background,
        ),
        (
            "gutter.line_number_active vs gutter.background",
            t.gutter.line_number_active,
            t.gutter.background,
        ),
    ] {
        assert_contrast(id, label, fg, bg, WcagLevel::AaNormal);
    }
}

fn verify_syntax_and_diagnostics(theme: &AetherTheme) {
    let id = &theme.metadata.id;
    let t = &theme.tokens;

    for kind in SyntaxTokenKind::ALL {
        let style = t.syntax.style(kind);
        assert_contrast(
            id,
            &format!("syntax.{kind:?}"),
            style.fg,
            t.editor.background,
            WcagLevel::AaNormal,
        );
    }

    for (label, fg) in [
        ("editor.diagnostic_error", t.editor.diagnostic_error),
        ("editor.diagnostic_warning", t.editor.diagnostic_warning),
        ("editor.diagnostic_info", t.editor.diagnostic_info),
        ("editor.diagnostic_hint", t.editor.diagnostic_hint),
    ] {
        assert_contrast(id, label, fg, t.editor.background, WcagLevel::AaNormal);
    }
}

fn verify_terminal_plugins_and_ui_controls(theme: &AetherTheme) {
    let id = &theme.metadata.id;
    let t = &theme.tokens;

    assert_contrast(
        id,
        "terminal.foreground vs terminal.background",
        t.terminal.foreground,
        t.terminal.background,
        WcagLevel::AaaNormal,
    );
    for ansi_idx in (1..=6).chain(8..=14) {
        let Some(ansi_color) = t.terminal.ansi(ansi_idx) else {
            panic!("ANSI index {ansi_idx} must exist");
        };
        assert_contrast(
            id,
            &format!("terminal.ansi({ansi_idx})"),
            ansi_color,
            t.terminal.background,
            WcagLevel::AaNormal,
        );
    }

    for variant in PluginVariant::ALL {
        let pv = theme.plugin_variant(variant);
        assert_contrast(
            id,
            &format!("plugins.{variant:?}.foreground vs background"),
            pv.foreground,
            pv.background,
            WcagLevel::AaNormal,
        );
        assert_contrast(
            id,
            &format!("plugins.{variant:?}.subtle_fg vs subtle_bg"),
            pv.subtle_fg,
            pv.subtle_bg,
            WcagLevel::AaNormal,
        );
        assert_contrast(
            id,
            &format!("plugins.{variant:?}.border vs surfaces.editor"),
            pv.border,
            t.surfaces.editor,
            WcagLevel::AaLargeOrUi,
        );
    }

    for (label, fg, bg) in [
        (
            "borders.focus vs surfaces.editor",
            t.borders.focus,
            t.surfaces.editor,
        ),
        (
            "borders.input vs surfaces.input",
            t.borders.input,
            t.surfaces.input,
        ),
        (
            "gutter.run_icon vs gutter.background",
            t.gutter.run_icon,
            t.gutter.background,
        ),
        (
            "gutter.debug_icon vs gutter.background",
            t.gutter.debug_icon,
            t.gutter.background,
        ),
        (
            "gutter.breakpoint vs gutter.background",
            t.gutter.breakpoint,
            t.gutter.background,
        ),
    ] {
        assert_contrast(id, label, fg, bg, WcagLevel::AaLargeOrUi);
    }
}

#[test]
fn builtin_themes_satisfy_all_wcag_thresholds() {
    for preset in BuiltinTheme::ALL {
        let theme = AetherTheme::builtin(preset);
        verify_surfaces_and_editor(&theme);
        verify_syntax_and_diagnostics(&theme);
        verify_terminal_plugins_and_ui_controls(&theme);
    }
}

#[test]
fn low_contrast_pair_fails_wcag_check() {
    let dark_bg = RgbaColor::rgb(0x1A, 0x20, 0x2B);
    let low_contrast_fg = RgbaColor::rgb(0x32, 0x3A, 0x4A);
    assert!(!meets_wcag(low_contrast_fg, dark_bg, WcagLevel::AaNormal));
    assert!(!meets_wcag(
        low_contrast_fg,
        dark_bg,
        WcagLevel::AaLargeOrUi
    ));
}
