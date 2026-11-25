//! `kova_theme` — Aether semantic color token system for UI, Tree-sitter, terminal and plugins.
//!
//! This crate belongs to the **Headless Core**: it compiles and passes all tests
//! without any windowing or GPU dependency (enforced by KOVA-003).

pub mod color;
pub mod plugin;
pub mod registry;
pub mod syntax;
pub mod tokens;
pub mod toml_theme;

pub use color::{ColorParseError, RgbaColor};
pub use plugin::{PluginVariant, PluginVariantTokens, PluginVariants};
pub use registry::{ThemeRegistry, ThemeRegistryError};
pub use syntax::{SyntaxLanguage, SyntaxStyle, SyntaxTokenKind, SyntaxTokens, resolve_capture};
pub use tokens::{
    BorderTokens, EditorTokens, GutterTokens, SemanticTokens, SurfaceTokens, TerminalPalette,
    UiTextTokens,
};
pub use toml_theme::{AetherTheme, BuiltinTheme, ThemeLoadError, ThemeMetadata, ThemePolarity};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_theme");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
