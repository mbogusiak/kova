//! Aether theme adapter mapping [`kova_theme`] semantic tokens to GPUI colors.

use std::sync::Arc;

use kova_theme::{
    AetherTheme, BorderTokens, BuiltinTheme, EditorTokens, GutterTokens, RgbaColor, SemanticTokens,
    SurfaceTokens, UiTextTokens,
};

/// Reactive theme state holding the active [`AetherTheme`] and a monotonic revision counter.
///
/// Prepared for live theme reloading in `KOVA-016`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeState {
    /// Currently active Aether theme definition.
    pub active: Arc<AetherTheme>,
    /// Monotonic revision incremented on each theme reload or switch.
    pub revision: u64,
}

impl ThemeState {
    /// Creates a new [`ThemeState`] at revision `0` from the given [`AetherTheme`].
    #[must_use]
    pub const fn new(active: Arc<AetherTheme>) -> Self {
        Self {
            active,
            revision: 0,
        }
    }

    /// Creates a [`ThemeState`] initialized to the built-in *Kova Islands Dark* theme.
    #[must_use]
    pub fn islands_dark() -> Self {
        Self::new(Arc::new(AetherTheme::builtin(
            BuiltinTheme::KovaIslandsDark,
        )))
    }

    /// Replaces the active theme and increments the monotonic `revision` counter.
    pub fn apply(&mut self, theme: Arc<AetherTheme>) -> u64 {
        self.active = theme;
        self.revision = self.revision.saturating_add(1);
        self.revision
    }
}

impl gpui::Global for ThemeState {}

/// Global GPUI theme state wrapping [`kova_theme::AetherTheme`].
#[derive(Debug, Clone)]
pub struct KovaTheme {
    /// Active Aether theme definition.
    pub active: Arc<AetherTheme>,
    /// Monotonic theme revision counter.
    pub revision: u64,
}

impl gpui::Global for KovaTheme {}

impl Default for KovaTheme {
    fn default() -> Self {
        Self::islands_dark()
    }
}

impl From<ThemeState> for KovaTheme {
    fn from(state: ThemeState) -> Self {
        Self {
            active: state.active,
            revision: state.revision,
        }
    }
}

impl From<&ThemeState> for KovaTheme {
    fn from(state: &ThemeState) -> Self {
        Self {
            active: Arc::clone(&state.active),
            revision: state.revision,
        }
    }
}

impl KovaTheme {
    /// Loads the built-in *Kova Islands Dark* theme from compiled-in memory without disk I/O.
    #[must_use]
    pub fn islands_dark() -> Self {
        Self {
            active: Arc::new(AetherTheme::builtin(BuiltinTheme::KovaIslandsDark)),
            revision: 0,
        }
    }

    /// Constructs a [`KovaTheme`] from an existing [`AetherTheme`] handle and revision number.
    #[must_use]
    pub const fn from_aether(theme: Arc<AetherTheme>, revision: u64) -> Self {
        Self {
            active: theme,
            revision,
        }
    }

    /// Returns a snapshot [`ThemeState`] matching this global theme.
    #[must_use]
    pub fn state(&self) -> ThemeState {
        ThemeState {
            active: Arc::clone(&self.active),
            revision: self.revision,
        }
    }

    /// Returns the human-readable display name of the active theme.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.active.metadata.name
    }

    /// Returns the canonical kebab-case identifier of the active theme.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.active.metadata.id
    }

    /// Returns the complete semantic token set for the active theme.
    #[must_use]
    pub fn tokens(&self) -> &SemanticTokens {
        &self.active.tokens
    }

    /// Returns the Islands UI surface color tokens (`[surfaces]`).
    #[must_use]
    pub fn surfaces(&self) -> &SurfaceTokens {
        &self.active.tokens.surfaces
    }

    /// Returns the UI typography color tokens (`[text]`).
    #[must_use]
    pub fn text(&self) -> &UiTextTokens {
        &self.active.tokens.text
    }

    /// Returns the structural border and focus ring tokens (`[borders]`).
    #[must_use]
    pub fn borders(&self) -> &BorderTokens {
        &self.active.tokens.borders
    }

    /// Returns the code editor viewport and diagnostic tokens (`[editor]`).
    #[must_use]
    pub fn editor(&self) -> &EditorTokens {
        &self.active.tokens.editor
    }

    /// Returns the editor gutter tokens (`[gutter]`).
    #[must_use]
    pub fn gutter(&self) -> &GutterTokens {
        &self.active.tokens.gutter
    }
}

/// Installs `theme` as the global [`KovaTheme`] (bumping its revision) and repaints every
/// window; editors drop their shaped-line caches on the revision change (`KOVA-016`).
///
/// Returns `false` (and does nothing) when `theme` equals the active theme.
pub fn apply_live_theme(theme: Arc<AetherTheme>, cx: &mut gpui::App) -> bool {
    let (unchanged, revision) = cx.try_global::<KovaTheme>().map_or((false, 0), |current| {
        (
            Arc::ptr_eq(&current.active, &theme) || *current.active == *theme,
            current.revision.saturating_add(1),
        )
    });
    if unchanged {
        return false;
    }
    cx.set_global(KovaTheme::from_aether(theme, revision));
    cx.refresh_windows();
    true
}
/// Losslessly converts an 8-bit sRGB [`RgbaColor`] to a normalized [`gpui::Rgba`].
#[must_use]
pub fn rgba_to_gpui(color: RgbaColor) -> gpui::Rgba {
    gpui::Rgba {
        r: f32::from(color.r) / 255.0,
        g: f32::from(color.g) / 255.0,
        b: f32::from(color.b) / 255.0,
        a: f32::from(color.a) / 255.0,
    }
}

/// Converts an 8-bit sRGB [`RgbaColor`] to a [`gpui::Hsla`] color for GPUI styling.
#[must_use]
pub fn hsla_from_rgba(color: RgbaColor) -> gpui::Hsla {
    gpui::Hsla::from(rgba_to_gpui(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-4,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn converts_opaque_and_translucent_colors_without_loss() {
        let theme = KovaTheme::islands_dark();
        let canvas = theme.surfaces().canvas;
        assert_eq!(canvas, RgbaColor::rgb(0x0D, 0x10, 0x17));

        let gpui_canvas = rgba_to_gpui(canvas);
        assert_close(gpui_canvas.r, 13.0 / 255.0);
        assert_close(gpui_canvas.g, 16.0 / 255.0);
        assert_close(gpui_canvas.b, 23.0 / 255.0);
        assert_close(gpui_canvas.a, 1.0);

        let editor = theme.surfaces().editor;
        assert_eq!(editor, RgbaColor::rgb(0x1A, 0x20, 0x2B));
        let hsla_editor = hsla_from_rgba(editor);
        let roundtrip_editor = gpui::Rgba::from(hsla_editor);
        assert_close(roundtrip_editor.r, f32::from(editor.r) / 255.0);
        assert_close(roundtrip_editor.g, f32::from(editor.g) / 255.0);
        assert_close(roundtrip_editor.b, f32::from(editor.b) / 255.0);
        assert_close(roundtrip_editor.a, 1.0);

        let overlay = theme.surfaces().overlay;
        assert_eq!(overlay, RgbaColor::rgba(0x09, 0x0C, 0x12, 0xCC));
        let hsla_overlay = hsla_from_rgba(overlay);
        assert_close(hsla_overlay.a, 204.0 / 255.0);
        let roundtrip_overlay = gpui::Rgba::from(hsla_overlay);
        assert_close(roundtrip_overlay.r, f32::from(overlay.r) / 255.0);
        assert_close(roundtrip_overlay.g, f32::from(overlay.g) / 255.0);
        assert_close(roundtrip_overlay.b, f32::from(overlay.b) / 255.0);
        assert_close(roundtrip_overlay.a, 204.0 / 255.0);
    }

    #[test]
    fn islands_dark_exposes_all_token_groups_and_reactive_state() {
        let theme = KovaTheme::islands_dark();
        assert_eq!(theme.id(), "kova-islands-dark");
        assert_eq!(theme.name(), "Kova Islands Dark");
        assert_eq!(theme.revision, 0);
        assert_eq!(
            theme.tokens().surfaces.statusbar,
            theme.surfaces().statusbar
        );
        assert_eq!(theme.tokens().text.primary, theme.text().primary);
        assert_eq!(theme.tokens().borders.island, theme.borders().island);
        assert_eq!(theme.tokens().editor.cursor, theme.editor().cursor);
        assert_eq!(theme.tokens().gutter.run_icon, theme.gutter().run_icon);

        let mut state = theme.state();
        let light = Arc::new(AetherTheme::builtin(BuiltinTheme::KovaLight));
        let rev = state.apply(Arc::clone(&light));
        assert_eq!(rev, 1);

        let updated = KovaTheme::from_aether(light, rev);
        assert_eq!(updated.id(), "kova-light");
        assert_eq!(updated.revision, 1);
    }
}
