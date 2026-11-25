//! `kova_ui` — GPUI presentation components: `IntelliJ` New UI dock, Focus/Tiling layout, editor and settings.
//!
//! GPU presentation layer (GPUI) — one of the only two crates allowed to depend
//! on windowing (`kova_ui` and `kova_app`).

pub mod shell;
pub mod theme;

pub use shell::{StartupMetrics, WorkspaceShell};
pub use theme::{KovaTheme, ThemeState, hsla_from_rgba, rgba_to_gpui};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_ui");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
