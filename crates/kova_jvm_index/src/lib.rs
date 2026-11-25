//! `kova_jvm_index` — project symbol index, `.jar`/`.class` bytecode scanner and fuzzy search.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod bytecode;
pub mod error;
pub mod model;
pub mod signature;

pub use bytecode::{
    ArchiveScanResult, ArchiveScanStats, BytecodeScanOptions, DEFAULT_MAX_CLASS_BYTES,
    DEFAULT_TARGET_JAVA_VERSION, parse_class_bytes, scan_jar_archive,
};
pub use error::JvmIndexError;
pub use model::{
    JvmMemberEntry, JvmModifiers, JvmSourceOrigin, JvmSymbolKind, JvmTypeEntry, JvmVisibility,
    ParsedClassUnit, internal_name_to_binary, internal_name_to_fqcn,
    is_compiler_generated_class_name, is_ignored_internal_name, split_package_and_simple,
    split_package_and_simple_name,
};
pub use signature::{
    DecodedClassSignature, MAX_SIGNATURE_DEPTH, TypeDisplayMode, decode_class_signature,
    decode_constructor_descriptor, decode_constructor_signature, decode_field_descriptor,
    decode_field_descriptor_with_mode, decode_field_signature, decode_field_signature_with_mode,
    decode_method_descriptor, decode_method_descriptor_with_mode, decode_method_signature,
    decode_method_signature_with_mode, parse_class_signature,
};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_jvm_index");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
