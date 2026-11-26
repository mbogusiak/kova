//! `kova_tools` — JVM developer tools: Git, `.http` request runner, `JUnit` results model and PTY terminal.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).
//!
//! The [`junit`] module (KOVA-040) holds the headless `JUnit` *Test Runner* model fed by BSP
//! notifications or `TEST-*.xml` reports.

pub mod junit;

pub mod terminal;

pub mod http_runner;

pub mod git;

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_tools");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
