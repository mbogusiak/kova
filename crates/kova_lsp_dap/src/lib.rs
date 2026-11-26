//! `kova_lsp_dap` — asynchronous LSP, BSP and DAP bridge with RAM limits and JVM auto-hibernation.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).
//!
//! The [`lsp`] module (KOVA-036) provides:
//! - [`lsp::transport`]: JSON-RPC 2.0 `Content-Length` framing over any async stream,
//! - [`lsp::session`]: the request/response multiplexer with timeouts and `$/cancelRequest`,
//! - [`lsp::lifecycle`]: the [`lsp::JdtlsClient`] state machine (`initialize` -> `Ready` -> `shutdown`),
//! - [`lsp::sync`]: incremental `textDocument/*` synchronisation driven by `kova_buffer::EditLog`.
//!
//! On top of it, KOVA-037 adds the Java semantics:
//! - [`lsp::diagnostics`]: the [`lsp::DiagnosticsStore`] fed by `publishDiagnostics`, with
//!   snapshots for hibernation,
//! - [`lsp::features`]: completion with auto-imports, inlay hints, hover, *Quick Fixes*
//!   (`Alt+Enter`), *Rename* (`Shift+F6`) and [`lsp::NormalizedWorkspaceEdit`],
//! - [`lsp::definition`]: *Go to Definition* with automatic Vineflower decompilation.
//!
//! The [`manager`] module (KOVA-038) provides the multi-project [`manager::JdtlsSessionManager`]
//! with RSS budgets, idle auto-hibernation of background `jdt.ls` processes and transparent,
//! single-flight wake-up.
//!
//! The [`bsp`] module (KOVA-040) talks to Gradle / Maven build servers: test runs stream
//! [`bsp::BspEvent`]s feeding the headless `kova_tools::junit::TestRun` tree.
//!
//! The [`dap`] module (KOVA-041) is the JVM debugger client: a Debug Adapter Protocol
//! session with `java-debug` (breakpoints, conditional breakpoints, logpoints, stepping,
//! threads, frames, lazily paged variables and *Evaluate Expression*).

pub mod bsp;
pub mod dap;
pub mod error;
pub mod lsp;
pub mod manager;

pub use error::LspDapError;

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
