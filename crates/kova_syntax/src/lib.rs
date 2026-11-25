//! `kova_syntax` — Incremental Tree-sitter parsing engine, language registry, and `Rope` adapters.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod engine;
pub mod error;
pub mod input;
pub mod registry;
pub mod tree;

pub use engine::{
    DEFAULT_PARSE_TIMEOUT, ParseJobHandle, ParseRequest, SyntaxEngine, SyntaxEngineConfig,
    SyntaxTreeStore,
};
pub use error::SyntaxError;
pub use input::{RopeBytesIter, RopeTextProvider, from_ts_point, parse_rope_with, to_ts_point};
pub use registry::{LanguageEntry, LanguageRegistry, detect_language};
pub use tree::{SyntaxEdit, SyntaxTree};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, LanguageRegistry, SyntaxEngine, SyntaxTree};

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_syntax");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }

    #[test]
    fn core_syntax_types_are_send_and_sync() {
        assert_send_sync::<SyntaxTree>();
        assert_send_sync::<LanguageRegistry>();
        assert_send_sync::<SyntaxEngine>();
    }
}
