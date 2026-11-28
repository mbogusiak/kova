//! `kova_jvm_index` — project symbol index, `.jar`/`.class` bytecode scanner and fuzzy search.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod bytecode;
pub mod cache;
pub mod classpath;
pub mod decompile_fallback;
pub mod discovery;
pub mod error;
pub mod external_lookup;
pub mod indexer;
pub mod jdk_lookup;
pub mod model;
pub mod search;
pub mod service;
pub mod signature;
pub mod snapshot;
pub mod source;

pub use bytecode::{
    ArchiveScanResult, ArchiveScanStats, BytecodeScanOptions, DEFAULT_MAX_CLASS_BYTES,
    DEFAULT_TARGET_JAVA_VERSION, JvmTypeSummary, parse_class_bytes, scan_jar_archive,
    summarize_class_bytes, type_parameter_names,
};
pub use classpath::{
    ClasspathDiscoveryConfig, ClasspathEntry, ClasspathOrigin, DEFAULT_MAX_JAR_COUNT,
    discover_classpath,
};
pub use decompile_fallback::{cache_bytecode_decompilation, decompile_class_bytes_to_java};
pub use discovery::{DiscoveredSourceFile, SourceDiscoveryOptions, collect_project_sources};
pub use error::JvmIndexError;
pub use external_lookup::{
    DEFAULT_MAX_DEPENDENCY_JARS, DependencyArchiveConfig, DependencyArchiveIndex,
    ExternalClassLocator, is_jdk_package_candidate,
};
pub use indexer::{
    IndexCancelToken, IndexStats, IndexerConfig, JvmWorkspaceIndex, WorkspaceIndexer,
};
pub use jdk_lookup::{
    JdkOnDemandIndex, JvmCompletionMember, LocatedClassBytes, SourceParamTableView,
    is_completable_type_name, is_public_api_type_name, split_display_signature,
};
pub use model::{
    JvmMemberEntry, JvmModifiers, JvmSourceOrigin, JvmSymbolKind, JvmTypeEntry, JvmVisibility,
    ParsedClassUnit, internal_name_to_binary, internal_name_to_fqcn,
    is_compiler_generated_class_name, is_ignored_internal_name, split_package_and_simple,
    split_package_and_simple_name,
};
pub use search::{
    ClassSearchHit, DEFAULT_SEARCH_LIMIT, JvmQueryEngine, ParsedClassQuery, QueryScratch,
    SearchQueryOptions, SymbolSearchHit, parse_class_query,
};
pub use service::{
    IndexJobHandle, IndexPhase, IndexReadyMetrics, IndexUpdateCallback, JvmIndexService,
};
pub use signature::{
    DecodedClassSignature, MAX_SIGNATURE_DEPTH, TypeDisplayMode, decode_class_signature,
    decode_constructor_descriptor, decode_constructor_signature, decode_field_descriptor,
    decode_field_descriptor_with_mode, decode_field_signature, decode_field_signature_with_mode,
    decode_method_descriptor, decode_method_descriptor_with_mode, decode_method_signature,
    decode_method_signature_with_mode, parse_class_signature,
};
pub use snapshot::JvmIndexSnapshot;
pub use source::{
    DEFAULT_MAX_SOURCE_BYTES, SourceScanOptions, parse_java_source, parse_kotlin_source,
    parse_source_file,
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
