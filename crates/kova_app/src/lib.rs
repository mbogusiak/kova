//! `kova_app` — startup binary, GPUI initialization and event loop for Kova IDE.
//!
//! This crate is the application entry point and one of the only two crates
//! allowed to depend on GPU windowing (`kova_ui` and `kova_app`).

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Application entry point.
///
/// Placeholder: GPUI window and event-loop initialisation lands in epic E3.
pub fn run() {}

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, run};

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_app");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }

    #[test]
    fn run_is_a_noop_placeholder() {
        run();
    }
}
