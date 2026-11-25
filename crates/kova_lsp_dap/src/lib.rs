//! `kova_lsp_dap` — asynchronous LSP, BSP and DAP bridge with RAM limits and JVM auto-hibernation.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_lsp_dap");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
